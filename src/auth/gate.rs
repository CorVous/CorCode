//! The authentication policy for the single configured account (ADR-0003):
//! who may sign in, and which cookies are theirs.

use std::sync::Mutex;
use std::time::SystemTime;

use anyhow::Result;
use argon2::{Argon2, PasswordHash, PasswordVerifier as _};
use tokio::task::spawn_blocking;

use super::keystore::KeyStore;
use super::rate_limit::LoginLimiter;
use super::session::{self, Session};
use crate::config::Config;

/// What came of a login attempt.
#[derive(Debug, PartialEq, Eq)]
pub enum SignIn {
    /// The credentials matched the configured account.
    Granted,
    /// The credentials did not match.
    Refused,
    /// Too many recent failures; the attempt was not even weighed.
    RateLimited,
}

/// Holds the credentials, the signing key, and the login backoff.
pub struct Gate {
    username: String,
    password_hash: String,
    keys: KeyStore,
    logins: Mutex<LoginLimiter>,
}

impl Gate {
    /// Take the account from `config` and the signing key from its data
    /// directory, generating the key if this is the first boot.
    pub fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            username: config.username.clone(),
            password_hash: config.password_hash.clone(),
            keys: KeyStore::open(&config.data_dir)?,
            logins: Mutex::new(LoginLimiter::default()),
        })
    }

    /// Weigh submitted credentials, counting the attempt against the login
    /// backoff. The argon2 work runs on a blocking thread, off the request
    /// worker and outside the backoff lock.
    pub async fn sign_in(&self, username: &str, password: &str, now: SystemTime) -> SignIn {
        if self.logins.lock().expect(POISONED).is_locked(now) {
            return SignIn::RateLimited;
        }
        let hash = self.password_hash.clone();
        let attempt = password.to_owned();
        let password_matches = spawn_blocking(move || verify_password(&hash, &attempt))
            .await
            .expect("verifying a password cannot panic");

        let mut logins = self.logins.lock().expect(POISONED);
        if password_matches && username == self.username {
            logins.record_success();
            SignIn::Granted
        } else {
            logins.record_failure(now);
            SignIn::Refused
        }
    }

    /// Mint a cookie value for a signed-in visitor.
    #[must_use]
    pub fn issue_cookie(&self, now: SystemTime) -> String {
        session::issue(&self.keys.current(), now)
    }

    /// Recognise a cookie value minted under the key now in force.
    #[must_use]
    pub fn recognise(&self, cookie: &str, now: SystemTime) -> Option<Session> {
        session::verify(&self.keys.current(), cookie, now)
    }

    /// Rotate the signing key, logging every device out.
    pub fn rotate_key(&self) -> Result<()> {
        self.keys.rotate()
    }
}

/// The login lock is only ever held across counter arithmetic, which
/// cannot panic, so it cannot be poisoned.
const POISONED: &str = "the login lock is never poisoned";

/// Whether `password` is the one behind `hash`, in constant time.
fn verify_password(hash: &str, password: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|expected| {
        Argon2::default()
            .verify_password(password.as_bytes(), &expected)
            .is_ok()
    })
}
