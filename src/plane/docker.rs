//! Workspace containers on the local Docker daemon, hardened per ADR-0001.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::Path;

use bollard::Docker;
use bollard::models::{ContainerCreateBody, HostConfig};

use super::{ContainerPlane, ContainerRef, PlaneError};
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

/// A registry login for the lazy pull (ADR-0009).
#[derive(Clone)]
pub struct RegistryCredentials {
    pub user: String,
    pub token: String,
}

impl fmt::Debug for RegistryCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegistryCredentials")
            .field("user", &self.user)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// What every spawn is the same about: the image and the box it runs in.
#[derive(Debug, Clone)]
pub struct PlaneSettings {
    pub image: String,
    pub memory_mb: u32,
    pub cpus: u32,
    pub registry: Option<RegistryCredentials>,
}

/// Spawns hardened siblings on the daemon holding the local socket.
#[derive(Debug)]
pub struct DockerPlane {
    docker: Docker,
    settings: PlaneSettings,
}

impl DockerPlane {
    /// Reach the daemon over the local socket.
    pub fn connect(_settings: PlaneSettings) -> Result<Self, PlaneError> {
        todo!("connect to the local daemon")
    }
}

impl ContainerPlane for DockerPlane {
    async fn spawn(
        &self,
        _chat_id: &str,
        _workspace_dir: &Path,
        _claude_dir: &Path,
        _env: &BTreeMap<String, String>,
    ) -> Result<ContainerRef, PlaneError> {
        todo!("pull if absent, then create and start the container")
    }

    async fn teardown(&self, _chat_id: &str) -> Result<(), PlaneError> {
        todo!("stop and remove the container")
    }

    async fn list_live(&self) -> Result<HashSet<String>, PlaneError> {
        todo!("list the labelled containers")
    }
}

impl ContainerLiveness for DockerPlane {
    async fn live_chat_ids(&self) -> anyhow::Result<HashSet<String>> {
        Ok(self.list_live().await?)
    }
}

/// The hardening contract of ADR-0001, spelled out for one chat.
fn create_body(
    _settings: &PlaneSettings,
    _chat_id: &str,
    _workspace_dir: &Path,
    _claude_dir: &Path,
    _env: &BTreeMap<String, String>,
) -> Result<ContainerCreateBody, PlaneError> {
    todo!("build the hardened container body")
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
        assert_eq!(body().user, Some(AGENT_USER.to_owned()));
    }

    #[test]
    fn the_rootfs_is_read_only_with_scratch_and_exactly_the_two_mounts() {
        let host_config = host_config();

        assert_eq!(host_config.readonly_rootfs, Some(true));
        assert_eq!(
            host_config.tmpfs,
            Some(HashMap::from([(
                SCRATCH_MOUNT.to_owned(),
                SCRATCH_OPTIONS.to_owned()
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
        assert_eq!(host_config.network_mode, Some(NETWORK.to_owned()));
    }

    #[test]
    fn the_image_the_label_and_the_env_come_from_the_caller() {
        let body = body();

        assert_eq!(body.image, Some(settings().image));
        assert_eq!(
            body.labels,
            Some(HashMap::from([(
                CHAT_ID_LABEL.to_owned(),
                CHAT_ID.to_owned()
            )]))
        );
        assert_eq!(
            body.env,
            Some(vec!["ANTHROPIC_API_KEY=sk-secret".to_owned()])
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
