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
}
