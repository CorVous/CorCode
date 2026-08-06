//! Putting an operational secret to the service it opens, so the operator
//! finds out it is wrong here rather than in the middle of a chat.

/// What one service made of the credential it was handed.
///
/// No variant carries anything the service said in its body: an upstream is
/// free to echo the request back, and the request is the secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verified {
    /// The service took it. GitHub names the account it authenticated as; a
    /// classic token short of the `repo` scope is flagged, and a fine-grained
    /// token, which states no scopes at all, is not.
    Accepted {
        login: Option<String>,
        without_repo_scope: bool,
    },
    /// The service turned it away with this status.
    Refused(u16),
    /// Nothing came back in time.
    Silent,
}
