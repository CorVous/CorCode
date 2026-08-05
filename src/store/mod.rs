//! Chat persistence over the two-tree dataset layout (ADR-0006).

mod error;
mod events;
mod liveness;
mod manifest;

use std::path::PathBuf;

use serde_json::Value;

pub use error::StoreError;
pub use events::Event;
pub use liveness::{ContainerLiveness, RuntimeStatus, runtime_status};
pub use manifest::{ChatState, MANIFEST_SCHEMA, Manifest, NewChat};

const CHATS_DIR: &str = "chats";
const WORKSPACES_DIR: &str = "workspaces";
const CLAUDE_DIR: &str = "claude";
const MANIFEST_FILE: &str = "manifest.json";
const EVENTS_FILE: &str = "events.jsonl";

/// Reader and writer of every chat under one dataset root.
pub struct ChatStore {
    root: PathBuf,
}

impl ChatStore {
    /// Serve the dataset root given by `CORCODE_DATA_DIR`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Durable home of a chat: manifest, event log, agent memory.
    #[must_use]
    pub fn chat_dir(&self, chat_id: &str) -> PathBuf {
        self.root.join(CHATS_DIR).join(chat_id)
    }

    /// Agent memory, mounted as `CLAUDE_CONFIG_DIR` (ADR-0004).
    #[must_use]
    pub fn claude_dir(&self, chat_id: &str) -> PathBuf {
        self.chat_dir(chat_id).join(CLAUDE_DIR)
    }

    /// Working tree of an open chat; exists iff the chat is open (ADR-0002).
    #[must_use]
    pub fn workspace_dir(&self, chat_id: &str) -> PathBuf {
        self.root.join(WORKSPACES_DIR).join(chat_id)
    }

    /// Lay down both trees for a new chat and persist its manifest.
    pub fn create_chat(&self, _new_chat: NewChat) -> Result<Manifest, StoreError> {
        todo!("B3")
    }

    /// Every chat on disk, most recently active first.
    pub fn scan(&self) -> Result<Vec<Manifest>, StoreError> {
        todo!("B3")
    }

    pub fn read_manifest(&self, _chat_id: &str) -> Result<Manifest, StoreError> {
        todo!("B3")
    }

    /// Replace a chat's manifest in one atomic step.
    pub fn write_manifest(&self, _manifest: &Manifest) -> Result<(), StoreError> {
        todo!("B3")
    }

    /// Append one ACP payload to the chat's display record, flushed on return.
    pub fn append_event(&self, _chat_id: &str, _event: &Value) -> Result<(), StoreError> {
        todo!("B3")
    }

    pub fn read_events(&self, _chat_id: &str) -> Result<Vec<Event>, StoreError> {
        todo!("B3")
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use chrono::{Duration, Utc};
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    fn store() -> (TempDir, ChatStore) {
        let root = TempDir::new().expect("temp dataset root should be created");
        let store = ChatStore::new(root.path());
        (root, store)
    }

    fn new_chat(title: &str) -> NewChat {
        NewChat {
            title: title.to_owned(),
            repo: "CorVous/CorCode".to_owned(),
            branch: format!("chat/{title}"),
            base_branch: "main".to_owned(),
        }
    }

    #[test]
    fn create_chat_lays_down_both_trees() {
        let (_root, store) = store();

        let manifest = store
            .create_chat(new_chat("persistence"))
            .expect("chat should be created");

        assert_eq!(manifest.state, ChatState::Open);
        assert_eq!(manifest.schema, MANIFEST_SCHEMA);
        assert!(store.claude_dir(&manifest.chat_id).is_dir());
        assert!(store.workspace_dir(&manifest.chat_id).is_dir());
        assert!(store.chat_dir(&manifest.chat_id).join(EVENTS_FILE).is_file());
        assert_eq!(
            store
                .read_manifest(&manifest.chat_id)
                .expect("manifest should read back"),
            manifest
        );
    }

    #[test]
    fn scan_lists_chats_most_recently_active_first() {
        let (_root, store) = store();
        let now = Utc::now();
        for (title, age_minutes) in [("stale", 60), ("fresh", 1), ("middling", 10)] {
            let mut manifest = store
                .create_chat(new_chat(title))
                .expect("chat should be created");
            manifest.last_active_at = now - Duration::minutes(age_minutes);
            store
                .write_manifest(&manifest)
                .expect("manifest should be written");
        }

        let titles: Vec<String> = store
            .scan()
            .expect("scan should succeed")
            .into_iter()
            .map(|manifest| manifest.title)
            .collect();

        assert_eq!(titles, ["fresh", "middling", "stale"]);
    }

    #[test]
    fn scan_fails_loudly_on_a_corrupt_manifest() {
        let (_root, store) = store();
        let manifest = store
            .create_chat(new_chat("corrupt"))
            .expect("chat should be created");
        let path = store.chat_dir(&manifest.chat_id).join(MANIFEST_FILE);
        fs::write(&path, "{ this is not json").expect("corruption should be writable");

        let error = store.scan().expect_err("corrupt manifest should fail");

        assert!(
            format!("{error}").contains(&path.display().to_string()),
            "error should name the file, got: {error}"
        );
    }

    #[test]
    fn read_manifest_rejects_an_unknown_schema() {
        let (_root, store) = store();
        let manifest = store
            .create_chat(new_chat("future"))
            .expect("chat should be created");
        let path = store.chat_dir(&manifest.chat_id).join(MANIFEST_FILE);
        let mut fields: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("manifest should be readable"))
                .expect("manifest should be json");
        fields["schema"] = json!(2);
        fs::write(&path, fields.to_string()).expect("manifest should be rewritable");

        let error = store
            .read_manifest(&manifest.chat_id)
            .expect_err("unknown schema should fail");

        let message = format!("{error}");
        assert!(
            message.contains(&path.display().to_string()) && message.contains('2'),
            "error should name the file and the schema, got: {message}"
        );
    }

    #[test]
    fn rewriting_a_manifest_replaces_it_and_leaves_no_temp_file() {
        let (_root, store) = store();
        let mut manifest = store
            .create_chat(new_chat("renamed"))
            .expect("chat should be created");
        manifest.title = "Renamed".to_owned();
        manifest.acp_session_id = Some("session-uuid".to_owned());

        store
            .write_manifest(&manifest)
            .expect("manifest should be written");

        assert_eq!(
            store
                .read_manifest(&manifest.chat_id)
                .expect("manifest should read back"),
            manifest
        );
        let leftovers: Vec<PathBuf> = fs::read_dir(store.chat_dir(&manifest.chat_id))
            .expect("chat dir should be readable")
            .map(|entry| entry.expect("entry should be readable").path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left behind: {leftovers:?}");
    }

    #[test]
    fn appended_events_read_back_in_order() {
        let (_root, store) = store();
        let manifest = store
            .create_chat(new_chat("events"))
            .expect("chat should be created");
        let payloads = [
            json!({"sessionUpdate": "user_message_chunk", "text": "hello"}),
            json!({"sessionUpdate": "agent_message_chunk", "text": "hi"}),
        ];
        for payload in &payloads {
            store
                .append_event(&manifest.chat_id, payload)
                .expect("event should append");
        }

        let events = store
            .read_events(&manifest.chat_id)
            .expect("events should read back");

        let read_payloads: Vec<Value> = events.iter().map(|event| event.event.clone()).collect();
        assert_eq!(read_payloads, payloads);
        assert!(events[0].ts <= events[1].ts);
    }

    #[test]
    fn read_events_fails_loudly_on_a_torn_line() {
        let (_root, store) = store();
        let manifest = store
            .create_chat(new_chat("torn"))
            .expect("chat should be created");
        store
            .append_event(&manifest.chat_id, &json!({"sessionUpdate": "plan"}))
            .expect("event should append");
        let path = store.chat_dir(&manifest.chat_id).join(EVENTS_FILE);
        let torn = format!(
            "{}{{\"ts\":\"2026-08-05T12:00",
            fs::read_to_string(&path).expect("events should be readable")
        );
        fs::write(&path, torn).expect("torn line should be writable");

        let error = store
            .read_events(&manifest.chat_id)
            .expect_err("torn line should fail");

        let message = format!("{error}");
        assert!(
            message.contains(&path.display().to_string()) && message.contains("line 2"),
            "error should name the file and line, got: {message}"
        );
    }
}
