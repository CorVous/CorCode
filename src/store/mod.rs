//! Chat persistence over the two-tree dataset layout (ADR-0006).

mod error;
mod events;
mod liveness;
mod manifest;

use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::de::DeserializeOwned;
use serde_json::Value;
use ulid::Ulid;

use manifest::SchemaTag;

pub use error::StoreError;
pub use events::Event;
pub use liveness::{ContainerLiveness, RuntimeStatus, runtime_status};
pub use manifest::{ChatState, MANIFEST_SCHEMA, Manifest, NewChat};

const CHATS_DIR: &str = "chats";
const INCOMING_DIR: &str = ".incoming";
const WORKSPACES_DIR: &str = "workspaces";
const CLAUDE_DIR: &str = "claude";
const MANIFEST_FILE: &str = "manifest.json";
const EVENTS_FILE: &str = "events.jsonl";

/// Who a tree belongs to, in the numbers a bind mount carries into a container
/// unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owner {
    pub uid: u32,
    pub gid: u32,
}

impl Owner {
    /// The workspace image's `agent` user (ADR-0001), spelled numerically.
    ///
    /// Who the plane runs a container as, enforcing non-root whatever image it
    /// is handed — and so who every tree handed to that container must belong
    /// to.
    pub const AGENT: Self = Self {
        uid: 1000,
        gid: 1000,
    };

    /// Whoever owns `path` as it stands — for a core that is not root, and so
    /// can hand a tree to nobody but itself.
    #[cfg(unix)]
    pub fn of(path: &Path) -> Result<Self, StoreError> {
        use std::os::unix::fs::MetadataExt as _;

        let entry = fs::symlink_metadata(path).map_err(StoreError::reading(path))?;
        Ok(Self {
            uid: entry.uid(),
            gid: entry.gid(),
        })
    }
}

impl fmt::Display for Owner {
    /// As docker spells an owner on the command line and in its API.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.uid, self.gid)
    }
}

/// Reader and writer of every chat under one dataset root.
pub struct ChatStore {
    root: PathBuf,
    host_root: PathBuf,
    agent: Owner,
}

impl ChatStore {
    /// Serve a dataset root the core and the daemon call by the same name.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            host_root: root.clone(),
            root,
            agent: Owner::AGENT,
        }
    }

    /// Serve a dataset the core reads through `root` while the sibling daemon
    /// only knows it as `host_root` (ADR-0001).
    pub fn mounted(root: impl Into<PathBuf>, host_root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            host_root: host_root.into(),
            agent: Owner::AGENT,
        }
    }

    /// Serve the same dataset for an agent that is not the image's own — a
    /// core that cannot give trees away can only hand them to itself.
    #[must_use]
    pub fn handing_trees_to(self, agent: Owner) -> Self {
        Self { agent, ..self }
    }

    /// Who the trees this store lays down are handed to.
    #[must_use]
    pub const fn agent(&self) -> Owner {
        self.agent
    }

    /// Durable home of a chat: manifest, event log, agent memory.
    #[must_use]
    pub fn chat_dir(&self, chat_id: &str) -> PathBuf {
        chat_dir_under(&self.root, chat_id)
    }

    /// Agent memory, mounted as `CLAUDE_CONFIG_DIR` (ADR-0004).
    #[must_use]
    pub fn claude_dir(&self, chat_id: &str) -> PathBuf {
        claude_dir_under(&self.root, chat_id)
    }

    /// Working tree of an open chat; exists iff the chat is open (ADR-0002).
    #[must_use]
    pub fn workspace_dir(&self, chat_id: &str) -> PathBuf {
        workspace_dir_under(&self.root, chat_id)
    }

    /// The agent memory to bind into a container, named as the daemon knows it.
    #[must_use]
    pub fn host_claude_dir(&self, chat_id: &str) -> PathBuf {
        claude_dir_under(&self.host_root, chat_id)
    }

    /// The working tree to bind into a container, named as the daemon knows it.
    #[must_use]
    pub fn host_workspace_dir(&self, chat_id: &str) -> PathBuf {
        workspace_dir_under(&self.host_root, chat_id)
    }

    /// Make the dataset ready to hold chats. The root itself is never
    /// created: a missing root means the dataset is not mounted, and papering
    /// over that would strand every chat (ADR-0007 rule 5).
    pub fn prepare(&self) -> Result<(), StoreError> {
        let chats = self.root.join(CHATS_DIR);
        match fs::create_dir(&chats) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(StoreError::writing(&chats)(error)),
        }
    }

    /// Lay down both trees for a new chat, publishing the chat dir whole. Both
    /// are mounted into the chat's container, so both are handed to the agent
    /// as they are made.
    pub fn create_chat(&self, new_chat: NewChat) -> Result<Manifest, StoreError> {
        let manifest = Manifest::open(new_chat);
        let chat_id = &manifest.chat_id;
        let workspace = self.workspace_dir(chat_id);
        fs::create_dir_all(&workspace).map_err(StoreError::writing(&workspace))?;
        hand_tree_to(&workspace, self.agent)?;
        let staged = self.staging_dir(chat_id);
        let claude = staged.join(CLAUDE_DIR);
        fs::create_dir_all(&claude).map_err(StoreError::writing(&claude))?;
        hand_tree_to(&claude, self.agent)?;
        let events = staged.join(EVENTS_FILE);
        File::create_new(&events).map_err(StoreError::writing(&events))?;
        write_manifest_in(&staged, &manifest)?;
        let chat_dir = self.chat_dir(chat_id);
        fs::rename(&staged, &chat_dir).map_err(StoreError::writing(&chat_dir))?;
        Ok(manifest)
    }

    /// Every chat on disk, most recently active first.
    pub fn scan(&self) -> Result<Vec<Manifest>, StoreError> {
        let chats = self.root.join(CHATS_DIR);
        let mut manifests = Vec::new();
        for entry in fs::read_dir(&chats).map_err(StoreError::reading(&chats))? {
            let entry = entry.map_err(StoreError::reading(&chats))?;
            if is_hidden(&entry.file_name()) {
                continue;
            }
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
        write_manifest_in(&self.chat_dir(&manifest.chat_id), manifest)
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

    /// Every chat id `workspaces/` holds a working tree for. A dataset that
    /// has never held a chat holds none, which is not a fault.
    pub fn workspace_ids(&self) -> Result<Vec<String>, StoreError> {
        let workspaces = self.root.join(WORKSPACES_DIR);
        let listing = match fs::read_dir(&workspaces) {
            Ok(listing) => listing,
            Err(failure) if failure.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(failure) => return Err(StoreError::reading(&workspaces)(failure)),
        };
        let mut chat_ids = Vec::new();
        for entry in listing {
            let name = entry.map_err(StoreError::reading(&workspaces))?.file_name();
            if !is_hidden(&name) {
                chat_ids.push(name.to_string_lossy().into_owned());
            }
        }
        Ok(chat_ids)
    }

    /// Delete a chat's working tree, once everything in it is on the remote
    /// (ADR-0002 rule 1). A tree that is already gone is the state asked for.
    pub fn remove_workspace(&self, chat_id: &str) -> Result<(), StoreError> {
        let workspace = self.workspace_dir(chat_id);
        match fs::remove_dir_all(&workspace) {
            Err(failure) if failure.kind() != io::ErrorKind::NotFound => {
                Err(StoreError::writing(&workspace)(failure))
            }
            _ => Ok(()),
        }
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

    /// Where a chat is assembled before it is published under its own id.
    fn staging_dir(&self, chat_id: &str) -> PathBuf {
        self.root.join(CHATS_DIR).join(INCOMING_DIR).join(chat_id)
    }

    fn manifest_path(&self, chat_id: &str) -> PathBuf {
        self.chat_dir(chat_id).join(MANIFEST_FILE)
    }

    fn events_path(&self, chat_id: &str) -> PathBuf {
        self.chat_dir(chat_id).join(EVENTS_FILE)
    }
}

/// Give `root` and every entry beneath it to `owner` (ADR-0001).
///
/// Whatever the core clones or creates it owns itself, and a tree the agent
/// does not own is one it can read and change nothing in: no commit, no
/// session state, no work at all.
pub fn hand_tree_to(root: &Path, owner: Owner) -> Result<(), StoreError> {
    for entry in tree_at(root)? {
        change_owner(&entry, owner).map_err(StoreError::writing(&entry))?;
    }
    Ok(())
}

/// `root` and every entry beneath it, links named but never walked through:
/// what a link points at is another tree, and whose that is is not this tree's
/// to say.
fn tree_at(root: &Path) -> Result<Vec<PathBuf>, StoreError> {
    let mut found = Vec::new();
    let mut unwalked = vec![root.to_owned()];
    while let Some(path) = unwalked.pop() {
        let entry = fs::symlink_metadata(&path).map_err(StoreError::reading(&path))?;
        if entry.is_dir() {
            for below in fs::read_dir(&path).map_err(StoreError::reading(&path))? {
                unwalked.push(below.map_err(StoreError::reading(&path))?.path());
            }
        }
        found.push(path);
    }
    Ok(found)
}

/// Change one entry's owner without following it: a link is handed over
/// itself, never the file it points at.
#[cfg(unix)]
fn change_owner(entry: &Path, owner: Owner) -> io::Result<()> {
    std::os::unix::fs::lchown(entry, Some(owner.uid), Some(owner.gid))
}

/// Nowhere but unix has an owner to change. This core is deployed as a Linux
/// container (ADR-0001); everywhere else only builds it.
#[cfg(not(unix))]
fn change_owner(_entry: &Path, _owner: Owner) -> io::Result<()> {
    Ok(())
}

fn chat_dir_under(root: &Path, chat_id: &str) -> PathBuf {
    root.join(CHATS_DIR).join(chat_id)
}

fn claude_dir_under(root: &Path, chat_id: &str) -> PathBuf {
    chat_dir_under(root, chat_id).join(CLAUDE_DIR)
}

fn workspace_dir_under(root: &Path, chat_id: &str) -> PathBuf {
    root.join(WORKSPACES_DIR).join(chat_id)
}

/// Publish a manifest into `chat_dir`, replacing any older one wholesale.
fn write_manifest_in(chat_dir: &Path, manifest: &Manifest) -> Result<(), StoreError> {
    let temp = chat_dir.join(format!("{MANIFEST_FILE}.{}.tmp", Ulid::generate()));
    let json = serde_json::to_string_pretty(manifest).expect("manifest should serialize");
    let mut file = File::create(&temp).map_err(StoreError::writing(&temp))?;
    writeln!(file, "{json}").map_err(StoreError::writing(&temp))?;
    file.sync_all().map_err(StoreError::writing(&temp))?;
    let path = chat_dir.join(MANIFEST_FILE);
    fs::rename(&temp, &path).map_err(StoreError::writing(&path))
}

/// Dot-prefixed entries under `chats/` are the store's own scratch space,
/// never chats.
fn is_hidden(entry_name: &OsStr) -> bool {
    entry_name.to_string_lossy().starts_with('.')
}

/// Read one manifest, refusing anything this build does not understand
/// (ADR-0007 rule 5: no auto-repair, no skipping).
fn read_manifest_at(path: &Path) -> Result<Manifest, StoreError> {
    let json = fs::read_to_string(path).map_err(StoreError::reading(path))?;
    let tag: SchemaTag = parse_manifest_json(path, &json)?;
    if tag.schema != MANIFEST_SCHEMA {
        return Err(StoreError::ManifestSchema {
            path: path.to_owned(),
            schema: tag.schema,
        });
    }
    let manifest: Manifest = parse_manifest_json(path, &json)?;
    if OsStr::new(manifest.chat_id.as_str()) == owning_dir_name(path) {
        Ok(manifest)
    } else {
        Err(StoreError::ChatIdMismatch {
            path: path.to_owned(),
            chat_id: manifest.chat_id,
        })
    }
}

fn parse_manifest_json<T: DeserializeOwned>(path: &Path, json: &str) -> Result<T, StoreError> {
    serde_json::from_str(json).map_err(|source| StoreError::Manifest {
        path: path.to_owned(),
        source,
    })
}

/// Name of the chat dir a file sits in — the second witness to a chat's id.
fn owning_dir_name(path: &Path) -> &OsStr {
    path.parent().and_then(Path::file_name).unwrap_or_default()
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

        fs::metadata(path).expect("manifest should exist").ino()
    }

    /// Somebody no test process is, so that handing them a tree is refused
    /// wherever the suite runs unprivileged.
    #[cfg(unix)]
    const A_STRANGER: Owner = Owner {
        uid: 424_242,
        gid: 424_243,
    };

    /// The owner this process writes as, read off something it wrote: the one
    /// owner a test is allowed to hand a tree to.
    #[cfg(unix)]
    fn ours(path: &Path) -> Owner {
        Owner::of(path).expect("what we wrote should be there")
    }

    /// A clone the core wrote as itself: nested dirs, files, and a link
    /// pointing out of the tree the way a checked-out repository may.
    #[cfg(unix)]
    fn tree_with_a_link_to(elsewhere: &Path, root: &Path) -> PathBuf {
        let tree = root.join("workspace");
        fs::create_dir_all(tree.join("nested").join("deeper")).expect("a tree should be writable");
        fs::write(tree.join("top.txt"), "").expect("a file should be writable");
        fs::write(tree.join("nested").join("deeper").join("bottom.txt"), "")
            .expect("a file should be writable");
        std::os::unix::fs::symlink(elsewhere, tree.join("elsewhere"))
            .expect("a link should be creatable");
        tree
    }

    /// Every entry has to change hands, and none of them is the way to
    /// somebody else's tree.
    #[cfg(unix)]
    #[test]
    fn a_tree_is_walked_to_every_entry_and_never_through_a_link_out_of_it() {
        let root = TempDir::new().expect("temp dir should be created");
        let outside = root.path().join("outside");
        fs::create_dir_all(outside.join("theirs")).expect("a tree should be writable");
        let tree = tree_with_a_link_to(&outside, root.path());

        let mut walked = tree_at(&tree).expect("a tree should walk");

        walked.sort();
        assert_eq!(
            walked,
            [
                tree.clone(),
                tree.join("elsewhere"),
                tree.join("nested"),
                tree.join("nested").join("deeper"),
                tree.join("nested").join("deeper").join("bottom.txt"),
                tree.join("top.txt"),
            ]
        );
    }

    /// A link out of the tree is handed over as the link it is: what it points
    /// at belongs to whoever owns it, down to the setuid bit a handover would
    /// strip off it on the way past.
    #[cfg(unix)]
    #[test]
    fn a_link_out_of_the_tree_changes_hands_and_what_it_points_at_does_not() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = TempDir::new().expect("temp dir should be created");
        let theirs = root.path().join("theirs");
        fs::write(&theirs, "").expect("a file should be writable");
        fs::set_permissions(&theirs, fs::Permissions::from_mode(0o4755))
            .expect("a file should be setuid");
        let tree = tree_with_a_link_to(&theirs, root.path());

        hand_tree_to(&tree, ours(&tree)).expect("a tree with a link out of it should change hands");

        assert_eq!(
            fs::read_link(tree.join("elsewhere")).expect("the link should still be a link"),
            theirs,
            "the link was replaced rather than handed over"
        );
        assert_eq!(
            fs::symlink_metadata(&theirs)
                .expect("what the link points at should still be there")
                .permissions()
                .mode()
                & 0o7777,
            0o4755,
            "the handover followed the link out of the tree and stripped what it found"
        );
    }

    /// Half a tree in the agent's hands is a workspace it cannot work in, so
    /// the caller hears which entry stopped the handover.
    #[cfg(unix)]
    #[test]
    fn a_tree_that_cannot_be_walked_names_what_stopped_it() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = TempDir::new().expect("temp dir should be created");
        let tree = tree_with_a_link_to(root.path(), root.path());
        let sealed = tree.join("nested");
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o000))
            .expect("a dir should be sealable");
        let us = ours(&tree);

        let error = hand_tree_to(&tree, us).expect_err("a sealed dir should stop the handover");

        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o700))
            .expect("a dir should be unsealable");
        assert!(
            format!("{error}").contains(&sealed.display().to_string()),
            "error should name the entry it stopped on, got: {error}"
        );
    }

    fn new_chat(title: &str) -> NewChat {
        NewChat {
            title: title.to_owned(),
            repo: "CorVous/CorCode".to_owned(),
            branch: format!("chat/{title}"),
            base_branch: "main".to_owned(),
        }
    }

    /// Both of a chat's trees are mounted into its container, so both leave
    /// the store belonging to whoever that container runs as.
    #[cfg(unix)]
    #[test]
    fn both_trees_of_a_new_chat_belong_to_the_agent_the_store_serves() {
        let root = TempDir::new().expect("temp dataset root should be created");
        let us = ours(root.path());
        let store = ChatStore::new(root.path()).handing_trees_to(us);
        store.prepare().expect("an existing root should prepare");

        let chat_id = store
            .create_chat(new_chat("owned"))
            .expect("a chat should be created")
            .chat_id;

        assert_eq!(ours(&store.workspace_dir(&chat_id)), us, "workspace");
        assert_eq!(ours(&store.claude_dir(&chat_id)), us, "claude dir");
    }

    /// A tree the agent does not own is one it can read and change nothing in,
    /// so a chat whose trees will not change hands is no chat: the operator
    /// hears which tree stopped it and the dataset holds nothing new.
    #[cfg(unix)]
    #[test]
    fn a_chat_whose_trees_will_not_change_hands_is_never_published() {
        let root = TempDir::new().expect("temp dataset root should be created");
        if ours(root.path()).uid == 0 {
            eprintln!("SKIPPED a_chat_whose_trees_will_not_change_hands_is_never_published: root");
            return;
        }
        let store = ChatStore::new(root.path()).handing_trees_to(A_STRANGER);
        store.prepare().expect("an existing root should prepare");

        let error = store
            .create_chat(new_chat("stranded"))
            .expect_err("a chat nobody can work in should not be created");

        assert!(
            format!("{error}").contains(&root.path().join(WORKSPACES_DIR).display().to_string()),
            "error should name the tree that would not change hands, got: {error}"
        );
        assert!(
            store.scan().expect("the dataset should scan").is_empty(),
            "a chat that cannot be worked in was published anyway"
        );
    }

    #[test]
    fn preparing_an_existing_dataset_lays_down_the_chats_dir_idempotently() {
        let (root, store) = store();

        store.prepare().expect("an existing root should prepare");
        store.prepare().expect("preparing twice should be harmless");

        assert!(root.path().join(CHATS_DIR).is_dir());
        assert!(
            store
                .scan()
                .expect("an empty dataset should scan")
                .is_empty()
        );
    }

    #[test]
    fn preparing_a_dataset_that_is_not_mounted_fails_loudly() {
        let root = TempDir::new().expect("temp dir should be created");
        let unmounted = root.path().join("unmounted");
        let store = ChatStore::new(&unmounted);

        let error = store
            .prepare()
            .expect_err("a missing dataset root should fail");

        assert!(
            format!("{error}").contains(&unmounted.join(CHATS_DIR).display().to_string()),
            "error should name the dir it could not create, got: {error}"
        );
        assert!(!unmounted.exists(), "the dataset root was conjured up");
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
    fn create_chat_leaves_no_half_built_chat_behind() {
        let (_root, store) = store();

        let manifest = store
            .create_chat(new_chat("staged"))
            .expect("chat should be created");

        let staging = store.staging_dir(&manifest.chat_id);
        assert!(!staging.exists(), "staged chat should have been moved");
        let residue: Vec<PathBuf> = fs::read_dir(
            staging
                .parent()
                .expect("staging dir should sit under the staging area"),
        )
        .expect("staging area should be readable")
        .map(|entry| entry.expect("entry should be readable").path())
        .collect();
        assert!(residue.is_empty(), "staging area holds: {residue:?}");
    }

    #[test]
    fn scan_ignores_the_staging_area() {
        let (_root, store) = store();
        store
            .create_chat(new_chat("published"))
            .expect("chat should be created");
        let crashed = store.staging_dir("01KZCRASHEDMIDCREATE00000");
        fs::create_dir_all(&crashed).expect("staging leftovers should be creatable");

        let chats = store.scan().expect("scan should ignore the staging area");

        assert_eq!(chats.len(), 1);
    }

    #[test]
    fn scan_still_fails_on_a_visible_directory_without_a_manifest() {
        let (_root, store) = store();
        store
            .create_chat(new_chat("published"))
            .expect("chat should be created");
        let intruder = store.chat_dir("not-a-chat");
        fs::create_dir_all(&intruder).expect("intruder dir should be creatable");

        let error = store
            .scan()
            .expect_err("a manifest-less chat dir should fail");

        assert!(
            format!("{error}").contains(&intruder.join(MANIFEST_FILE).display().to_string()),
            "error should name the missing manifest, got: {error}"
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

    /// A containerised core reads the dataset through a mount of its own,
    /// but the sibling daemon it asks for containers only knows the host's
    /// name for the same directories (ADR-0001).
    #[test]
    fn a_mounted_dataset_reads_through_its_own_path_and_binds_the_hosts() {
        let store = ChatStore::mounted("/data", "/mnt/tank/corcode");

        assert_eq!(
            store.workspace_dir("01K1CHAT"),
            Path::new("/data/workspaces/01K1CHAT")
        );
        assert_eq!(
            store.claude_dir("01K1CHAT"),
            Path::new("/data/chats/01K1CHAT/claude")
        );
        assert_eq!(
            store.host_workspace_dir("01K1CHAT"),
            Path::new("/mnt/tank/corcode/workspaces/01K1CHAT")
        );
        assert_eq!(
            store.host_claude_dir("01K1CHAT"),
            Path::new("/mnt/tank/corcode/chats/01K1CHAT/claude")
        );
    }

    #[test]
    fn a_dataset_the_core_reads_at_its_own_name_binds_that_same_name() {
        let store = ChatStore::new("/mnt/tank/corcode");

        assert_eq!(
            store.host_workspace_dir("01K1CHAT"),
            store.workspace_dir("01K1CHAT")
        );
        assert_eq!(
            store.host_claude_dir("01K1CHAT"),
            store.claude_dir("01K1CHAT")
        );
    }

    #[test]
    fn the_working_trees_on_disk_are_listed_for_the_sweep_to_read() {
        let (_root, store) = store();
        let manifest = store
            .create_chat(new_chat("open"))
            .expect("chat should be created");

        assert_eq!(
            store.workspace_ids().expect("workspaces should list"),
            [manifest.chat_id]
        );
    }

    #[test]
    fn a_dataset_that_has_held_no_chat_yet_holds_no_working_trees() {
        let (_root, store) = store();

        assert!(
            store
                .workspace_ids()
                .expect("a dataset with no workspaces dir should still list")
                .is_empty()
        );
    }

    #[test]
    fn a_working_tree_is_removed_whole_and_removing_it_twice_is_harmless() {
        let (_root, store) = store();
        let manifest = store
            .create_chat(new_chat("archived"))
            .expect("chat should be created");
        let workspace = store.workspace_dir(&manifest.chat_id);
        fs::write(workspace.join("README.md"), "fixture").expect("a file should be writable");

        store
            .remove_workspace(&manifest.chat_id)
            .expect("a working tree should be removable");
        store
            .remove_workspace(&manifest.chat_id)
            .expect("removing what is already gone is the state asked for");

        assert!(!workspace.exists());
        assert!(
            store.chat_dir(&manifest.chat_id).is_dir(),
            "the chat's durable half went with its working tree"
        );
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

    /// The checkpoint branch was added to schema 1 after chats had already
    /// been written: a manifest without it is an older chat, not damage
    /// (ADR-0006 amendment).
    #[test]
    fn a_manifest_written_before_checkpoint_branches_reads_as_having_none() {
        let (_root, store) = store();
        let manifest = store
            .create_chat(new_chat("older"))
            .expect("chat should be created");
        let path = store.manifest_path(&manifest.chat_id);
        let mut fields: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("manifest should be readable"))
                .expect("manifest should be json");
        assert!(
            fields
                .as_object_mut()
                .expect("a manifest is an object")
                .remove("checkpoint_branch")
                .is_some(),
            "a manifest this build writes should carry the field"
        );
        fs::write(&path, fields.to_string()).expect("manifest should be rewritable");

        let older = store
            .read_manifest(&manifest.chat_id)
            .expect("an older manifest should still read");

        assert_eq!(older.checkpoint_branch, None);
        assert_eq!(older.schema, MANIFEST_SCHEMA);
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
        fields["mood"] = json!("a field this build has never heard of");
        fs::write(&path, fields.to_string()).expect("manifest should be rewritable");

        let error = store
            .read_manifest(&manifest.chat_id)
            .expect_err("unknown schema should fail");

        assert!(
            matches!(&error, StoreError::ManifestSchema { path: named, schema: 2 } if named == &path),
            "version skew should read as version skew, got: {error}"
        );
    }

    #[test]
    fn scan_rejects_a_manifest_that_claims_another_chats_id() {
        let (_root, store) = store();
        let manifest = store
            .create_chat(new_chat("original"))
            .expect("chat should be created");
        let copy = store.chat_dir("01KZCOPYOFANOTHERCHATSDIR");
        fs::create_dir_all(&copy).expect("copied chat dir should be creatable");
        let copied_manifest = copy.join(MANIFEST_FILE);
        fs::copy(store.manifest_path(&manifest.chat_id), &copied_manifest)
            .expect("manifest should be copyable");

        let error = store.scan().expect_err("a copied chat dir should fail");

        let message = format!("{error}");
        assert!(
            message.contains(&copied_manifest.display().to_string())
                && message.contains(&manifest.chat_id),
            "error should name the file and the id it claims, got: {message}"
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
            json!({"sessionId": "s", "prompt": [{"type": "text", "text": "hello"}]}),
            json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "hi"},
            }),
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
            .append_event(
                &manifest.chat_id,
                &json!({"sessionUpdate": "plan", "entries": []}),
            )
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
