//! Docker-gated: the hardening contract of ADR-0001 as a real daemon sees it.
//! Skipped, loudly, wherever no docker socket answers.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use bollard::Docker;
use bollard::errors::Error as DockerError;
use bollard::exec::StartExecResults;
use bollard::models::{
    ContainerCreateBody, ContainerInspectResponse, ExecConfig, HostConfig, NetworkCreateRequest,
};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, RemoveContainerOptionsBuilder,
    StopContainerOptionsBuilder, WaitContainerOptionsBuilder,
};
use cor_code::plane::{ContainerPlane, DockerPlane, PlaneError, PlaneSettings, container_name};
use cor_code::store::{ChatStore, NewChat, Owner, hand_tree_to};
use futures_util::StreamExt as _;
use tempfile::TempDir;
use tokio::sync::{Mutex, MutexGuard};

const DOCKER_SOCKET: &str = "/var/run/docker.sock";
/// The one network every chat's container joins, and so the one piece of
/// daemon state these tests all reach for.
const AGENT_NETWORK: &str = "corcode-agents";
/// Small, and never the multi-gigabyte workspace image. Its default command is
/// a shell that exits at once without a tty — exactly what the plane's
/// container must not be left to.
const TEST_IMAGE: &str = "alpine:3.22";
const CHAT_ID: &str = "01K1DOCKERGATEDTEST00000";
const KEEP_ALIVE_CHAT_ID: &str = "01K1DOCKERKEEPALIVE00000";
const MIGRATION_CHAT_ID: &str = "01K1DOCKERMIGRATION00000";
const IN_USE_CHAT_ID: &str = "01K1DOCKERNETWORKINUSE00";
const OWNERSHIP_CHAT_ID: &str = "01K1DOCKEROWNERSHIP00000";
/// A shell making the two writes an agent cannot get through its first turn
/// without: a file in the workspace it commits from, and the session state the
/// adapter keeps beside it.
const AGENTS_OWN_WORK: [&str; 3] = [
    "sh",
    "-c",
    "touch /workspace/committed /home/agent/.claude/session-env",
];
/// The only user that can give a tree away, and so the only one this suite can
/// prove a handover to the agent under.
const ROOT: u32 = 0;
/// A container the plane did not spawn and must not throw away.
const INTERLOPER: &str = "corcode-test-interloper";
const MEMORY_MB: u32 = 512;
const CPUS: u32 = 1;
/// Long enough for an image command to have run out: the adapter reading EOF
/// off a closed stdin took about a second to bring its container down.
const SETTLE: Duration = Duration::from_secs(3);
/// The parked keep-alive discards SIGTERM, so every second of grace is a
/// second of test; nothing in a test container has anything to finish.
const TEST_STOP_GRACE_SECONDS: i32 = 1;
/// The state the daemon has finished committing an exit into.
const NOT_RUNNING: &str = "not-running";
/// How the daemon says it has never heard of a container or a network.
const NOT_FOUND: u16 = 404;

/// The daemon, taken one test at a time. The migration case replaces the agent
/// network wholesale, which the daemon refuses while another test's container
/// is attached and which would strand that container if it did not — so the
/// tests that share the network queue rather than race. A tokio mutex needs no
/// new dependency and, unlike the std one, survives a failing test unpoisoned.
static DAEMON: Mutex<()> = Mutex::const_new(());

async fn the_daemon_to_ourselves() -> MutexGuard<'static, ()> {
    DAEMON.lock().await
}

#[tokio::test]
async fn a_real_spawned_container_wears_every_hardening_flag() {
    let Some(docker) = reachable_daemon() else {
        eprintln!("SKIPPED a_real_spawned_container_wears_every_hardening_flag: no docker daemon");
        return;
    };
    let _daemon = the_daemon_to_ourselves().await;
    let plane = DockerPlane::connect(settings()).expect("the daemon should be reachable");
    clear_any_leftover(&plane, CHAT_ID).await;
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
        .inspect_network(AGENT_NETWORK, None)
        .await
        .expect("the agent network should exist");
    stop(&docker, &container.name).await;
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
    assert_eq!(
        network.internal,
        Some(false),
        "an internal network would leave the agent unable to reach the API"
    );
    assert_hardened(inspected, workspace.path(), claude.path());
}

/// Running the container as the agent is only half of ADR-0001: the core lays
/// both of a chat's trees down as itself, and a container that runs as
/// somebody else can read a tree it does not own and change nothing in it.
/// Live, that was an agent whose every commit and every session file failed
/// (issue #46). The workspace changes hands here the way it does after a
/// clone, which this test has no repository to make.
#[tokio::test]
async fn a_real_container_can_write_in_both_of_the_trees_it_was_handed() {
    let Some(docker) = reachable_daemon() else {
        eprintln!(
            "SKIPPED a_real_container_can_write_in_both_of_the_trees_it_was_handed: no docker daemon"
        );
        return;
    };
    let dataset = TempDir::new().expect("dataset root should be created");
    let us = Owner::of(dataset.path()).expect("we own what we just made");
    if us.uid != ROOT && us != Owner::AGENT {
        eprintln!(
            "SKIPPED a_real_container_can_write_in_both_of_the_trees_it_was_handed: \
             {us} can hand no tree to {}",
            Owner::AGENT
        );
        return;
    }
    let _daemon = the_daemon_to_ourselves().await;
    let plane = DockerPlane::connect(settings()).expect("the daemon should be reachable");
    clear_any_leftover(&plane, OWNERSHIP_CHAT_ID).await;
    let store = ChatStore::new(dataset.path());
    store.prepare().expect("the dataset should prepare");
    let chat_id = store
        .create_chat(NewChat {
            title: "docker gated".to_owned(),
            repo: "CorVous/fixture".to_owned(),
            branch: "chat/owned".to_owned(),
            base_branch: "main".to_owned(),
        })
        .expect("the chat should be laid down")
        .chat_id;
    hand_tree_to(&store.workspace_dir(&chat_id), Owner::AGENT)
        .expect("a tree the core owns should change hands");

    let container = plane
        .spawn(
            &chat_id,
            &store.workspace_dir(&chat_id),
            &store.claude_dir(&chat_id),
            &BTreeMap::new(),
        )
        .await
        .expect("the chat should spawn");
    let wrote = exec_exit_code(&docker, &container.name, &AGENTS_OWN_WORK).await;
    plane
        .teardown(&chat_id)
        .await
        .expect("the chat should tear down");

    assert_eq!(
        wrote.expect("the daemon should answer an exec"),
        Some(0),
        "the agent cannot write in the trees it was handed: {AGENTS_OWN_WORK:?}"
    );
}

/// A deployment older than the plain-bridge decision holds `corcode-agents` as
/// an internal network (ADR-0001). The spawn that finds it has to replace it,
/// or every turn the upgraded core runs is an agent with no route to the API.
#[tokio::test]
async fn a_spawn_replaces_the_internal_network_an_older_deployment_left() {
    let Some(docker) = reachable_daemon() else {
        eprintln!(
            "SKIPPED a_spawn_replaces_the_internal_network_an_older_deployment_left: no docker daemon"
        );
        return;
    };
    let _daemon = the_daemon_to_ourselves().await;
    let plane = DockerPlane::connect(settings()).expect("the daemon should be reachable");
    clear_any_leftover(&plane, MIGRATION_CHAT_ID).await;
    put_back_the_internal_network(&docker).await;
    let workspace = TempDir::new().expect("workspace dir should be created");
    let claude = TempDir::new().expect("claude dir should be created");

    plane
        .spawn(
            MIGRATION_CHAT_ID,
            workspace.path(),
            claude.path(),
            &BTreeMap::new(),
        )
        .await
        .expect("the chat should spawn");

    let network = docker
        .inspect_network(AGENT_NETWORK, None)
        .await
        .expect("the agent network should exist");
    let live = plane.list_live().await.expect("liveness should answer");
    plane
        .teardown(MIGRATION_CHAT_ID)
        .await
        .expect("the chat should tear down");

    assert_eq!(
        network.internal,
        Some(false),
        "the internal network should have been replaced, not reused"
    );
    assert!(
        live.contains(MIGRATION_CHAT_ID),
        "the chat's container should be running on the network that replaced it"
    );
}

/// The one thing a spawn will not do to an old internal network: take it away
/// from something already running on it. The remedy has to reach the operator
/// as an error, since the plane cannot tell whose container that is.
#[tokio::test]
async fn a_spawn_refuses_an_internal_network_with_a_container_on_it() {
    let Some(docker) = reachable_daemon() else {
        eprintln!(
            "SKIPPED a_spawn_refuses_an_internal_network_with_a_container_on_it: no docker daemon"
        );
        return;
    };
    let _daemon = the_daemon_to_ourselves().await;
    let plane = DockerPlane::connect(settings()).expect("the daemon should be reachable");
    put_back_the_internal_network(&docker).await;
    put_a_container_on_the_network(&docker).await;
    let workspace = TempDir::new().expect("workspace dir should be created");
    let claude = TempDir::new().expect("claude dir should be created");

    let refusal = plane
        .spawn(
            IN_USE_CHAT_ID,
            workspace.path(),
            claude.path(),
            &BTreeMap::new(),
        )
        .await;

    let network = docker.inspect_network(AGENT_NETWORK, None).await;
    let interloper = docker.inspect_container(INTERLOPER, None).await;
    force_remove(&docker, INTERLOPER).await;
    clear_any_leftover(&plane, IN_USE_CHAT_ID).await;

    let refusal = refusal.expect_err("a spawn should refuse a network it cannot replace");
    assert!(
        matches!(refusal, PlaneError::NetworkInUse { ref network } if network == AGENT_NETWORK),
        "the refusal should name the network and what to stop, got: {refusal}"
    );
    assert_eq!(
        network
            .expect("the network in use should still be there")
            .internal,
        Some(true),
        "the network someone is on should have survived the refused spawn"
    );
    assert_eq!(
        interloper
            .ok()
            .and_then(|interloper| interloper.state)
            .and_then(|state| state.running),
        Some(true),
        "the container the plane refused to cut off should still be running"
    );
}

/// The daemon state an upgraded deployment wakes up in: the agents' name held
/// by an internal network, with nothing on it. Whatever an earlier run left
/// attached goes first — a leaked container would otherwise fail the migration
/// cases here as though the plane were at fault.
async fn put_back_the_internal_network(docker: &Docker) {
    for attached in attached_to_the_network(docker).await {
        force_remove(docker, &attached).await;
    }
    match docker.remove_network(AGENT_NETWORK).await {
        Ok(())
        | Err(DockerError::DockerResponseServerError {
            status_code: NOT_FOUND,
            ..
        }) => {}
        Err(error) => panic!(
            "the agent network should be ours to remove, so something outside these tests is on {AGENT_NETWORK}: {error}"
        ),
    }
    docker
        .create_network(NetworkCreateRequest {
            name: AGENT_NETWORK.to_owned(),
            driver: Some("bridge".to_owned()),
            internal: Some(true),
            ..NetworkCreateRequest::default()
        })
        .await
        .expect("an internal network should be creatable");
}

/// Every container the daemon says is on the agent network, by id. A network
/// it has never heard of holds nothing.
async fn attached_to_the_network(docker: &Docker) -> Vec<String> {
    docker
        .inspect_network(AGENT_NETWORK, None)
        .await
        .ok()
        .and_then(|network| network.containers)
        .map(|attached| attached.into_keys().collect())
        .unwrap_or_default()
}

/// Someone else's container on the agents' network: what makes the old
/// internal network one the plane must refuse rather than replace.
async fn put_a_container_on_the_network(docker: &Docker) {
    pull_the_test_image(docker).await;
    force_remove(docker, INTERLOPER).await;
    let options = CreateContainerOptionsBuilder::new()
        .name(INTERLOPER)
        .build();
    let created = docker
        .create_container(
            Some(options),
            ContainerCreateBody {
                image: Some(TEST_IMAGE.to_owned()),
                entrypoint: Some(vec![String::new()]),
                cmd: Some(vec!["sleep".to_owned(), "infinity".to_owned()]),
                host_config: Some(HostConfig {
                    network_mode: Some(AGENT_NETWORK.to_owned()),
                    ..HostConfig::default()
                }),
                ..ContainerCreateBody::default()
            },
        )
        .await
        .expect("a container should be creatable on the agent network");
    docker
        .start_container(&created.id, None)
        .await
        .expect("the container on the agent network should start");
}

/// The plane pulls the image it spawns from, but a spawn that never gets past
/// the network has pulled nothing.
async fn pull_the_test_image(docker: &Docker) {
    let options = CreateImageOptionsBuilder::new()
        .from_image(TEST_IMAGE)
        .build();
    let mut pull = docker.create_image(Some(options), None, None);
    while let Some(progress) = pull.next().await {
        progress.expect("the test image should pull");
    }
}

/// Force, because a container in the way of a test is in the way running.
async fn force_remove(docker: &Docker, container: &str) {
    let force = RemoveContainerOptionsBuilder::new().force(true).build();
    match docker.remove_container(container, Some(force)).await {
        Ok(())
        | Err(DockerError::DockerResponseServerError {
            status_code: NOT_FOUND,
            ..
        }) => {}
        Err(error) => panic!("a container in the way should be removable: {error}"),
    }
}

/// The image's own command exits at once; the chat's container may not go with
/// it. Everything the core does with a chat — above all the `docker exec` the
/// adapter speaks over — needs the container to still be there afterwards.
#[tokio::test]
async fn a_container_outlives_the_command_its_image_would_have_run() {
    let Some(docker) = reachable_daemon() else {
        eprintln!(
            "SKIPPED a_container_outlives_the_command_its_image_would_have_run: no docker daemon"
        );
        return;
    };
    let _daemon = the_daemon_to_ourselves().await;
    let plane = DockerPlane::connect(settings()).expect("the daemon should be reachable");
    clear_any_leftover(&plane, KEEP_ALIVE_CHAT_ID).await;
    let workspace = TempDir::new().expect("workspace dir should be created");
    let claude = TempDir::new().expect("claude dir should be created");

    let container = plane
        .spawn(
            KEEP_ALIVE_CHAT_ID,
            workspace.path(),
            claude.path(),
            &BTreeMap::new(),
        )
        .await
        .expect("the chat should spawn");

    tokio::time::sleep(SETTLE).await;
    let live = plane.list_live().await.expect("liveness should answer");
    let exec_exit_code = exec_exit_code(&docker, &container.name, &["true"]).await;
    plane
        .teardown(KEEP_ALIVE_CHAT_ID)
        .await
        .expect("the chat should tear down");

    assert!(
        live.contains(KEEP_ALIVE_CHAT_ID),
        "the container took the image's command with it instead of parking"
    );
    assert_eq!(
        exec_exit_code.expect("the daemon should answer an exec"),
        Some(0),
        "the adapter's transport is an exec into a container that is still there"
    );
}

/// What `cmd` came to inside a live container, run the way the ACP transport
/// runs the adapter. Answered rather than asserted, so a refused exec is
/// reported by a test that has already torn its container down.
async fn exec_exit_code(
    docker: &Docker,
    name: &str,
    cmd: &[&str],
) -> Result<Option<i64>, DockerError> {
    let exec = docker
        .create_exec(
            name,
            ExecConfig {
                attach_stdout: Some(true),
                cmd: Some(cmd.iter().map(|word| (*word).to_owned()).collect()),
                ..ExecConfig::default()
            },
        )
        .await?;
    if let StartExecResults::Attached { mut output, .. } = docker.start_exec(&exec.id, None).await?
    {
        while output.next().await.is_some() {}
    }
    Ok(docker.inspect_exec(&exec.id).await?.exit_code)
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
    assert_eq!(host_config.network_mode, Some(AGENT_NETWORK.to_owned()));
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

/// The plane's containers stay up on their own, so an exited one is now
/// something a test has to arrange — and liveness must still not count it.
/// The stop call answers while the daemon may still be committing the exit, so
/// the wait is the barrier that makes "exited" true before anything asks. Its
/// answer is discarded either way: a keep-alive that had to be killed exits by
/// a signal, and that is the arrangement working.
async fn stop(docker: &Docker, name: &str) {
    let grace = StopContainerOptionsBuilder::new()
        .t(TEST_STOP_GRACE_SECONDS)
        .build();
    docker
        .stop_container(name, Some(grace))
        .await
        .expect("a live container should stop");
    let exited = WaitContainerOptionsBuilder::new()
        .condition(NOT_RUNNING)
        .build();
    let mut settling = docker.wait_container(name, Some(exited));
    while settling.next().await.is_some() {}
}

/// A run that died mid-test leaves the fixed-name container behind; it would
/// meet every later run with `AlreadyLive`.
async fn clear_any_leftover(plane: &DockerPlane, chat_id: &str) {
    match plane.teardown(chat_id).await {
        Ok(()) | Err(PlaneError::NotLive { .. }) => {}
        Err(error) => panic!("a leftover container should be removable: {error}"),
    }
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
