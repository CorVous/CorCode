//! Integration tests for ADR-0007's resume flow: the first prompt into a chat
//! with no live connection climbs the reconnect ladder, and one into an
//! archived chat brings its workspace back from the remote.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};
use tempfile::TempDir;
use ulid::Ulid;

use cor_code::acp::ScriptedAdapter;
use cor_code::chats::{Chats, WantedChat};
use cor_code::config::{Config, DEFAULT_CONTAINER_CPUS, DEFAULT_CONTAINER_MEMORY_MB};
use cor_code::git::Remotes;
use cor_code::plane::MemoryPlane;
use cor_code::store::{ChatStore, RuntimeStatus};
use cor_code::ui;

const REPO: &str = "CorVous/fixture";
const BARE: &str = "CorVous/fixture.git";

/// The session the adapter opens whenever it is asked for a new one.
const SESSION: &str = "3f2b1c4d-0000-4000-8000-000000000001";

/// The session a chat remembers from before whatever was holding it went
/// away, which is the one the ladder asks about.
const FORGOTTEN: &str = "3f2b1c4d-0000-4000-8000-00000000dead";

const SAID: &str = "ship the ladder";

const RESUME_SESSION: &str = "unstable_resumeSession";
const LOAD_SESSION: &str = "session/load";

#[tokio::test]
async fn a_prompt_into_a_parked_chat_spins_it_up_and_resumes_the_session_it_had() {
    let dataset = Dataset::of(ScriptedAdapter::resuming(
        SESSION,
        &[update(FORGOTTEN, "on it")],
    ));
    let chat = dataset.create("parked").await;
    dataset.forget_the_session(&chat);
    dataset.park(&chat).await;

    dataset
        .prompt(&chat, SAID)
        .await
        .expect("a parked chat should wake");

    assert_eq!(dataset.status(&chat).await, RuntimeStatus::Live);
    assert_eq!(dataset.asked_to(RESUME_SESSION), [FORGOTTEN]);
    assert!(
        dataset.asked_to(LOAD_SESSION).is_empty(),
        "the ladder went on climbing after a rung that worked"
    );
    assert_eq!(
        dataset.events(&chat),
        [prompt_of(FORGOTTEN, SAID), recorded("on it")],
        "a resumed session should cost the chat's log nothing"
    );
    assert_eq!(dataset.manifest(&chat)["acp_session_id"], FORGOTTEN);
}

/// The replay is the whole point of rung 2 and the whole risk of it: it
/// rebuilds agent memory and must land nowhere the operator reads
/// (ADR-0007 rule 3).
#[tokio::test]
async fn an_adapter_that_can_only_load_replays_the_transcript_into_nothing_on_disk() {
    let dataset = Dataset::of(ScriptedAdapter::loading(
        SESSION,
        &[
            update(FORGOTTEN, "said before the core restarted"),
            update(FORGOTTEN, "and before that"),
        ],
        &[update(FORGOTTEN, "on it")],
    ));
    let chat = dataset.create("loaded").await;
    dataset.forget_the_session(&chat);
    dataset.park(&chat).await;

    dataset
        .prompt(&chat, SAID)
        .await
        .expect("a parked chat should wake");

    assert_eq!(dataset.asked_to(RESUME_SESSION), [FORGOTTEN]);
    assert_eq!(dataset.asked_to(LOAD_SESSION), [FORGOTTEN]);
    assert_eq!(
        dataset.events(&chat),
        [prompt_of(FORGOTTEN, SAID), recorded("on it")],
        "the replay was written into the log the operator reads"
    );
    assert_eq!(dataset.manifest(&chat)["acp_session_id"], FORGOTTEN);
}

#[tokio::test]
async fn an_adapter_that_remembers_nothing_opens_a_new_session_and_the_chat_is_told() {
    let dataset = Dataset::of(ScriptedAdapter::answering(
        SESSION,
        &[update(SESSION, "on it")],
    ));
    let chat = dataset.create("forgotten").await;
    dataset.forget_the_session(&chat);
    dataset.park(&chat).await;

    dataset
        .prompt(&chat, SAID)
        .await
        .expect("a parked chat should wake");

    assert_eq!(dataset.asked_to(RESUME_SESSION), [FORGOTTEN]);
    assert_eq!(dataset.asked_to(LOAD_SESSION), [FORGOTTEN]);
    assert_eq!(
        dataset.manifest(&chat)["acp_session_id"],
        SESSION,
        "the session the chat will be prompted over was not written down"
    );
    let events = dataset.events(&chat);
    assert_eq!(events[0]["corcode"], "reset_notice");
    assert!(
        events[0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("memory")),
        "the chat is not told its agent came back empty: {events:?}"
    );
    assert_eq!(
        events[1..],
        [prompt_of(SESSION, SAID), recorded("on it")],
        "the new session's turn is not the whole of what followed the notice"
    );
    assert!(
        dataset.rendered_log(&chat).contains("<blockquote>"),
        "the notice does not read as one: {}",
        dataset.rendered_log(&chat)
    );
}

#[tokio::test]
async fn a_prompt_into_an_archived_chat_clones_it_back_and_says_what_the_clone_cost() {
    let dataset = Dataset::of(ScriptedAdapter::resuming(
        SESSION,
        &[update(SESSION, "on it")],
    ));
    let chat = dataset.create("archived").await;
    let pushed = dataset.commit_in_workspace(&chat, "the agent's own commit");
    dataset.archive(&chat).await;

    dataset
        .prompt(&chat, SAID)
        .await
        .expect("an archived chat should revive");

    assert_eq!(dataset.status(&chat).await, RuntimeStatus::Live);
    assert_eq!(dataset.manifest(&chat)["state"], "open");
    assert_eq!(
        says(&dataset.workspace(&chat), &["rev-parse", "HEAD"]),
        pushed,
        "the revived workspace does not stand where the chat was archived"
    );
    assert!(dataset.workspace(&chat).join("third.txt").is_file());
    let events = dataset.events(&chat);
    let notice = events[0]["text"].as_str().unwrap_or_default();
    assert_eq!(events[0]["corcode"], "reset_notice");
    assert!(
        notice.contains(&format!("{}@{pushed}", dataset.branch(&chat)))
            && notice.contains("untracked"),
        "the notice does not say where the workspace came back or what it lost: {notice}"
    );
    assert_eq!(events[1..], [prompt_of(SESSION, SAID), recorded("on it")]);
    assert!(dataset.rendered_log(&chat).contains("<blockquote>"));
}

#[tokio::test]
async fn an_archived_chat_whose_commit_the_branch_lost_comes_back_at_the_tip_and_says_so() {
    let dataset = Dataset::of(ScriptedAdapter::resuming(
        SESSION,
        &[update(SESSION, "on it")],
    ));
    let chat = dataset.create("drifted").await;
    let pushed = dataset.commit_in_workspace(&chat, "the agent's own commit");
    dataset.archive(&chat).await;
    let tip = dataset.origin_says(&["rev-parse", "main"]);
    dataset.move_the_branch(&chat, &tip);

    dataset
        .prompt(&chat, SAID)
        .await
        .expect("a drifted chat should still revive");

    assert_eq!(
        says(&dataset.workspace(&chat), &["rev-parse", "HEAD"]),
        tip,
        "the revival ignored the remote it was given"
    );
    let notice = dataset.events(&chat)[0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        notice.contains(&pushed) && notice.contains("tip"),
        "the chat is not told the commit it pushed is gone: {notice}"
    );
}

#[tokio::test]
async fn an_archived_chat_whose_branch_is_gone_stays_archived_and_can_be_tried_again() {
    let dataset = Dataset::of(ScriptedAdapter::resuming(
        SESSION,
        &[update(SESSION, "on it")],
    ));
    let chat = dataset.create("deleted").await;
    let pushed = dataset.commit_in_workspace(&chat, "the agent's own commit");
    dataset.archive(&chat).await;
    dataset.delete_the_branch(&chat);

    let failure = dataset
        .prompt(&chat, SAID)
        .await
        .expect_err("a chat whose branch is gone cannot be revived");

    assert_eq!(dataset.manifest(&chat)["state"], "archived");
    assert_eq!(dataset.status(&chat).await, RuntimeStatus::Archived);
    assert!(
        !dataset.workspace(&chat).exists(),
        "a revival that failed left a workspace behind: {failure}"
    );
    let told = dataset.events(&chat);
    assert!(
        told.last()
            .and_then(|line| line["text"].as_str())
            .is_some_and(|text| text.contains(&dataset.branch(&chat))),
        "the operator is never told which branch is missing: {told:?}"
    );

    dataset.move_the_branch(&chat, &pushed);
    dataset
        .prompt(&chat, SAID)
        .await
        .expect("the branch is back, so the chat should revive");
    assert_eq!(dataset.manifest(&chat)["state"], "open");
}

/// A workspace that is not there can mean a dataset that is not mounted, so
/// nothing is rebuilt and nothing is started (ADR-0007 rule 5).
#[tokio::test]
async fn a_parked_chat_whose_workspace_is_gone_fails_loudly_and_touches_nothing() {
    let dataset = Dataset::of(ScriptedAdapter::resuming(
        SESSION,
        &[update(FORGOTTEN, "on it")],
    ));
    let chat = dataset.create("unmounted").await;
    dataset.park(&chat).await;
    fs::remove_dir_all(dataset.workspace(&chat)).expect("the workspace should be removable");

    dataset
        .prompt(&chat, SAID)
        .await
        .expect_err("a chat with no workspace cannot be woken");

    assert!(
        !dataset.workspace(&chat).exists(),
        "a missing workspace was conjured up"
    );
    assert_eq!(
        dataset.status(&chat).await,
        RuntimeStatus::Parked,
        "a container was started over a workspace that is not there"
    );
    assert!(
        dataset
            .events(&chat)
            .last()
            .and_then(|line| line["text"].as_str())
            .is_some_and(|text| text.contains("workspace")),
        "the operator is told nothing about why their prompt went nowhere"
    );
}

/// One dataset, its remote, and the fake adapter every chat in it talks to.
struct Dataset {
    chats: Chats<MemoryPlane, ScriptedAdapter>,
    adapter: ScriptedAdapter,
    data_dir: TempDir,
    origin: TempDir,
}

impl Dataset {
    fn of(adapter: ScriptedAdapter) -> Self {
        let data_dir = TempDir::new().expect("temp dir should be creatable");
        let (origin, remotes) = seeded_repository();
        let config = test_config(data_dir.path().to_path_buf());
        ChatStore::new(data_dir.path())
            .prepare()
            .expect("the dataset should prepare, as serving does");
        let chats = Chats::new(&config, MemoryPlane::default(), adapter.clone(), remotes);
        Self {
            chats,
            adapter,
            data_dir,
            origin,
        }
    }

    async fn create(&self, slug: &str) -> String {
        self.chats
            .create(WantedChat {
                repo: REPO.to_owned(),
                base_branch: "main".to_owned(),
                slug: slug.to_owned(),
                direct_on_base: false,
            })
            .await
            .expect("a chat should be cut")
    }

    async fn prompt(&self, chat_id: &str, said: &str) -> Result<(), cor_code::chats::PromptError> {
        self.chats.prompt(&ulid(chat_id), said).await
    }

    async fn archive(&self, chat_id: &str) {
        self.chats
            .archive(&ulid(chat_id))
            .await
            .expect("a clean chat should archive");
    }

    /// Push a chat out of the warm pool, which is the only way a chat comes
    /// to be parked: its container goes, its workspace stays (ADR-0002).
    async fn park(&self, chat_id: &str) {
        self.create("filler").await;
        assert_eq!(
            self.status(chat_id).await,
            RuntimeStatus::Parked,
            "the chat under test is still holding a container"
        );
    }

    async fn status(&self, chat_id: &str) -> RuntimeStatus {
        self.chats
            .survey()
            .await
            .expect("the dataset should survey")
            .into_iter()
            .find(|(manifest, _)| manifest.chat_id == chat_id)
            .map(|(_, status)| status)
            .expect("the chat should be on the console")
    }

    /// Leave the chat remembering a session its adapter does not, as a core
    /// that restarted leaves every chat it was holding.
    fn forget_the_session(&self, chat_id: &str) {
        let path = self.chat_dir(chat_id).join("manifest.json");
        let mut manifest = self.manifest(chat_id);
        manifest["acp_session_id"] = json!(FORGOTTEN);
        fs::write(&path, manifest.to_string()).expect("the manifest should be rewritable");
    }

    /// The session ids the adapter was asked about at one rung, in order.
    fn asked_to(&self, method: &str) -> Vec<String> {
        self.adapter
            .requests()
            .iter()
            .filter(|request| request["method"] == method)
            .map(|request| request["params"]["sessionId"].to_string().replace('"', ""))
            .collect()
    }

    fn workspace(&self, chat_id: &str) -> PathBuf {
        self.data_dir.path().join("workspaces").join(chat_id)
    }

    fn chat_dir(&self, chat_id: &str) -> PathBuf {
        self.data_dir.path().join("chats").join(chat_id)
    }

    fn manifest(&self, chat_id: &str) -> Value {
        let path = self.chat_dir(chat_id).join("manifest.json");
        serde_json::from_str(&fs::read_to_string(&path).expect("the manifest should be readable"))
            .expect("the manifest should be json")
    }

    fn branch(&self, chat_id: &str) -> String {
        self.manifest(chat_id)["branch"]
            .as_str()
            .expect("a chat names its branch")
            .to_owned()
    }

    /// The ACP payloads the chat's log holds, in order.
    fn events(&self, chat_id: &str) -> Vec<Value> {
        fs::read_to_string(self.chat_dir(chat_id).join("events.jsonl"))
            .expect("the event log should be readable")
            .lines()
            .map(|line| {
                let event: Value = serde_json::from_str(line).expect("a line should be json");
                event["event"].clone()
            })
            .collect()
    }

    /// The chat's log as the browser is served it.
    fn rendered_log(&self, chat_id: &str) -> String {
        let events = ChatStore::new(self.data_dir.path())
            .read_events(chat_id)
            .expect("the event log should read back");
        ui::event_log(chat_id, &events)
    }

    /// One commit in the chat's workspace, as its agent would make it,
    /// answering with the sha it landed on.
    fn commit_in_workspace(&self, chat_id: &str, message: &str) -> String {
        let workspace = self.workspace(chat_id);
        fs::write(workspace.join("third.txt"), message).expect("a file should be writable");
        for args in [
            vec!["config", "user.email", "agent@example.invalid"],
            vec!["config", "user.name", "Agent"],
            vec!["add", "."],
            vec!["commit", "-m", message],
        ] {
            says(&workspace, &args);
        }
        says(&workspace, &["rev-parse", "HEAD"])
    }

    /// Move the chat's branch on the remote, as a force push does.
    fn move_the_branch(&self, chat_id: &str, commit: &str) {
        says(
            &self.origin.path().join(BARE),
            &["update-ref", &self.branch_ref(chat_id), commit],
        );
    }

    fn delete_the_branch(&self, chat_id: &str) {
        says(
            &self.origin.path().join(BARE),
            &["update-ref", "-d", &self.branch_ref(chat_id)],
        );
    }

    fn branch_ref(&self, chat_id: &str) -> String {
        format!("refs/heads/{}", self.branch(chat_id))
    }

    fn origin_says(&self, args: &[&str]) -> String {
        says(&self.origin.path().join(BARE), args)
    }
}

fn ulid(chat_id: &str) -> Ulid {
    chat_id.parse().expect("a chat id is a ulid")
}

/// One `session/update` notification's params, as the adapter sends them.
fn update(session_id: &str, said: &str) -> Value {
    json!({
        "sessionId": session_id,
        "update": {
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": said},
        },
    })
}

/// The same update as ADR-0006 writes it into `events.jsonl`.
fn recorded(said: &str) -> Value {
    json!({
        "sessionUpdate": "agent_message_chunk",
        "content": {"type": "text", "text": said},
    })
}

/// An outbound prompt as ADR-0006 writes it down, over the session the ladder
/// left the chat on.
fn prompt_of(session_id: &str, said: &str) -> Value {
    json!({"sessionId": session_id, "prompt": [{"type": "text", "text": said}]})
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
    (dir, Remotes::new(served_from, None))
}

fn spelled(path: &Path) -> String {
    path.to_str()
        .expect("temp paths should be spellable")
        .to_owned()
}

fn says(cwd: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A dataset that keeps one container warm, so that a second chat parks the
/// first and the tests have something to wake.
fn test_config(data_dir: PathBuf) -> Config {
    Config {
        data_dir,
        bind_addr: "127.0.0.1:0".parse().expect("valid address"),
        username: "cassidy".to_owned(),
        password_hash: "$argon2id$v=19$m=8,t=1,p=1$c2FsdHNhbHRzYWx0c2FsdA$0".to_owned(),
        workspace_image: "ghcr.io/corvous/corcode-workspace:2026-08-05".to_owned(),
        container_memory_mb: DEFAULT_CONTAINER_MEMORY_MB,
        container_cpus: DEFAULT_CONTAINER_CPUS,
        warm_pool: 1,
        registry: None,
        repos: vec![REPO.to_owned()],
        github_token: None,
        anthropic_api_key: None,
    }
}
