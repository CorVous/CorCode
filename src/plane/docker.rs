//! Workspace containers on the local Docker daemon, hardened per ADR-0001.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use bollard::Docker;
use bollard::auth::DockerCredentials;
use bollard::errors::Error as DockerError;
use bollard::exec::StartExecResults;
use bollard::models::{
    ContainerCreateBody, ContainerSummary, ExecConfig, HostConfig, NetworkCreateRequest,
    NetworkInspect,
};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, ListContainersOptions,
    ListContainersOptionsBuilder, StopContainerOptionsBuilder,
};
use futures_util::StreamExt as _;
use tokio::io::AsyncWriteExt as _;

use super::{ContainerPlane, ContainerRef, PlaneError, ScriptRun, WORKSPACE_MOUNT, container_name};
use crate::config::RegistryCredentials;
use crate::store::{ContainerLiveness, Owner};

/// A bridge of the agents' own, off whatever the core is on (ADR-0001).
const NETWORK: &str = "corcode-agents";
/// Stamped on every container so liveness is a label query.
const CHAT_ID_LABEL: &str = "corcode.chat-id";
/// `CLAUDE_CONFIG_DIR` in the workspace image (ADR-0004, ADR-0006).
const CLAUDE_MOUNT: &str = "/home/agent/.claude";
/// The only writable spot outside the two mounts, since the rootfs is
/// read-only.
const SCRATCH_MOUNT: &str = "/tmp";
/// What PID 1 does for a living: nothing, until the container is torn down.
/// The agent is started by `docker exec` (ADR-0001), so an image's own command
/// running as PID 1 would only be a second, stdin-less adapter — one that
/// reads EOF, exits, and takes the whole container down with it.
const KEEP_ALIVE: [&str; 2] = ["sleep", "infinity"];
/// No entrypoint at all, which the daemon spells as one empty string: the
/// keep-alive is the container's whole process, wrapped by nothing the image
/// brought with it.
const NO_ENTRYPOINT: [&str; 1] = [""];
const BYTES_PER_MB: i64 = 1024 * 1024;
const NANOS_PER_CPU: i64 = 1_000_000_000;
const STOP_GRACE_SECONDS: i32 = 10;
const NOT_FOUND: u16 = 404;
/// How the daemon refuses to remove a network that still has endpoints on it.
const STILL_IN_USE: u16 = 403;
const NAME_TAKEN: u16 = 409;

/// What every spawn is the same about: the image and the box it runs in.
#[derive(Debug, Clone)]
pub struct PlaneSettings {
    pub image: String,
    pub memory_mb: u32,
    pub cpus: u32,
    pub scratch_mb: u32,
    pub registry: Option<RegistryCredentials>,
}

/// Spawns hardened siblings on the daemon holding the local socket.
pub struct DockerPlane {
    docker: Docker,
    settings: PlaneSettings,
}

impl DockerPlane {
    /// Reach the daemon over the local socket.
    pub fn connect(settings: PlaneSettings) -> Result<Self, PlaneError> {
        let docker = Docker::connect_with_local_defaults()
            .map_err(PlaneError::runtime("reach the local docker daemon"))?;
        Ok(Self { docker, settings })
    }

    /// The agents' own network, off the core's own but routing out to the API
    /// (ADR-0001). Whatever answers to the name is judged, and one of the
    /// wrong shape is replaced where nobody is on it.
    async fn ensure_network(&self) -> Result<(), PlaneError> {
        match network_found(self.agent_network().await?.as_ref()) {
            NetworkFound::RoutesOut => return Ok(()),
            NetworkFound::Nothing => {}
            NetworkFound::WrongShapeAndEmpty => self.remove_agent_network().await?,
            NetworkFound::WrongShapeAndInUse => {
                return Err(PlaneError::NetworkInUse {
                    network: NETWORK.to_owned(),
                });
            }
        }
        self.create_agent_network().await?;
        if self
            .agent_network()
            .await?
            .is_some_and(|created| routes_out(&created))
        {
            Ok(())
        } else {
            Err(PlaneError::UnusableNetwork {
                network: NETWORK.to_owned(),
            })
        }
    }

    async fn agent_network(&self) -> Result<Option<NetworkInspect>, PlaneError> {
        match self.docker.inspect_network(NETWORK, None).await {
            Ok(network) => Ok(Some(network)),
            Err(DockerError::DockerResponseServerError {
                status_code: NOT_FOUND,
                ..
            }) => Ok(None),
            Err(source) => Err(PlaneError::runtime(format!(
                "look for the {NETWORK} network"
            ))(source)),
        }
    }

    /// A second core racing us to it is a win, not a failure.
    async fn create_agent_network(&self) -> Result<(), PlaneError> {
        match self
            .docker
            .create_network(NetworkCreateRequest {
                name: NETWORK.to_owned(),
                driver: Some("bridge".to_owned()),
                internal: Some(false),
                ..NetworkCreateRequest::default()
            })
            .await
        {
            Ok(_)
            | Err(DockerError::DockerResponseServerError {
                status_code: NAME_TAKEN,
                ..
            }) => Ok(()),
            Err(source) => {
                Err(PlaneError::runtime(format!("create the {NETWORK} network"))(source))
            }
        }
    }

    /// Make room for the bridge by dropping an empty network of the wrong
    /// shape. Another core getting there first is a win, not a failure.
    async fn remove_agent_network(&self) -> Result<(), PlaneError> {
        match self.docker.remove_network(NETWORK).await {
            Ok(())
            | Err(DockerError::DockerResponseServerError {
                status_code: NOT_FOUND,
                ..
            }) => Ok(()),
            Err(source) => Err(removal_failure(source)),
        }
    }

    /// Pull the configured tag the first time a spawn misses it locally
    /// (ADR-0009).
    async fn ensure_image(&self) -> Result<(), PlaneError> {
        let image = &self.settings.image;
        match self.docker.inspect_image(image).await {
            Ok(_) => Ok(()),
            Err(DockerError::DockerResponseServerError {
                status_code: NOT_FOUND,
                ..
            }) => self.pull_image().await,
            Err(source) => Err(PlaneError::runtime(format!("look for image {image}"))(
                source,
            )),
        }
    }

    async fn pull_image(&self) -> Result<(), PlaneError> {
        let image = &self.settings.image;
        let options = CreateImageOptionsBuilder::new().from_image(image).build();
        let credentials = self
            .settings
            .registry
            .as_ref()
            .map(|registry| DockerCredentials {
                username: Some(registry.user.clone()),
                password: Some(registry.token.clone()),
                ..DockerCredentials::default()
            });
        let mut pull = self.docker.create_image(Some(options), None, credentials);
        while let Some(progress) = pull.next().await {
            progress.map_err(PlaneError::runtime(format!("pull image {image}")))?;
        }
        Ok(())
    }
}

impl ContainerPlane for DockerPlane {
    async fn spawn(
        &self,
        chat_id: &str,
        workspace_dir: &Path,
        claude_dir: &Path,
        env: &BTreeMap<String, String>,
    ) -> Result<ContainerRef, PlaneError> {
        self.ensure_network().await?;
        self.ensure_image().await?;
        let options = CreateContainerOptionsBuilder::new()
            .name(&container_name(chat_id))
            .build();
        let body = create_body(&self.settings, chat_id, workspace_dir, claude_dir, env)?;
        let created = self
            .docker
            .create_container(Some(options), body)
            .await
            .map_err(spawn_failure(chat_id))?;
        self.docker
            .start_container(&created.id, None)
            .await
            .map_err(PlaneError::runtime(format!(
                "start the container of chat {chat_id}"
            )))?;
        Ok(ContainerRef::new(chat_id, created.id))
    }

    async fn run_startup_script(
        &self,
        chat_id: &str,
        script: &str,
        env: &BTreeMap<String, String>,
    ) -> Result<ScriptRun, PlaneError> {
        let container = container_name(chat_id);
        let blame = |action: &str| {
            PlaneError::runtime(format!("{action} the startup script of chat {chat_id}"))
        };
        let exec = self
            .docker
            .create_exec(&container, startup_exec(env))
            .await
            .map_err(blame("prepare"))?;
        let StartExecResults::Attached {
            mut output,
            mut input,
        } = self
            .docker
            .start_exec(&exec.id, None)
            .await
            .map_err(blame("start"))?
        else {
            return Err(blame("attach to")(DockerError::IOError {
                err: std::io::Error::other("the daemon detached the startup script"),
            }));
        };
        input
            .write_all(script.as_bytes())
            .await
            .map_err(|err| blame("feed")(DockerError::IOError { err }))?;
        input
            .shutdown()
            .await
            .map_err(|err| blame("close the input of")(DockerError::IOError { err }))?;
        drop(input);
        let mut combined = String::new();
        while let Some(chunk) = output.next().await {
            combined.push_str(&String::from_utf8_lossy(
                chunk.map_err(blame("read"))?.as_ref(),
            ));
        }
        let exit_code = self
            .docker
            .inspect_exec(&exec.id)
            .await
            .map_err(blame("read the exit code of"))?
            .exit_code
            .unwrap_or_default();
        Ok(ScriptRun {
            output: combined,
            exit_code,
        })
    }

    async fn teardown(&self, chat_id: &str) -> Result<(), PlaneError> {
        let name = container_name(chat_id);
        let stop = StopContainerOptionsBuilder::new()
            .t(STOP_GRACE_SECONDS)
            .build();
        self.docker
            .stop_container(&name, Some(stop))
            .await
            .map_err(teardown_failure(chat_id))?;
        self.docker
            .remove_container(&name, None)
            .await
            .map_err(teardown_failure(chat_id))
    }

    async fn list_live(&self) -> Result<HashSet<String>, PlaneError> {
        let running = self
            .docker
            .list_containers(Some(live_container_query()))
            .await
            .map_err(PlaneError::runtime("list the workspace containers"))?;
        Ok(chat_ids_of(running))
    }
}

/// A name clash means the chat already holds its one container.
fn spawn_failure(chat_id: &str) -> impl FnOnce(DockerError) -> PlaneError + use<> {
    let chat_id = chat_id.to_owned();
    move |source| match source {
        DockerError::DockerResponseServerError {
            status_code: NAME_TAKEN,
            ..
        } => PlaneError::AlreadyLive { chat_id },
        source => PlaneError::runtime(format!("spawn a container for chat {chat_id}"))(source),
    }
}

/// A container can attach between the look and the removal, and the daemon is
/// the one that knows: endpoints it still holds turn a replacement into the
/// same refusal the inspection would have given.
fn removal_failure(source: DockerError) -> PlaneError {
    match source {
        DockerError::DockerResponseServerError {
            status_code: STILL_IN_USE,
            ..
        } => PlaneError::NetworkInUse {
            network: NETWORK.to_owned(),
        },
        source => PlaneError::runtime(format!("replace the {NETWORK} network"))(source),
    }
}

fn teardown_failure(chat_id: &str) -> impl FnOnce(DockerError) -> PlaneError + use<> {
    let chat_id = chat_id.to_owned();
    move |source| match source {
        DockerError::DockerResponseServerError {
            status_code: NOT_FOUND,
            ..
        } => PlaneError::NotLive { chat_id },
        source => PlaneError::runtime(format!("tear down the container of chat {chat_id}"))(source),
    }
}

impl ContainerLiveness for DockerPlane {
    async fn live_chat_ids(&self) -> anyhow::Result<HashSet<String>> {
        Ok(self.list_live().await?)
    }
}

/// The hardening contract of ADR-0001, spelled out for one chat.
fn create_body(
    settings: &PlaneSettings,
    chat_id: &str,
    workspace_dir: &Path,
    claude_dir: &Path,
    env: &BTreeMap<String, String>,
) -> Result<ContainerCreateBody, PlaneError> {
    let memory = i64::from(settings.memory_mb) * BYTES_PER_MB;
    Ok(ContainerCreateBody {
        image: Some(settings.image.clone()),
        entrypoint: Some(NO_ENTRYPOINT.map(str::to_owned).into()),
        cmd: Some(KEEP_ALIVE.map(str::to_owned).into()),
        user: Some(Owner::AGENT.to_string()),
        env: Some(
            env.iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect(),
        ),
        labels: Some(HashMap::from([(
            CHAT_ID_LABEL.to_owned(),
            chat_id.to_owned(),
        )])),
        host_config: Some(HostConfig {
            binds: Some(vec![
                bind(workspace_dir, WORKSPACE_MOUNT)?,
                bind(claude_dir, CLAUDE_MOUNT)?,
            ]),
            cap_drop: Some(vec!["ALL".to_owned()]),
            security_opt: Some(vec!["no-new-privileges:true".to_owned()]),
            readonly_rootfs: Some(true),
            tmpfs: Some(HashMap::from([(
                SCRATCH_MOUNT.to_owned(),
                scratch_options(settings.scratch_mb),
            )])),
            memory: Some(memory),
            memory_swap: Some(memory),
            nano_cpus: Some(i64::from(settings.cpus) * NANOS_PER_CPU),
            network_mode: Some(NETWORK.to_owned()),
            ..HostConfig::default()
        }),
        ..ContainerCreateBody::default()
    })
}

/// The exec that runs a chat's startup script: a shell reading the script off
/// its own stdin — so no quoting in the script can trip the invocation — as the
/// agent, in the workspace, with the chat's env visible (issue #14).
fn startup_exec(env: &BTreeMap<String, String>) -> ExecConfig {
    ExecConfig {
        attach_stdin: Some(true),
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        tty: Some(false),
        cmd: Some(vec!["sh".to_owned(), "-s".to_owned()]),
        user: Some(Owner::AGENT.to_string()),
        working_dir: Some(WORKSPACE_MOUNT.to_owned()),
        env: Some(
            env.iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect(),
        ),
        ..ExecConfig::default()
    }
}

/// How the scratch tmpfs is mounted at the size this deployment allows it:
/// writable, and nothing a container could be made more dangerous by.
fn scratch_options(scratch_mb: u32) -> String {
    format!("rw,nosuid,nodev,noexec,size={scratch_mb}m")
}

/// A network the agents may be put on: the plane's own bridge, routing out to
/// the API the agents work through (ADR-0001). An internal one — or one that
/// will not say — is a network no turn could ever run on.
fn routes_out(network: &NetworkInspect) -> bool {
    network.internal == Some(false)
}

/// What answers to the agent network's name, and so what a spawn must do about
/// it first.
#[derive(Debug, PartialEq, Eq)]
enum NetworkFound {
    Nothing,
    RoutesOut,
    WrongShapeAndEmpty,
    WrongShapeAndInUse,
}

/// A network of the wrong shape is the plane's to replace exactly while
/// nothing is on it: containers on an internal network belong to someone, and
/// pulling it out from under them is not a spawn's business.
fn network_found(network: Option<&NetworkInspect>) -> NetworkFound {
    match network {
        None => NetworkFound::Nothing,
        Some(network) if routes_out(network) => NetworkFound::RoutesOut,
        Some(network) if said_to_hold_nothing(network) => NetworkFound::WrongShapeAndEmpty,
        Some(_) => NetworkFound::WrongShapeAndInUse,
    }
}

/// An empty roster is the daemon saying nobody is on the network; a roster it
/// left out says nothing at all, and a spawn deletes on the answer it has.
fn said_to_hold_nothing(network: &NetworkInspect) -> bool {
    network.containers.as_ref().is_some_and(HashMap::is_empty)
}

/// Ask for the chats' containers that are running: an exited one is a chat
/// whose agent is gone, and gone chats are parked (ADR-0002).
fn live_container_query() -> ListContainersOptions {
    let filters = HashMap::from([("label".to_owned(), vec![CHAT_ID_LABEL.to_owned()])]);
    ListContainersOptionsBuilder::new()
        .all(false)
        .filters(&filters)
        .build()
}

/// The chat each container belongs to, skipping whatever else the daemon runs.
fn chat_ids_of(containers: Vec<ContainerSummary>) -> HashSet<String> {
    containers
        .into_iter()
        .filter_map(|container| {
            container
                .labels
                .and_then(|mut labels| labels.remove(CHAT_ID_LABEL))
        })
        .collect()
}

/// A bind is three colon-separated fields, so a host path is only mountable
/// while it is absolute and colon-free.
fn bind(host_dir: &Path, container_path: &str) -> Result<String, PlaneError> {
    host_dir
        .to_str()
        .filter(|host_dir| host_dir.starts_with('/') && !host_dir.contains(':'))
        .map(|host_dir| format!("{host_dir}:{container_path}:rw"))
        .ok_or_else(|| PlaneError::UnmountablePath {
            path: host_dir.to_owned(),
        })
}

#[cfg(test)]
mod tests {
    use bollard::models::EndpointResource;

    use super::*;

    const CHAT_ID: &str = "01K1TESTCHATID0000000000";

    fn settings() -> PlaneSettings {
        PlaneSettings {
            image: "ghcr.io/corvous/corcode-workspace:2026-08-05".to_owned(),
            memory_mb: 512,
            cpus: 3,
            scratch_mb: 64,
            registry: None,
        }
    }

    fn body() -> ContainerCreateBody {
        create_body(
            &settings(),
            CHAT_ID,
            Path::new("/mnt/tank/corcode/workspaces/01K1TESTCHATID0000000000"),
            Path::new("/mnt/tank/corcode/chats/01K1TESTCHATID0000000000/claude"),
            &BTreeMap::from([("ANTHROPIC_API_KEY".to_owned(), "sk-secret".to_owned())]),
        )
        .expect("usable paths should build a body")
    }

    fn host_config() -> HostConfig {
        body().host_config.expect("body should be hardened")
    }

    #[test]
    fn the_container_drops_every_capability_and_gains_no_privileges() {
        let host_config = host_config();

        assert_eq!(host_config.cap_drop, Some(vec!["ALL".to_owned()]));
        assert_eq!(host_config.cap_add, None);
        assert_eq!(
            host_config.security_opt,
            Some(vec!["no-new-privileges:true".to_owned()])
        );
        assert_ne!(host_config.privileged, Some(true));
        assert_eq!(body().user, Some("1000:1000".to_owned()));
    }

    #[test]
    fn the_rootfs_is_read_only_with_scratch_and_exactly_the_two_mounts() {
        let host_config = host_config();

        assert_eq!(host_config.readonly_rootfs, Some(true));
        assert_eq!(
            host_config.tmpfs,
            Some(HashMap::from([(
                "/tmp".to_owned(),
                "rw,nosuid,nodev,noexec,size=64m".to_owned()
            )])),
            "the scratch tmpfs is the size this deployment allows it, and no bigger"
        );
        assert_eq!(
            host_config.binds,
            Some(vec![
                "/mnt/tank/corcode/workspaces/01K1TESTCHATID0000000000:/workspace:rw".to_owned(),
                "/mnt/tank/corcode/chats/01K1TESTCHATID0000000000/claude:/home/agent/.claude:rw"
                    .to_owned(),
            ])
        );
        assert_eq!(host_config.mounts, None);
    }

    #[test]
    fn memory_cpu_and_the_agent_network_are_pinned() {
        let host_config = host_config();

        assert_eq!(host_config.memory, Some(512 * 1024 * 1024));
        assert_eq!(
            host_config.memory_swap,
            Some(512 * 1024 * 1024),
            "swap left open would double the ceiling"
        );
        assert_eq!(host_config.nano_cpus, Some(3_000_000_000));
        assert_eq!(host_config.network_mode, Some("corcode-agents".to_owned()));
    }

    #[test]
    fn the_image_the_label_and_the_env_come_from_the_caller() {
        let body = body();

        assert_eq!(body.image, Some(settings().image));
        assert_eq!(
            body.labels,
            Some(HashMap::from([(
                "corcode.chat-id".to_owned(),
                CHAT_ID.to_owned()
            )]))
        );
        assert_eq!(
            body.env,
            Some(vec!["ANTHROPIC_API_KEY=sk-secret".to_owned()])
        );
    }

    #[test]
    fn the_container_is_parked_on_the_planes_own_keep_alive() {
        let body = body();

        assert_eq!(
            body.cmd,
            Some(vec!["sleep".to_owned(), "infinity".to_owned()]),
            "whatever the image would run, the plane's container waits for exec"
        );
        assert_eq!(
            body.entrypoint,
            Some(vec![String::new()]),
            "an image entrypoint left in place could divert or outlive the keep-alive"
        );
    }

    fn network(internal: Option<bool>) -> NetworkInspect {
        NetworkInspect {
            name: Some(NETWORK.to_owned()),
            internal,
            ..NetworkInspect::default()
        }
    }

    fn network_holding(internal: Option<bool>, container: &str) -> NetworkInspect {
        NetworkInspect {
            containers: Some(HashMap::from([(
                container.to_owned(),
                EndpointResource::default(),
            )])),
            ..network(internal)
        }
    }

    #[test]
    fn a_routing_bridge_is_the_only_network_agents_may_join() {
        assert!(routes_out(&network(Some(false))));
    }

    #[test]
    fn an_internal_network_of_the_same_name_is_refused() {
        assert!(
            !routes_out(&network(Some(true))),
            "an internal network leaves every agent unable to reach the API"
        );
        assert!(
            !routes_out(&network(None)),
            "a network that will not say is no better"
        );
    }

    #[test]
    fn nothing_under_the_name_is_the_networks_to_create() {
        assert_eq!(network_found(None), NetworkFound::Nothing);
    }

    #[test]
    fn a_routing_bridge_is_taken_as_it_stands() {
        assert_eq!(
            network_found(Some(&network(Some(false)))),
            NetworkFound::RoutesOut
        );
    }

    #[test]
    fn an_internal_network_the_daemon_calls_empty_is_the_planes_to_replace() {
        assert_eq!(
            network_found(Some(&NetworkInspect {
                containers: Some(HashMap::new()),
                ..network(Some(true))
            })),
            NetworkFound::WrongShapeAndEmpty
        );
    }

    #[test]
    fn an_internal_network_with_no_roster_at_all_is_left_where_it_is() {
        assert_eq!(
            network_found(Some(&network(Some(true)))),
            NetworkFound::WrongShapeAndInUse,
            "a roster the daemon did not send is no promise that nothing is on it"
        );
    }

    #[test]
    fn an_internal_network_with_a_container_on_it_is_nobodys_to_delete() {
        assert_eq!(
            network_found(Some(&network_holding(Some(true), &container_name(CHAT_ID)))),
            NetworkFound::WrongShapeAndInUse,
            "replacing it would cut off whatever is running on it"
        );
    }

    const OTHER_CHAT_ID: &str = "01K1OTHERCHATID000000000";

    fn container_of(labels: HashMap<String, String>) -> ContainerSummary {
        ContainerSummary {
            labels: Some(labels),
            ..ContainerSummary::default()
        }
    }

    fn container_of_chat(chat_id: &str) -> ContainerSummary {
        container_of(HashMap::from([(
            "corcode.chat-id".to_owned(),
            chat_id.to_owned(),
        )]))
    }

    #[test]
    fn liveness_asks_only_for_the_running_chat_containers() {
        let query = live_container_query();

        assert!(!query.all, "an exited container is not a live agent");
        assert_eq!(
            query.filters,
            Some(HashMap::from([(
                "label".to_owned(),
                vec!["corcode.chat-id".to_owned()]
            )]))
        );
    }

    #[test]
    fn every_listed_container_names_its_chat() {
        let live = chat_ids_of(vec![
            container_of_chat(CHAT_ID),
            container_of_chat(OTHER_CHAT_ID),
        ]);

        assert_eq!(
            live,
            HashSet::from([CHAT_ID.to_owned(), OTHER_CHAT_ID.to_owned()])
        );
    }

    #[test]
    fn a_container_belonging_to_no_chat_is_not_live() {
        let live = chat_ids_of(vec![
            ContainerSummary::default(),
            container_of(HashMap::new()),
            container_of(HashMap::from([("chat-id".to_owned(), CHAT_ID.to_owned())])),
        ]);

        assert!(
            live.is_empty(),
            "only the plane's own label names a chat, got: {live:?}"
        );
    }

    fn server_said(status_code: u16) -> DockerError {
        DockerError::DockerResponseServerError {
            status_code,
            message: "as the daemon put it".to_owned(),
        }
    }

    #[test]
    fn a_taken_name_means_the_chat_is_already_live() {
        let error = spawn_failure(CHAT_ID)(server_said(409));

        assert!(
            matches!(error, PlaneError::AlreadyLive { ref chat_id } if chat_id == CHAT_ID),
            "a name clash should name the live chat, got: {error}"
        );
    }

    #[test]
    fn a_network_something_is_still_on_is_not_the_planes_to_remove() {
        let error = removal_failure(server_said(403));

        assert!(
            matches!(error, PlaneError::NetworkInUse { ref network } if network == NETWORK),
            "a removal the daemon refused should say what to stop, got: {error}"
        );
    }

    #[test]
    fn any_other_refusal_to_remove_says_what_was_being_done() {
        let error = removal_failure(server_said(500));

        assert!(
            format!("{error}").contains(&format!("replace the {NETWORK} network")),
            "error should say what failed, got: {error}"
        );
    }

    #[test]
    fn a_missing_container_means_the_chat_is_not_live() {
        let error = teardown_failure(CHAT_ID)(server_said(404));

        assert!(
            matches!(error, PlaneError::NotLive { ref chat_id } if chat_id == CHAT_ID),
            "a missing container should name the dead chat, got: {error}"
        );
    }

    #[test]
    fn any_other_daemon_refusal_says_what_was_being_done() {
        let error = teardown_failure(CHAT_ID)(server_said(500));

        assert!(
            format!("{error}").contains(&format!("tear down the container of chat {CHAT_ID}")),
            "error should say what failed, got: {error}"
        );
    }

    #[test]
    fn the_startup_script_runs_as_the_agent_in_the_workspace_over_a_shell() {
        let config = startup_exec(&BTreeMap::from([("EDITOR".to_owned(), "helix".to_owned())]));

        assert_eq!(
            config.cmd,
            Some(vec!["sh".to_owned(), "-s".to_owned()]),
            "the script is fed to a shell on stdin, so no quoting can trip it"
        );
        assert_eq!(
            config.user,
            Some(Owner::AGENT.to_string()),
            "the script runs as the agent, never as root"
        );
        assert_eq!(
            config.working_dir,
            Some(WORKSPACE_MOUNT.to_owned()),
            "the script runs with the workspace as its cwd"
        );
        assert_eq!(config.env, Some(vec!["EDITOR=helix".to_owned()]));
        assert_eq!(
            config.attach_stdin,
            Some(true),
            "the script arrives on stdin"
        );
        assert_eq!(config.attach_stdout, Some(true));
        assert_eq!(
            config.attach_stderr,
            Some(true),
            "stderr is captured alongside stdout"
        );
    }

    #[test]
    fn a_path_docker_cannot_take_fails_loudly() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let unspellable = Path::new(OsStr::from_bytes(b"/mnt/tank/\xff"));

        assert_blamed(unspellable);
    }

    #[test]
    fn a_relative_host_path_is_no_mount() {
        assert_blamed(Path::new("workspaces/01K1TESTCHATID0000000000"));
    }

    #[test]
    fn a_host_path_bearing_a_colon_is_no_mount() {
        assert_blamed(Path::new("/mnt/tank:backup/corcode/workspaces"));
    }

    /// A mount docker would misread is a mount the plane refuses to ask for.
    fn assert_blamed(workspace_dir: &Path) {
        let error = create_body(
            &settings(),
            CHAT_ID,
            workspace_dir,
            Path::new("/mnt/tank/corcode/chats/01K1TESTCHATID0000000000/claude"),
            &BTreeMap::new(),
        )
        .expect_err("an unmountable path should fail");

        assert!(
            matches!(&error, PlaneError::UnmountablePath { path } if path == workspace_dir),
            "error should blame {}, got: {error}",
            workspace_dir.display()
        );
    }
}
