//! Workspace containers, spawned as hardened siblings behind a swappable
//! trait (ADR-0001).

mod docker;
mod error;
mod memory;

use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::path::Path;

pub use docker::{DockerPlane, PlaneSettings};
pub use error::PlaneError;
pub use memory::MemoryPlane;

const NAME_PREFIX: &str = "corcode-chat-";

/// Where a chat's workspace is mounted, the same for every chat forever: the
/// adapter's transcript path encodes it (ADR-0006).
pub const WORKSPACE_MOUNT: &str = "/workspace";

/// The one container a chat may own, named after it so that liveness needs no
/// bookkeeping of its own.
#[must_use]
pub fn container_name(chat_id: &str) -> String {
    format!("{NAME_PREFIX}{chat_id}")
}

/// A chat's live workspace container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerRef {
    pub chat_id: String,
    pub container_id: String,
    pub name: String,
}

impl ContainerRef {
    fn new(chat_id: &str, container_id: impl Into<String>) -> Self {
        Self {
            chat_id: chat_id.to_owned(),
            container_id: container_id.into(),
            name: container_name(chat_id),
        }
    }
}

/// The agent plane: one workspace container per chat, spawned on demand and
/// thrown away on teardown.
pub trait ContainerPlane {
    /// Start a chat's container over its workspace and agent-memory dirs.
    /// `env` carries the chat's secrets; its values are never logged.
    fn spawn(
        &self,
        chat_id: &str,
        workspace_dir: &Path,
        claude_dir: &Path,
        env: &BTreeMap<String, String>,
    ) -> impl Future<Output = Result<ContainerRef, PlaneError>> + Send;

    /// Stop and remove a chat's container, leaving its dirs alone.
    fn teardown(&self, chat_id: &str) -> impl Future<Output = Result<(), PlaneError>> + Send;

    /// The chats that own a running container right now.
    fn list_live(&self) -> impl Future<Output = Result<HashSet<String>, PlaneError>> + Send;
}
