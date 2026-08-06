//! End-to-end test: `Config` reaches a real bound socket. Socket-gated —
//! serving builds its Docker clients before it binds, and those refuse to be
//! built unless the daemon's socket file is there. No daemon is talked to.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// Where bollard looks for the daemon, whatever `DOCKER_HOST` says.
const DOCKER_SOCKET: &str = "/var/run/docker.sock";

#[tokio::test]
async fn serve_binds_the_configured_address() {
    if !socket_is_there() {
        eprintln!("SKIPPED serve_binds_the_configured_address: no docker socket");
        return;
    }
    let data_dir = TempDir::new().expect("temp dir should be creatable");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("ephemeral port should bind");
    let addr = listener
        .local_addr()
        .expect("listener should report address");
    drop(listener);

    let mut child = Command::new(env!("CARGO_BIN_EXE_cor-code"))
        .env_remove("RUST_LOG")
        .env("CORCODE_DATA_DIR", data_dir.path())
        .env("CORCODE_BIND_ADDR", addr.to_string())
        .env("CORCODE_USERNAME", "cassidy")
        .env(
            "CORCODE_PASSWORD_HASH",
            "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA",
        )
        .env(
            "CORCODE_WORKSPACE_IMAGE",
            "ghcr.io/corvous/corcode-workspace:2026-08-05",
        )
        .env("CORCODE_REPOS", "CorVous/CorCode")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("binary should spawn");

    let url = format!("http://{addr}/health");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(response) = reqwest::get(&url).await {
            assert_eq!(response.status(), 200);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "server on CORCODE_BIND_ADDR={addr} did not become healthy in time"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    child.kill().expect("child should be killable");
    child.wait().expect("child should exit after being killed");
}

/// Whether the socket bollard will open is there. `DOCKER_HOST` is no help:
/// the local-defaults connector never reads it, so a `tcp://` daemon is a
/// daemon these tests cannot reach.
fn socket_is_there() -> bool {
    Path::new(DOCKER_SOCKET).exists()
}
