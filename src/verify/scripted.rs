//! A verifier that answers from a script, so no test in this suite ever
//! reaches the network.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::secrets::Secret;

use super::{Verified, VerifyClient};

/// Answers every check the same way and remembers every credential it was
/// handed, so a test can prove which value was put to the service.
///
/// A clone is the same verifier, not another one: a test that has handed its
/// fake to the thing under test keeps a copy to read what it heard.
#[derive(Clone)]
pub struct ScriptedVerifier {
    answer: Verified,
    heard: Arc<Mutex<Vec<(Secret, String)>>>,
}

impl ScriptedVerifier {
    /// A verifier that answers `answer` to every credential it is handed.
    #[must_use]
    pub fn answering(answer: Verified) -> Self {
        Self {
            answer,
            heard: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Every credential this verifier has been handed, in order.
    #[must_use]
    pub fn heard(&self) -> Vec<(Secret, String)> {
        self.locked().clone()
    }

    fn locked(&self) -> MutexGuard<'_, Vec<(Secret, String)>> {
        self.heard.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Default for ScriptedVerifier {
    /// A verifier nothing is expected to reach: were something to, it would
    /// read as a service that did not answer.
    fn default() -> Self {
        Self::answering(Verified::Silent)
    }
}

impl VerifyClient for ScriptedVerifier {
    async fn verify(&self, secret: Secret, value: &str) -> Verified {
        self.locked().push((secret, value.to_owned()));
        self.answer.clone()
    }
}

impl fmt::Debug for ScriptedVerifier {
    /// Says how it answers and which secrets it has been asked about, never
    /// what it was handed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let asked: Vec<Secret> = self.locked().iter().map(|&(secret, _)| secret).collect();
        f.debug_struct("ScriptedVerifier")
            .field("answer", &self.answer)
            .field("heard", &asked)
            .finish()
    }
}
