//! Docker-gated: the hardening contract of ADR-0001 as a real daemon sees it.
//! Skipped, loudly, wherever no docker socket answers.

use std::collections::BTreeMap;
use std::path::Path;

use bollard::Docker;
use cor_code::plane::{ContainerPlane, DockerPlane, PlaneSettings, container_name};
use tempfile::TempDir;

const DOCKER_SOCKET: &str = "/var/run/docker.sock";
/// Small, and never the multi-gigabyte workspace image.
const TEST_IMAGE: &str = "alpine:3.22";
const CHAT_ID: &str = "01K1DOCKERGATEDTEST00000";
const MEMORY_MB: u32 = 512;
const CPUS: u32 = 1;

#[tokio::test]
async fn a_real_spawned_container_wears_every_hardening_flag() {
    let Some(docker) = reachable_daemon() else {
        eprintln!("SKIPPED a_real_spawned_container_wears_every_hardening_flag: no docker daemon");
        return;
    };
    let plane = DockerPlane::connect(settings()).expect("the daemon should be reachable");
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
    plane
        .teardown(CHAT_ID)
        .await
        .expect("the chat should tear down");

    assert_eq!(container.name, container_name(CHAT_ID));
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
    assert_eq!(host_config.nano_cpus, Some(i64::from(CPUS) * 1_000_000_000));
    assert_eq!(host_config.network_mode, Some("corcode-agents".to_owned()));
    assert_eq!(
        host_config.binds,
        Some(vec![
            format!("{}:/workspace:rw", path_of(workspace.path())),
            format!("{}:/home/agent/.claude:rw", path_of(claude.path())),
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

fn reachable_daemon() -> Option<Docker> {
    if !Path::new(DOCKER_SOCKET).exists() && std::env::var_os("DOCKER_HOST").is_none() {
        return None;
    }
    Docker::connect_with_local_defaults().ok()
}

fn path_of(dir: &Path) -> &str {
    dir.to_str().expect("temp dirs should be spellable")
}
