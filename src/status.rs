//! The container picture the status line reads (ADR-0008).

use chrono::TimeDelta;

use crate::sweep::Swept;

/// A warm-pool slot: whose chat holds the container, and how long since that
/// chat last took a turn — the ordering the pool is capped by (ADR-0002).
pub struct Slot {
    pub title: String,
    pub idle: TimeDelta,
}

/// Whether the container plane answered the pass this picture was taken in.
///
/// It is the console's to say out loud: a picture that left it out would read
/// as an empty pool rather than as no answer (issue #25).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Containers {
    Known,
    /// What the daemon said instead of answering, causes and all.
    Unknown(String),
}

/// Everything the status line says, taken in one pass over the dataset.
pub struct Status {
    /// Whether the pool and parked counts below mean anything.
    pub containers: Containers,
    /// The chats holding a container, most recently active first.
    pub pool: Vec<Slot>,
    /// How many containers this deployment keeps warm (ADR-0002 rule 2).
    pub warm_pool: usize,
    /// Open chats whose container has been given up, workspace kept.
    pub parked: usize,
    /// The pinned image every chat runs (ADR-0004).
    pub image: String,
    /// What the last orphan sweep found, or nothing if none has run yet.
    pub sweep: Option<Swept>,
}
