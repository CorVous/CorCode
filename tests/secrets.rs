//! Integration tests for the operational secrets: the key a container is
//! spawned with and the token git clones and pushes over are read at the
//! moment they are used, so a rotation lands without a restart (ADR-0001,
//! ADR-0005).
//!
//! A token rides in a URL only when the site is reached over https, so the
//! `file://` fixture here can hold no credential at all: that a clone or push
//! leaves none in `.git/config` is pinned over a credentialled remote in
//! `src/git.rs`, the one place able to build one.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use tempfile::TempDir;
use ulid::Ulid;

use cor_code::acp::ScriptedAdapter;
use cor_code::chats::{ArchiveError, Chats, WantedChat};
use cor_code::config::{
    Config, DEFAULT_CONTAINER_CPUS, DEFAULT_CONTAINER_MEMORY_MB, DEFAULT_SCRATCH_MB,
};
use cor_code::git::Remotes;
use cor_code::plane::MemoryPlane;
use cor_code::secrets::{Secret, Secrets};
use cor_code::store::{ChatStore, Owner};

const REPO: &str = "CorVous/fixture";
const BARE: &str = "CorVous/fixture.git";
const SESSION: &str = "3f2b1c4d-0000-4000-8000-000000000001";

/// The variable the agent reads an Anthropic API key from (ADR-0001).
const API_KEY: &str = "ANTHROPIC_API_KEY";

/// The variable it reads a subscription token from instead.
const OAUTH_TOKEN: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// The variable `gh` reads the GitHub token from, and the one everything else
/// looks for (ADR-0005).
const GH_TOKEN: &str = "GH_TOKEN";
const GITHUB_TOKEN: &str = "GITHUB_TOKEN";

const BOOTSTRAPPED_KEY: &str = "sk-ant-bootstrapped";
const ROTATED_KEY: &str = "sk-ant-rotated";
const SUBSCRIPTION: &str = "sk-ant-oat01-subscription-secret";
const TOKEN: &str = "ghs-clone-secret";
const ROTATED_TOKEN: &str = "ghs-rotated-secret";

#[tokio::test]
async fn a_container_is_spawned_with_the_key_the_environment_bootstrapped() {
    let dataset = Dataset::bootstrapped_with(Some(BOOTSTRAPPED_KEY));

    let chat = dataset.create("first").await;

    assert_eq!(dataset.key_of(&chat).as_deref(), Some(BOOTSTRAPPED_KEY));
}

/// A subscription token is not a key, and the agent reads it from somewhere
/// else. Handing it over as a key is how it gets turned away mid-chat.
#[tokio::test]
async fn a_container_spawned_over_a_subscription_token_carries_it_as_one() {
    let dataset = Dataset::bootstrapped_with(None);
    dataset.write(Secret::AnthropicKey, SUBSCRIPTION);

    let chat = dataset.create("subscribed").await;

    assert_eq!(
        dataset.env_of(&chat, OAUTH_TOKEN).as_deref(),
        Some(SUBSCRIPTION)
    );
    assert_eq!(
        dataset.env_of(&chat, API_KEY),
        None,
        "the token was handed over as a key as well"
    );
}

/// The reverse: a key is a key, and nothing tells the agent to look for a
/// subscription it has not got.
#[tokio::test]
async fn a_container_spawned_over_a_key_carries_no_subscription_token() {
    let dataset = Dataset::bootstrapped_with(Some(BOOTSTRAPPED_KEY));

    let chat = dataset.create("keyed").await;

    assert_eq!(
        dataset.env_of(&chat, API_KEY).as_deref(),
        Some(BOOTSTRAPPED_KEY)
    );
    assert_eq!(dataset.env_of(&chat, OAUTH_TOKEN), None);
}

#[tokio::test]
async fn a_rotated_key_reaches_the_next_container() {
    let dataset = Dataset::bootstrapped_with(Some(BOOTSTRAPPED_KEY));
    let before = dataset.create("before").await;

    dataset.write(Secret::AnthropicKey, ROTATED_KEY);
    let after = dataset.create("after").await;

    assert_eq!(
        dataset.key_of(&after).as_deref(),
        Some(ROTATED_KEY),
        "the container was spawned with the key the core booted with"
    );
    assert_eq!(
        dataset.key_of(&before).as_deref(),
        Some(BOOTSTRAPPED_KEY),
        "a container already up was somehow handed the new key"
    );
}

/// The agent pushes its own commits (ADR-0005), so the container is handed the
/// token under both names its tooling looks for: `gh` prefers one and
/// everything else the other.
#[tokio::test]
async fn a_container_is_spawned_with_the_github_token_under_both_the_names_its_tools_read() {
    let dataset = Dataset::bootstrapped_with(None);
    dataset.write(Secret::GithubToken, TOKEN);

    let chat = dataset.create("credentialed").await;

    assert_eq!(dataset.env_of(&chat, GH_TOKEN).as_deref(), Some(TOKEN));
    assert_eq!(dataset.env_of(&chat, GITHUB_TOKEN).as_deref(), Some(TOKEN));
}

/// A deployment holding no token spawns an agent that holds none either: the
/// image's credential helper then answers nothing and the stop hook stands
/// down (ADR-0005), which is only true while neither name is set to anything.
#[tokio::test]
async fn a_container_spawned_where_no_github_token_is_held_carries_neither_name() {
    let dataset = Dataset::bootstrapped_with(Some(BOOTSTRAPPED_KEY));

    let chat = dataset.create("tokenless").await;

    assert_eq!(dataset.env_of(&chat, GH_TOKEN), None);
    assert_eq!(dataset.env_of(&chat, GITHUB_TOKEN), None);
}

#[tokio::test]
async fn a_rotated_github_token_reaches_the_next_container() {
    let dataset = Dataset::bootstrapped_with(None);
    dataset.write(Secret::GithubToken, TOKEN);
    let before = dataset.create("before").await;

    dataset.write(Secret::GithubToken, ROTATED_TOKEN);
    let after = dataset.create("after").await;

    assert_eq!(
        dataset.env_of(&after, GH_TOKEN).as_deref(),
        Some(ROTATED_TOKEN),
        "the container was spawned with the token the core booted with"
    );
    assert_eq!(
        dataset.env_of(&before, GH_TOKEN).as_deref(),
        Some(TOKEN),
        "a container already up was somehow handed the new token"
    );
}

/// Cutting a chat hands the token to a container and writes a great deal to
/// the dataset — the manifest, the event log, a workspace tree with its own
/// git config — and none of that is a place a token belongs. What the clone
/// URL itself carries is `git`'s to answer for, and does:
/// `a_token_rides_in_the_url_and_nowhere_else`.
#[tokio::test]
async fn a_chat_cut_over_a_token_writes_it_nowhere_on_the_dataset() {
    let dataset = Dataset::bootstrapped_with(None);
    dataset.write(Secret::GithubToken, TOKEN);

    dataset.create("contained").await;

    assert_eq!(
        dataset.dataset_files_holding(TOKEN),
        Vec::<PathBuf>::new(),
        "the token was written where the agent, or anyone reading the dataset, can find it"
    );
}

/// Cutting a chat and archiving it are two operations, and the token in force
/// at the second is not the one the first went out on.
#[tokio::test]
async fn a_chat_cut_and_archived_over_a_rotated_token_lands_its_branch() {
    let dataset = Dataset::bootstrapped_with(None);
    dataset.write(Secret::GithubToken, TOKEN);
    let chat = dataset.create("archived").await;

    dataset.write(Secret::GithubToken, ROTATED_TOKEN);
    dataset.archive(&chat).await.expect("a clean chat archives");

    assert!(
        !dataset.workspace(&chat).exists(),
        "an archived chat keeps no workspace"
    );
    assert!(
        dataset.branches_on_the_remote().contains("archived"),
        "the archive pushed the chat's branch nowhere"
    );
}

/// A token read at the moment of the push is one the archive cannot have
/// taken at boot.
#[cfg(unix)]
#[tokio::test]
async fn a_token_that_cannot_be_read_stops_the_archive_and_names_the_file() {
    let dataset = Dataset::bootstrapped_with(None);
    dataset.write(Secret::GithubToken, TOKEN);
    let chat = dataset.create("unreadable").await;
    dataset.unreadable(Secret::GithubToken);

    let failure = dataset
        .archive(&chat)
        .await
        .expect_err("an unreadable token should stop the archive");

    let ArchiveError::Broke(source) = failure else {
        panic!("an unreadable token is nothing the request did wrong: {failure}")
    };
    let said = format!("{source:#}");
    assert!(
        said.contains("github_token"),
        "the failure should name the file, got: {said}"
    );
    assert!(said.contains(&dataset.secrets_dir().display().to_string()));
    assert!(!said.contains(TOKEN), "the token leaked: {said}");
}

/// One dataset, its remote, and the secrets both are reached over.
struct Dataset {
    chats: Chats<MemoryPlane, ScriptedAdapter>,
    plane: MemoryPlane,
    secrets: Arc<Secrets>,
    data_dir: TempDir,
    origin: TempDir,
}

impl Dataset {
    /// A dataset whose environment carried `anthropic_api_key` in and nothing
    /// else, as a first boot does.
    fn bootstrapped_with(anthropic_api_key: Option<&str>) -> Self {
        let data_dir = TempDir::new().expect("temp dir should be creatable");
        let (origin, remotes) = seeded_repository();
        let config = test_config(data_dir.path().to_path_buf(), anthropic_api_key);
        ChatStore::new(data_dir.path())
            .prepare()
            .expect("the dataset should prepare, as serving does");
        let plane = MemoryPlane::default();
        let secrets = Arc::new(Secrets::from_config(&config));
        let chats = Chats::new(
            &config,
            Owner::of(&config.data_dir).expect("we own the dataset we just made"),
            plane.clone(),
            ScriptedAdapter::opening(SESSION),
            remotes,
            Arc::clone(&secrets),
        );
        Self {
            chats,
            plane,
            secrets,
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

    async fn archive(&self, chat_id: &str) -> Result<(), ArchiveError> {
        let chat_id: Ulid = chat_id.parse().expect("a chat id is a ulid");
        self.chats.archive(&chat_id).await
    }

    fn write(&self, secret: Secret, value: &str) {
        self.secrets
            .write(secret, value)
            .expect("a secret should be writable");
    }

    /// Put something in the secret's place that no read can make sense of, as
    /// a dataset that is not mounted the way it was leaves behind.
    fn unreadable(&self, secret: Secret) {
        let path = self.secrets_dir().join(match secret {
            Secret::GithubToken => "github_token",
            Secret::AnthropicKey => "anthropic_key",
        });
        fs::remove_file(&path).expect("the secret should be on disk");
        fs::create_dir(&path).expect("a directory should be creatable");
    }

    fn secrets_dir(&self) -> PathBuf {
        self.data_dir.path().join("secrets")
    }

    /// The Anthropic key the chat's container was spawned with, if it holds
    /// one.
    fn key_of(&self, chat_id: &str) -> Option<String> {
        self.env_of(chat_id, API_KEY)
    }

    /// What the chat's container was spawned with under `variable`, if it was
    /// spawned with anything under it at all.
    fn env_of(&self, chat_id: &str, variable: &str) -> Option<String> {
        self.plane
            .env_of(chat_id)
            .expect("the chat should be holding a container")
            .get(variable)
            .cloned()
    }

    fn workspace(&self, chat_id: &str) -> PathBuf {
        self.data_dir.path().join("workspaces").join(chat_id)
    }

    /// Every file on the dataset spelling `secret` out, but for the secrets
    /// directory, which is the one place a secret is kept.
    fn dataset_files_holding(&self, secret: &str) -> Vec<PathBuf> {
        let mut holding = Vec::new();
        let mut looking = vec![self.data_dir.path().to_path_buf()];
        while let Some(dir) = looking.pop() {
            if dir == self.secrets_dir() {
                continue;
            }
            for entry in fs::read_dir(&dir).expect("the dataset should be readable") {
                let path = entry.expect("an entry should be readable").path();
                if path.is_dir() {
                    looking.push(path);
                } else if fs::read(&path)
                    .expect("a dataset file should be readable")
                    .windows(secret.len())
                    .any(|window| window == secret.as_bytes())
                {
                    holding.push(path);
                }
            }
        }
        holding
    }

    fn branches_on_the_remote(&self) -> String {
        git_says(
            &self.origin.path().join(BARE),
            &["branch", "--format=%(refname:short)"],
        )
    }
}

/// A bare repository with a commit on `main`, reachable over `file://` so
/// that no test needs the network.
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
    git_says(cwd, args);
}

fn git_says(cwd: &Path, args: &[&str]) -> String {
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

fn test_config(data_dir: PathBuf, anthropic_api_key: Option<&str>) -> Config {
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
        warm_pool: 2,
        registry: None,
        repos: vec![REPO.to_owned()],
        github_token: None,
        anthropic_api_key: anthropic_api_key.map(ToOwned::to_owned),
    }
}
