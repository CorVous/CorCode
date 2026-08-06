//! Docker-gated: the hardening contract of ADR-0001 as a real daemon sees it.
//! Skipped, loudly, wherever no docker socket answers.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use bollard::Docker;
use bollard::models::ContainerInspectResponse;
use cor_code::plane::{ContainerPlane, DockerPlane, PlaneError, PlaneSettings, container_name};
use tempfile::TempDir;

const DOCKER_SOCKET: &str = "/var/run/docker.sock";
/// Small, and never the multi-gigabyte workspace image.
const TEST_IMAGE: &str = "alpine:3.22";
const CHAT_ID: &str = "01K1DOCKERGATEDTEST00000";
const MEMORY_MB: u32 = 512;
const CPUS: u32 = 1;
const EXIT_POLLS: u32 = 50;
const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[tokio::test]
async fn a_real_spawned_container_wears_every_hardening_flag() {
    let Some(docker) = reachable_daemon() else {
        eprintln!("SKIPPED a_real_spawned_container_wears_every_hardening_flag: no docker daemon");
        return;
    };
    let plane = DockerPlane::connect(settings()).expect("the daemon should be reachable");
    clear_any_leftover(&plane).await;
    let workspace = TempDir::new().expect("workspace dir should be created");
    let claude = TempDir::new().expect("claude dir should be created");
    let env = BTreeMap::from([("CORCODE_TEST".to_owned(), "present".to_owned())]);

    let container = plane
        .spawn(CHAT_ID, workspace.path(), claude.path(), &env)
        .await
        .expect("the chat should spawn");
    let inspected = docker
        .inspect_container(&container.name, None)
        .await
        .expect("the container should be inspectable");
    let network = docker
        .inspect_network("corcode-agents", None)
        .await
        .expect("the agent network should exist");
    wait_until_exited(&docker, &container.name).await;
    let live_while_exited = plane.list_live().await.expect("liveness should answer");
    let exited_container_still_exists = docker
        .inspect_container(&container.name, None)
        .await
        .is_ok();
    plane
        .teardown(CHAT_ID)
        .await
        .expect("the chat should tear down");

    assert_eq!(container.name, container_name(CHAT_ID));
    assert!(
        exited_container_still_exists,
        "the exited container should still be on the daemon to be excluded from"
    );
    assert!(
        !live_while_exited.contains(CHAT_ID),
        "an exited container is a parked chat, not a live one"
    );
    assert!(
        !plane
            .list_live()
            .await
            .expect("liveness should answer")
            .contains(CHAT_ID),
        "teardown should leave no live container behind"
    );
    assert!(
        docker
            .inspect_container(&container.name, None)
            .await
            .is_err(),
        "teardown should remove the container"
    );
    assert_eq!(network.internal, Some(true));
    assert_hardened(inspected, workspace.path(), claude.path());
}

/// Every flag ADR-0001 makes mandatory, as the daemon recorded it.
fn assert_hardened(inspected: ContainerInspectResponse, workspace: &Path, claude: &Path) {
    let host_config = inspected
        .host_config
        .expect("a container is hardened by its host config");
    assert_eq!(host_config.cap_drop, Some(vec!["ALL".to_owned()]));
    assert!(
        host_config.cap_add.unwrap_or_default().is_empty(),
        "no capability may come back"
    );
    assert_eq!(
        host_config.security_opt,
        Some(vec!["no-new-privileges:true".to_owned()])
    );
    assert_ne!(host_config.privileged, Some(true));
    assert_eq!(host_config.readonly_rootfs, Some(true));
    assert_eq!(
        host_config
            .tmpfs
            .expect("the read-only rootfs needs scratch")
            .get("/tmp")
            .map(String::as_str),
        Some("rw,nosuid,nodev,noexec,size=256m")
    );
    assert_eq!(host_config.memory, Some(i64::from(MEMORY_MB) * 1024 * 1024));
    assert_eq!(
        host_config.memory_swap,
        Some(i64::from(MEMORY_MB) * 1024 * 1024),
        "swap left open would double the ceiling"
    );
    assert_eq!(host_config.nano_cpus, Some(i64::from(CPUS) * 1_000_000_000));
    assert_eq!(host_config.network_mode, Some("corcode-agents".to_owned()));
    assert_eq!(
        host_config.binds,
        Some(vec![
            format!("{}:/workspace:rw", path_of(workspace)),
            format!("{}:/home/agent/.claude:rw", path_of(claude)),
        ]),
        "the workspace and the agent memory are the only mounts"
    );

    let config = inspected.config.expect("a container has a config");
    assert_eq!(config.user, Some("1000:1000".to_owned()));
    assert!(
        config
            .env
            .unwrap_or_default()
            .contains(&"CORCODE_TEST=present".to_owned()),
        "the caller's env should reach the container"
    );
    assert_eq!(
        config
            .labels
            .expect("liveness reads a label")
            .get("corcode.chat-id")
            .map(String::as_str),
        Some(CHAT_ID)
    );
}

fn settings() -> PlaneSettings {
    PlaneSettings {
        image: TEST_IMAGE.to_owned(),
        memory_mb: MEMORY_MB,
        cpus: CPUS,
        registry: None,
    }
}

/// A run that died mid-test leaves the fixed-name container behind; it would
/// meet every later run with `AlreadyLive`.
async fn clear_any_leftover(plane: &DockerPlane) {
    match plane.teardown(CHAT_ID).await {
        Ok(()) | Err(PlaneError::NotLive { .. }) => {}
        Err(error) => panic!("a leftover container should be removable: {error}"),
    }
}

/// Alpine's shell exits at once without a tty, which is the state liveness must
/// not count.
async fn wait_until_exited(docker: &Docker, name: &str) {
    for _ in 0..EXIT_POLLS {
        let state = docker
            .inspect_container(name, None)
            .await
            .expect("the container should be inspectable")
            .state
            .expect("a container has a state");
        if state.running != Some(true) {
            return;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    panic!("{name} should have exited within {EXIT_POLLS} polls");
}

/// The daemon behind the socket bollard will open. `DOCKER_HOST` is no help:
/// the local-defaults connector never reads it, so a `tcp://` daemon is a
/// daemon this test cannot reach.
fn reachable_daemon() -> Option<Docker> {
    if !Path::new(DOCKER_SOCKET).exists() {
        return None;
    }
    Docker::connect_with_local_defaults().ok()
}

fn path_of(dir: &Path) -> &str {
    dir.to_str().expect("temp dirs should be spellable")
}
