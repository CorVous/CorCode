//! Integration tests for the new-chat vertical: form submit to a cloned
//! workspace, a cut branch, a spawned container and a recorded ACP session
//! (ADR-0005, ADR-0006).

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use argon2::password_hash::{PasswordHasher as _, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use chrono::Utc;
use reqwest::{Client, StatusCode, redirect::Policy};
use serde_json::Value;
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
use cor_code::secrets::Secrets;
use cor_code::server;
use cor_code::store::ChatStore;

const USERNAME: &str = "cassidy";
const PASSWORD: &str = "correct horse battery staple";
const REPO: &str = "CorVous/fixture";
const BARE: &str = "CorVous/fixture.git";
const SESSION: &str = "3f2b1c4d-0000-4000-8000-000000000001";

#[tokio::test]
async fn a_new_chat_arrives_live_with_a_checked_out_workspace_and_a_session() {
    let app = TestApp::start().await;

    let response = app
        .create(&[
            ("repo", REPO),
            ("base_branch", "main"),
            ("slug", "Resume Ladder!"),
        ])
        .await;

    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let chat_id = chat_id_of(&response);
    let branch = format!("chat/{}-resume-ladder", Utc::now().format("%Y-%m-%d"));
    let manifest = app.manifest(&chat_id);
    assert_eq!(
        manifest["title"], "Resume Ladder!",
        "the operator's own words should survive into the manifest"
    );
    assert_eq!(manifest["branch"], branch);
    assert_eq!(manifest["base_branch"], "main");
    assert_eq!(manifest["repo"], REPO);
    assert_eq!(manifest["acp_session_id"], SESSION);
    let workspace = app.workspace(&chat_id);
    assert!(
        workspace.join("README.md").is_file(),
        "the workspace was not cloned: {workspace:?}"
    );
    assert_eq!(head_of(&workspace), branch);
    assert_eq!(
        fs::read_to_string(app.chat_dir(&chat_id).join("events.jsonl"))
            .expect("the event log should exist"),
        "",
        "opening a session is not an event (ADR-0006)"
    );
    let console = app.body("/").await;
    assert!(
        under_live(&console, &chat_id),
        "the new chat is not live on the console: {console}"
    );
    app.stop().await;
}

#[tokio::test]
async fn a_chat_can_opt_out_and_work_directly_on_the_base_branch() {
    let app = TestApp::start().await;

    let response = app
        .create(&[
            ("repo", REPO),
            ("base_branch", "main"),
            ("slug", "straight-to-main"),
            ("direct_on_base", "on"),
        ])
        .await;

    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let chat_id = chat_id_of(&response);
    assert_eq!(app.manifest(&chat_id)["branch"], "main");
    assert_eq!(head_of(&app.workspace(&chat_id)), "main");
    app.stop().await;
}

#[tokio::test]
async fn the_console_form_submits_the_fields_the_handler_reads() {
    let app = TestApp::start().await;
    let console = app.body("/").await;
    let fields = new_chat_fields(&console);

    let response = app
        .create(
            &fields
                .iter()
                .map(|field| (field.as_str(), typed_into(field)))
                .collect::<Vec<_>>(),
        )
        .await;

    assert!(
        console.contains("<input type=\"checkbox\" name=\"direct_on_base\">"),
        "the opt-out is not a checkbox on the form: {console}"
    );
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(app.manifest(&chat_id_of(&response))["repo"], REPO);
    app.stop().await;
}

/// The field names the console's new-chat form would submit.
fn new_chat_fields(console: &str) -> Vec<String> {
    let form = console
        .split_once("action=\"/chats\"")
        .expect("the console should carry a new-chat form")
        .1
        .split_once("</form>")
        .expect("the new-chat form should be closed")
        .0;
    form.match_indices("name=\"")
        .map(|(at, marker)| {
            form[at + marker.len()..]
                .split_once('"')
                .expect("an attribute should be quoted")
                .0
                .to_owned()
        })
        .collect()
}

/// What a browser would put in each field the form offers.
fn typed_into(field: &str) -> &'static str {
    match field {
        "repo" => REPO,
        "base_branch" => "main",
        "slug" => "from the console",
        "direct_on_base" => "on",
        other => panic!("the form offers {other}, which the handler was never shown"),
    }
}

#[tokio::test]
async fn a_slug_that_names_nothing_is_refused_before_anything_is_built() {
    let app = TestApp::start().await;

    let response = app
        .create(&[("repo", REPO), ("base_branch", "main"), ("slug", "!!!")])
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(app.chats_on_disk(), 0, "a refused chat was still built");
    app.stop().await;
}

#[tokio::test]
async fn a_repository_this_deployment_does_not_offer_is_refused() {
    let app = TestApp::start().await;

    let response = app
        .create(&[
            ("repo", "someone/else"),
            ("base_branch", "main"),
            ("slug", "elsewhere"),
        ])
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(app.chats_on_disk(), 0, "a refused chat was still built");
    app.stop().await;
}

#[tokio::test]
async fn a_clone_that_fails_leaves_the_chat_on_disk_and_nothing_running() {
    let app = TestApp::start().await;

    let response = app
        .create(&[
            ("repo", REPO),
            ("base_branch", "no-such-branch"),
            ("slug", "doomed"),
        ])
        .await;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = response.text().await.expect("body");
    assert!(
        !body.contains(&app.data_dir.path().display().to_string()),
        "the failure spelled out where the dataset lives: {body}"
    );
    assert_eq!(
        app.chats_on_disk(),
        1,
        "the half-built chat was cleaned up behind the operator's back"
    );
    let console = app.body("/").await;
    assert!(
        !under_live(&console, "doomed"),
        "a chat that never cloned is live: {console}"
    );
    app.stop().await;
}

#[tokio::test]
async fn creating_a_chat_is_behind_the_session_gate() {
    let app = TestApp::start().await;

    let response = client()
        .post(app.url("/chats"))
        .form(&[("repo", REPO), ("base_branch", "main"), ("slug", "sneaked")])
        .send()
        .await
        .expect("request");

    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response
            .headers()
            .get("location")
            .expect("a turned-away request is sent to the login form")
            .to_str()
            .expect("a location is text"),
        "/login"
    );
    assert_eq!(app.chats_on_disk(), 0, "an ungated chat was built");
    app.stop().await;
}

#[tokio::test]
async fn a_chat_cannot_be_created_from_another_origin() {
    let app = TestApp::start().await;

    let response = client()
        .post(app.url("/chats"))
        .header("cookie", &app.cookie)
        .header("origin", "http://evil.example")
        .form(&[
            ("repo", REPO),
            ("base_branch", "main"),
            ("slug", "cross-site"),
        ])
        .send()
        .await
        .expect("request");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(app.chats_on_disk(), 0, "a cross-site chat was built");
    app.stop().await;
}

/// The core writes the dataset through whatever it has mounted, but the
/// sibling daemon only knows the host's name for the same directories
/// (ADR-0001): a bind carrying the core's own mount point would hand the
/// agent an empty workspace.
#[tokio::test]
async fn a_containerised_core_hands_the_daemon_the_hosts_own_paths() {
    let host_root = Path::new("/mnt/tank/corcode");
    let app = TestApp::start_reading_the_dataset_at(Some(host_root)).await;

    let response = app
        .create(&[("repo", REPO), ("base_branch", "main"), ("slug", "mounted")])
        .await;

    let chat_id = chat_id_of(&response);
    let mounts = app
        .plane
        .mounts_of(&chat_id)
        .expect("the new chat should hold a container");
    assert_eq!(
        mounts.workspace,
        host_root.join("workspaces").join(&chat_id)
    );
    assert_eq!(
        mounts.claude,
        host_root.join("chats").join(&chat_id).join("claude")
    );
    assert!(
        app.workspace(&chat_id).join("README.md").is_file(),
        "the core stopped writing through its own mount"
    );
    app.stop().await;
}

#[tokio::test]
async fn a_core_on_the_host_binds_the_very_paths_it_reads_itself() {
    let app = TestApp::start().await;

    let response = app
        .create(&[("repo", REPO), ("base_branch", "main"), ("slug", "on-box")])
        .await;

    let chat_id = chat_id_of(&response);
    let mounts = app
        .plane
        .mounts_of(&chat_id)
        .expect("the new chat should hold a container");
    assert_eq!(mounts.workspace, app.workspace(&chat_id));
    assert_eq!(mounts.claude, app.chat_dir(&chat_id).join("claude"));
    app.stop().await;
}

/// The chat id the redirect points a browser at.
fn chat_id_of(response: &reqwest::Response) -> String {
    let location = response
        .headers()
        .get("location")
        .expect("a created chat redirects to itself")
        .to_str()
        .expect("a location is text");
    location
        .strip_prefix("/chats/")
        .unwrap_or_else(|| panic!("the redirect does not point at a chat: {location}"))
        .to_owned()
}

/// Whether `needle` is rendered under the console's Live heading.
fn under_live(console: &str, needle: &str) -> bool {
    let live = console.find("<h2>Live</h2>");
    let parked = console.find("<h2>Parked</h2>");
    match (live, parked, console.find(needle)) {
        (Some(live), Some(parked), Some(at)) => live < at && at < parked,
        _ => false,
    }
}

fn head_of(workspace: &Path) -> String {
    git_says(workspace, &["rev-parse", "--abbrev-ref", "HEAD"])
}

struct TestApp {
    address: SocketAddr,
    cookie: String,
    plane: MemoryPlane,
    shutdown: oneshot::Sender<()>,
    server: JoinHandle<anyhow::Result<()>>,
    data_dir: TempDir,
    _origin: TempDir,
}

impl TestApp {
    async fn start() -> Self {
        Self::start_reading_the_dataset_at(None).await
    }

    /// The app over a dataset the sibling daemon knows by `host_data_dir`,
    /// which is the core's own path unless the core is containerised.
    async fn start_reading_the_dataset_at(host_data_dir: Option<&Path>) -> Self {
        let data_dir = TempDir::new().expect("temp dir should be creatable");
        let (origin, remotes) = seeded_repository();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral port should bind");
        let address = listener.local_addr().expect("listener reports its address");
        let config = test_config(
            data_dir.path().to_path_buf(),
            host_data_dir.map_or_else(|| data_dir.path().to_path_buf(), Path::to_path_buf),
        );
        ChatStore::new(data_dir.path())
            .prepare()
            .expect("the dataset should prepare, as serving does");
        let plane = MemoryPlane::default();
        let chats = Chats::new(
            &config,
            plane.clone(),
            ScriptedAdapter::opening(SESSION),
            remotes,
            Arc::new(Secrets::from_config(&config)),
        );
        let router = server::router(&config, chats).expect("router should build");
        let (shutdown, shutdown_rx) = oneshot::channel();
        let server = tokio::spawn(server::serve(listener, router, async {
            shutdown_rx.await.ok();
        }));
        let cookie = sign_in(address).await;
        Self {
            address,
            cookie,
            plane,
            shutdown,
            server,
            data_dir,
            _origin: origin,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    async fn create(&self, form: &[(&str, &str)]) -> reqwest::Response {
        client()
            .post(self.url("/chats"))
            .header("cookie", &self.cookie)
            .form(form)
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

    fn chat_dir(&self, chat_id: &str) -> PathBuf {
        self.data_dir.path().join("chats").join(chat_id)
    }

    fn workspace(&self, chat_id: &str) -> PathBuf {
        self.data_dir.path().join("workspaces").join(chat_id)
    }

    fn manifest(&self, chat_id: &str) -> Value {
        let path = self.chat_dir(chat_id).join("manifest.json");
        serde_json::from_str(&fs::read_to_string(&path).expect("the manifest should be readable"))
            .expect("the manifest should be json")
    }

    /// How many chats the dataset holds, staging area aside.
    fn chats_on_disk(&self) -> usize {
        fs::read_dir(self.data_dir.path().join("chats"))
            .expect("the chats dir should be readable")
            .filter(|entry| {
                !entry
                    .as_ref()
                    .expect("entry should be readable")
                    .file_name()
                    .to_string_lossy()
                    .starts_with('.')
            })
            .count()
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
    (dir, Remotes::new(served_from))
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

fn git_says(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git should run");
    assert!(output.status.success(), "git {args:?} failed");
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

fn test_config(data_dir: PathBuf, host_data_dir: PathBuf) -> Config {
    Config {
        data_dir,
        host_data_dir,
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
