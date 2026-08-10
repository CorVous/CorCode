//! Docker-gated: the whole vertical against the real workspace image — a
//! hardened container, the real ACP adapter, a real session id, and a real
//! turn. Skipped, loudly, wherever the socket, the image, or a key is missing.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, Once};

use argon2::password_hash::{PasswordHasher as _, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use log::{LevelFilter, Log, Metadata, Record};
use reqwest::{Client, StatusCode, redirect::Policy};
use serde_json::Value;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use cor_code::acp::{DockerExec, OPENED_IN};
use cor_code::chats::Chats;
use cor_code::config::{
    Config, DEFAULT_CONTAINER_CPUS, DEFAULT_CONTAINER_MEMORY_MB, DEFAULT_SCRATCH_MB,
    DEFAULT_WARM_POOL,
};
use cor_code::git::Remotes;
use cor_code::plane::{
    ContainerPlane, DockerPlane, PlaneError, PlaneSettings, managed_default_mode,
};
use cor_code::secrets::Secrets;
use cor_code::server;
use cor_code::settings::Settings;
use cor_code::store::{ChatStore, Owner};
use cor_code::verify::ScriptedVerifier;

const DOCKER_SOCKET: &str = "/var/run/docker.sock";
const USERNAME: &str = "cassidy";
const PASSWORD: &str = "correct horse battery staple";
const REPO: &str = "CorVous/fixture";
const BARE: &str = "CorVous/fixture.git";

/// A prompt that costs one short answer and needs nothing of the workspace.
const TRIVIAL: &str = "Reply with the single word: ready";

/// The word the trivial prompt asks for, as the rendered log would hold it.
const READY: &str = "ready";

/// What the core calls a session running in a mode nobody asked for. The real
/// adapter clamps the managed mode when its model will not take it, and this
/// line is the only place that shows (ADR-0001, issue #58).
const MODE_NOTICE: &str = "mode_notice";

#[tokio::test]
async fn the_real_adapter_opens_a_session_inside_a_hardened_container() {
    let Some(deployment) =
        deployment("the_real_adapter_opens_a_session_inside_a_hardened_container")
    else {
        return;
    };
    let serving = Serving::start(&deployment).await;

    let created = serving.create_a_chat().await;

    serving.stop(&deployment).await;
    assert_created(&created);
}

#[tokio::test]
async fn a_real_agent_answers_a_prompt_into_the_log_the_browser_reads() {
    let Some(deployment) =
        deployment("a_real_agent_answers_a_prompt_into_the_log_the_browser_reads")
    else {
        return;
    };
    let serving = Serving::start(&deployment).await;

    let created = serving.create_a_chat().await;
    let turn = serving.take_a_turn(created.chat_id.as_deref()).await;

    serving.stop(&deployment).await;
    assert_created(&created);
    assert_answered(&turn.expect("a created chat should be promptable"));
}

/// What creating a chat leaves behind, gathered before anything is torn down
/// so that a failed assertion cannot strand a container.
struct Created {
    status: StatusCode,
    chat_id: Option<String>,
    manifest: Option<Value>,
    notices: Vec<Value>,
    console: String,
}

/// What one real turn leaves behind: what the prompt answered, what the turn
/// wrote down, and what the browser's next poll would show.
struct Turn {
    status: StatusCode,
    events: Vec<Value>,
    log: String,
}

/// The console, served over the real daemon for the length of one test.
struct Serving {
    address: SocketAddr,
    cookie: String,
    data_dir: TempDir,
    shutdown: oneshot::Sender<()>,
    server: JoinHandle<anyhow::Result<()>>,
    _origin: TempDir,
}

impl Serving {
    async fn start(deployment: &Deployment) -> Self {
        capture_the_log();
        let data_dir = TempDir::new().expect("temp dir should be creatable");
        let (origin, remotes) = seeded_repository();
        let config = test_config(data_dir.path().to_path_buf(), deployment);
        ChatStore::new(data_dir.path())
            .prepare()
            .expect("the dataset should prepare, as serving does");
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral port should bind");
        let address = listener.local_addr().expect("listener reports its address");
        let secrets = Arc::new(Secrets::from_config(&config));
        let chats = Chats::new(
            &config,
            Owner::of(&config.data_dir).expect("we own the dataset we just made"),
            DockerPlane::connect(settings(deployment)).expect("the daemon should be reachable"),
            DockerExec::connect().expect("the daemon should be reachable"),
            remotes,
            Arc::clone(&secrets),
        );
        let router = server::router(
            &config,
            chats,
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
            data_dir,
            shutdown,
            server,
            _origin: origin,
        }
    }

    async fn create_a_chat(&self) -> Created {
        let response = client()
            .post(self.url("/chats"))
            .header("cookie", &self.cookie)
            .form(&[
                ("repo", REPO),
                ("base_branch", "main"),
                ("slug", "docker gated"),
            ])
            .send()
            .await
            .expect("request");
        let status = response.status();
        let chat_id = only_chat(self.data_dir.path());
        let manifest = chat_id.as_ref().and_then(|chat_id| {
            let path = self
                .data_dir
                .path()
                .join("chats")
                .join(chat_id)
                .join("manifest.json");
            fs::read_to_string(path)
                .ok()
                .and_then(|json| serde_json::from_str(&json).ok())
        });
        let notices = chat_id
            .as_deref()
            .map(|chat_id| core_lines(self.data_dir.path(), chat_id))
            .unwrap_or_default();
        Created {
            status,
            chat_id,
            manifest,
            notices,
            console: self.body("/").await,
        }
    }

    /// Put the trivial prompt to the agent and wait out the whole turn.
    async fn take_a_turn(&self, chat_id: Option<&str>) -> Option<Turn> {
        let chat_id = chat_id?;
        let response = client()
            .post(self.url(&format!("/chats/{chat_id}/prompt")))
            .header("cookie", &self.cookie)
            .form(&[("prompt", TRIVIAL)])
            .send()
            .await
            .expect("request");
        Some(Turn {
            status: response.status(),
            events: events(self.data_dir.path(), chat_id),
            log: self.body(&format!("/chats/{chat_id}/events")).await,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    async fn body(&self, path: &str) -> String {
        client()
            .get(self.url(path))
            .header("cookie", &self.cookie)
            .send()
            .await
            .expect("request")
            .text()
            .await
            .expect("body")
    }

    /// Everything this test started, gone again whatever it decided.
    async fn stop(self, deployment: &Deployment) {
        if let Some(chat_id) = only_chat(self.data_dir.path()) {
            teardown(deployment, &chat_id).await;
        }
        self.shutdown.send(()).expect("server should be listening");
        self.server
            .await
            .expect("server task should not panic")
            .expect("server should shut down cleanly");
    }
}

fn assert_created(created: &Created) {
    assert_eq!(created.status, StatusCode::SEE_OTHER);
    let chat_id = created
        .chat_id
        .as_ref()
        .expect("the dataset should hold the new chat");
    let manifest = created
        .manifest
        .as_ref()
        .expect("the new chat should have a manifest");
    let session_id = manifest["acp_session_id"]
        .as_str()
        .expect("the real adapter should hand back a session id");
    assert!(
        !session_id.is_empty(),
        "the adapter opened a session under no id"
    );
    assert_eq!(
        logged_mode_of(session_id).as_deref(),
        Some(managed_default_mode()),
        "the real adapter named no mode this build could read, or named another one"
    );
    let clamped: Vec<&Value> = created
        .notices
        .iter()
        .filter(|notice| notice["corcode"] == MODE_NOTICE)
        .collect();
    assert!(
        clamped.is_empty(),
        "a session in the mode this deployment asks for was written up as clamped: {clamped:?}"
    );
    let console = &created.console;
    let live = console
        .find("<h2>Live</h2>")
        .expect("the console has groups");
    let parked = console
        .find("<h2>Parked</h2>")
        .expect("the console has groups");
    let row = console
        .find(chat_id.as_str())
        .expect("the new chat should be on the console");
    assert!(
        live < row && row < parked,
        "the new chat is not live on the console: {console}"
    );
}

fn assert_answered(turn: &Turn) {
    assert_eq!(turn.status, StatusCode::OK);
    let said = agent_text(&turn.events);
    assert!(
        said.to_lowercase().contains(READY),
        "the agent never answered in the log on disk: {:?}",
        turn.events
    );
    assert!(
        turn.log.to_lowercase().contains(READY),
        "the agent's answer is not in the log the browser polls: {}",
        turn.log
    );
}

/// Everything the agent said over the turn, run together.
fn agent_text(events: &[Value]) -> String {
    events
        .iter()
        .filter(|event| event["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|event| event["content"]["text"].as_str())
        .collect()
}

/// The ACP payloads one chat's event log holds, in order.
fn events(data_dir: &Path, chat_id: &str) -> Vec<Value> {
    let path = data_dir.join("chats").join(chat_id).join("events.jsonl");
    fs::read_to_string(&path)
        .expect("the event log should exist")
        .lines()
        .map(|line| {
            let event: Value = serde_json::from_str(line).expect("a line should be json");
            event["event"].clone()
        })
        .collect()
}

/// Everything the core logged in this process. The mode a session opened in
/// is written down nowhere — a notice only says when it was the wrong one —
/// so the log is where this suite reads it, and reading it is the whole of
/// what proves the answer's field is still the one the core parses.
static LOGGED: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct Capture;

impl Log for Capture {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &Record<'_>) {
        LOGGED
            .lock()
            .expect("no holder of the lock panics")
            .push(record.args().to_string());
    }

    fn flush(&self) {}
}

/// Keep what the core logs from here on. Every test in this binary shares the
/// one logger the process may have, so the first to ask installs it.
fn capture_the_log() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        log::set_logger(&Capture).expect("nothing else logs in this suite");
        log::set_max_level(LevelFilter::Info);
    });
}

/// The permission mode the core logged `session_id` as being in, where it
/// logged one at all.
fn logged_mode_of(session_id: &str) -> Option<String> {
    LOGGED
        .lock()
        .expect("no holder of the lock panics")
        .iter()
        .filter(|line| line.contains(session_id))
        .find_map(|line| line.split_once(OPENED_IN))
        .map(|(_, mode)| mode.trim().to_owned())
}

/// The lines the core wrote in a chat's log in its own voice (ADR-0006).
fn core_lines(data_dir: &Path, chat_id: &str) -> Vec<Value> {
    events(data_dir, chat_id)
        .into_iter()
        .filter(|event| event["corcode"].is_string())
        .collect()
}

/// The one chat the dataset holds, whether or not creating it got as far as
/// the redirect: a chat that failed halfway may still own a container.
fn only_chat(data_dir: &Path) -> Option<String> {
    fs::read_dir(data_dir.join("chats"))
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .find(|name| !name.starts_with('.'))
}

/// What this deployment needs before the test means anything: a daemon, the
/// workspace image, and a key the agent can authenticate with.
struct Deployment {
    image: String,
    anthropic_api_key: String,
}

fn deployment(test_name: &str) -> Option<Deployment> {
    let reachable = socket_is_there();
    let image = std::env::var("CORCODE_WORKSPACE_IMAGE").ok();
    let anthropic_api_key = std::env::var("CORCODE_ANTHROPIC_API_KEY")
        .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
        .ok();
    match (reachable, image, anthropic_api_key) {
        (true, Some(image), Some(anthropic_api_key)) => Some(Deployment {
            image,
            anthropic_api_key,
        }),
        (reachable, image, _) => {
            let missing = if reachable {
                if image.is_none() {
                    "no CORCODE_WORKSPACE_IMAGE"
                } else {
                    "no ANTHROPIC_API_KEY"
                }
            } else {
                "no docker socket"
            };
            eprintln!("SKIPPED {test_name}: {missing}");
            None
        }
    }
}

/// Whether the socket bollard will open is there. `DOCKER_HOST` is no help:
/// the local-defaults connector never reads it, so a `tcp://` daemon is a
/// daemon this test cannot reach.
fn socket_is_there() -> bool {
    Path::new(DOCKER_SOCKET).exists()
}

/// The container the chat ran in, gone again whatever the test decided.
async fn teardown(deployment: &Deployment, chat_id: &str) {
    let plane =
        DockerPlane::connect(settings(deployment)).expect("the daemon should still be reachable");
    match plane.teardown(chat_id).await {
        Ok(()) | Err(PlaneError::NotLive { .. }) => {}
        Err(error) => panic!("the chat's container should be removable: {error}"),
    }
}

fn settings(deployment: &Deployment) -> PlaneSettings {
    PlaneSettings {
        image: deployment.image.clone(),
        memory_mb: DEFAULT_CONTAINER_MEMORY_MB,
        cpus: DEFAULT_CONTAINER_CPUS,
        scratch_mb: DEFAULT_SCRATCH_MB,
        registry: None,
    }
}

fn test_config(data_dir: PathBuf, deployment: &Deployment) -> Config {
    Config {
        host_data_dir: data_dir.clone(),
        data_dir,
        bind_addr: "127.0.0.1:0".parse().expect("valid address"),
        username: USERNAME.to_owned(),
        password_hash: password_hash(PASSWORD),
        workspace_image: deployment.image.clone(),
        container_memory_mb: DEFAULT_CONTAINER_MEMORY_MB,
        container_cpus: DEFAULT_CONTAINER_CPUS,
        scratch_mb: DEFAULT_SCRATCH_MB,
        warm_pool: DEFAULT_WARM_POOL,
        registry: None,
        repos: vec![REPO.to_owned()],
        github_token: None,
        anthropic_api_key: Some(deployment.anthropic_api_key.clone()),
    }
}

/// A bare repository with a commit on `main`, reachable over `file://` so
/// that even this test needs no network of its own.
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

/// A deliberately cheap argon2 hash: this test verifies one of them.
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
