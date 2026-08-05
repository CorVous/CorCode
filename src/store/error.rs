//! Failures of the chat store, each naming the file it happened to.

use std::io;
use std::path::{Path, PathBuf};

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

impl StoreError {
    /// Blame `path` for whatever I/O error comes back from reading it.
    pub(super) fn reading(path: &Path) -> impl FnOnce(io::Error) -> Self + use<> {
        let path = path.to_owned();
        move |source| Self::Read { path, source }
    }

    /// Blame `path` for whatever I/O error comes back from writing it.
    pub(super) fn writing(path: &Path) -> impl FnOnce(io::Error) -> Self + use<> {
        let path = path.to_owned();
        move |source| Self::Write { path, source }
    }
}
