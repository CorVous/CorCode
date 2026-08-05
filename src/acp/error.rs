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
    #[error("the adapter said nothing to {method} for {}s", patience.as_secs())]
    Silent { method: String, patience: Duration },
    #[error("the adapter refused {method}: {complaint}")]
    Refused { method: String, complaint: String },
    #[error("the adapter answered {method} with something unreadable: {answer}")]
    Unreadable { method: String, answer: String },
}
