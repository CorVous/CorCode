//! `hash-password` subcommand — turn a password into the hash
//! `CORCODE_PASSWORD_HASH` wants (ADR-0003).

use std::io::{self, IsTerminal as _, Write};

use anyhow::{Context as _, Result, bail};

use crate::auth::password::hash_password;

/// Hash the password on stdin, where no shell history and no process listing
/// can hold it.
pub fn run() -> Result<()> {
    let typed = read_password().context("the password could not be read from stdin")?;
    print_hash(&typed, &mut io::stdout())
}

/// Typed at a terminal the password does not echo; piped in, it is one line
/// like any other input, which is how a script or a test feeds it.
fn read_password() -> io::Result<String> {
    if io::stdin().is_terminal() {
        rpassword::read_password()
    } else {
        rpassword::read_password_from_bufread(&mut io::stdin().lock())
    }
}

fn print_hash(password: &str, output: &mut impl Write) -> Result<()> {
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

    fn hashed(password: &str) -> String {
        let mut written = Vec::new();
        print_hash(password, &mut written).expect("a password should hash");
        String::from_utf8(written).expect("the hash should be text")
    }

    #[test]
    fn the_password_is_printed_as_a_hash_the_gate_accepts() {
        let printed = hashed(PASSWORD);

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
    fn stdin_with_no_password_on_it_is_refused_rather_than_hashed() {
        let failure = print_hash("", &mut Vec::new()).expect_err("nothing should not hash");

        assert!(
            format!("{failure:#}").contains("password"),
            "the refusal does not say what was missing: {failure:#}"
        );
    }
}
