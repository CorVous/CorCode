//! Integration tests for the read-only console (ADR-0008), served over a
//! fixture dataset on disk.

use std::fs;
use std::net::SocketAddr;
use std::sync::Arc;

use argon2::password_hash::{PasswordHasher as _, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use reqwest::{Client, StatusCode, redirect::Policy};
use serde_json::json;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use cor_code::acp::ScriptedAdapter;
use cor_code::chats::Chats;
use cor_code::config::{
    Config, DEFAULT_CONTAINER_CPUS, DEFAULT_CONTAINER_MEMORY_MB, DEFAULT_SCRATCH_MB,
    DEFAULT_WARM_POOL,
};
use cor_code::git::{GITHUB, Remotes};
use cor_code::plane::MemoryPlane;
use cor_code::secrets::Secrets;
use cor_code::server;
use cor_code::settings::Settings;
use cor_code::store::{ChatStore, Manifest, NewChat, Owner};
use cor_code::verify::ScriptedVerifier;

const USERNAME: &str = "cassidy";
const PASSWORD: &str = "correct horse battery staple";

/// The one answer a chat id that is not a chat ever gets.
const NO_SUCH_CHAT: &str = "No such chat.\n";

#[tokio::test]
async fn the_console_lists_a_chat_from_the_dataset() {
    let app = TestApp::start(with_fixture_chat()).await;

    let body = app.body("/").await;

    assert!(
        body.contains("Resume ladder") && body.contains("chat/2026-08-05-resume-ladder"),
        "the fixture chat is not on the console: {body}"
    );
    app.stop().await;
}

#[tokio::test]
async fn the_chat_view_renders_the_event_log_from_disk() {
    let (data_dir, chat_id) = fixture_chat();
    let app = TestApp::start(data_dir).await;

    let body = app.body(&format!("/chats/{chat_id}")).await;

    let prompt = body
        .find("ship the ladder")
        .expect("the prompt should render");
    let reply = body.find("on it").expect("the reply should render");
    assert!(prompt < reply, "the log is out of order: {body}");
    assert!(
        body.contains("<small>plan</small>"),
        "the unknown event vanished: {body}"
    );
    app.stop().await;
}

/// Reading a chat is a file read (ADR-0006), so it stands whatever the
/// container daemon is doing (issue #25).
#[tokio::test]
async fn a_chat_renders_while_the_container_daemon_is_down() {
    let (data_dir, chat_id) = fixture_chat();
    let plane = MemoryPlane::default();
    plane.refuse_to_list_containers();
    let app = TestApp::start_over(data_dir, plane).await;

    let body = app.body(&format!("/chats/{chat_id}")).await;

    assert!(
        body.contains("ship the ladder") && body.contains("on it"),
        "the log did not render with the daemon down: {body}"
    );
    app.stop().await;
}

#[tokio::test]
async fn a_corrupt_event_log_surfaces_as_an_error_rather_than_a_gap() {
    let (data_dir, chat_id) = fixture_chat();
    let events = data_dir
        .path()
        .join("chats")
        .join(&chat_id)
        .join("events.jsonl");
    fs::write(&events, "{\"ts\":\"2026-08-05T12:00").expect("corruption should be writable");
    let app = TestApp::start(data_dir).await;

    let response = app.get(&format!("/chats/{chat_id}")).await;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = response.text().await.expect("body");
    assert!(
        body.contains("events.jsonl"),
        "the error does not name the file: {body}"
    );
    app.stop().await;
}

/// A daemon that will not answer costs the live/parked grouping and nothing
/// else: the chats are still on disk, and the operator is told why they are
/// ungrouped (issue #25).
#[tokio::test]
async fn the_console_stands_while_the_container_daemon_is_down() {
    let plane = MemoryPlane::default();
    plane.refuse_to_list_containers();
    let app = TestApp::start_over(with_fixture_chat(), plane).await;

    let body = app.body("/").await;

    assert!(
        body.contains("Container status unknown"),
        "the console does not say the plane went quiet: {body}"
    );
    assert!(
        body.contains("Resume ladder"),
        "the open chat is not listed: {body}"
    );
    assert!(
        status_details(&body).contains("list the workspace containers"),
        "the daemon's own words are not in the status details: {body}"
    );
    app.stop().await;
}

/// The console's Refresh button asks for this fragment, so it degrades where
/// the console does or the button is a dead end (issue #25).
#[tokio::test]
async fn the_chat_list_fragment_stands_while_the_container_daemon_is_down() {
    let plane = MemoryPlane::default();
    plane.refuse_to_list_containers();
    let app = TestApp::start_over(with_fixture_chat(), plane).await;

    let body = app.body("/chats").await;

    assert!(
        body.contains("Container status unknown"),
        "the fragment does not say the plane went quiet: {body}"
    );
    assert!(
        body.contains("Resume ladder"),
        "the open chat is not listed: {body}"
    );
    assert!(
        !body.contains("<h2>Live</h2>") && !body.contains("<h2>Parked</h2>"),
        "the fragment groups chats nothing answered for: {body}"
    );
    app.stop().await;
}

/// Reading degrades with the daemon down; changing something must not. A
/// prompt that cannot reach a container is a failure, and says so
/// (ADR-0007 rule 5).
#[tokio::test]
async fn a_prompt_still_fails_loudly_while_the_container_daemon_is_down() {
    let (data_dir, chat_id) = fixture_chat();
    let plane = MemoryPlane::default();
    plane.refuse_to_list_containers();
    let app = TestApp::start_over(data_dir, plane).await;

    let response = app
        .post(&format!("/chats/{chat_id}/prompt"), &[("prompt", "ship it")])
        .await;

    assert!(
        response.status().is_server_error(),
        "a prompt was answered as though the plane had spoken: {}",
        response.status()
    );
    app.stop().await;
}

/// What the status line holds when it is expanded.
fn status_details(console: &str) -> &str {
    let details = console
        .split_once("<details id=\"status\"")
        .expect("the console carries a status line")
        .1;
    details
        .split_once("</details>")
        .expect("the status line is closed")
        .0
}

#[tokio::test]
async fn the_chat_list_fragment_comes_back_without_the_page_around_it() {
    let app = TestApp::start(with_fixture_chat()).await;

    let body = app.body("/chats").await;

    assert!(
        body.starts_with("<section id=\"chats\""),
        "the fragment is not a bare section: {body}"
    );
    assert!(
        !body.contains("<html"),
        "the fragment carries a whole page: {body}"
    );
    assert!(body.contains("Resume ladder"));
    app.stop().await;
}

#[tokio::test]
async fn the_status_fragment_comes_back_without_the_page_around_it() {
    let app = TestApp::start(with_fixture_chat()).await;

    let body = app.body("/status").await;

    assert!(
        body.starts_with("<summary>pool"),
        "the fragment is not the inside of the status line: {body}"
    );
    assert!(
        !body.contains("<html") && !body.contains("<details"),
        "the fragment carries more than the picture: {body}"
    );
    assert!(
        body.contains("pool 0/2 · parked 1"),
        "the fragment does not read the dataset: {body}"
    );
    app.stop().await;
}

#[tokio::test]
async fn htmx_is_served_by_the_app_itself() {
    let app = TestApp::start(TempDir::new().expect("temp dir")).await;

    let response = app.get("/assets/htmx.js").await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.expect("body");
    assert!(body.contains("htmx"), "that is not htmx: {}", &body[..80]);
    app.stop().await;
}

#[tokio::test]
async fn the_console_is_still_behind_the_session_gate() {
    let app = TestApp::start(with_fixture_chat()).await;

    for path in ["/", "/chats", "/status", "/assets/htmx.js"] {
        let response = client().get(app.url(path)).send().await.expect("request");
        assert_eq!(
            response.status(),
            StatusCode::SEE_OTHER,
            "{path} answered without a session"
        );
    }
    app.stop().await;
}

#[tokio::test]
async fn a_fresh_dataset_renders_an_empty_console() {
    let app = TestApp::start(TempDir::new().expect("temp dir")).await;

    assert!(app.body("/").await.contains("<h2>Live</h2>"));
    app.stop().await;
}

#[tokio::test]
async fn a_chat_id_that_climbs_out_of_the_dataset_is_not_a_chat() {
    let (data_dir, _) = fixture_chat();
    plant_secret(&data_dir);
    let app = TestApp::start(data_dir).await;

    for path in [
        "/chats/..%2Fsecret",
        "/chats/%2e%2e%2fsecret",
        "/chats/..%2f..%2f..%2fetc",
    ] {
        let response = app.get(path).await;

        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{path} was served"
        );
        assert_eq!(
            response.text().await.expect("body"),
            NO_SUCH_CHAT,
            "{path} answered with more than a refusal"
        );
    }
    app.stop().await;
}

#[tokio::test]
async fn an_unknown_chat_id_says_nothing_about_the_filesystem() {
    let (data_dir, _) = fixture_chat();
    let vanished = vanished_chat(&data_dir);
    let app = TestApp::start(data_dir).await;

    let response = app.get(&format!("/chats/{vanished}")).await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response.text().await.expect("body"),
        NO_SUCH_CHAT,
        "the body is a path oracle"
    );
    app.stop().await;
}

/// A dataset holding one chat whose log covers an outbound prompt, agent
/// text, and an event shape this build has never heard of, in the shapes
/// ADR-0006 records.
fn fixture_chat() -> (TempDir, String) {
    let data_dir = TempDir::new().expect("temp dir should be creatable");
    let store = ChatStore::new(data_dir.path())
        .handing_trees_to(Owner::of(data_dir.path()).expect("we own the dataset we just made"));
    let manifest = store
        .create_chat(NewChat {
            title: "Resume ladder".to_owned(),
            repo: "CorVous/CorCode".to_owned(),
            branch: "chat/2026-08-05-resume-ladder".to_owned(),
            base_branch: "main".to_owned(),
            env: std::collections::BTreeMap::new(),
            startup_script: None,
        })
        .expect("fixture chat should be created");
    for payload in [
        json!({
            "sessionId": "3f2b1c4d-0000-4000-8000-000000000001",
            "prompt": [{"type": "text", "text": "ship the ladder"}],
        }),
        json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "on it"},
        }),
        json!({"sessionUpdate": "plan", "entries": []}),
    ] {
        store
            .append_event(&manifest.chat_id, &payload)
            .expect("fixture event should append");
    }
    let Manifest { chat_id, .. } = manifest;
    (data_dir, chat_id)
}

fn with_fixture_chat() -> TempDir {
    fixture_chat().0
}

/// A chat dir planted outside `chats/`, where no URL should reach it.
fn plant_secret(data_dir: &TempDir) {
    let secret = data_dir.path().join("secret");
    fs::create_dir_all(&secret).expect("secret dir should be creatable");
    let manifest = json!({
        "schema": 1,
        "chat_id": "secret",
        "title": "TOP SECRET",
        "state": "open",
        "repo": "CorVous/CorCode",
        "branch": "secret",
        "base_branch": "main",
        "last_pushed_commit": null,
        "acp_session_id": null,
        "created_at": "2026-08-05T00:00:00Z",
        "last_active_at": "2026-08-05T00:00:00Z",
    });
    fs::write(secret.join("manifest.json"), manifest.to_string())
        .expect("secret manifest should be writable");
    fs::write(secret.join("events.jsonl"), "").expect("secret log should be writable");
}

/// A well-formed chat id with nothing behind it: minted, then removed.
fn vanished_chat(data_dir: &TempDir) -> String {
    let store = ChatStore::new(data_dir.path())
        .handing_trees_to(Owner::of(data_dir.path()).expect("we own the dataset we just made"));
    let manifest = store
        .create_chat(NewChat {
            title: "Gone".to_owned(),
            repo: "CorVous/CorCode".to_owned(),
            branch: "chat/2026-08-05-gone".to_owned(),
            base_branch: "main".to_owned(),
            env: std::collections::BTreeMap::new(),
            startup_script: None,
        })
        .expect("chat should be created");
    fs::remove_dir_all(store.chat_dir(&manifest.chat_id)).expect("chat dir should be removable");
    manifest.chat_id
}

struct TestApp {
    address: SocketAddr,
    cookie: String,
    shutdown: oneshot::Sender<()>,
    server: JoinHandle<anyhow::Result<()>>,
    _data_dir: TempDir,
}

impl TestApp {
    async fn start(data_dir: TempDir) -> Self {
        Self::start_over(data_dir, MemoryPlane::default()).await
    }

    async fn start_over(data_dir: TempDir, plane: MemoryPlane) -> Self {
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
                plane,
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
        let cookie = sign_in(address).await;
        Self {
            address,
            cookie,
            shutdown,
            server,
            _data_dir: data_dir,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        client()
            .get(self.url(path))
            .header("cookie", &self.cookie)
            .send()
            .await
            .expect("request")
    }

    async fn post(&self, path: &str, form: &[(&str, &str)]) -> reqwest::Response {
        client()
            .post(self.url(path))
            .header("cookie", &self.cookie)
            .form(form)
            .send()
            .await
            .expect("request")
    }

    async fn body(&self, path: &str) -> String {
        let response = self.get(path).await;
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
        scratch_mb: DEFAULT_SCRATCH_MB,
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
