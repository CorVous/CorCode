//! How a failure reaches the log.

use std::error::Error;

use anyhow::Chain;

/// A failure and every cause beneath it, as one log line.
///
/// A typed error displays its own summary and no more, so without this the
/// log would keep the one thing the summary was standing in for: what
/// actually went wrong.
pub fn with_causes(failure: &(dyn Error + 'static)) -> String {
    Chain::new(failure)
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ")
}
