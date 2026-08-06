//! The one argon2 scheme the deployment's single password is kept under
//! (ADR-0003): what `hash-password` writes and what the gate weighs.

use anyhow::{Context as _, Result, anyhow};
use argon2::password_hash::{PasswordHasher as _, SaltString};
use argon2::{Argon2, PasswordHash, PasswordVerifier as _};

/// Bytes of salt per password, the length argon2's own generator picks.
const SALT_BYTES: usize = 16;

/// Put `password` under a fresh salt, in the form `CORCODE_PASSWORD_HASH`
/// takes.
pub fn hash_password(password: &str) -> Result<String> {
    let mut bytes = [0u8; SALT_BYTES];
    getrandom::fill(&mut bytes).context("the operating system refused random bytes")?;
    let salt =
        SaltString::encode_b64(&bytes).map_err(|failure| anyhow!("unusable salt: {failure}"))?;
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|failure| anyhow!("the password could not be hashed: {failure}"))?
        .to_string())
}

/// Whether `password` is the one behind `hash`, in constant time.
#[must_use]
pub fn verify_password(hash: &str, password: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|expected| {
        Argon2::default()
            .verify_password(password.as_bytes(), &expected)
            .is_ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWORD: &str = "correct horse battery staple";

    #[test]
    fn a_freshly_hashed_password_is_one_the_gate_lets_through() {
        let hash = hash_password(PASSWORD).expect("hashing should succeed");

        assert!(verify_password(&hash, PASSWORD));
        assert!(!verify_password(&hash, "correct horse battery stapl"));
    }

    #[test]
    fn the_same_password_hashes_differently_every_time() {
        let once = hash_password(PASSWORD).expect("hashing should succeed");
        let again = hash_password(PASSWORD).expect("hashing should succeed");

        assert_ne!(once, again, "the hash carries no salt");
    }

    #[test]
    fn anything_that_is_not_a_hash_lets_nobody_through() {
        assert!(!verify_password("", PASSWORD));
        assert!(!verify_password("not a phc string", PASSWORD));
    }
}
