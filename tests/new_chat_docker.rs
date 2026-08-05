//! Docker-gated: the whole new-chat vertical against the real workspace
//! image — a hardened container, the real ACP adapter, a real session id.
//! Skipped, loudly, wherever the daemon, the image, or a key is missing.

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

    let response = client()
        .post(format!("http://{address}/chats"))
        .header("cookie", &cookie)
        .form(&[
            ("repo", REPO),
            ("base_branch", "main"),
            ("slug", "docker gated"),
        ])
        .send()
        .await
        .expect("request");

    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let chat_id = response
        .headers()
        .get("location")
        .expect("a created chat redirects to itself")
        .to_str()
        .expect("a location is text")
        .trim_start_matches("/chats/")
        .to_owned();
    let manifest: Value = serde_json::from_str(
        &fs::read_to_string(
            data_dir
                .path()
                .join("chats")
                .join(&chat_id)
                .join("manifest.json"),
        )
        .expect("the manifest should be readable"),
    )
    .expect("the manifest should be json");
    let session_id = manifest["acp_session_id"]
        .as_str()
        .expect("the real adapter should hand back a session id");
    assert!(
        !session_id.is_empty(),
        "the adapter opened a session under no id"
    );
    let console = client()
        .get(format!("http://{address}/"))
        .header("cookie", &cookie)
        .send()
        .await
        .expect("request")
        .text()
        .await
        .expect("body");
    let live = console
        .find("<h2>Live</h2>")
        .expect("the console has groups");
    let parked = console
        .find("<h2>Parked</h2>")
        .expect("the console has groups");
    let row = console
        .find(&chat_id)
        .expect("the new chat should be on the console");
    assert!(
        live < row && row < parked,
        "the new chat is not live on the console: {console}"
    );

    teardown(&deployment, &chat_id).await;
    shutdown.send(()).expect("server should be listening");
    server
        .await
        .expect("server task should not panic")
        .expect("server should shut down cleanly");
}

/// What this deployment needs before the test means anything: a daemon, the
/// workspace image, and a key the agent can authenticate with.
struct Deployment {
    image: String,
    anthropic_api_key: String,
}

fn deployment() -> Option<Deployment> {
    let reachable = Path::new(DOCKER_SOCKET).exists() || std::env::var_os("DOCKER_HOST").is_some();
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
                "no docker daemon"
            };
            eprintln!("SKIPPED {TEST_NAME}: {missing}");
            None
        }
    }
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
