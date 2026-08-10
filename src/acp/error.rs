//! Failures of the ACP conversation, each naming the call it happened to.

use std::io;
use std::time::Duration;

use thiserror::Error;

/// Something the adapter did instead of answering.
#[derive(Debug, Error)]
pub enum AcpError {
    #[error("no adapter could be started in container {container}")]
    Unreachable {
        container: String,
        source: bollard::errors::Error,
    },
    #[error("the adapter's channel broke while {doing}")]
    Broken { doing: String, source: io::Error },
    #[error("the adapter closed its channel without answering")]
    Closed,
    #[error("no adapter started in container {container} within {}s", patience.as_secs())]
    Unstarted {
        container: String,
        patience: Duration,
    },
    #[error("the adapter said nothing to {method} for {}s", patience.as_secs())]
    Silent { method: String, patience: Duration },
    #[error("the adapter refused {method}: {complaint}")]
    Refused { method: String, complaint: String },
    #[error("the adapter answered {method} with something unreadable: {answer}")]
    Unreadable { method: String, answer: String },
    #[error(
        "the adapter's stream stopped being json-rpc while {method} was outstanding, last reading: {sample}"
    )]
    Garbled { method: String, sample: String },
    #[error("the turn could not be written down")]
    Unrecorded {
        #[source]
        source: anyhow::Error,
    },
}

impl AcpError {
    /// Whether the conversation is past using. A pipe that broke, a silence
    /// nobody ended, an answer that made no sense: the adapter is not somewhere
    /// another turn can start from. A turn that failed at our end instead —
    /// nothing written down — leaves the adapter exactly where it was.
    #[must_use]
    pub const fn spent_the_connection(&self) -> bool {
        !matches!(self, Self::Unrecorded { .. })
    }

    /// Whether the adapter itself said no. A refusal — a method it has never
    /// heard of among them — is an answer, and a channel that carried one can
    /// carry the next question (ADR-0007 rule 3). Anything else leaves nothing
    /// to ask over.
    #[must_use]
    pub const fn answered(&self) -> bool {
        matches!(self, Self::Refused { .. })
    }
}
