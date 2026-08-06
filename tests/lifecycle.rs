//! Integration tests for the session lifecycle: the warm pool's cap, the
//! archive gate that empties a workspace onto the remote, and the sweep that
//! keeps `workspaces/` honest (ADR-0002, ADR-0005).

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;

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
use cor_code::config::{
    Config, DEFAULT_CONTAINER_CPUS, DEFAULT_CONTAINER_MEMORY_MB, DEFAULT_WARM_POOL,
};
use cor_code::git::Remotes;
use cor_code::plane::MemoryPlane;
use cor_code::server;
use cor_code::store::ChatStore;

const USERNAME: &str = "cassidy";
const PASSWORD: &str = "correct horse battery staple";
const REPO: &str = "CorVous/fixture";
const BARE: &str = "CorVous/fixture.git";
const SESSION: &str = "3f2b1c4d-0000-4000-8000-000000000001";

#[tokio::test]
async fn a_chat_beyond_the_pool_gives_up_its_container_and_keeps_its_workspace() {
    let app = TestApp::start().await;
    let first = app.create_chat("first").await;
    let second = app.create_chat("second").await;

    let third = app.create_chat("third").await;

    let console = app.body("/").await;
    assert_eq!(group(&console, &first), "Parked");
    assert_eq!(group(&console, &second), "Live");
    assert_eq!(group(&console, &third), "Live");
    assert!(
        app.workspace(&first).join("README.md").is_file(),
        "parking took the workspace with it"
    );
    assert_eq!(
        app.manifest(&first)["state"],
        "open",
        "parking is not a state on disk (ADR-0002)"
    );
}

#[tokio::test]
async fn a_parked_chat_is_no_longer_holding_a_connection() {
    let app = TestApp::start().await;
    let parked = app.create_chat("first").await;
    app.create_chat("second").await;
    app.create_chat("third").await;

    let response = app.prompt(&parked, "still there?").await;

    assert_eq!(response.status(), StatusCode::TOO_EARLY);
}

#[tokio::test]
async fn a_completed_turn_moves_its_chat_to_the_front_of_the_pool() {
    let app = TestApp::start().await;
    app.create_chat("first").await;
    let second = app.create_chat("second").await;
    let third = app.create_chat("third").await;

    let turn = app.prompt(&second, "ship the ladder").await;
    let fourth = app.create_chat("fourth").await;

    assert_eq!(turn.status(), StatusCode::OK);
    let console = app.body("/").await;
    assert_eq!(
        group(&console, &third),
        "Parked",
        "the chat that took a turn was parked ahead of an idler: {console}"
    );
    assert_eq!(group(&console, &second), "Live");
    assert_eq!(group(&console, &fourth), "Live");
}

#[tokio::test]
async fn archiving_a_clean_chat_pushes_its_branch_and_empties_its_workspace() {
    let app = TestApp::start().await;
    let chat = app.create_chat("first").await;
    let semantic = app.commit_in_workspace(&chat, "the agent's own commit");

    let response = app.archive(&chat).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        app.origin_says(&["rev-parse", &app.branch(&chat)]),
        semantic,
        "the chat branch did not reach the remote"
    );
    let manifest = app.manifest(&chat);
    assert_eq!(manifest["state"], "archived");
    assert_eq!(manifest["last_pushed_commit"], semantic);
    assert_eq!(manifest["checkpoint_branch"], Value::Null);
    assert!(
        !app.workspace(&chat).exists(),
        "an archived chat kept its workspace (ADR-0002 rule 1)"
    );
    assert_eq!(group(&app.body("/").await, &chat), "Archived");
}

#[tokio::test]
async fn archiving_a_dirty_chat_checkpoints_it_and_leaves_the_branch_where_the_agent_left_it() {
    let app = TestApp::start().await;
    let chat = app.create_chat("first").await;
    let semantic = app.commit_in_workspace(&chat, "the agent's own commit");
    fs::write(app.workspace(&chat).join("scratch.txt"), "work in flight")
        .expect("a dirty file should be writable");

    let response = app.archive(&chat).await;

    assert_eq!(response.status(), StatusCode::OK);
    let checkpoint = app.manifest(&chat)["checkpoint_branch"]
        .as_str()
        .expect("a dirty tree should be archived onto a checkpoint branch")
        .to_owned();
    assert_eq!(
        app.origin_says(&["rev-parse", &app.branch(&chat)]),
        semantic,
        "the chat branch left the agent's last semantic commit"
    );
    assert_eq!(
        app.origin_says(&["show", &format!("{checkpoint}:scratch.txt")]),
        "work in flight",
        "the work in flight never reached the remote"
    );
}

#[tokio::test]
async fn a_chat_whose_push_fails_keeps_its_container_and_says_so() {
    let app = TestApp::start().await;
    let chat = app.create_chat("first").await;
    fs::write(app.workspace(&chat).join("scratch.txt"), "work in flight")
        .expect("a dirty file should be writable");
    app.lose_the_remote();

    let response = app.archive(&chat).await;

    assert!(
        response.status().is_server_error(),
        "a failed archive answered {}",
        response.status()
    );
    assert_eq!(app.manifest(&chat)["state"], "open");
    assert!(
        app.workspace(&chat).join("scratch.txt").is_file(),
        "the work nobody pushed was deleted anyway"
    );
    assert_eq!(group(&app.body("/").await, &chat), "Live");
    assert!(
        app.events(&chat).contains("push_failure"),
        "the failure was not written down: {}",
        app.events(&chat)
    );
    let log = app.body(&format!("/chats/{chat}/events")).await;
    assert!(
        log.contains("<blockquote>"),
        "the operator is told nothing when they next look: {log}"
    );
}

#[tokio::test]
async fn archiving_a_chat_also_sweeps_the_working_trees_nothing_claims() {
    let app = TestApp::start().await;
    let chat = app.create_chat("first").await;
    let orphan = app.stray_workspace();

    app.archive(&chat).await;

    assert!(
        !orphan.exists(),
        "the sweep left a pile behind (ADR-0002 rule 4)"
    );
}

/// Which of the console's headings a chat is rendered under.
fn group(console: &str, chat_id: &str) -> String {
    let at = console
        .find(chat_id)
        .unwrap_or_else(|| panic!("{chat_id} is not on the console: {console}"));
    ["Live", "Parked", "Archived"]
        .into_iter()
        .rfind(|heading| {
            console
                .find(&format!("<h2>{heading}</h2>"))
                .is_some_and(|found| found < at)
        })
        .expect("every chat sits under a heading")
        .to_owned()
}

struct TestApp {
    address: SocketAddr,
    cookie: String,
    data_dir: TempDir,
    origin: TempDir,
    _server: JoinHandle<anyhow::Result<()>>,
    _shutdown: oneshot::Sender<()>,
}

impl TestApp {
    async fn start() -> Self {
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
        let chats = Chats::new(
            &config,
            MemoryPlane::default(),
            ScriptedAdapter::answering(SESSION, &[update("on it")]),
            remotes,
        );
        let router = server::router(&config, chats).expect("router should build");
        let (shutdown, shutdown_rx) = oneshot::channel();
        let server = tokio::spawn(server::serve(listener, router, async {
            shutdown_rx.await.ok();
        }));
        Self {
            cookie: sign_in(address).await,
            address,
            data_dir,
            origin,
            _server: server,
            _shutdown: shutdown,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    async fn create_chat(&self, slug: &str) -> String {
        let response = client()
            .post(self.url("/chats"))
            .header("cookie", &self.cookie)
            .form(&[("repo", REPO), ("base_branch", "main"), ("slug", slug)])
            .send()
            .await
            .expect("request");
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "chat not created");
        response
            .headers()
            .get("location")
            .expect("a created chat redirects to itself")
            .to_str()
            .expect("a location is text")
            .strip_prefix("/chats/")
            .expect("the redirect points at a chat")
            .to_owned()
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

    async fn archive(&self, chat_id: &str) -> reqwest::Response {
        client()
            .post(self.url(&format!("/chats/{chat_id}/archive")))
            .header("cookie", &self.cookie)
            .send()
            .await
            .expect("request")
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

    fn workspace(&self, chat_id: &str) -> PathBuf {
        self.data_dir.path().join("workspaces").join(chat_id)
    }

    fn manifest(&self, chat_id: &str) -> Value {
        let path = self.chat_dir(chat_id).join("manifest.json");
        serde_json::from_str(&fs::read_to_string(&path).expect("the manifest should be readable"))
            .expect("the manifest should be json")
    }

    fn events(&self, chat_id: &str) -> String {
        fs::read_to_string(self.chat_dir(chat_id).join("events.jsonl"))
            .expect("the event log should be readable")
    }

    fn chat_dir(&self, chat_id: &str) -> PathBuf {
        self.data_dir.path().join("chats").join(chat_id)
    }

    fn branch(&self, chat_id: &str) -> String {
        self.manifest(chat_id)["branch"]
            .as_str()
            .expect("a chat names its branch")
            .to_owned()
    }

    /// One commit in the chat's workspace, as its agent would make it,
    /// answering with the sha it landed on.
    fn commit_in_workspace(&self, chat_id: &str, message: &str) -> String {
        let workspace = self.workspace(chat_id);
        fs::write(workspace.join("third.txt"), message).expect("a file should be writable");
        for args in [
            vec!["config", "user.email", "agent@example.invalid"],
            vec!["config", "user.name", "Agent"],
            vec!["add", "."],
            vec!["commit", "-m", message],
        ] {
            run(&workspace, &args);
        }
        says(&workspace, &["rev-parse", "HEAD"])
    }

    /// What the remote says, once whatever was pushed has reached it.
    fn origin_says(&self, args: &[&str]) -> String {
        says(&self.origin.path().join(BARE), args)
    }

    /// A working tree left behind by a chat this dataset has never heard of,
    /// as a crash mid-teardown would leave one.
    fn stray_workspace(&self) -> PathBuf {
        let stray = self.workspace("01KSTRAYWORKSPACE00000000");
        fs::create_dir_all(&stray).expect("a stray workspace should be creatable");
        stray
    }

    /// Take the remote away, so the next push has nowhere to land.
    fn lose_the_remote(&self) {
        fs::remove_dir_all(self.origin.path().join(BARE)).expect("the remote should be removable");
    }
}

/// One `session/update` notification's params, as the adapter sends them.
fn update(said: &str) -> Value {
    json!({
        "sessionId": SESSION,
        "update": {
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": said},
        },
    })
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
    says(cwd, args);
}

fn says(cwd: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
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
        warm_pool: DEFAULT_WARM_POOL,
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
