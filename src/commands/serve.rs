//! Default action: run the HTTP service until shutdown.

use anyhow::{Context as _, Result};
use log::info;
use tokio::net::TcpListener;

use crate::config::Config;
use crate::plane::MemoryPlane;
use crate::server;

/// Execute the serve command.
pub fn run() -> Result<()> {
    let config = Config::from_env()?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to start the async runtime")?
        .block_on(serve(&config))
}

async fn serve(config: &Config) -> Result<()> {
    let listener = TcpListener::bind(config.bind_addr)
        .await
        .with_context(|| format!("failed to bind {}", config.bind_addr))?;
    info!(
        "Serving on {} with data dir {}",
        config.bind_addr,
        config.data_dir.display()
    );
    server::serve(
        listener,
        server::router(config, MemoryPlane::default())?,
        server::shutdown_signal(),
    )
    .await
}
