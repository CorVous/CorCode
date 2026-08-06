//! Integration tests for the prompt round trip: a prompt over the chat's own
//! ACP connection, every frame of the turn on disk, and the log the browser
//! polls showing it (ADR-0006, ADR-0007).

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use argon2::password_hash::{PasswordHasher as _, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use reqwest::{Client, StatusCode, redirect::Policy};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use cor_code::acp::ScriptedAdapter;
use cor_code::chats::Chats;
use cor_code::config::{Config, DEFAULT_CONTAINER_CPUS, DEFAULT_CONTAINER_MEMORY_MB};
use cor_code::git::Remotes;
use cor_code::plane::MemoryPlane;
use cor_code::server;
use cor_code::store::{ChatStore, NewChat};

const USERNAME: &str = "cassidy";
const PASSWORD: &str = "correct horse battery staple";
const REPO: &str = "CorVous/fixture";
const BARE: &str = "CorVous/fixture.git";
const SESSION: &str = "3f2b1c4d-0000-4000-8000-000000000001";
const SAID: &str = "ship the ladder";

/// Long enough for a request to be in flight when the next one arrives, short
/// enough that no test waits on it noticeably.
const DAWDLE: Duration = Duration::from_millis(200);

/// A way out of `chats/` spelled so that no client normalises it away before
/// the router sees it: it is one path segment until the server decodes it.
const TRAVERSAL: &str = "%2e%2e%2f%2e%2e%2fetc%2fpasswd";

/// One `session/update` notification's params, as the adapter sends them.
fn update(session_id: &str, said: &str) -> Value {
    json!({
        "sessionId": session_id,
        "update": {
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": said},
        },
    })
}

/// The same update as ADR-0006 writes it into `events.jsonl`.
fn recorded(said: &str) -> Value {
    json!({
        "sessionUpdate": "agent_message_chunk",
        "content": {"type": "text", "text": said},
    })
}

/// The outbound prompt as ADR-0006 writes it into `events.jsonl`.
fn recorded_prompt(said: &str) -> Value {
    json!({"sessionId": SESSION, "prompt": [{"type": "text", "text": said}]})
}

/// A refusal as the core writes it into the chat's own log, so that the next
/// poll shows the operator why their prompt went nowhere.
fn recorded_refusal(why: &str) -> Value {
    json!({"corcode": "refusal", "text": format!("Prompt not sent: {why}.")})
}

#[tokio::test]
async fn a_prompt_lands_on_disk_and_in_the_log_the_browser_reads() {
    let app = TestApp::start(ScriptedAdapter::answering(
        SESSION,
        &[update(SESSION, "on it"), update(SESSION, " — done")],
    ))
    .await;
    let chat_id = app.create_chat().await;

    let response = app.prompt(&chat_id, SAID).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        app.events(&chat_id),
        [
            recorded_prompt(SAID),
            recorded("on it"),
            recorded(" — done"),
        ]
    );
    let swapped = response.text().await.expect("body");
    let polled = app.body(&format!("/chats/{chat_id}/events")).await;
    for rendered in [
        &swapped,
        &polled,
        &app.body(&format!("/chats/{chat_id}")).await,
    ] {
        assert!(
            rendered.contains("<b>you:</b> ship the ladder") && rendered.contains("<p>on it</p>"),
            "the turn is not in the rendered log: {rendered}"
        );
    }
    app.stop().await;
}

#[tokio::test]
async fn a_second_prompt_while_the_first_turn_runs_is_refused() {
    let app = TestApp::start(ScriptedAdapter::dawdling(
        SESSION,
        &[update(SESSION, "on it")],
        DAWDLE,
    ))
    .await;
    let chat_id = app.create_chat().await;

    let in_flight = app.spawn_prompt(&chat_id, SAID);
    tokio::time::sleep(DAWDLE / 5).await;
    let second = app.prompt(&chat_id, "and again").await;

    assert_eq!(second.status(), StatusCode::CONFLICT);
    assert_eq!(
        in_flight
            .await
            .expect("the first prompt should not panic")
            .status(),
        StatusCode::OK
    );
    let events = app.events(&chat_id);
    assert!(
        !events.contains(&recorded_prompt("and again")),
        "the refused prompt reached the agent anyway: {events:?}"
    );
    assert!(
        events.contains(&recorded_refusal(
            "this chat is still answering the last prompt"
        )),
        "the operator is never told why their prompt went nowhere: {events:?}"
    );
    assert!(
        events.contains(&recorded_prompt(SAID)) && events.contains(&recorded("on it")),
        "the turn that was running lost its own record: {events:?}"
    );
    app.stop().await;
}

#[tokio::test]
async fn a_prompt_into_a_chat_with_no_live_connection_says_so() {
    let app = TestApp::start(ScriptedAdapter::answering(SESSION, &[])).await;
    let chat_id = app.chat_on_disk_alone();

    let response = app.prompt(&chat_id, SAID).await;

    assert_eq!(response.status(), StatusCode::TOO_EARLY);
    let body = response.text().await.expect("body");
    assert!(
        body.contains("no live connection"),
        "the refusal does not say what is missing: {body}"
    );
    assert_eq!(
        app.events(&chat_id),
        [recorded_refusal("this chat has no live connection")],
        "a prompt nobody heard was sent, or its refusal was never written down"
    );
    let polled = app.body(&format!("/chats/{chat_id}/events")).await;
    assert!(
        polled.contains("no live connection"),
        "the next poll shows the operator nothing: {polled}"
    );
    app.stop().await;
}

#[tokio::test]
async fn a_turn_the_dataset_cannot_take_keeps_the_connection_it_was_going_over() {
    let app = TestApp::start(ScriptedAdapter::answering(
        SESSION,
        &[update(SESSION, "on it")],
    ))
    .await;
    let chat_id = app.create_chat().await;
    app.block_the_event_log(&chat_id);

    let broken = app.prompt(&chat_id, SAID).await;

    assert_eq!(broken.status(), StatusCode::INTERNAL_SERVER_ERROR);
    app.unblock_the_event_log(&chat_id);
    assert_eq!(
        app.prompt(&chat_id, "still there?").await.status(),
        StatusCode::OK,
        "a healthy connection was dropped over a write of ours that failed"
    );
    assert_eq!(
        app.events(&chat_id),
        [recorded_prompt("still there?"), recorded("on it")]
    );
    app.stop().await;
}

#[tokio::test]
async fn an_adapter_that_dies_mid_turn_keeps_what_it_said_and_drops_the_connection() {
    let app = TestApp::start(ScriptedAdapter::dying_mid_turn(
        SESSION,
        &[update(SESSION, "on i")],
    ))
    .await;
    let chat_id = app.create_chat().await;

    let response = app.prompt(&chat_id, SAID).await;

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        app.events(&chat_id),
        [recorded_prompt(SAID), recorded("on i")],
        "a broken turn took its own record with it"
    );
    assert_eq!(
        app.prompt(&chat_id, "still there?").await.status(),
        StatusCode::TOO_EARLY,
        "the dead connection is still being handed out"
    );
    app.stop().await;
}

#[tokio::test]
async fn prompting_is_behind_the_session_gate_and_this_origin() {
    let app = TestApp::start(ScriptedAdapter::answering(SESSION, &[])).await;
    let chat_id = app.create_chat().await;

    let ungated = client()
        .post(app.url(&format!("/chats/{chat_id}/prompt")))
        .form(&[("prompt", SAID)])
        .send()
        .await
        .expect("request");
    let cross_site = client()
        .post(app.url(&format!("/chats/{chat_id}/prompt")))
        .header("cookie", &app.cookie)
        .header("origin", "http://evil.example")
        .form(&[("prompt", SAID)])
        .send()
        .await
        .expect("request");

    assert_eq!(ungated.status(), StatusCode::SEE_OTHER);
    assert_eq!(cross_site.status(), StatusCode::FORBIDDEN);
    assert!(
        app.events(&chat_id).is_empty(),
        "an unauthorised prompt reached the agent"
    );
    app.stop().await;
}

#[tokio::test]
async fn a_chat_id_that_is_not_a_ulid_reaches_no_file() {
    let app = TestApp::start(ScriptedAdapter::answering(SESSION, &[])).await;

    let prompted = app.prompt(TRAVERSAL, SAID).await;
    let polled = client()
        .get(app.url(&format!("/chats/{TRAVERSAL}/events")))
        .header("cookie", &app.cookie)
        .send()
        .await
        .expect("request");

    assert_eq!(prompted.status(), StatusCode::NOT_FOUND);
    assert_eq!(polled.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        polled.text().await.expect("body"),
        "No such chat.\n",
        "the router turned this away before the handler could, so nothing is proved"
    );
    app.stop().await;
}

struct TestApp {
    address: SocketAddr,
    cookie: String,
    shutdown: oneshot::Sender<()>,
    server: JoinHandle<anyhow::Result<()>>,
    data_dir: TempDir,
    _origin: TempDir,
}

impl TestApp {
    async fn start(transport: ScriptedAdapter) -> Self {
        let data_dir = TempDir::new().expect("temp dir should be creatable");
        let (origin, remotes) = seeded_repository();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral port should bind");
        let address = listener.local_addr().expect("listener reports its address");
        let config = test_config(data_dir.path().to_path_buf());
        ChatStore::new(data_dir.path())
            .prepare()
            .expect("the dataset should prepare, as serving does");
        let chats = Chats::new(&config, MemoryPlane::default(), transport, remotes);
        let router = server::router(&config, chats).expect("router should build");
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
            data_dir,
            _origin: origin,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    /// A chat cut the way the console cuts one, so that it owns a live
    /// connection.
    async fn create_chat(&self) -> String {
        let response = client()
            .post(self.url("/chats"))
            .header("cookie", &self.cookie)
            .form(&[("repo", REPO), ("base_branch", "main"), ("slug", "prompt")])
            .send()
            .await
            .expect("request");
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "chat not created");
        let location = response
            .headers()
            .get("location")
            .expect("a created chat redirects to itself")
            .to_str()
            .expect("a location is text");
        location
            .strip_prefix("/chats/")
            .expect("the redirect points at a chat")
            .to_owned()
    }

    /// A chat laid down behind the core's back, as one whose container and
    /// connection are long gone reads on a restart.
    fn chat_on_disk_alone(&self) -> String {
        ChatStore::new(self.data_dir.path())
            .create_chat(NewChat {
                title: "parked".to_owned(),
                repo: REPO.to_owned(),
                branch: "chat/parked".to_owned(),
                base_branch: "main".to_owned(),
            })
            .expect("a chat should be creatable")
            .chat_id
    }

    async fn prompt(&self, chat_id: &str, said: &str) -> reqwest::Response {
        client()
            .post(self.url(&format!("/chats/{chat_id}/prompt")))
            .header("cookie", &self.cookie)
            .form(&[("prompt", said)])
            .send()
            .await
            .expect("request")
    }

    /// A prompt sent without waiting for the turn it starts.
    fn spawn_prompt(&self, chat_id: &str, said: &str) -> JoinHandle<reqwest::Response> {
        let request = client()
            .post(self.url(&format!("/chats/{chat_id}/prompt")))
            .header("cookie", &self.cookie)
            .form(&[("prompt", said)]);
        tokio::spawn(async move { request.send().await.expect("request") })
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

    fn events_path(&self, chat_id: &str) -> PathBuf {
        self.data_dir
            .path()
            .join("chats")
            .join(chat_id)
            .join("events.jsonl")
    }

    /// The chat's event log made unwritable in a way no user can write past,
    /// not even one running as root: a directory where the file goes.
    fn block_the_event_log(&self, chat_id: &str) {
        let path = self.events_path(chat_id);
        fs::remove_file(&path).expect("the event log should exist");
        fs::create_dir(&path).expect("the log's place should be takeable");
    }

    fn unblock_the_event_log(&self, chat_id: &str) {
        let path = self.events_path(chat_id);
        fs::remove_dir(&path).expect("the blockage should be removable");
        fs::write(&path, "").expect("the event log should be writable again");
    }

    /// The ACP payloads the chat's event log holds, in order.
    fn events(&self, chat_id: &str) -> Vec<Value> {
        let path = self.events_path(chat_id);
        fs::read_to_string(&path)
            .expect("the event log should exist")
            .lines()
            .map(|line| {
                let event: Value = serde_json::from_str(line).expect("a line should be json");
                assert!(
                    event["ts"].is_string(),
                    "a line carries no timestamp: {line}"
                );
                event["event"].clone()
            })
            .collect()
    }

    async fn stop(self) {
        self.shutdown.send(()).expect("server should be listening");
        self.server
            .await
            .expect("server task should not panic")
            .expect("server should shut down cleanly");
    }
}

/// A bare repository with a commit on `main`, reachable over `file://` so
/// that no test needs the network.
fn seeded_repository() -> (TempDir, Remotes) {
    let dir = TempDir::new().expect("origin dir should be created");
    let bare = dir.path().join(BARE);
    let work = dir.path().join("seed");
    run(
        dir.path(),
        &["init", "--bare", "--initial-branch=main", &spelled(&bare)],
    );
    run(
        dir.path(),
        &["init", "--initial-branch=main", &spelled(&work)],
    );
    run(&work, &["config", "user.email", "seed@example.invalid"]);
    run(&work, &["config", "user.name", "Seed"]);
    fs::write(work.join("README.md"), "fixture").expect("seed file should be writable");
    run(&work, &["add", "."]);
    run(&work, &["commit", "-m", "first"]);
    run(&work, &["remote", "add", "origin", &spelled(&bare)]);
    run(&work, &["push", "origin", "main"]);
    let served_from = format!("file://{}", spelled(dir.path()));
    (dir, Remotes::new(served_from, None))
}

fn spelled(path: &Path) -> String {
    path.to_str()
        .expect("temp paths should be spellable")
        .to_owned()
}

fn run(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git should run");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
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

fn test_config(data_dir: PathBuf) -> Config {
    Config {
        data_dir,
        bind_addr: "127.0.0.1:0".parse().expect("valid address"),
        username: USERNAME.to_owned(),
        password_hash: password_hash(PASSWORD),
        workspace_image: "ghcr.io/corvous/corcode-workspace:2026-08-05".to_owned(),
        container_memory_mb: DEFAULT_CONTAINER_MEMORY_MB,
        container_cpus: DEFAULT_CONTAINER_CPUS,
        registry: None,
        repos: vec![REPO.to_owned()],
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
