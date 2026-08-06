//! Default action: run the HTTP service until shutdown.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use log::info;
use tokio::net::TcpListener;

use crate::acp::DockerExec;
use crate::chats::Chats;
use crate::config::Config;
use crate::git::{GITHUB, Remotes};
use crate::plane::{DockerPlane, PlaneSettings};
use crate::secrets::Secrets;
use crate::server;
use crate::settings::Settings;
use crate::store::ChatStore;
use crate::verify::UpstreamVerifier;

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
    let secrets = Arc::new(Secrets::from_config(config));
    let chats = chats(config, Arc::clone(&secrets))?;
    chats.sweep().await;
    let router = server::router(
        config,
        chats,
        Settings::new(secrets, UpstreamVerifier::new()),
    )?;
    let listener = TcpListener::bind(config.bind_addr)
        .await
        .with_context(|| format!("failed to bind {}", config.bind_addr))?;
    info!(
        "Serving on {} with data dir {}",
        config.bind_addr,
        config.data_dir.display()
    );
    server::serve(listener, router, server::shutdown_signal()).await
}

/// The dataset as this deployment reaches it: real containers, real adapters,
/// real repositories on GitHub. Both Docker clients want the daemon's socket
/// to be there before the address is taken.
fn chats(config: &Config, secrets: Arc<Secrets>) -> Result<Chats<DockerPlane, DockerExec>> {
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
        Remotes::new(GITHUB),
        secrets,
    ))
}
