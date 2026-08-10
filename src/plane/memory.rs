//! A plane that spawns nothing, for tests that care about the contract
//! rather than about Docker.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use super::{ContainerPlane, ContainerRef, PlaneError, ScriptRun};
use crate::store::ContainerLiveness;

/// The two directories a chat's container was asked to bind (ADR-0006), in
/// the names the plane was handed rather than the names it reads them by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mounts {
    pub workspace: PathBuf,
    pub claude: PathBuf,
}

/// Records spawns in memory and answers liveness from them. Clones share one
/// record, so a test can keep watching a plane it has given away.
#[derive(Debug, Default, Clone)]
pub struct MemoryPlane {
    live: Arc<Mutex<HashMap<String, Spawned>>>,
    scripts: Arc<Mutex<Vec<StartupRun>>>,
    outcome: Arc<Mutex<ScriptRun>>,
    daemon_down: Arc<AtomicBool>,
}

/// One startup script this plane was asked to run, kept so a test can read what
/// it was handed and prove the container was up when it ran (issue #14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupRun {
    pub chat_id: String,
    pub script: String,
    pub env: BTreeMap<String, String>,
    /// Whether the chat's container was live when the script ran: a script that
    /// runs only over a live container is one that runs after the spawn.
    pub over_a_live_container: bool,
}

#[derive(Clone)]
struct Spawned {
    container: ContainerRef,
    mounts: Mounts,
    env: BTreeMap<String, String>,
}

impl fmt::Debug for Spawned {
    /// The environment a container was handed carries the agent's credentials
    /// (ADR-0001): which variables it holds is the plane's to say, what they
    /// are worth is not.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Spawned")
            .field("container", &self.container)
            .field("mounts", &self.mounts)
            .field("env", &self.env.keys())
            .finish()
    }
}

impl ContainerPlane for MemoryPlane {
    async fn spawn(
        &self,
        chat_id: &str,
        workspace_dir: &Path,
        claude_dir: &Path,
        env: &BTreeMap<String, String>,
    ) -> Result<ContainerRef, PlaneError> {
        let spawned = Spawned {
            container: ContainerRef::new(chat_id, format!("in-memory-{chat_id}")),
            mounts: Mounts {
                workspace: workspace_dir.to_path_buf(),
                claude: claude_dir.to_path_buf(),
            },
            env: env.clone(),
        };
        match self.live().entry(chat_id.to_owned()) {
            Entry::Occupied(_) => Err(PlaneError::AlreadyLive {
                chat_id: chat_id.to_owned(),
            }),
            Entry::Vacant(slot) => Ok(slot.insert(spawned).container.clone()),
        }
    }

    async fn run_startup_script(
        &self,
        chat_id: &str,
        script: &str,
        env: &BTreeMap<String, String>,
    ) -> Result<ScriptRun, PlaneError> {
        let over_a_live_container = self.live().contains_key(chat_id);
        self.scripts
            .lock()
            .expect("no holder of the lock panics")
            .push(StartupRun {
                chat_id: chat_id.to_owned(),
                script: script.to_owned(),
                env: env.clone(),
                over_a_live_container,
            });
        Ok(self
            .outcome
            .lock()
            .expect("no holder of the lock panics")
            .clone())
    }

    async fn teardown(&self, chat_id: &str) -> Result<(), PlaneError> {
        self.live()
            .remove(chat_id)
            .map(|_| ())
            .ok_or_else(|| PlaneError::NotLive {
                chat_id: chat_id.to_owned(),
            })
    }

    async fn list_live(&self) -> Result<HashSet<String>, PlaneError> {
        if self.daemon_down.load(Ordering::Relaxed) {
            return Err(PlaneError::runtime("list the workspace containers")(
                io::Error::from(io::ErrorKind::ConnectionRefused).into(),
            ));
        }
        Ok(self.live().keys().cloned().collect())
    }
}

impl MemoryPlane {
    /// What a live chat's container was told to bind, or nothing if the chat
    /// holds no container.
    #[must_use]
    pub fn mounts_of(&self, chat_id: &str) -> Option<Mounts> {
        self.live()
            .get(chat_id)
            .map(|spawned| spawned.mounts.clone())
    }

    /// What a live chat's container was spawned with, or nothing if the chat
    /// holds no container.
    #[must_use]
    pub fn env_of(&self, chat_id: &str) -> Option<BTreeMap<String, String>> {
        self.live().get(chat_id).map(|spawned| spawned.env.clone())
    }

    /// Make every startup script from here on answer with `output` and
    /// `exit_code`, as a container's shell would.
    pub fn program_startup(&self, output: &str, exit_code: i64) {
        *self.outcome.lock().expect("no holder of the lock panics") = ScriptRun {
            output: output.to_owned(),
            exit_code,
        };
    }

    /// Answer every liveness question as a plane whose daemon has stopped:
    /// the socket is still there and nothing is behind it. Spawning and
    /// teardown are left alone, so a test can tell what degrades from what
    /// fails (issue #25).
    pub fn refuse_to_list_containers(&self) {
        self.daemon_down.store(true, Ordering::Relaxed);
    }

    /// Every startup script this plane was asked to run, in order.
    #[must_use]
    pub fn startup_runs(&self) -> Vec<StartupRun> {
        self.scripts
            .lock()
            .expect("no holder of the lock panics")
            .clone()
    }

    fn live(&self) -> MutexGuard<'_, HashMap<String, Spawned>> {
        self.live.lock().expect("no holder of the lock panics")
    }
}

impl ContainerLiveness for MemoryPlane {
    async fn live_chat_ids(&self) -> anyhow::Result<HashSet<String>> {
        Ok(self.list_live().await?)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::store::{ChatState, MANIFEST_SCHEMA, Manifest, RuntimeStatus, runtime_status};

    use super::*;

    async fn spawn(plane: &MemoryPlane, chat_id: &str) -> ContainerRef {
        plane
            .spawn(
                chat_id,
                &PathBuf::from("/mnt/tank/corcode/workspaces").join(chat_id),
                &PathBuf::from("/mnt/tank/corcode/chats")
                    .join(chat_id)
                    .join("claude"),
                &BTreeMap::from([("ANTHROPIC_API_KEY".to_owned(), "secret".to_owned())]),
            )
            .await
            .expect("chat should spawn")
    }

    #[tokio::test]
    async fn a_spawned_chat_is_live_under_its_deterministic_name() {
        let plane = MemoryPlane::default();

        let container = spawn(&plane, "01K1TESTCHATID0000000000").await;

        assert_eq!(container.chat_id, "01K1TESTCHATID0000000000");
        assert_eq!(container.name, "corcode-chat-01K1TESTCHATID0000000000");
        assert_eq!(
            plane.list_live().await.expect("liveness should answer"),
            HashSet::from(["01K1TESTCHATID0000000000".to_owned()])
        );
    }

    /// What a container is handed at spawn is the whole of what the agent in
    /// it can read (ADR-0001), so a test of the credentials it runs on has
    /// nowhere else to look.
    #[tokio::test]
    async fn a_spawn_records_the_environment_it_was_handed() {
        let plane = MemoryPlane::default();

        spawn(&plane, "01K1TESTCHATID0000000000").await;

        assert_eq!(
            plane.env_of("01K1TESTCHATID0000000000"),
            Some(BTreeMap::from([(
                "ANTHROPIC_API_KEY".to_owned(),
                "secret".to_owned()
            )]))
        );
        assert_eq!(plane.env_of("01K1NOSUCHCHAT0000000000"), None);
    }

    /// The environment carries the agent's credentials, so a plane that is
    /// printed says which variables a container holds and never what they are.
    #[tokio::test]
    async fn a_plane_never_says_what_a_container_was_handed() {
        let plane = MemoryPlane::default();
        spawn(&plane, "01K1TESTCHATID0000000000").await;

        let said = format!("{plane:?}");

        assert!(said.contains("ANTHROPIC_API_KEY"));
        assert!(!said.contains("secret"), "the key leaked: {said}");
    }

    #[tokio::test]
    async fn teardown_leaves_the_other_chats_live() {
        let plane = MemoryPlane::default();
        spawn(&plane, "01K1FIRSTCHAT00000000000").await;
        spawn(&plane, "01K1SECONDCHAT0000000000").await;

        plane
            .teardown("01K1FIRSTCHAT00000000000")
            .await
            .expect("live chat should tear down");

        assert_eq!(
            plane.list_live().await.expect("liveness should answer"),
            HashSet::from(["01K1SECONDCHAT0000000000".to_owned()])
        );
    }

    #[tokio::test]
    async fn spawning_a_live_chat_again_fails_loudly() {
        let plane = MemoryPlane::default();
        spawn(&plane, "01K1TESTCHATID0000000000").await;

        let error = plane
            .spawn(
                "01K1TESTCHATID0000000000",
                Path::new("/workspaces/one"),
                Path::new("/chats/one/claude"),
                &BTreeMap::new(),
            )
            .await
            .expect_err("a second spawn should fail");

        assert!(
            matches!(error, PlaneError::AlreadyLive { ref chat_id } if chat_id == "01K1TESTCHATID0000000000"),
            "error should name the live chat, got: {error}"
        );
    }

    #[tokio::test]
    async fn tearing_down_a_dead_chat_fails_loudly() {
        let plane = MemoryPlane::default();

        let error = plane
            .teardown("01K1TESTCHATID0000000000")
            .await
            .expect_err("tearing down nothing should fail");

        assert!(
            matches!(error, PlaneError::NotLive { ref chat_id } if chat_id == "01K1TESTCHATID0000000000"),
            "error should name the dead chat, got: {error}"
        );
    }

    #[tokio::test]
    async fn the_plane_feeds_the_parked_derivation() {
        let plane = MemoryPlane::default();
        let manifest = manifest("01K1TESTCHATID0000000000");
        let live = plane.live_chat_ids().await.expect("liveness should answer");
        assert_eq!(runtime_status(&manifest, &live), RuntimeStatus::Parked);

        spawn(&plane, &manifest.chat_id).await;

        let live = plane.live_chat_ids().await.expect("liveness should answer");
        assert_eq!(runtime_status(&manifest, &live), RuntimeStatus::Live);
    }

    fn manifest(chat_id: &str) -> Manifest {
        let now = chrono::Utc::now();
        Manifest {
            schema: MANIFEST_SCHEMA,
            chat_id: chat_id.to_owned(),
            title: "A chat".to_owned(),
            state: ChatState::Open,
            repo: "CorVous/CorCode".to_owned(),
            branch: "chat/test".to_owned(),
            base_branch: "main".to_owned(),
            checkpoint_branch: None,
            last_pushed_commit: None,
            acp_session_id: None,
            env: std::collections::BTreeMap::new(),
            startup_script: None,
            created_at: now,
            last_active_at: now,
        }
    }
}
