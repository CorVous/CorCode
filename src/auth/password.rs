//! The one argon2 scheme the deployment's single password is kept under
//! (ADR-0003): what `hash-password` writes and what the gate weighs.

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
