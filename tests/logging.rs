//! What level the core's own lines go out at. A deployment reads warnings and
//! nothing quieter unless it is asked to, so a site demoted to `debug!` goes
//! silent in production with every test of its wording still green (issue
//! #84). These tests drive the sites and read the level off the logger.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use log::Level;
use serde_json::{Value, json};
use tempfile::TempDir;
use ulid::Ulid;

use cor_code::acp::{AcpChannel, AcpError, AcpTransport, Adapter, OPENED_IN, ScriptedAdapter};
use cor_code::chats::{Chats, WantedChat};
use cor_code::config::{
    Config, DEFAULT_CONTAINER_CPUS, DEFAULT_CONTAINER_MEMORY_MB, DEFAULT_SCRATCH_MB,
};
use cor_code::git::Remotes;
use cor_code::logs::{LoggedLines, capturing_lines};
use cor_code::plane::MemoryPlane;
use cor_code::secrets::Secrets;
use cor_code::store::{ChatStore, Owner};

const REPO: &str = "CorVous/fixture";
const BARE: &str = "CorVous/fixture.git";
const SESSION: &str = "3f2b1c4d-0000-4000-8000-000000000001";

/// A session of its own for the pin on the open line, so that the one line
/// this suite reads back is the one that test drove.
const OPENED_SESSION: &str = "3f2b1c4d-0000-4000-8000-000000000002";

/// A mode the managed settings never ask for, which is what the adapter
/// clamps a session down to (ADR-0001).
const ASKING_MODE: &str = "default";

const SAID: &str = "ship the ladder";
const CONTAINER: &str = "corcode-chat-01K1TESTCHATID0000000000";

/// A line an adapter should never send, and does when it is wedged: the one
/// symptom of a session that has stopped speaking JSON-RPC.
const GARBLED: &str = "Error: EACCES: permission denied";

/// What the filesystem says of a read that found a directory. It is the
/// store's summary that names the file; only this says what was wrong with it.
const EISDIR: &str = "Is a directory";

/// What a daemon that is not answering says for itself, under the plane's own
/// summary of the action it refused.
const REFUSED_CONNECTION: &str = "connection refused";

/// A wedged adapter is the incident, and the operator only learns of it from
/// this line (issue #66).
#[tokio::test]
async fn a_line_that_is_not_json_rpc_reaches_the_operator_as_a_warning() {
    let logged = capturing_lines();
    let adapter = Adapter::new(Garbling);

    adapter
        .open_session(CONTAINER)
        .await
        .expect("the garbling adapter still answers every call it is sent");

    assert_eq!(
        logged.quietest_level_of("the adapter's stream carried no message"),
        Some(Level::Warn),
        "src/acp/mod.rs: a wedged adapter below WARN is a wedged adapter nobody hears about"
    );
}

/// A connection dropped mid-turn leaves no other trace on this side, so the
/// line has to be one a deployment reads (issue #66).
#[tokio::test]
async fn a_dropped_adapter_connection_reaches_the_operator_as_a_warning() {
    let logged = capturing_lines();
    let dataset = Dataset::of(ScriptedAdapter::dying_mid_turn(SESSION, &[]));
    let chat = dataset.create("dropped").await;

    dataset
        .prompt(&chat, SAID)
        .await
        .expect_err("this adapter dies mid turn");

    assert_eq!(
        logged.quietest_level_of("dropped its adapter connection"),
        Some(Level::Warn),
        "src/chats.rs: a connection lost below WARN is the silent core of issue #66"
    );
}

/// The mode a session opened in is written down nowhere else, so this line is
/// the only place an operator can read which one they are watching (issue
/// #58, ADR-0001).
#[tokio::test]
async fn the_mode_a_session_opened_in_reaches_the_operator_as_chatter() {
    let logged = capturing_lines();
    let adapter = Adapter::new(ScriptedAdapter::opening_in_mode(
        OPENED_SESSION,
        ASKING_MODE,
    ));

    adapter
        .open_session(CONTAINER)
        .await
        .expect("the scripted adapter opens a session");

    assert_eq!(
        logged.quietest_level_of(&format!("{OPENED_SESSION} {OPENED_IN}")),
        Some(Level::Info),
        "src/acp/mod.rs: a mode nobody can read under RUST_LOG=info is a mode nobody reads"
    );
}

/// A session the adapter clamped is one the operator has to know about: the
/// chat's own log says so too, but only this line reaches a deployment that
/// is watching the core rather than the chat (issue #58).
#[tokio::test]
async fn a_session_clamped_into_another_mode_reaches_the_operator_as_a_warning() {
    let logged = capturing_lines();
    let dataset = Dataset::of(ScriptedAdapter::opening_in_mode(SESSION, ASKING_MODE));

    let chat = dataset.create("clamped").await;

    assert_eq!(
        logged.quietest_level_of(&format!("{chat} opened a session in permission mode")),
        Some(Level::Warn),
        "src/chats.rs: a clamped session below WARN is a mute agent nobody was warned about"
    );
}

/// The store's own summary names the file and stops there, so the cause is
/// the whole of what says whether the operator has a permission to fix or a
/// mount to unbusy. The wording of these lines is pinned where they are
/// built; this drives the real site, which is where the causes could be
/// dropped without a test noticing (issues #41, #81).
#[tokio::test]
async fn a_turn_the_store_could_not_date_is_logged_with_what_the_filesystem_said() {
    let logged = capturing_lines();
    let dataset = Dataset::of(ScriptedAdapter::answering(SESSION, &[]));
    let chat = dataset.create("undated").await;
    dataset.stand_a_directory_where_the_manifest_was(&chat);

    dataset
        .prompt(&chat, SAID)
        .await
        .expect("a turn goes through whatever the manifest is doing");

    assert!(
        said_by_a_line(
            &logged,
            &format!("{chat} took a turn that could not be dated"),
            EISDIR
        ),
        "src/chats.rs: the store family's warning reached the operator without the cause \
         under it, which is the only part that says what to do about it"
    );
}

/// The plane's own summary names the action and stops there: what the daemon
/// said is under it, and reverting the warning to the failure alone would
/// leave the operator the summary of a summary (issue #41).
#[tokio::test]
async fn a_container_that_would_not_stop_is_logged_with_what_the_daemon_said() {
    let logged = capturing_lines();
    let plane = MemoryPlane::default();
    let dataset = Dataset::over(plane.clone(), ScriptedAdapter::answering(SESSION, &[]));
    let chat = dataset.create("stubborn").await;
    plane.refuse_to_tear_down();

    dataset.archive(&chat).await;

    assert!(
        said_by_a_line(
            &logged,
            &format!("{chat} (archived) would not stop"),
            REFUSED_CONNECTION
        ),
        "src/chats.rs: the plane family's warning reached the operator without the daemon's \
         own complaint under it"
    );
}

/// Whether any captured line saying `saying` carries `cause` too.
fn said_by_a_line(logged: &LoggedLines, saying: &str, cause: &str) -> bool {
    logged
        .lines_saying(saying)
        .iter()
        .any(|(_, line)| line.contains(cause))
}

/// An adapter that answers every call, having said something that is not
/// JSON-RPC first — as a wedged one does when its stdio carries a crash.
struct Garbling;

impl AcpTransport for Garbling {
    type Channel = GarblingChannel;

    async fn open(&self, _container: &str) -> Result<Self::Channel, AcpError> {
        Ok(GarblingChannel {
            unread: VecDeque::new(),
        })
    }
}

struct GarblingChannel {
    unread: VecDeque<String>,
}

impl AcpChannel for GarblingChannel {
    async fn send(&mut self, message: &str) -> Result<(), AcpError> {
        let request: Value = serde_json::from_str(message).expect("the client should send JSON");
        self.unread.push_back(GARBLED.to_owned());
        self.unread.push_back(
            json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": {"sessionId": SESSION},
            })
            .to_string(),
        );
        Ok(())
    }

    async fn receive(&mut self) -> Result<String, AcpError> {
        self.unread.pop_front().ok_or(AcpError::Closed)
    }
}

/// One dataset, its remote, and the fake adapter every chat in it talks to.
struct Dataset {
    chats: Chats<MemoryPlane, ScriptedAdapter>,
    data_dir: TempDir,
    _origin: TempDir,
}

impl Dataset {
    fn of(adapter: ScriptedAdapter) -> Self {
        Self::over(MemoryPlane::default(), adapter)
    }

    fn over(plane: MemoryPlane, adapter: ScriptedAdapter) -> Self {
        let data_dir = TempDir::new().expect("temp dir should be creatable");
        let (origin, remotes) = seeded_repository();
        let config = test_config(data_dir.path().to_path_buf());
        ChatStore::new(data_dir.path())
            .prepare()
            .expect("the dataset should prepare, as serving does");
        let chats = Chats::new(
            &config,
            Owner::of(&config.data_dir).expect("we own the dataset we just made"),
            plane,
            adapter,
            remotes,
            Arc::new(Secrets::from_config(&config)),
        );
        Self {
            chats,
            data_dir,
            _origin: origin,
        }
    }

    async fn create(&self, slug: &str) -> String {
        self.chats
            .create(WantedChat {
                repo: REPO.to_owned(),
                base_branch: "main".to_owned(),
                slug: slug.to_owned(),
                direct_on_base: false,
                env: std::collections::BTreeMap::new(),
                startup_script: None,
            })
            .await
            .expect("a chat should be cut")
    }

    async fn prompt(&self, chat_id: &str, said: &str) -> Result<(), cor_code::chats::PromptError> {
        let chat_id: Ulid = chat_id.parse().expect("a chat id is a ulid");
        self.chats.prompt(&chat_id, said).await
    }

    async fn archive(&self, chat_id: &str) {
        let chat_id: Ulid = chat_id.parse().expect("a chat id is a ulid");
        self.chats
            .archive(&chat_id)
            .await
            .expect("a chat whose work is on the remote should archive");
    }

    /// Leave a directory where the chat's manifest was, so that the store
    /// fails to read it for a reason the filesystem has words for.
    fn stand_a_directory_where_the_manifest_was(&self, chat_id: &str) {
        let manifest = self
            .data_dir
            .path()
            .join("chats")
            .join(chat_id)
            .join("manifest.json");
        fs::remove_file(&manifest).expect("a created chat has a manifest");
        fs::create_dir(&manifest).expect("the manifest's place should be takeable");
    }
}

fn test_config(data_dir: PathBuf) -> Config {
    Config {
        host_data_dir: data_dir.clone(),
        data_dir,
        bind_addr: "127.0.0.1:0".parse().expect("valid address"),
        username: "cassidy".to_owned(),
        password_hash: "$argon2id$v=19$m=8,t=1,p=1$c2FsdHNhbHRzYWx0c2FsdA$0".to_owned(),
        workspace_image: "ghcr.io/corvous/corcode-workspace:2026-08-05".to_owned(),
        container_memory_mb: DEFAULT_CONTAINER_MEMORY_MB,
        container_cpus: DEFAULT_CONTAINER_CPUS,
        scratch_mb: DEFAULT_SCRATCH_MB,
        warm_pool: 1,
        registry: None,
        repos: vec![REPO.to_owned()],
        github_token: None,
        anthropic_api_key: None,
    }
}

/// A bare repository with a commit on `main`, reachable over `file://` so
/// that no test needs the network.
fn seeded_repository() -> (TempDir, Remotes) {
    let dir = TempDir::new().expect("origin dir should be created");
    let bare = dir.path().join(BARE);
    let work = dir.path().join("seed");
    says(
        dir.path(),
        &["init", "--bare", "--initial-branch=main", &spelled(&bare)],
    );
    says(
        dir.path(),
        &["init", "--initial-branch=main", &spelled(&work)],
    );
    says(&work, &["config", "user.email", "seed@example.invalid"]);
    says(&work, &["config", "user.name", "Seed"]);
    fs::write(work.join("README.md"), "fixture").expect("seed file should be writable");
    says(&work, &["add", "."]);
    says(&work, &["commit", "-m", "first"]);
    says(&work, &["remote", "add", "origin", &spelled(&bare)]);
    says(&work, &["push", "origin", "main"]);
    let served_from = format!("file://{}", spelled(dir.path()));
    (dir, Remotes::new(served_from))
}

fn spelled(path: &Path) -> String {
    path.to_str()
        .expect("temp paths should be spellable")
        .to_owned()
}

fn says(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git should run");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
