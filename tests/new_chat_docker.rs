//! Docker-gated: the whole new-chat vertical against the real workspace
//! image — a hardened container, the real ACP adapter, a real session id.
//! Skipped, loudly, wherever the socket, the image, or a key is missing.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;

use argon2::password_hash::{PasswordHasher as _, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use reqwest::{Client, StatusCode, redirect::Policy};
use serde_json::Value;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use cor_code::acp::DockerExec;
use cor_code::chats::Chats;
use cor_code::config::{Config, DEFAULT_CONTAINER_CPUS, DEFAULT_CONTAINER_MEMORY_MB};
use cor_code::git::Remotes;
use cor_code::plane::{ContainerPlane, DockerPlane, PlaneError, PlaneSettings};
use cor_code::server;
use cor_code::store::ChatStore;

const DOCKER_SOCKET: &str = "/var/run/docker.sock";
const USERNAME: &str = "cassidy";
const PASSWORD: &str = "correct horse battery staple";
const REPO: &str = "CorVous/fixture";
const BARE: &str = "CorVous/fixture.git";
const TEST_NAME: &str = "the_real_adapter_opens_a_session_inside_a_hardened_container";

#[tokio::test]
async fn the_real_adapter_opens_a_session_inside_a_hardened_container() {
    let Some(deployment) = deployment() else {
        return;
    };
    let data_dir = TempDir::new().expect("temp dir should be creatable");
    let (_origin, remotes) = seeded_repository();
    let config = test_config(data_dir.path().to_path_buf(), &deployment);
    ChatStore::new(data_dir.path())
        .prepare()
        .expect("the dataset should prepare, as serving does");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("ephemeral port should bind");
    let address = listener.local_addr().expect("listener reports its address");
    let chats = Chats::new(
        &config,
        DockerPlane::connect(settings(&deployment)).expect("the daemon should be reachable"),
        DockerExec::connect().expect("the daemon should be reachable"),
        remotes,
    );
    let router = server::router(&config, chats).expect("router should build");
    let (shutdown, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(server::serve(listener, router, async {
        shutdown_rx.await.ok();
    }));
    let cookie = sign_in(address).await;

    let created = create_a_chat(address, &cookie, data_dir.path()).await;

    if let Some(chat_id) = only_chat(data_dir.path()) {
        teardown(&deployment, &chat_id).await;
    }
    shutdown.send(()).expect("server should be listening");
    server
        .await
        .expect("server task should not panic")
        .expect("server should shut down cleanly");
    assert_created(&created);
}

/// What the run leaves behind, gathered before anything is torn down so that
/// a failed assertion cannot strand a container.
struct Created {
    status: StatusCode,
    chat_id: Option<String>,
    manifest: Option<Value>,
    console: String,
}

async fn create_a_chat(address: SocketAddr, cookie: &str, data_dir: &Path) -> Created {
    let response = client()
        .post(format!("http://{address}/chats"))
        .header("cookie", cookie)
        .form(&[
            ("repo", REPO),
            ("base_branch", "main"),
            ("slug", "docker gated"),
        ])
        .send()
        .await
        .expect("request");
    let status = response.status();
    let chat_id = only_chat(data_dir);
    let manifest = chat_id.as_ref().and_then(|chat_id| {
        let path = data_dir.join("chats").join(chat_id).join("manifest.json");
        fs::read_to_string(path)
            .ok()
            .and_then(|json| serde_json::from_str(&json).ok())
    });
    let console = client()
        .get(format!("http://{address}/"))
        .header("cookie", cookie)
        .send()
        .await
        .expect("request")
        .text()
        .await
        .expect("body");
    Created {
        status,
        chat_id,
        manifest,
        console,
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

fn deployment() -> Option<Deployment> {
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
            eprintln!("SKIPPED {TEST_NAME}: {missing}");
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
        registry: None,
    }
}

fn test_config(data_dir: PathBuf, deployment: &Deployment) -> Config {
    Config {
        data_dir,
        bind_addr: "127.0.0.1:0".parse().expect("valid address"),
        username: USERNAME.to_owned(),
        password_hash: password_hash(PASSWORD),
        workspace_image: deployment.image.clone(),
        container_memory_mb: DEFAULT_CONTAINER_MEMORY_MB,
        container_cpus: DEFAULT_CONTAINER_CPUS,
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
