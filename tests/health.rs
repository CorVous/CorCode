//! Integration tests for the HTTP server.

use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use cor_code::config::{Config, DEFAULT_CONTAINER_CPUS, DEFAULT_CONTAINER_MEMORY_MB};
use cor_code::plane::MemoryPlane;

#[tokio::test]
async fn health_endpoint_answers_on_ephemeral_port() {
    let data_dir = TempDir::new().expect("temp dir should be creatable");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("ephemeral port should bind");
    let addr = listener
        .local_addr()
        .expect("listener should report address");
    let config = Config {
        data_dir: data_dir.path().to_path_buf(),
        bind_addr: addr,
        username: "cassidy".to_owned(),
        password_hash: "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA".to_owned(),
        workspace_image: "ghcr.io/corvous/corcode-workspace:2026-08-05".to_owned(),
        container_memory_mb: DEFAULT_CONTAINER_MEMORY_MB,
        container_cpus: DEFAULT_CONTAINER_CPUS,
        registry: None,
        repos: vec!["CorVous/CorCode".to_owned()],
        github_token: None,
        anthropic_api_key: None,
    };
    let router =
        cor_code::server::router(&config, MemoryPlane::default()).expect("router should build");
    let (shutdown, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(cor_code::server::serve(listener, router, async {
        shutdown_rx.await.ok();
    }));

    let response = reqwest::get(format!("http://{addr}/health"))
        .await
        .expect("health request should reach the server");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("response should have a body");
    assert_eq!(body, "ok");

    shutdown.send(()).expect("server should still be listening");
    server
        .await
        .expect("server task should not panic")
        .expect("server should shut down cleanly");
}
