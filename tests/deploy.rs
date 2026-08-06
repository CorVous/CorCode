//! What `deploy/compose.yaml` configures, read without a daemon. Whether
//! Docker accepts the file, and whether the image it builds boots, are in
//! `deploy_docker.rs`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use cor_code::config::Config;

/// Values that would be a leaked credential rather than a placeholder.
const SECRET_SHAPES: [&str; 4] = ["$argon2", "ghp_", "github_pat_", "sk-ant-"];

#[test]
fn the_compose_file_boots_the_core_with_every_setting_it_requires() {
    let config = Config::from_vars(compose_environment()).expect("compose should configure a core");

    assert_eq!(
        config.data_dir,
        Path::new("/data"),
        "the core reads the dataset through its own mount"
    );
    assert!(
        compose().contains(&format!(
            "{}:/data",
            config
                .host_data_dir
                .display()
                .to_string()
                .trim_end_matches('/')
        )),
        "the dataset bound into the core is not the one the daemon is told about: {}",
        config.host_data_dir.display()
    );
}

/// `build: .` would resolve against `deploy/`, which holds no Dockerfile.
#[test]
fn the_compose_file_builds_from_a_context_that_holds_the_dockerfile() {
    let context = compose_build_context();

    assert!(
        context.join("Dockerfile").is_file(),
        "the build context holds no Dockerfile: {}",
        context.display()
    );
}

/// A build context carrying the target directory, the git history and the
/// toolchain file would ship gigabytes and download a second Rust.
#[test]
fn the_build_context_leaves_out_what_the_image_must_not_carry() {
    let ignored = fs::read_to_string(repo_root().join(".dockerignore"))
        .expect("the ignore file should be readable");

    for unwanted in ["target", ".git", ".claude", "rust-toolchain.toml"] {
        assert!(
            ignored.lines().any(|line| line.trim() == unwanted),
            "the build context still carries {unwanted}: {ignored}"
        );
    }
}

#[test]
fn the_compose_file_hands_the_core_the_daemon_it_spawns_siblings_through() {
    assert!(
        compose().contains("/var/run/docker.sock:/var/run/docker.sock"),
        "the core cannot reach the daemon it spawns agent containers through"
    );
}

#[test]
fn the_compose_file_carries_placeholders_rather_than_credentials() {
    for shape in SECRET_SHAPES {
        assert!(
            !compose().contains(shape),
            "the committed compose file holds something shaped like a secret: {shape}"
        );
    }
}

fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn compose_path() -> PathBuf {
    repo_root().join("deploy").join("compose.yaml")
}

/// Where the service's `build:` context points, as Docker would resolve it:
/// against the directory the compose file sits in.
fn compose_build_context() -> PathBuf {
    let compose = compose();
    let context = compose
        .lines()
        .filter_map(|line| line.trim().strip_prefix("context:"))
        .next()
        .unwrap_or_else(|| panic!("the compose file names no build context: {compose}"));
    compose_path()
        .parent()
        .expect("the compose file sits in a directory")
        .join(unquoted(context.trim()))
}

fn compose() -> String {
    fs::read_to_string(compose_path()).expect("the compose file should be readable")
}

/// The `NAME: value` settings under the service's `environment:` block, as
/// the container would receive them.
fn compose_environment() -> Vec<(String, String)> {
    let compose = compose();
    let mut settings = HashMap::new();
    let mut block_indent = None;
    for line in compose.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if trimmed == "environment:" {
            block_indent = Some(indent);
            continue;
        }
        let Some(opened_at) = block_indent else {
            continue;
        };
        if indent <= opened_at {
            block_indent = None;
            continue;
        }
        let (name, value) = trimmed.split_once(':').unwrap_or_else(|| {
            panic!("the environment block holds a setting-less line: {trimmed}")
        });
        settings.insert(name.to_owned(), unquoted(value.trim()).to_owned());
    }
    assert!(
        !settings.is_empty(),
        "no environment block was found in the compose file"
    );
    settings.into_iter().collect()
}

fn unquoted(value: &str) -> &str {
    value.trim_matches('"')
}
