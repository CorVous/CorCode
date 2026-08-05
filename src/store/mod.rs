//! Chat persistence over the two-tree dataset layout (ADR-0006).

mod error;
mod events;
mod liveness;
mod manifest;

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde_json::Value;
use ulid::Ulid;

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
    pub fn create_chat(&self, new_chat: NewChat) -> Result<Manifest, StoreError> {
        let manifest = Manifest::open(new_chat);
        let chat_id = &manifest.chat_id;
        for dir in [self.claude_dir(chat_id), self.workspace_dir(chat_id)] {
            fs::create_dir_all(&dir).map_err(StoreError::writing(&dir))?;
        }
        let events = self.events_path(chat_id);
        File::create_new(&events).map_err(StoreError::writing(&events))?;
        self.write_manifest(&manifest)?;
        Ok(manifest)
    }

    /// Every chat on disk, most recently active first.
    pub fn scan(&self) -> Result<Vec<Manifest>, StoreError> {
        let chats = self.root.join(CHATS_DIR);
        let mut manifests = Vec::new();
        for entry in fs::read_dir(&chats).map_err(StoreError::reading(&chats))? {
            let entry = entry.map_err(StoreError::reading(&chats))?;
            manifests.push(read_manifest_at(&entry.path().join(MANIFEST_FILE))?);
        }
        manifests.sort_by(|left, right| {
            right
                .last_active_at
                .cmp(&left.last_active_at)
                .then_with(|| left.chat_id.cmp(&right.chat_id))
        });
        Ok(manifests)
    }

    pub fn read_manifest(&self, chat_id: &str) -> Result<Manifest, StoreError> {
        read_manifest_at(&self.manifest_path(chat_id))
    }

    /// Replace a chat's manifest in one atomic step.
    pub fn write_manifest(&self, manifest: &Manifest) -> Result<(), StoreError> {
        let temp = self
            .chat_dir(&manifest.chat_id)
            .join(format!("{MANIFEST_FILE}.{}.tmp", Ulid::generate()));
        let json = serde_json::to_string_pretty(manifest).expect("manifest should serialize");
        let mut file = File::create(&temp).map_err(StoreError::writing(&temp))?;
        writeln!(file, "{json}").map_err(StoreError::writing(&temp))?;
        file.sync_all().map_err(StoreError::writing(&temp))?;
        let path = self.manifest_path(&manifest.chat_id);
        fs::rename(&temp, &path).map_err(StoreError::writing(&path))
    }

    /// Append one ACP payload to the chat's display record, flushed on return.
    pub fn append_event(&self, chat_id: &str, event: &Value) -> Result<(), StoreError> {
        let line = serde_json::to_string(&Event {
            ts: Utc::now(),
            event: event.clone(),
        })
        .expect("event should serialize");
        let path = self.events_path(chat_id);
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(StoreError::writing(&path))?;
        writeln!(file, "{line}").map_err(StoreError::writing(&path))?;
        file.flush().map_err(StoreError::writing(&path))
    }

    pub fn read_events(&self, chat_id: &str) -> Result<Vec<Event>, StoreError> {
        let path = self.events_path(chat_id);
        let log = fs::read_to_string(&path).map_err(StoreError::reading(&path))?;
        log.lines()
            .enumerate()
            .map(|(index, line)| {
                serde_json::from_str(line).map_err(|source| StoreError::Event {
                    path: path.clone(),
                    line: index + 1,
                    source,
                })
            })
            .collect()
    }

    fn manifest_path(&self, chat_id: &str) -> PathBuf {
        self.chat_dir(chat_id).join(MANIFEST_FILE)
    }

    fn events_path(&self, chat_id: &str) -> PathBuf {
        self.chat_dir(chat_id).join(EVENTS_FILE)
    }
}

/// Read one manifest, refusing anything this build does not understand
/// (ADR-0007 rule 5: no auto-repair, no skipping).
fn read_manifest_at(path: &Path) -> Result<Manifest, StoreError> {
    let json = fs::read_to_string(path).map_err(StoreError::reading(path))?;
    let manifest: Manifest =
        serde_json::from_str(&json).map_err(|source| StoreError::Manifest {
            path: path.to_owned(),
            source,
        })?;
    if manifest.schema == MANIFEST_SCHEMA {
        Ok(manifest)
    } else {
        Err(StoreError::ManifestSchema {
            path: path.to_owned(),
            schema: manifest.schema,
        })
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

    /// Identity of the file itself, not of its name: a rename publishes a
    /// different inode, an in-place truncate reuses one.
    #[cfg(unix)]
    fn inode(path: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt as _;

        fs::metadata(path)
            .expect("manifest should exist")
            .ino()
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
        assert!(
            store
                .chat_dir(&manifest.chat_id)
                .join(EVENTS_FILE)
                .is_file()
        );
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
            message.contains(&path.display().to_string()) && message.contains("schema 2"),
            "error should name the file and the schema, got: {message}"
        );
    }

    #[test]
    fn rewriting_a_manifest_replaces_it_and_leaves_no_temp_file() {
        let (_root, store) = store();
        let mut manifest = store
            .create_chat(new_chat("renamed"))
            .expect("chat should be created");
        #[cfg(unix)]
        let published = inode(&store.manifest_path(&manifest.chat_id));
        manifest.title = "Renamed".to_owned();
        manifest.acp_session_id = Some("session-uuid".to_owned());

        store
            .write_manifest(&manifest)
            .expect("manifest should be written");

        #[cfg(unix)]
        assert_ne!(
            published,
            inode(&store.manifest_path(&manifest.chat_id)),
            "a rewrite reusing the file in place is not atomic"
        );
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
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
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
