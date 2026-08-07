//! Integration tests for the authentication gate (ADR-0003).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use argon2::password_hash::{PasswordHasher as _, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use reqwest::{Client, StatusCode, redirect::Policy};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use cor_code::acp::ScriptedAdapter;
use cor_code::auth::keystore::KeyStore;
use cor_code::auth::rate_limit::FREE_ATTEMPTS;
use cor_code::auth::session::{self, LIFETIME, REFRESH_AFTER, SigningKey};
use cor_code::chats::Chats;
use cor_code::config::{
    Config, DEFAULT_CONTAINER_CPUS, DEFAULT_CONTAINER_MEMORY_MB, DEFAULT_WARM_POOL,
};
use cor_code::git::{GITHUB, Remotes};
use cor_code::plane::MemoryPlane;
use cor_code::secrets::Secrets;
use cor_code::server::{self, SESSION_COOKIE};
use cor_code::settings::Settings;
use cor_code::store::{ChatStore, Owner};
use cor_code::verify::ScriptedVerifier;

const USERNAME: &str = "cassidy";
const PASSWORD: &str = "correct horse battery staple";

#[tokio::test]
async fn an_unauthenticated_request_is_sent_to_the_login_form() {
    let app = TestApp::start().await;

    let response = client().get(app.url("/")).send().await.expect("request");

    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/login");
    app.stop().await;
}

#[tokio::test]
async fn a_route_outside_the_public_list_is_gated_without_opting_in() {
    let app = TestApp::start().await;

    let response = client()
        .get(app.url("/a-route-a-later-increment-adds"))
        .send()
        .await
        .expect("request");

    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/login");
    app.stop().await;
}

#[tokio::test]
async fn the_login_form_is_served_without_a_session() {
    let app = TestApp::start().await;

    let response = client()
        .get(app.url("/login"))
        .send()
        .await
        .expect("request");

    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.text().await.expect("body").contains("<form"));
    app.stop().await;
}

#[tokio::test]
async fn a_wrong_password_is_rejected_without_a_cookie() {
    let app = TestApp::start().await;

    let response = app.attempt_login(USERNAME, "hunter2").await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response.headers().get("set-cookie").is_none());
    assert!(response.text().await.expect("body").contains("<form"));
    app.stop().await;
}

#[tokio::test]
async fn an_unknown_username_is_rejected_without_a_cookie() {
    let app = TestApp::start().await;

    let response = app.attempt_login("intruder", PASSWORD).await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response.headers().get("set-cookie").is_none());
    app.stop().await;
}

#[tokio::test]
async fn a_correct_login_hands_out_a_cookie_that_opens_the_app() {
    let app = TestApp::start().await;

    let response = app.attempt_login(USERNAME, PASSWORD).await;

    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/");
    let cookie = set_cookie(&response);
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Strict"), "{cookie}");
    assert!(cookie.contains("Path=/"), "{cookie}");
    assert!(!cookie.contains("Secure"), "{cookie}");

    let home = client()
        .get(app.url("/"))
        .header("cookie", session_cookie(&cookie))
        .send()
        .await
        .expect("request");
    assert_eq!(home.status(), StatusCode::OK);
    app.stop().await;
}

#[tokio::test]
async fn an_expired_cookie_is_sent_to_the_login_form() {
    let app = TestApp::start().await;
    let stale = app.cookie_issued_at(SystemTime::now() - LIFETIME - Duration::from_secs(60));

    let response = app.get_home(&stale).await;

    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/login");
    app.stop().await;
}

#[tokio::test]
async fn a_tampered_cookie_is_sent_to_the_login_form() {
    let app = TestApp::start().await;
    let valid = app.cookie_issued_at(SystemTime::now());
    let forged = format!("{valid}tampered");

    let response = app.get_home(&forged).await;

    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/login");
    app.stop().await;
}

#[tokio::test]
async fn an_aging_cookie_is_re_issued_with_a_later_expiry() {
    let app = TestApp::start().await;
    let aging = app.cookie_issued_at(SystemTime::now() - REFRESH_AFTER - Duration::from_secs(60));

    let refreshed = app.get_home(&aging).await;

    assert_eq!(refreshed.status(), StatusCode::OK);
    let reissued = session_cookie(&set_cookie(&refreshed));
    assert!(
        app.expiry_of(&reissued) > app.expiry_of(&aging),
        "the refreshed cookie should outlive the one it replaces"
    );
    assert_eq!(app.get_home(&reissued).await.status(), StatusCode::OK);
    app.stop().await;
}

#[tokio::test]
async fn a_young_cookie_is_left_as_it_is() {
    let app = TestApp::start().await;
    let young = app.cookie_issued_at(SystemTime::now());

    let response = app.get_home(&young).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get("set-cookie").is_none());
    app.stop().await;
}

#[tokio::test]
async fn rotating_the_key_invalidates_existing_cookies() {
    let app = TestApp::start().await;
    let cookie = app.cookie_issued_at(SystemTime::now());

    let logout = client()
        .post(app.url("/logout-all"))
        .header("cookie", &cookie)
        .send()
        .await
        .expect("request");

    assert_eq!(logout.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&logout), "/login");
    let after = app.get_home(&cookie).await;
    assert_eq!(after.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&after), "/login");
    app.stop().await;
}

#[tokio::test]
async fn logging_out_everywhere_hands_back_no_live_cookie() {
    let app = TestApp::start().await;
    let aging = app.cookie_issued_at(SystemTime::now() - REFRESH_AFTER - Duration::from_secs(60));

    let logout = client()
        .post(app.url("/logout-all"))
        .header("cookie", &aging)
        .send()
        .await
        .expect("request");

    assert_eq!(logout.status(), StatusCode::SEE_OTHER);
    if let Some(handed_back) = logout.headers().get("set-cookie") {
        let handed_back = session_cookie(handed_back.to_str().expect("cookie should be text"));
        let home = app.get_home(&handed_back).await;
        assert_eq!(
            home.status(),
            StatusCode::SEE_OTHER,
            "logging out everywhere left a working cookie behind: {handed_back}"
        );
    }
    app.stop().await;
}

#[tokio::test]
async fn logging_out_everywhere_needs_a_session() {
    let app = TestApp::start().await;

    let response = client()
        .post(app.url("/logout-all"))
        .send()
        .await
        .expect("request");

    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/login");
    app.stop().await;
}

#[tokio::test]
async fn repeated_failures_are_locked_out() {
    let app = TestApp::start().await;
    for _ in 0..FREE_ATTEMPTS {
        app.attempt_login(USERNAME, "hunter2").await;
    }

    let response = app.attempt_login(USERNAME, PASSWORD).await;

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    app.stop().await;
}

#[tokio::test]
async fn a_cross_origin_post_is_refused() {
    let app = TestApp::start().await;

    let response = client()
        .post(app.url("/login"))
        .header("origin", "http://evil.example")
        .form(&[("username", USERNAME), ("password", PASSWORD)])
        .send()
        .await
        .expect("request");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    app.stop().await;
}

#[tokio::test]
async fn a_same_origin_post_is_allowed() {
    let app = TestApp::start().await;

    let response = client()
        .post(app.url("/login"))
        .header("origin", format!("http://{}", app.address))
        .form(&[("username", USERNAME), ("password", PASSWORD)])
        .send()
        .await
        .expect("request");

    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    app.stop().await;
}

struct TestApp {
    address: SocketAddr,
    data_dir: TempDir,
    shutdown: oneshot::Sender<()>,
    server: JoinHandle<anyhow::Result<()>>,
}

impl TestApp {
    async fn start() -> Self {
        let data_dir = TempDir::new().expect("temp dir should be creatable");
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral port should bind");
        let address = listener.local_addr().expect("listener reports its address");
        let config = test_config(data_dir.path().to_path_buf());
        ChatStore::new(data_dir.path())
            .prepare()
            .expect("the dataset should prepare, as serving does");
        let secrets = Arc::new(Secrets::from_config(&config));
        let router = server::router(
            &config,
            Chats::new(
                &config,
                Owner::of(&config.data_dir).expect("we own the dataset we just made"),
                MemoryPlane::default(),
                ScriptedAdapter::silent(),
                Remotes::new(GITHUB),
                Arc::clone(&secrets),
            ),
            Settings::new(secrets, ScriptedVerifier::default()),
        )
        .expect("router should build");
        let (shutdown, shutdown_rx) = oneshot::channel();
        let server = tokio::spawn(server::serve(listener, router, async {
            shutdown_rx.await.ok();
        }));
        Self {
            address,
            data_dir,
            shutdown,
            server,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    fn cookie_issued_at(&self, when: SystemTime) -> String {
        format!("{SESSION_COOKIE}={}", session::issue(&self.key(), when))
    }

    fn expiry_of(&self, cookie: &str) -> SystemTime {
        let value = cookie
            .split_once('=')
            .expect("a cookie has a value")
            .1
            .to_owned();
        session::verify(&self.key(), &value, SystemTime::now())
            .expect("the cookie should still be valid")
            .expires_at()
    }

    fn key(&self) -> SigningKey {
        KeyStore::open(self.data_dir.path())
            .expect("the running server left a key file")
            .current()
    }

    async fn attempt_login(&self, username: &str, password: &str) -> reqwest::Response {
        client()
            .post(self.url("/login"))
            .form(&[("username", username), ("password", password)])
            .send()
            .await
            .expect("request")
    }

    async fn get_home(&self, cookie: &str) -> reqwest::Response {
        client()
            .get(self.url("/"))
            .header("cookie", cookie)
            .send()
            .await
            .expect("request")
    }

    async fn stop(self) {
        self.shutdown.send(()).expect("server should be listening");
        self.server
            .await
            .expect("server task should not panic")
            .expect("server should shut down cleanly");
    }
}

fn test_config(data_dir: std::path::PathBuf) -> Config {
    Config {
        host_data_dir: data_dir.clone(),
        data_dir,
        bind_addr: "127.0.0.1:0".parse().expect("valid address"),
        username: USERNAME.to_owned(),
        password_hash: password_hash(PASSWORD),
        workspace_image: "ghcr.io/corvous/corcode-workspace:2026-08-05".to_owned(),
        container_memory_mb: DEFAULT_CONTAINER_MEMORY_MB,
        container_cpus: DEFAULT_CONTAINER_CPUS,
        warm_pool: DEFAULT_WARM_POOL,
        registry: None,
        repos: vec!["CorVous/CorCode".to_owned()],
        github_token: None,
        anthropic_api_key: None,
    }
}

/// A deliberately cheap argon2 hash: these tests verify plenty of them.
fn password_hash(password: &str) -> String {
    let params = Params::new(Params::MIN_M_COST, 1, 1, None).expect("valid argon2 parameters");
    let salt = SaltString::from_b64("c2FsdHNhbHRzYWx0c2FsdA").expect("valid salt");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password(password.as_bytes(), &salt)
        .expect("password should hash")
        .to_string()
}

fn client() -> Client {
    Client::builder()
        .redirect(Policy::none())
        .build()
        .expect("client should build")
}

fn location(response: &reqwest::Response) -> &str {
    response
        .headers()
        .get("location")
        .expect("redirect should carry a location")
        .to_str()
        .expect("location should be text")
}

fn set_cookie(response: &reqwest::Response) -> String {
    response
        .headers()
        .get("set-cookie")
        .expect("response should set a cookie")
        .to_str()
        .expect("cookie should be text")
        .to_owned()
}

fn session_cookie(set_cookie: &str) -> String {
    set_cookie
        .split(';')
        .next()
        .expect("a cookie has a value")
        .to_owned()
}
