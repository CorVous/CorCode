//! Integration tests for CLI functionality.

use assert_cmd::Command;
use predicates::prelude::*;
use predicates::str::is_match;

const SEMVER: &str = r"\d+\.\d+\.\d+";

fn cli() -> Command {
    let mut cmd = Command::cargo_bin(env!("CARGO_PKG_NAME")).expect("binary should build");
    // Keep tests hermetic: the environment's RUST_LOG must not leak in
    cmd.env_remove("RUST_LOG");
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
fn cli_no_subcommand_shows_help() {
    cli()
        .assert()
        .success()
        .stdout(predicate::str::contains("Usage:"));
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

#[test]
fn example_subcommand() {
    cli()
        .args(["example", "World"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Hello, World!"));
}

#[test]
fn example_subcommand_with_greeting() {
    cli()
        .args(["example", "World", "--greeting", "Hi"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Hi, World!"));
}
