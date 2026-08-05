//! HTTP server: routes, serving, and graceful shutdown.

use std::future::Future;

use anyhow::{Context as _, Result};
use axum::Router;
use axum::routing::get;
use tokio::net::TcpListener;

/// Build the application's routes.
pub fn router() -> Router {
    Router::new().route("/health", get(health))
}

/// Unauthenticated liveness probe (ADR-0003).
async fn health() -> &'static str {
    "ok"
}

/// Serve the application on `listener` until `shutdown` resolves,
/// then wait for in-flight requests to finish.
pub async fn serve<S>(listener: TcpListener, shutdown: S) -> Result<()>
where
    S: Future<Output = ()> + Send + 'static,
{
    axum::serve(listener, router())
        .with_graceful_shutdown(shutdown)
        .await
        .context("HTTP server failed")
}
