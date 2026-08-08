//! Integration tests for the per-chat startup script: a shell run in the
//! container after the workspace and env are ready and before the session
//! opens, on every (re)spawn, its outcome surfaced in the transcript and never
//! blocking the chat (issue #14).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use serde_json::Value;
use tempfile::TempDir;
use ulid::Ulid;

use cor_code::acp::ScriptedAdapter;
use cor_code::chats::{Chats, CreateError, WantedChat};
use cor_code::config::{
    Config, DEFAULT_CONTAINER_CPUS, DEFAULT_CONTAINER_MEMORY_MB, DEFAULT_SCRATCH_MB,
};
use cor_code::git::Remotes;
use cor_code::plane::MemoryPlane;
use cor_code::secrets::Secrets;
use cor_code::store::{ChatStore, Owner, RuntimeStatus};

const REPO: &str = "CorVous/fixture";
const BARE: &str = "CorVous/fixture.git";
const SESSION: &str = "3f2b1c4d-0000-4000-8000-000000000001";
const KEY: &str = "sk-ant-test";
const API_KEY: &str = "ANTHROPIC_API_KEY";

/// The script's outcome reaches the transcript with a confirmation on a clean
/// exit, and the chat opens (issue #14).
#[tokio::test]
async fn a_clean_run_records_a_confirmation_and_opens_the_chat() {
    let dataset = Dataset::opening();
    dataset.plane.program_startup("all set\n", 0);

    let chat = dataset
        .create("clean", BTreeMap::new(), Some("true"))
        .await
        .expect("a clean script should not stop the chat");

    let notice = dataset.startup_notice(&chat);
    assert!(notice.contains("exit 0"), "no confirmation: {notice}");
    assert!(
        notice.contains("all set"),
        "the output was dropped: {notice}"
    );
}

/// A non-zero exit does not block the chat, and the code and output are both in
/// the transcript for the operator to act on (issue #14).
#[tokio::test]
async fn a_failing_run_still_opens_the_chat_and_records_its_code_and_output() {
    let dataset = Dataset::opening();
    dataset.plane.program_startup("boom\n", 3);

    let chat = dataset
        .create("failing", BTreeMap::new(), Some("exit 3"))
        .await
        .expect("a failing script must not stop the chat");

    let notice = dataset.startup_notice(&chat);
    assert!(notice.contains("exited 3"), "the code was lost: {notice}");
    assert!(notice.contains("boom"), "the output was lost: {notice}");
}

/// Very long output is cut so it cannot bury the chat, and the cut is marked
/// (issue #14).
#[tokio::test]
async fn very_long_output_is_truncated_in_the_transcript() {
    let dataset = Dataset::opening();
    let flood = "x".repeat(20_000);
    dataset.plane.program_startup(&flood, 0);

    let chat = dataset
        .create("flood", BTreeMap::new(), Some("yes"))
        .await
        .expect("a chatty script should not stop the chat");

    let notice = dataset.startup_notice(&chat);
    assert!(
        notice.contains("(truncated)"),
        "no truncation mark: len {}",
        notice.len()
    );
    assert!(
        notice.len() < flood.len(),
        "the whole flood was kept: {} bytes",
        notice.len()
    );
}

/// The script runs over a live container with the whole env visible — the
/// system credentials and the chat's own custom vars — which is the proof it
/// runs only after the workspace and env are ready (issue #14).
#[tokio::test]
async fn the_script_runs_over_a_live_container_with_the_env_visible() {
    let dataset = Dataset::opening();

    let chat = dataset
        .create(
            "ready",
            BTreeMap::from([("EDITOR".to_owned(), "helix".to_owned())]),
            Some("env"),
        )
        .await
        .expect("the chat should open");

    let runs = dataset.plane.startup_runs();
    let run = runs
        .iter()
        .find(|run| run.chat_id == chat)
        .expect("the chat's script should have run");
    assert!(
        run.over_a_live_container,
        "the script ran before the container was up"
    );
    assert_eq!(run.env.get(API_KEY).map(String::as_str), Some(KEY));
    assert_eq!(run.env.get("EDITOR").map(String::as_str), Some("helix"));
    assert_eq!(run.script, "env");
}

/// The session never opening does not stop the script: with an adapter that
/// refuses to open a session the chat fails to be cut, yet the script has
/// already run and been recorded — so it runs before the session opens
/// (issue #14).
#[tokio::test]
async fn the_script_runs_before_the_session_opens() {
    let dataset = Dataset::serving(ScriptedAdapter::refusing(
        "session/new",
        "no session for you",
    ));

    let failure = dataset
        .create("no-session", BTreeMap::new(), Some("echo hi"))
        .await
        .expect_err("a chat whose session never opens is not created");
    assert!(matches!(failure, CreateError::Broke(_)));

    let runs = dataset.plane.startup_runs();
    assert_eq!(
        runs.len(),
        1,
        "the script did not run though the workspace was ready"
    );
    assert_eq!(runs[0].script, "echo hi");
}

/// The script runs only after the workspace is ready: a checkout that fails
/// never spawns a container, so the script never runs (issue #14).
#[tokio::test]
async fn a_failed_checkout_never_reaches_the_script() {
    let dataset = Dataset::opening();

    let failure = dataset
        .create_on_base("no-clone", "no-such-branch", Some("echo hi"))
        .await
        .expect_err("a checkout of a missing branch fails");
    assert!(matches!(failure, CreateError::Broke(_)));

    assert!(
        dataset.plane.startup_runs().is_empty(),
        "the script ran though the workspace never cloned"
    );
}

/// Containers are ephemeral, so the script re-runs whenever a parked chat is
/// spun back up, not only on first creation (issue #14).
#[tokio::test]
async fn the_script_re_runs_when_a_parked_chat_is_spun_back_up() {
    let dataset = Dataset::serving(ScriptedAdapter::resuming(SESSION, &[]));

    let chat = dataset
        .create("revived", BTreeMap::new(), Some("echo hi"))
        .await
        .expect("the chat should open");
    assert_eq!(
        dataset.runs_for(&chat),
        1,
        "the script did not run on creation"
    );

    dataset.park(&chat).await;
    dataset.prompt(&chat, "are you there").await;

    assert_eq!(
        dataset.runs_for(&chat),
        2,
        "spinning a parked chat back up did not re-run the script"
    );
}

/// One dataset over a `MemoryPlane`, a scripted adapter, and a file:// remote.
struct Dataset {
    chats: Chats<MemoryPlane, ScriptedAdapter>,
    plane: MemoryPlane,
    data_dir: TempDir,
    _origin: TempDir,
}

impl Dataset {
    fn opening() -> Self {
        Self::serving(ScriptedAdapter::opening(SESSION))
    }

    fn serving(adapter: ScriptedAdapter) -> Self {
        let data_dir = TempDir::new().expect("temp dir should be creatable");
        let (origin, remotes) = seeded_repository();
        let config = test_config(data_dir.path().to_path_buf());
        ChatStore::new(data_dir.path())
            .prepare()
            .expect("the dataset should prepare, as serving does");
        let plane = MemoryPlane::default();
        let secrets = Arc::new(Secrets::from_config(&config));
        let chats = Chats::new(
            &config,
            Owner::of(&config.data_dir).expect("we own the dataset we just made"),
            plane.clone(),
            adapter,
            remotes,
            secrets,
        );
        Self {
            chats,
            plane,
            data_dir,
            _origin: origin,
        }
    }

    async fn create(
        &self,
        slug: &str,
        env: BTreeMap<String, String>,
        startup_script: Option<&str>,
    ) -> Result<String, CreateError> {
        self.create_full(slug, "main", env, startup_script).await
    }

    async fn create_full(
        &self,
        slug: &str,
        base_branch: &str,
        env: BTreeMap<String, String>,
        startup_script: Option<&str>,
    ) -> Result<String, CreateError> {
        self.chats
            .create(WantedChat {
                repo: REPO.to_owned(),
                base_branch: base_branch.to_owned(),
                slug: slug.to_owned(),
                direct_on_base: false,
                env,
                startup_script: startup_script.map(ToOwned::to_owned),
            })
            .await
    }

    async fn create_on_base(
        &self,
        slug: &str,
        base_branch: &str,
        startup_script: Option<&str>,
    ) -> Result<String, CreateError> {
        self.create_full(slug, base_branch, BTreeMap::new(), startup_script)
            .await
    }

    async fn prompt(&self, chat_id: &str, said: &str) {
        let chat_id: Ulid = chat_id.parse().expect("a chat id is a ulid");
        self.chats
            .prompt(&chat_id, said)
            .await
            .expect("a revived chat should take a prompt");
    }

    /// Push the chat out of the warm pool, the only way a chat comes to be
    /// parked: its container goes and its connection with it, so the next prompt
    /// has to spin it back up (ADR-0002).
    async fn park(&self, chat_id: &str) {
        self.create("filler", BTreeMap::new(), None)
            .await
            .expect("a filler chat should be cut");
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

    fn runs_for(&self, chat_id: &str) -> usize {
        self.plane
            .startup_runs()
            .iter()
            .filter(|run| run.chat_id == chat_id)
            .count()
    }

    /// The text of the chat's one startup-script transcript line.
    fn startup_notice(&self, chat_id: &str) -> String {
        let path = self
            .data_dir
            .path()
            .join("chats")
            .join(chat_id)
            .join("events.jsonl");
        let log = fs::read_to_string(&path).expect("the event log should exist");
        log.lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|event| event["event"]["corcode"] == "startup_script")
            .and_then(|event| event["event"]["text"].as_str().map(ToOwned::to_owned))
            .expect("a startup-script line should be in the transcript")
    }
}

fn seeded_repository() -> (TempDir, Remotes) {
    let dir = TempDir::new().expect("origin dir should be created");
    let bare = dir.path().join(BARE);
    let work = dir.path().join("seed");
    run(
        dir.path(),
        &["init", "--bare", "--initial-branch=main", &spelled(&bare)],
    );
    run(
        dir.path(),
        &["init", "--initial-branch=main", &spelled(&work)],
    );
    run(&work, &["config", "user.email", "seed@example.invalid"]);
    run(&work, &["config", "user.name", "Seed"]);
    fs::write(work.join("README.md"), "fixture").expect("seed file should be writable");
    run(&work, &["add", "."]);
    run(&work, &["commit", "-m", "first"]);
    run(&work, &["remote", "add", "origin", &spelled(&bare)]);
    run(&work, &["push", "origin", "main"]);
    let served_from = format!("file://{}", spelled(dir.path()));
    (dir, Remotes::new(served_from))
}

fn spelled(path: &Path) -> String {
    path.to_str()
        .expect("temp paths should be spellable")
        .to_owned()
}

fn run(cwd: &Path, args: &[&str]) {
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
        anthropic_api_key: Some(KEY.to_owned()),
    }
}
