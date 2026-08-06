//! Integration tests for CLI functionality.

use assert_cmd::Command;
use predicates::prelude::*;
use predicates::str::is_match;

const SEMVER: &str = r"\d+\.\d+\.\d+";

fn cli() -> Command {
    let mut cmd = Command::cargo_bin(env!("CARGO_PKG_NAME")).expect("binary should build");
    // Keep tests hermetic: the environment's RUST_LOG must not leak in
    cmd.env_remove("RUST_LOG");
    for key in [
        "CORCODE_DATA_DIR",
        "CORCODE_BIND_ADDR",
        "CORCODE_USERNAME",
        "CORCODE_PASSWORD_HASH",
        "CORCODE_WORKSPACE_IMAGE",
    ] {
        cmd.env_remove(key);
    }
    cmd
}

#[test]
fn cli_help() {
    cli()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Usage:"));
}

#[test]
fn cli_version_flag() {
    cli()
        .arg("--version")
        .assert()
        .success()
        .stdout(is_match(SEMVER).expect("valid regex"));
}

#[test]
fn cli_version_subcommand() {
    cli()
        .arg("version")
        .assert()
        .success()
        .stdout(is_match(SEMVER).expect("valid regex"));
}

#[test]
fn cli_hash_password_subcommand_reads_the_password_off_stdin() {
    const PASSWORD: &str = "correct horse battery staple";

    let printed = cli()
        .arg("hash-password")
        .write_stdin(format!("{PASSWORD}\n"))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let printed = String::from_utf8(printed).expect("the hash should be text");
    assert!(
        cor_code::auth::password::verify_password(printed.trim_end(), PASSWORD),
        "the gate would refuse the hash the CLI printed: {printed}"
    );
}

#[test]
fn cli_verbose_flag_enables_info() {
    cli()
        .args(["-v", "version"])
        .assert()
        .success()
        .stderr(predicate::str::contains("INFO:"));
}

#[test]
fn cli_very_verbose_flag_enables_debug() {
    cli()
        .args(["-vv", "version"])
        .assert()
        .success()
        .stderr(predicate::str::contains("DEBUG:"));
}

#[test]
fn cli_default_verbosity_is_quiet() {
    cli()
        .arg("version")
        .assert()
        .success()
        .stderr(predicate::str::contains("INFO:").not());
}

#[test]
fn cli_rust_log_overrides_verbosity() {
    let mut cmd = Command::cargo_bin(env!("CARGO_PKG_NAME")).expect("binary should build");
    cmd.env("RUST_LOG", "debug")
        .arg("version")
        .assert()
        .success()
        .stderr(predicate::str::contains("DEBUG:"));
}

#[test]
fn cli_no_subcommand_serving_without_config_fails() {
    cli()
        .assert()
        .failure()
        .stderr(predicate::str::contains("CORCODE_DATA_DIR"));
}

#[test]
fn cli_invalid_subcommand() {
    cli()
        .arg("invalid")
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid"));
}

#[test]
fn version_subcommand_help() {
    cli()
        .args(["version", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("version"));
}
