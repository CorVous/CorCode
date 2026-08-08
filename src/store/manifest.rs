//! The per-chat `manifest.json` (ADR-0006).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

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
    /// Where the archive gate put a dirty tree, if it found one (ADR-0005).
    /// Chats archived before this field existed carry no such branch, so a
    /// manifest without it reads as one rather than as a broken file.
    #[serde(default)]
    pub checkpoint_branch: Option<String>,
    pub last_pushed_commit: Option<String>,
    pub acp_session_id: Option<String>,
    /// Custom environment injected into the agent container on every spawn,
    /// system credentials always winning a name clash (issue #14). Chats cut
    /// before this field existed carry none, so a manifest without it reads as
    /// empty rather than as a broken file.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// A shell run inside the container after the workspace and env are ready
    /// and before the session opens, on every (re)spawn (issue #14). Chats cut
    /// before this field existed carry none.
    #[serde(default)]
    pub startup_script: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_active_at: DateTime<Utc>,
}

/// The version tag alone, read before the fields it governs.
#[derive(Deserialize)]
pub(super) struct SchemaTag {
    pub schema: u32,
}

/// What the caller has to decide when a chat is created; the rest is ours.
pub struct NewChat {
    pub title: String,
    pub repo: String,
    pub branch: String,
    pub base_branch: String,
    pub env: BTreeMap<String, String>,
    pub startup_script: Option<String>,
}

impl Manifest {
    /// Open a brand new chat under a freshly minted id.
    #[must_use]
    pub fn open(new_chat: NewChat) -> Self {
        let now = Utc::now();
        Self {
            schema: MANIFEST_SCHEMA,
            chat_id: Ulid::generate().to_string(),
            title: new_chat.title,
            state: ChatState::Open,
            repo: new_chat.repo,
            branch: new_chat.branch,
            base_branch: new_chat.base_branch,
            checkpoint_branch: None,
            last_pushed_commit: None,
            acp_session_id: None,
            env: new_chat.env,
            startup_script: new_chat.startup_script,
            created_at: now,
            last_active_at: now,
        }
    }
}
