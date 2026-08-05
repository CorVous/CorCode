//! Failures of the chat store, each naming the file it happened to.

use std::io;
use std::path::PathBuf;

use thiserror::Error;

use super::manifest::MANIFEST_SCHEMA;

/// Something the store refuses to guess about. Nothing here is repairable by
/// the core (ADR-0007 rule 5).
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("{} could not be read", path.display())]
    Read { path: PathBuf, source: io::Error },
    #[error("{} could not be written", path.display())]
    Write { path: PathBuf, source: io::Error },
    #[error("{} is not a valid chat manifest", path.display())]
    Manifest {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("{} has manifest schema {schema}, expected {MANIFEST_SCHEMA}", path.display())]
    ManifestSchema { path: PathBuf, schema: u32 },
    #[error("{} line {line} is not a valid event", path.display())]
    Event {
        path: PathBuf,
        line: usize,
        source: serde_json::Error,
    },
}
