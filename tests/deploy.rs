//! The deployment artefacts: what `deploy/compose.yaml` configures, and —
//! where a docker binary exists — whether Docker itself accepts the file.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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

#[test]
fn docker_accepts_the_compose_file() {
    let Some(docker) = docker_binary() else {
        eprintln!("SKIPPED docker_accepts_the_compose_file: no docker binary");
        return;
    };
    let checked = Command::new(docker)
        .args(["compose", "--file"])
        .arg(compose_path())
        .args(["config", "--quiet"])
        .output()
        .expect("docker should run");

    assert!(
        checked.status.success(),
        "docker refused the compose file: {}",
        String::from_utf8_lossy(&checked.stderr)
    );
}

fn compose_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("deploy")
        .join("compose.yaml")
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

fn docker_binary() -> Option<&'static str> {
    ["/usr/bin/docker", "/usr/local/bin/docker", "/bin/docker"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
}
