//! Lines of the per-chat `events.jsonl` display record (ADR-0006).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One ACP payload as the core saw it, stamped with its arrival time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub ts: DateTime<Utc>,
    pub event: Value,
}
