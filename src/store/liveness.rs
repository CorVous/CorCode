//! Runtime status: the manifest's state crossed with container liveness.

use std::collections::HashSet;

use anyhow::Result;

use super::manifest::{ChatState, Manifest};

/// Which chats currently own a running container. Answered by Docker in
/// production, by a fake in tests.
pub trait ContainerLiveness {
    fn live_chat_ids(&self) -> Result<HashSet<String>>;
}

/// What the UI shows for a chat right now (ADR-0002 vocabulary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeStatus {
    Live,
    Parked,
    Archived,
}

/// Derive a chat's runtime status; `parked` exists only here, never on disk.
#[must_use]
pub fn runtime_status(_manifest: &Manifest, _live_chat_ids: &HashSet<String>) -> RuntimeStatus {
    todo!("B3")
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::super::manifest::MANIFEST_SCHEMA;
    use super::*;

    struct FakeLiveness {
        live: HashSet<String>,
    }

    impl ContainerLiveness for FakeLiveness {
        fn live_chat_ids(&self) -> Result<HashSet<String>> {
            Ok(self.live.clone())
        }
    }

    fn manifest(state: ChatState) -> Manifest {
        let now = Utc::now();
        Manifest {
            schema: MANIFEST_SCHEMA,
            chat_id: "01K1TESTCHATID0000000000".to_owned(),
            title: "A chat".to_owned(),
            state,
            repo: "CorVous/CorCode".to_owned(),
            branch: "chat/test".to_owned(),
            base_branch: "main".to_owned(),
            last_pushed_commit: None,
            acp_session_id: None,
            created_at: now,
            last_active_at: now,
        }
    }

    fn liveness_of(chat_ids: &[&str]) -> HashSet<String> {
        let liveness = FakeLiveness {
            live: chat_ids.iter().map(|id| (*id).to_owned()).collect(),
        };
        liveness
            .live_chat_ids()
            .expect("fake liveness should answer")
    }

    #[test]
    fn open_chat_with_a_container_is_live() {
        let manifest = manifest(ChatState::Open);
        let live = liveness_of(&[&manifest.chat_id]);

        assert_eq!(runtime_status(&manifest, &live), RuntimeStatus::Live);
    }

    #[test]
    fn open_chat_without_a_container_is_parked() {
        let manifest = manifest(ChatState::Open);
        let live = liveness_of(&["01K1SOMEOTHERCHAT0000000"]);

        assert_eq!(runtime_status(&manifest, &live), RuntimeStatus::Parked);
    }

    #[test]
    fn archived_chat_stays_archived_whatever_docker_says() {
        let manifest = manifest(ChatState::Archived);
        let live = liveness_of(&[&manifest.chat_id]);

        assert_eq!(runtime_status(&manifest, &live), RuntimeStatus::Archived);
    }
}
