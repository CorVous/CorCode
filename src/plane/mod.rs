//! Workspace containers, spawned as hardened siblings behind a swappable
//! trait (ADR-0001).

mod docker;
mod error;
mod memory;

use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::path::Path;
use std::sync::LazyLock;

use serde_json::Value;

pub use docker::{DockerPlane, PlaneSettings};
pub use error::PlaneError;
pub use memory::{MemoryPlane, Mounts, StartupRun, Teardown};

const NAME_PREFIX: &str = "corcode-chat-";

/// Where a chat's workspace is mounted, the same for every chat forever: the
/// adapter's transcript path encodes it (ADR-0006).
pub const WORKSPACE_MOUNT: &str = "/workspace";

/// The settings the workspace image bakes into `/etc/claude-code`, read here
/// so the file the image ships is the only place they are written down.
const MANAGED_SETTINGS: &str = include_str!("../../docker/workspace/managed-settings.json");

/// The permission mode every session in a workspace container is meant to open
/// in (ADR-0001). A session in any other mode was clamped by the adapter.
#[must_use]
pub fn managed_default_mode() -> &'static str {
    static MODE: LazyLock<String> = LazyLock::new(|| {
        serde_json::from_str::<Value>(MANAGED_SETTINGS)
            .expect("the baked managed settings are json")["permissions"]["defaultMode"]
            .as_str()
            .expect("the baked managed settings name a default permission mode")
            .to_owned()
    });
    &MODE
}

/// The one container a chat may own, named after it so that liveness needs no
/// bookkeeping of its own.
#[must_use]
pub fn container_name(chat_id: &str) -> String {
    format!("{NAME_PREFIX}{chat_id}")
}

/// What running a chat's startup script came to: its combined stdout and
/// stderr as the shell interleaved them, and the exit code it returned
/// (issue #14).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScriptRun {
    pub output: String,
    pub exit_code: i64,
}

/// How long a stop waits before the daemon kills what is in a container
/// (issue #40).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopGrace {
    /// A live container: whatever the chat is still talking to gets every
    /// second the plane allows to finish.
    Full,
    /// A parked container: PID 1 is a keep-alive over a bind-mounted
    /// workspace, so there is nothing in it to wait for.
    None,
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

    /// Run `script` in a chat's live container as the agent, with the workspace
    /// as cwd and `env` visible, once the workspace and env are ready and before
    /// the session opens (issue #14). Answers with the combined output and exit
    /// code; a non-zero exit is a `ScriptRun`, not an error.
    fn run_startup_script(
        &self,
        chat_id: &str,
        script: &str,
        env: &BTreeMap<String, String>,
    ) -> impl Future<Output = Result<ScriptRun, PlaneError>> + Send;

    /// Stop and remove a chat's container, leaving its dirs alone. `grace`
    /// says how long the stop waits for what is inside it.
    fn teardown(
        &self,
        chat_id: &str,
        grace: StopGrace,
    ) -> impl Future<Output = Result<(), PlaneError>> + Send;

    /// The chats that own a running container right now.
    fn list_live(&self) -> impl Future<Output = Result<HashSet<String>, PlaneError>> + Send;
}
