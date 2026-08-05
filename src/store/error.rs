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
    #[error("{} claims chat id {chat_id}, which is not its directory", path.display())]
    ChatIdMismatch { path: PathBuf, chat_id: String },
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

    /// Whether the failure is simply that nothing is there — the one store
    /// failure a caller may answer for rather than surface.
    #[must_use]
    pub fn is_missing(&self) -> bool {
        matches!(self, Self::Read { source, .. } if source.kind() == io::ErrorKind::NotFound)
    }

    /// Blame `path` for whatever I/O error comes back from writing it.
    pub(super) fn writing(path: &Path) -> impl FnOnce(io::Error) -> Self + use<> {
        let path = path.to_owned();
        move |source| Self::Write { path, source }
    }
}
