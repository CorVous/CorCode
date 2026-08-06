//! Docker-gated: the core image the compose file builds, booted for real
//! against a fresh dataset directory. Skipped, loudly, wherever no docker
//! binary answers.

use std::env;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use cor_code::auth::password::verify_password;
use tempfile::TempDir;
use tokio::time::sleep;

const IMAGE: &str = "corcode-core:deploy-test";
const CONTAINER: &str = "corcode-deploy-test";
const PASSWORD: &str = "correct horse battery staple";
const BOOT_POLLS: u32 = 60;
const POLL_INTERVAL: Duration = Duration::from_millis(500);

#[test]
fn docker_accepts_the_compose_file() {
    let Some(docker) = docker_or_skip("docker_accepts_the_compose_file") else {
        return;
    };

    let checked = run(
        &docker,
        &["compose", "--file", &compose_path(), "config", "--quiet"],
    );

    assert!(
        checked.status.success(),
        "docker refused the compose file: {}",
        String::from_utf8_lossy(&checked.stderr)
    );
}

#[tokio::test]
async fn the_core_image_boots_against_a_fresh_dataset_and_answers() {
    let Some(docker) = docker_or_skip("the_core_image_boots_against_a_fresh_dataset_and_answers")
    else {
        return;
    };
    build_image(&docker);
    let dataset = TempDir::new().expect("a dataset dir should be created");
    let _container = Container::start(&docker, dataset.path());

    let port = published_port(&docker);
    let health = await_health(port).await;

    assert_eq!(health, "ok", "the booted core does not answer /health");
    assert!(
        dataset.path().join("chats").is_dir(),
        "the core did not lay down the dataset it was pointed at"
    );
}

#[test]
fn the_core_image_hashes_a_password_off_a_piped_stdin() {
    let Some(docker) = docker_or_skip("the_core_image_hashes_a_password_off_a_piped_stdin") else {
        return;
    };
    build_image(&docker);

    let printed = piped(
        &docker,
        &["run", "--rm", "--interactive", IMAGE, "hash-password"],
        PASSWORD,
    );

    assert!(
        verify_password(printed.trim_end(), PASSWORD),
        "the image printed a hash the gate refuses: {printed}"
    );
}

/// The container under test, torn down however the test ends.
struct Container {
    docker: String,
}

impl Container {
    fn start(docker: &str, dataset: &Path) -> Self {
        let container = Self {
            docker: docker.to_owned(),
        };
        container.remove();
        let bind = format!("{}:/data", dataset.display());
        let started = run(
            docker,
            &[
                "run",
                "--detach",
                "--name",
                CONTAINER,
                "--publish",
                "127.0.0.1::8080",
                "--volume",
                &bind,
                "--env",
                "CORCODE_DATA_DIR=/data",
                "--env",
                "CORCODE_USERNAME=cassidy",
                "--env",
                &format!("CORCODE_PASSWORD_HASH={}", hashed(docker)),
                "--env",
                "CORCODE_WORKSPACE_IMAGE=ghcr.io/corvous/corcode-workspace:2026-08-05",
                "--env",
                "CORCODE_REPOS=CorVous/CorCode",
                IMAGE,
            ],
        );
        assert!(
            started.status.success(),
            "the core image would not start: {}",
            String::from_utf8_lossy(&started.stderr)
        );
        container
    }

    fn remove(&self) {
        run(&self.docker, &["rm", "--force", CONTAINER]);
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        self.remove();
    }
}

fn build_image(docker: &str) {
    let built = run(
        docker,
        &[
            "build",
            "--tag",
            IMAGE,
            "--file",
            &dockerfile_path(),
            &root(),
        ],
    );
    assert!(
        built.status.success(),
        "the core image would not build: {}",
        String::from_utf8_lossy(&built.stderr)
    );
}

fn hashed(docker: &str) -> String {
    piped(
        docker,
        &["run", "--rm", "--interactive", IMAGE, "hash-password"],
        PASSWORD,
    )
    .trim_end()
    .to_owned()
}

fn published_port(docker: &str) -> u16 {
    let published = run(docker, &["port", CONTAINER, "8080/tcp"]);
    let mapping = String::from_utf8_lossy(&published.stdout);
    mapping
        .lines()
        .next()
        .and_then(|line| line.rsplit(':').next())
        .and_then(|port| port.trim().parse().ok())
        .unwrap_or_else(|| panic!("docker published no port: {mapping}"))
}

async fn await_health(port: u16) -> String {
    let url = format!("http://127.0.0.1:{port}/health");
    for _ in 0..BOOT_POLLS {
        if let Ok(answer) = reqwest::get(&url).await
            && answer.status().is_success()
        {
            return answer.text().await.expect("health should read as text");
        }
        sleep(POLL_INTERVAL).await;
    }
    panic!("the core never answered {url}");
}

fn run(docker: &str, args: &[&str]) -> std::process::Output {
    Command::new(docker)
        .args(args)
        .output()
        .expect("docker should run")
}

fn piped(docker: &str, args: &[&str], stdin: &str) -> String {
    use std::io::Write as _;

    let mut child = Command::new(docker)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("docker should run");
    child
        .stdin
        .take()
        .expect("stdin should be piped")
        .write_all(stdin.as_bytes())
        .expect("the password should be written");
    let finished = child.wait_with_output().expect("docker should finish");
    assert!(
        finished.status.success(),
        "docker failed: {}",
        String::from_utf8_lossy(&finished.stderr)
    );
    String::from_utf8(finished.stdout).expect("docker should answer in text")
}

fn root() -> String {
    env!("CARGO_MANIFEST_DIR").to_owned()
}

fn dockerfile_path() -> String {
    Path::new(&root()).join("Dockerfile").display().to_string()
}

fn compose_path() -> String {
    Path::new(&root())
        .join("deploy")
        .join("compose.yaml")
        .display()
        .to_string()
}

/// The docker binary, or nothing and a line saying why the test did not run.
fn docker_or_skip(test: &str) -> Option<String> {
    let found = docker_binary();
    if found.is_none() {
        eprintln!("SKIPPED {test}: no docker binary on PATH");
    }
    found
}

fn docker_binary() -> Option<String> {
    env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths)
            .map(|dir| dir.join("docker"))
            .find(|candidate| candidate.is_file())
            .map(|found| found.display().to_string())
    })
}
