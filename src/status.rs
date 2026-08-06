//! The container picture the status line reads (ADR-0008).

use chrono::TimeDelta;

use crate::sweep::Sweep;

/// A warm-pool slot: whose chat holds the container, and how long since that
/// chat last took a turn — the ordering the pool is capped by (ADR-0002).
pub struct Slot {
    pub title: String,
    pub idle: TimeDelta,
}

/// Everything the status line says, taken in one pass over the dataset.
pub struct Status {
    /// The chats holding a container, most recently active first.
    pub pool: Vec<Slot>,
    /// How many containers this deployment keeps warm (ADR-0002 rule 2).
    pub warm_pool: usize,
    /// Open chats whose container has been given up, workspace kept.
    pub parked: usize,
    /// The pinned image every chat runs (ADR-0004).
    pub image: String,
    /// What the last orphan sweep found, or nothing if none has run yet.
    pub sweep: Option<Sweep>,
}
