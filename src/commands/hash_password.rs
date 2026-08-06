//! `hash-password` subcommand — turn a password into the hash
//! `CORCODE_PASSWORD_HASH` wants (ADR-0003).

use std::io::{self, BufRead, Write};

use anyhow::{Context as _, Result, bail};

use crate::auth::password::hash_password;

/// Hash the password on stdin, where no shell history and no process listing
/// can hold it.
pub fn run() -> Result<()> {
    hash_stdin(io::stdin().lock(), &mut io::stdout())
}

fn hash_stdin(mut input: impl BufRead, output: &mut impl Write) -> Result<()> {
    let mut typed = String::new();
    input
        .read_line(&mut typed)
        .context("the password could not be read from stdin")?;
    let password = typed.trim_end_matches(['\n', '\r']);
    if password.is_empty() {
        bail!("no password arrived on stdin");
    }
    writeln!(output, "{}", hash_password(password)?)
        .context("the hash could not be written to stdout")
}

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
