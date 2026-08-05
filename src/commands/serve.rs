//! Default action: run the HTTP service until shutdown.

use anyhow::{Context as _, Result};
use log::info;
use tokio::net::TcpListener;

use crate::acp::DockerExec;
use crate::chats::Chats;
use crate::config::Config;
use crate::git::{GITHUB, Remotes};
use crate::plane::{DockerPlane, PlaneSettings};
use crate::server;
use crate::store::ChatStore;

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
    ChatStore::new(&config.data_dir).prepare()?;
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
        server::router(config, chats(config)?)?,
        server::shutdown_signal(),
    )
    .await
}

/// The dataset as this deployment reaches it: real containers, real adapters,
/// real repositories on GitHub.
fn chats(config: &Config) -> Result<Chats<DockerPlane, DockerExec>> {
    let plane = DockerPlane::connect(PlaneSettings {
        image: config.workspace_image.clone(),
        memory_mb: config.container_memory_mb,
        cpus: config.container_cpus,
        registry: config.registry.clone(),
    })?;
    Ok(Chats::new(
        config,
        plane,
        DockerExec::connect()?,
        Remotes::new(GITHUB, config.github_token.clone()),
    ))
}
