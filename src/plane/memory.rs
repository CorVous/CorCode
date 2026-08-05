//! A plane that spawns nothing, for tests that care about the contract
//! rather than about Docker.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;

use super::{ContainerPlane, ContainerRef, PlaneError};
use crate::store::ContainerLiveness;

/// Records spawns in memory and answers liveness from them.
#[derive(Debug, Default)]
pub struct MemoryPlane {
    live: Mutex<HashMap<String, ContainerRef>>,
}

impl ContainerPlane for MemoryPlane {
    async fn spawn(
        &self,
        _chat_id: &str,
        _workspace_dir: &Path,
        _claude_dir: &Path,
        _env: &BTreeMap<String, String>,
    ) -> Result<ContainerRef, PlaneError> {
        todo!("record the spawn")
    }

    async fn teardown(&self, _chat_id: &str) -> Result<(), PlaneError> {
        todo!("forget the spawn")
    }

    async fn list_live(&self) -> Result<HashSet<String>, PlaneError> {
        todo!("answer from the recorded spawns")
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

    use crate::plane::container_name;
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
        assert_eq!(container.name, container_name("01K1TESTCHATID0000000000"));
        assert_eq!(
            plane.list_live().await.expect("liveness should answer"),
            HashSet::from(["01K1TESTCHATID0000000000".to_owned()])
        );
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
            last_pushed_commit: None,
            acp_session_id: None,
            created_at: now,
            last_active_at: now,
        }
    }
}
