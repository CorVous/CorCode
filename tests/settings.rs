//! Integration tests for the settings panel: the two operational secrets set,
//! cleared and checked from the one screen (ADR-0008), over a dataset on disk
//! and a verifier that never reaches the network.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use argon2::password_hash::{PasswordHasher as _, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use reqwest::{Client, StatusCode, redirect::Policy};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use cor_code::acp::ScriptedAdapter;
use cor_code::chats::Chats;
use cor_code::config::{
    Config, DEFAULT_CONTAINER_CPUS, DEFAULT_CONTAINER_MEMORY_MB, DEFAULT_WARM_POOL,
};
use cor_code::git::{GITHUB, Remotes};
use cor_code::plane::MemoryPlane;
use cor_code::secrets::Secrets;
use cor_code::server;
use cor_code::settings::Settings;
use cor_code::store::{ChatStore, NewChat};
use cor_code::verify::{ScriptedVerifier, Verified};

const USERNAME: &str = "cassidy";
const PASSWORD: &str = "correct horse battery staple";

/// A value distinctive enough that finding it in any rendered byte is proof
/// of a leak.
const SENTINEL: &str = "ghp-sentinel-set-from-the-console";

/// The same, for the value the environment bootstrapped.
const FROM_ENV: &str = "ghp-sentinel-bootstrapped";

/// The same again, carrying the prefix that makes the Anthropic slot read a
/// value as a subscription token. The prefix is looked at; nothing after it
/// may ever reach a page.
const SUBSCRIPTION_SENTINEL: &str = "sk-ant-oat01-sentinel-set-from-the-console";

/// Every path the settings panel posts to.
const MUTATING: [&str; 6] = [
    "/settings/github_token",
    "/settings/github_token/clear",
    "/settings/github_token/verify",
    "/settings/anthropic_key",
    "/settings/anthropic_key/clear",
    "/settings/anthropic_key/verify",
];

#[tokio::test]
async fn the_console_says_where_each_secret_comes_from() {
    let app = TestApp::bootstrapped().await;

    let body = app.body("/").await;

    assert!(
        body.contains("GitHub token") && body.contains("Anthropic API key"),
        "the settings panel is not on the console: {body}"
    );
    assert!(
        body.contains("set (from environment)") && body.contains("not set"),
        "the console does not say where each secret comes from: {body}"
    );
    app.stop().await;
}

#[tokio::test]
async fn a_saved_secret_reads_as_set_from_the_settings() {
    let app = TestApp::bootstrapped().await;

    let fragment = app.save("github_token", SENTINEL).await;

    assert!(
        fragment.contains("set (from settings)"),
        "the panel does not report the secret as set here: {fragment}"
    );
    assert!(
        fragment.contains("saved"),
        "the panel does not say it saved anything: {fragment}"
    );
    assert!(
        app.console().await.contains("set (from settings)"),
        "the console still reads the old status"
    );
    app.stop().await;
}

#[tokio::test]
async fn a_blank_save_changes_nothing_and_says_so() {
    let app = TestApp::bootstrapped().await;
    app.save("github_token", SENTINEL).await;

    let fragment = app.save("github_token", "   ").await;

    assert!(
        fragment.contains("nothing given, nothing changed"),
        "a blank save reads as something having happened: {fragment}"
    );
    assert!(
        app.secret_file("github_token").exists(),
        "a blank save took the set value away"
    );
    assert!(
        fragment.contains("set (from settings)"),
        "a blank save unset the secret: {fragment}"
    );
    app.stop().await;
}

#[tokio::test]
async fn a_blank_save_over_a_secret_nothing_holds_writes_nothing() {
    let app = TestApp::bootstrapped().await;

    let fragment = app.save("github_token", "   ").await;

    assert!(
        !app.secret_file("github_token").exists(),
        "a blank save reached the disk"
    );
    assert!(fragment.contains("set (from environment)"));
    app.stop().await;
}

#[tokio::test]
async fn a_cleared_secret_falls_back_to_the_one_the_environment_carried() {
    let app = TestApp::bootstrapped().await;
    app.save("github_token", SENTINEL).await;

    let fragment = app.act("/settings/github_token/clear").await;

    assert!(
        fragment.contains("set (from environment)"),
        "clearing did not fall back to the environment: {fragment}"
    );
    assert!(
        !app.secret_file("github_token").exists(),
        "the cleared secret is still on disk"
    );
    app.stop().await;
}

#[tokio::test]
async fn clearing_the_only_value_there_ever_was_leaves_the_secret_unset() {
    let app = TestApp::bare().await;
    app.save("anthropic_key", SENTINEL).await;

    let fragment = app.act("/settings/anthropic_key/clear").await;

    assert!(
        fragment.contains("not set"),
        "the secret still reads as set: {fragment}"
    );
    app.stop().await;
}

/// The Anthropic slot takes either a key or a subscription token, and the two
/// open the service in different ways. The panel says which one is in there
/// without saying any of it.
#[tokio::test]
async fn the_anthropic_slot_says_which_kind_of_credential_it_is_holding() {
    let app = TestApp::bare().await;

    let subscribed = app.save("anthropic_key", SUBSCRIPTION_SENTINEL).await;
    let keyed = app.save("anthropic_key", SENTINEL).await;

    assert!(
        subscribed.contains("set (from settings) — OAuth token"),
        "a subscription token does not read as one: {subscribed}"
    );
    assert!(
        keyed.contains("set (from settings) — API key"),
        "a key does not read as one: {keyed}"
    );
    assert!(
        app.console()
            .await
            .contains("set (from settings) — API key"),
        "the console does not name the kind in the slot"
    );
    app.stop().await;
}

/// The one thing the panel exists to handle is the one thing it must never
/// say back, in any state, on any page, in any fragment.
#[tokio::test]
async fn no_page_and_no_fragment_ever_renders_a_secret() {
    let app = TestApp::bootstrapped().await;
    let chat = app.fixture_chat();

    let mut rendered = vec![
        app.save("github_token", SENTINEL).await,
        app.save("anthropic_key", SENTINEL).await,
        app.save("anthropic_key", SUBSCRIPTION_SENTINEL).await,
        app.act("/settings/anthropic_key/verify").await,
        app.act("/settings/github_token/verify").await,
        app.act("/settings/github_token/clear").await,
    ];
    for path in ["/", &format!("/chats/{chat}"), "/chats", "/status"] {
        rendered.push(app.body(path).await);
    }

    for body in &rendered {
        for leaked in [SENTINEL, SUBSCRIPTION_SENTINEL] {
            assert!(!body.contains(leaked), "a set secret leaked: {body}");
        }
        assert!(
            !body.contains(FROM_ENV),
            "a bootstrapped secret leaked: {body}"
        );
    }
    app.stop().await;
}

#[tokio::test]
async fn a_working_token_reads_as_ok_and_names_who_it_authenticated_as() {
    let app = TestApp::start(
        Some(FROM_ENV),
        ScriptedVerifier::answering(Verified::Accepted {
            login: Some("cassidy".to_owned()),
            without_repo_scope: false,
        }),
    )
    .await;

    let fragment = app.act("/settings/github_token/verify").await;

    assert!(
        fragment.contains("ok — authenticated as cassidy"),
        "a working token does not read as working: {fragment}"
    );
    assert!(!fragment.contains("repo scope"));
    app.stop().await;
}

#[tokio::test]
async fn a_token_that_cannot_reach_private_repositories_is_flagged() {
    let app = TestApp::start(
        Some(FROM_ENV),
        ScriptedVerifier::answering(Verified::Accepted {
            login: Some("cassidy".to_owned()),
            without_repo_scope: true,
        }),
    )
    .await;

    let fragment = app.act("/settings/github_token/verify").await;

    assert!(
        fragment.contains("ok — authenticated as cassidy") && fragment.contains("repo scope"),
        "a token short of the repo scope passes without a word: {fragment}"
    );
    app.stop().await;
}

#[tokio::test]
async fn a_credential_the_service_turned_away_reads_as_its_status_alone() {
    let app = TestApp::start(
        Some(FROM_ENV),
        ScriptedVerifier::answering(Verified::Refused(401)),
    )
    .await;

    let fragment = app.act("/settings/github_token/verify").await;

    assert!(
        fragment.contains("invalid or expired (401)"),
        "a refused token does not read as refused: {fragment}"
    );
    app.stop().await;
}

#[tokio::test]
async fn a_service_that_did_not_answer_in_time_says_so() {
    let app = TestApp::start(
        Some(FROM_ENV),
        ScriptedVerifier::answering(Verified::Silent),
    )
    .await;

    let fragment = app.act("/settings/github_token/verify").await;

    assert!(
        fragment.contains("could not be reached"),
        "a silent service reads as something else: {fragment}"
    );
    app.stop().await;
}

#[tokio::test]
async fn a_check_puts_the_value_set_here_and_not_the_one_the_environment_carried() {
    let app = TestApp::bootstrapped().await;

    app.act("/settings/github_token/verify").await;
    app.save("github_token", SENTINEL).await;
    app.act("/settings/github_token/verify").await;

    let heard: Vec<String> = app
        .verifier
        .heard()
        .into_iter()
        .map(|(_, value)| value)
        .collect();
    assert_eq!(heard, vec![FROM_ENV.to_owned(), SENTINEL.to_owned()]);
    app.stop().await;
}

#[tokio::test]
async fn a_secret_nothing_holds_is_never_spent_on_a_call() {
    let app = TestApp::bare().await;

    let fragment = app.act("/settings/anthropic_key/verify").await;

    assert!(
        fragment.contains("nothing to check"),
        "an unset secret was checked anyway: {fragment}"
    );
    assert!(app.verifier.heard().is_empty());
    app.stop().await;
}

#[tokio::test]
async fn every_settings_route_is_behind_the_session_gate() {
    let app = TestApp::bootstrapped().await;

    for path in MUTATING {
        let response = client()
            .post(app.url(path))
            .form(&[("value", SENTINEL)])
            .send()
            .await
            .expect("request");

        assert_eq!(
            response.status(),
            StatusCode::SEE_OTHER,
            "{path} answered without a session"
        );
    }
    assert!(
        !app.secret_file("github_token").exists(),
        "an ungated request set a secret"
    );
    app.stop().await;
}

#[tokio::test]
async fn every_settings_route_refuses_a_post_from_another_origin() {
    let app = TestApp::bootstrapped().await;

    for path in MUTATING {
        let response = client()
            .post(app.url(path))
            .header("cookie", &app.cookie)
            .header("origin", "http://evil.example")
            .form(&[("value", SENTINEL)])
            .send()
            .await
            .expect("request");

        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{path} served another origin"
        );
    }
    assert!(
        !app.secret_file("github_token").exists(),
        "a cross-origin request set a secret"
    );
    app.stop().await;
}

#[tokio::test]
async fn a_path_that_names_no_secret_is_not_a_secret() {
    let app = TestApp::bootstrapped().await;

    let response = app.post("/settings/password/verify").await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    app.stop().await;
}

struct TestApp {
    address: SocketAddr,
    cookie: String,
    verifier: ScriptedVerifier,
    shutdown: oneshot::Sender<()>,
    server: JoinHandle<anyhow::Result<()>>,
    data_dir: TempDir,
}

impl TestApp {
    /// A deployment whose environment carried a GitHub token in.
    async fn bootstrapped() -> Self {
        Self::start(Some(FROM_ENV), ScriptedVerifier::default()).await
    }

    /// A deployment that was given no credentials at all.
    async fn bare() -> Self {
        Self::start(None, ScriptedVerifier::default()).await
    }

    async fn start(github_token: Option<&str>, verifier: ScriptedVerifier) -> Self {
        let data_dir = TempDir::new().expect("temp dir should be creatable");
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral port should bind");
        let address = listener.local_addr().expect("listener reports its address");
        let config = test_config(data_dir.path().to_path_buf(), github_token);
        ChatStore::new(data_dir.path())
            .prepare()
            .expect("the dataset should prepare, as serving does");
        let secrets = Arc::new(Secrets::from_config(&config));
        let router = server::router(
            &config,
            Chats::new(
                &config,
                MemoryPlane::default(),
                ScriptedAdapter::silent(),
                Remotes::new(GITHUB),
                Arc::clone(&secrets),
            ),
            Settings::new(secrets, verifier.clone()),
        )
        .expect("router should build");
        let (shutdown, shutdown_rx) = oneshot::channel();
        let server = tokio::spawn(server::serve(listener, router, async {
            shutdown_rx.await.ok();
        }));
        let cookie = sign_in(address).await;
        Self {
            address,
            cookie,
            verifier,
            shutdown,
            server,
            data_dir,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    fn secret_file(&self, name: &str) -> PathBuf {
        self.data_dir.path().join("secrets").join(name)
    }

    /// A chat on disk, so that the chat page can be searched for leaks too.
    fn fixture_chat(&self) -> String {
        ChatStore::new(self.data_dir.path())
            .create_chat(NewChat {
                title: "Resume ladder".to_owned(),
                repo: "CorVous/CorCode".to_owned(),
                branch: "chat/2026-08-05-resume-ladder".to_owned(),
                base_branch: "main".to_owned(),
            })
            .expect("fixture chat should be created")
            .chat_id
    }

    async fn body(&self, path: &str) -> String {
        let response = client()
            .get(self.url(path))
            .header("cookie", &self.cookie)
            .send()
            .await
            .expect("request");
        assert_eq!(response.status(), StatusCode::OK, "{path} did not answer");
        response.text().await.expect("body")
    }

    async fn console(&self) -> String {
        self.body("/").await
    }

    async fn post(&self, path: &str) -> reqwest::Response {
        self.submit(path, &[]).await
    }

    async fn submit(&self, path: &str, form: &[(&str, &str)]) -> reqwest::Response {
        client()
            .post(self.url(path))
            .header("cookie", &self.cookie)
            .form(form)
            .send()
            .await
            .expect("request")
    }

    /// Click one secret's Save and hand back the section that swaps in.
    async fn save(&self, name: &str, value: &str) -> String {
        let response = self
            .submit(&format!("/settings/{name}"), &[("value", value)])
            .await;
        assert_eq!(response.status(), StatusCode::OK, "the save did not answer");
        response.text().await.expect("body")
    }

    /// Click one secret's Clear or Verify and hand back the section.
    async fn act(&self, path: &str) -> String {
        let response = self.post(path).await;
        assert_eq!(response.status(), StatusCode::OK, "{path} did not answer");
        response.text().await.expect("body")
    }

    async fn stop(self) {
        self.shutdown.send(()).expect("server should be listening");
        self.server
            .await
            .expect("server task should not panic")
            .expect("server should shut down cleanly");
    }
}

async fn sign_in(address: SocketAddr) -> String {
    let response = client()
        .post(format!("http://{address}/login"))
        .form(&[("username", USERNAME), ("password", PASSWORD)])
        .send()
        .await
        .expect("request");
    response
        .headers()
        .get("set-cookie")
        .expect("a correct login hands out a cookie")
        .to_str()
        .expect("cookie should be text")
        .split(';')
        .next()
        .expect("a cookie has a value")
        .to_owned()
}

fn test_config(data_dir: PathBuf, github_token: Option<&str>) -> Config {
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
        github_token: github_token.map(ToOwned::to_owned),
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
