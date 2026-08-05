//! HTTP server: routes, serving, and graceful shutdown.

use std::future::Future;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use axum::Router;
use axum::routing::get;
use tokio::net::TcpListener;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};

use crate::auth::keystore::KeyStore;
use crate::config::Config;

/// Name of the cookie carrying the session (ADR-0003).
pub const SESSION_COOKIE: &str = "corcode_session";

/// Everything the handlers share.
#[derive(Clone)]
struct AppState {
    keys: Arc<KeyStore>,
}

/// Build the application's routes.
pub fn router(config: &Config) -> Result<Router> {
    let state = AppState {
        keys: Arc::new(KeyStore::open(&config.data_dir)?),
    };
    Ok(Router::new()
        .route("/health", get(health))
        .with_state(state))
}

/// Unauthenticated liveness probe (ADR-0003).
async fn health() -> &'static str {
    "ok"
}

/// Serve `router` on `listener` until `shutdown` resolves, then wait for
/// in-flight requests to finish.
pub async fn serve<S>(listener: TcpListener, router: Router, shutdown: S) -> Result<()>
where
    S: Future<Output = ()> + Send + 'static,
{
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
        .context("HTTP server failed")
}

/// Resolve when the process is asked to stop: Ctrl-C or SIGTERM.
#[cfg(unix)]
pub async fn shutdown_signal() {
    let mut interrupt = signal(SignalKind::interrupt()).expect("SIGINT handler should install");
    let mut terminate = signal(SignalKind::terminate()).expect("SIGTERM handler should install");
    tokio::select! {
        _ = interrupt.recv() => {},
        _ = terminate.recv() => {},
    }
}

/// Resolve when the process is asked to stop: Ctrl-C (SIGTERM has no Unix
/// signal handling to fall back to here).
#[cfg(not(unix))]
pub async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("Ctrl-C handler should install");
}
