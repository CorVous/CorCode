//! The per-chat `manifest.json` (ADR-0006).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The only manifest schema this build understands.
pub const MANIFEST_SCHEMA: u32 = 1;

/// The persisted half of a chat's lifecycle (ADR-0002); `parked` is derived,
/// never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatState {
    Open,
    Archived,
}

/// Everything about a chat that cannot be derived from disk or Docker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    pub chat_id: String,
    pub title: String,
    pub state: ChatState,
    pub repo: String,
    pub branch: String,
    pub base_branch: String,
    pub last_pushed_commit: Option<String>,
    pub acp_session_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_active_at: DateTime<Utc>,
}

/// What the caller has to decide when a chat is created; the rest is ours.
pub struct NewChat {
    pub title: String,
    pub repo: String,
    pub branch: String,
    pub base_branch: String,
}

impl Manifest {
    /// Open a brand new chat under a freshly minted id.
    #[must_use]
    pub fn open(_new_chat: NewChat) -> Self {
        todo!("B3")
    }
}
