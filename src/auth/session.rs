//! Stateless session cookies: an expiry timestamp carried by the client,
//! authenticated with an HMAC over the server's signing key (ADR-0003).

use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64;
use hmac::{Hmac, KeyInit as _, Mac as _};
use sha2::Sha256;

/// How long a freshly issued session stays valid.
pub const LIFETIME: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// How much of a session's life may pass before the next authenticated
/// request re-issues it, sliding the window forward.
pub const REFRESH_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// The secret that authenticates session cookies. Rotating it invalidates
/// every cookie ever issued — "log out everywhere".
#[derive(Clone)]
pub struct SigningKey([u8; 32]);

/// A cookie that carried a valid signature and had not expired.
pub struct Session {
    expires_at: SystemTime,
}

impl SigningKey {
    /// Draw a fresh key from the operating system's entropy source.
    pub fn random() -> Result<Self> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).context("the operating system refused random bytes")?;
        Ok(Self(bytes))
    }

    /// Adopt key material read from storage.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw key material, for persisting it.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Mint a cookie value valid for [`LIFETIME`] from `now`.
#[must_use]
pub fn issue(key: &SigningKey, now: SystemTime) -> String {
    let payload = BASE64.encode(unix_seconds(now + LIFETIME).to_string());
    let signature = BASE64.encode(mac(key, &payload).finalize().into_bytes());
    format!("{payload}.{signature}")
}

/// Authenticate a cookie value, yielding the session it stands for.
#[must_use]
pub fn verify(key: &SigningKey, cookie: &str, now: SystemTime) -> Option<Session> {
    let (payload, signature) = cookie.split_once('.')?;
    let signature = BASE64.decode(signature).ok()?;
    mac(key, payload).verify_slice(&signature).ok()?;
    let expiry = String::from_utf8(BASE64.decode(payload).ok()?).ok()?;
    let expires_at = SystemTime::UNIX_EPOCH + Duration::from_secs(expiry.parse().ok()?);
    (expires_at > now).then_some(Session { expires_at })
}

impl Session {
    /// Whether this session has aged past [`REFRESH_AFTER`] and should be
    /// re-issued to slide its window forward.
    #[must_use]
    pub fn needs_refresh(&self, now: SystemTime) -> bool {
        self.expires_at
            .duration_since(now)
            .is_ok_and(|remaining| remaining + REFRESH_AFTER < LIFETIME)
    }
}

/// The HMAC-SHA256 state authenticating a cookie payload.
fn mac(key: &SigningKey, payload: &str) -> Hmac<Sha256> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("HMAC accepts keys of any length");
    mac.update(payload.as_bytes());
    mac
}

/// Seconds since the Unix epoch; times before it cannot arise here.
fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .expect("session times are never before the Unix epoch")
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes([seed; 32])
    }

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn a_freshly_issued_cookie_verifies() {
        let cookie = issue(&key(1), at(1_000));

        assert!(verify(&key(1), &cookie, at(1_000)).is_some());
    }

    #[test]
    fn another_key_rejects_the_cookie() {
        let cookie = issue(&key(1), at(1_000));

        assert!(verify(&key(2), &cookie, at(1_000)).is_none());
    }

    #[test]
    fn a_tampered_payload_is_rejected() {
        let cookie = issue(&key(1), at(1_000));
        let (payload, signature) = cookie
            .split_once('.')
            .expect("cookie should have two parts");
        let forged = format!("{payload}x.{signature}");

        assert!(verify(&key(1), &forged, at(1_000)).is_none());
    }

    #[test]
    fn a_cookie_past_its_expiry_is_rejected() {
        let cookie = issue(&key(1), at(1_000));

        let just_after = at(1_000) + LIFETIME + Duration::from_secs(1);
        assert!(verify(&key(1), &cookie, just_after).is_none());
    }

    #[test]
    fn a_malformed_cookie_is_rejected() {
        for malformed in ["", "no-separator", "!!!.???", "MTAwMA"] {
            assert!(
                verify(&key(1), malformed, at(1_000)).is_none(),
                "{malformed:?} should not verify"
            );
        }
    }

    #[test]
    fn a_young_session_is_not_refreshed() {
        let cookie = issue(&key(1), at(1_000));

        let session = verify(&key(1), &cookie, at(1_000)).expect("fresh cookie should verify");

        assert!(!session.needs_refresh(at(1_000)));
    }

    #[test]
    fn a_session_past_the_refresh_age_is_refreshed() {
        let cookie = issue(&key(1), at(1_000));

        let later = at(1_000) + REFRESH_AFTER + Duration::from_secs(1);
        let session = verify(&key(1), &cookie, later).expect("aged cookie should still verify");

        assert!(session.needs_refresh(later));
    }
}
