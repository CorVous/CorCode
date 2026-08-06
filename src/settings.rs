//! The operational secrets as the console sets them: what one action on one
//! secret came to, and nothing of what the secret is.

use crate::verify::Verified;

/// What was just done to one secret, as its panel reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing: the panel as it is drawn with the page around it.
    Untouched,
    /// A value was given and is in force from the next read on.
    Saved,
    /// A blank submission. Unsetting a secret is [`Outcome::Cleared`]'s to do,
    /// so a save with nothing in it changes nothing.
    NothingGiven,
    /// The value on disk was taken away, leaving the environment's in force.
    Cleared,
    /// Nothing is set, so there was nothing to put to the service.
    NothingToVerify,
    /// The service was asked, and this is what it made of the credential.
    Verified(Verified),
}
