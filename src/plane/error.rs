//! Failures of the container plane, each naming the chat it happened to.

use std::path::PathBuf;

use thiserror::Error;

/// Something the plane refuses to guess about.
#[derive(Debug, Error)]
pub enum PlaneError {
    #[error("chat {chat_id} has no live container")]
    NotLive { chat_id: String },
    #[error("chat {chat_id} already has a live container")]
    AlreadyLive { chat_id: String },
    #[error("{} cannot be spelled as a mount", path.display())]
    UnmountablePath { path: PathBuf },
    #[error("the {network} network routes nowhere: agents on it could not reach the API")]
    UnusableNetwork { network: String },
    #[error(
        "the {network} network routes nowhere and has containers on it: stop them, then start a chat again to have it replaced"
    )]
    NetworkInUse { network: String },
    #[error("the container runtime failed to {action}")]
    Runtime {
        action: String,
        source: bollard::errors::Error,
    },
}

impl PlaneError {
    /// Blame `action` for whatever the daemon says went wrong doing it.
    pub(super) fn runtime(
        action: impl Into<String>,
    ) -> impl FnOnce(bollard::errors::Error) -> Self {
        let action = action.into();
        move |source| Self::Runtime { action, source }
    }
}
