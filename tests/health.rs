//! Integration tests for the HTTP server.

use tokio::net::TcpListener;
use tokio::sync::oneshot;

#[tokio::test]
async fn health_endpoint_answers_on_ephemeral_port() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("ephemeral port should bind");
    let addr = listener
        .local_addr()
        .expect("listener should report address");
    let (shutdown, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(cor_code::server::serve(listener, async {
        shutdown_rx.await.ok();
    }));

    let response = reqwest::get(format!("http://{addr}/health"))
        .await
        .expect("health request should reach the server");
    assert_eq!(response.status(), 200);

    shutdown.send(()).expect("server should still be listening");
    server
        .await
        .expect("server task should not panic")
        .expect("server should shut down cleanly");
}
