//! `hash-password` subcommand — turn a password into the hash
//! `CORCODE_PASSWORD_HASH` wants (ADR-0003).

#[cfg(test)]
mod tests {
    use crate::auth::password::verify_password;

    use super::*;

    const PASSWORD: &str = "correct horse battery staple";

    fn hashed(input: &str) -> String {
        let mut written = Vec::new();
        hash_stdin(input.as_bytes(), &mut written).expect("a password should hash");
        String::from_utf8(written).expect("the hash should be text")
    }

    #[test]
    fn the_password_read_from_stdin_is_hashed_for_the_gate() {
        let printed = hashed(&format!("{PASSWORD}\n"));

        assert!(
            verify_password(printed.trim_end(), PASSWORD),
            "the gate would refuse this hash: {printed}"
        );
        assert!(
            !printed.contains(PASSWORD),
            "the password itself was printed: {printed}"
        );
    }

    #[test]
    fn a_password_keeps_every_character_but_the_line_it_was_typed_on() {
        let printed = hashed("  spaced  out  \r\n");

        assert!(
            verify_password(printed.trim_end(), "  spaced  out  "),
            "the password was trimmed as well as unwrapped: {printed}"
        );
    }

    #[test]
    fn stdin_with_no_password_on_it_is_refused_rather_than_hashed() {
        let failure = hash_stdin(&b""[..], &mut Vec::new()).expect_err("nothing should not hash");

        assert!(
            format!("{failure:#}").contains("password"),
            "the refusal does not say what was missing: {failure:#}"
        );
    }
}
