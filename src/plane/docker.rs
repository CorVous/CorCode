//! Workspace containers on the local Docker daemon, hardened per ADR-0001.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use bollard::Docker;
use bollard::auth::DockerCredentials;
use bollard::errors::Error as DockerError;
use bollard::models::{ContainerCreateBody, HostConfig, NetworkCreateRequest, NetworkInspect};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, ListContainersOptionsBuilder,
    ListNetworksOptionsBuilder, StopContainerOptionsBuilder,
};
use futures_util::StreamExt as _;

use super::{ContainerPlane, ContainerRef, PlaneError, container_name};
use crate::config::RegistryCredentials;
use crate::store::ContainerLiveness;

/// Agent containers talk to nothing but each other (ADR-0001).
const NETWORK: &str = "corcode-agents";
/// Stamped on every container so liveness is a label query.
const CHAT_ID_LABEL: &str = "corcode.chat-id";
/// The same path for every chat, forever: the adapter's transcript encodes it
/// (ADR-0006).
const WORKSPACE_MOUNT: &str = "/workspace";
/// `CLAUDE_CONFIG_DIR` in the workspace image (ADR-0004, ADR-0006).
const CLAUDE_MOUNT: &str = "/home/agent/.claude";
/// The only writable spot outside the two mounts, since the rootfs is
/// read-only.
const SCRATCH_MOUNT: &str = "/tmp";
const SCRATCH_OPTIONS: &str = "rw,nosuid,nodev,noexec,size=256m";
/// The workspace image's `agent` user, spelled numerically so the plane
/// enforces non-root whatever image it is handed.
const AGENT_USER: &str = "1000:1000";
const BYTES_PER_MB: i64 = 1024 * 1024;
const NANOS_PER_CPU: i64 = 1_000_000_000;
const STOP_GRACE_SECONDS: i32 = 10;
const NOT_FOUND: u16 = 404;
const NAME_TAKEN: u16 = 409;

/// What every spawn is the same about: the image and the box it runs in.
#[derive(Debug, Clone)]
pub struct PlaneSettings {
    pub image: String,
    pub memory_mb: u32,
    pub cpus: u32,
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

    /// The agents' own network, cut off from the core and the wider host
    /// (ADR-0001).
    async fn ensure_network(&self) -> Result<(), PlaneError> {
        let filters = HashMap::from([("name".to_owned(), vec![NETWORK.to_owned()])]);
        let options = ListNetworksOptionsBuilder::new().filters(&filters).build();
        let existing =
            self.docker
                .list_networks(Some(options))
                .await
                .map_err(PlaneError::runtime(format!(
                    "look for the {NETWORK} network"
                )))?;
        if existing
            .iter()
            .any(|network| network.name.as_deref() == Some(NETWORK))
        {
            return Ok(());
        }
        self.docker
            .create_network(NetworkCreateRequest {
                name: NETWORK.to_owned(),
                driver: Some("bridge".to_owned()),
                internal: Some(true),
                ..NetworkCreateRequest::default()
            })
            .await
            .map_err(PlaneError::runtime(format!("create the {NETWORK} network")))?;
        Ok(())
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
        let filters = HashMap::from([("label".to_owned(), vec![CHAT_ID_LABEL.to_owned()])]);
        let options = ListContainersOptionsBuilder::new()
            .filters(&filters)
            .build();
        let running = self
            .docker
            .list_containers(Some(options))
            .await
            .map_err(PlaneError::runtime("list the workspace containers"))?;
        Ok(running
            .into_iter()
            .filter_map(|container| {
                container
                    .labels
                    .and_then(|mut labels| labels.remove(CHAT_ID_LABEL))
            })
            .collect())
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
    Ok(ContainerCreateBody {
        image: Some(settings.image.clone()),
        user: Some(AGENT_USER.to_owned()),
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
                SCRATCH_OPTIONS.to_owned(),
            )])),
            memory: Some(i64::from(settings.memory_mb) * BYTES_PER_MB),
            nano_cpus: Some(i64::from(settings.cpus) * NANOS_PER_CPU),
            network_mode: Some(NETWORK.to_owned()),
            ..HostConfig::default()
        }),
        ..ContainerCreateBody::default()
    })
}

/// A network the agents may be put on: it is there, and it routes nowhere
/// (ADR-0001). Anything else — a leftover bridge, a compose-declared network —
/// would hand every agent a way out.
fn routes_nowhere(_network: Option<&NetworkInspect>) -> bool {
    todo!("judge the network")
}

fn bind(host_dir: &Path, container_path: &str) -> Result<String, PlaneError> {
    host_dir
        .to_str()
        .map(|host_dir| format!("{host_dir}:{container_path}:rw"))
        .ok_or_else(|| PlaneError::UnmountablePath {
            path: host_dir.to_owned(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHAT_ID: &str = "01K1TESTCHATID0000000000";

    fn settings() -> PlaneSettings {
        PlaneSettings {
            image: "ghcr.io/corvous/corcode-workspace:2026-08-05".to_owned(),
            memory_mb: 512,
            cpus: 3,
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
                "rw,nosuid,nodev,noexec,size=256m".to_owned()
            )]))
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

    fn network(internal: Option<bool>) -> NetworkInspect {
        NetworkInspect {
            name: Some(NETWORK.to_owned()),
            internal,
            ..NetworkInspect::default()
        }
    }

    #[test]
    fn an_internal_network_is_the_only_one_agents_may_join() {
        assert!(routes_nowhere(Some(&network(Some(true)))));
    }

    #[test]
    fn a_routable_network_of_the_same_name_is_refused() {
        assert!(
            !routes_nowhere(Some(&network(Some(false)))),
            "a routable network would give every agent a way out"
        );
        assert!(
            !routes_nowhere(Some(&network(None))),
            "a network that will not say is no better"
        );
    }

    #[test]
    fn a_missing_network_is_refused() {
        assert!(!routes_nowhere(None));
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
    fn a_path_docker_cannot_take_fails_loudly() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let error = create_body(
            &settings(),
            CHAT_ID,
            Path::new(OsStr::from_bytes(b"/mnt/tank/\xff")),
            Path::new("/mnt/tank/corcode/chats/01K1TESTCHATID0000000000/claude"),
            &BTreeMap::new(),
        )
        .expect_err("an unspellable path should fail");

        assert!(
            matches!(error, PlaneError::UnmountablePath { .. }),
            "error should blame the path, got: {error}"
        );
    }
}
