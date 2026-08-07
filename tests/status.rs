//! The status line on live plane state (ADR-0008): warm-pool slots with
//! per-chat idle times, the parked count, the pinned tag, and the last
//! orphan sweep.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use tempfile::TempDir;

use cor_code::acp::ScriptedAdapter;
use cor_code::chats::Chats;
use cor_code::config::{
    Config, DEFAULT_CONTAINER_CPUS, DEFAULT_CONTAINER_MEMORY_MB, DEFAULT_WARM_POOL,
};
use cor_code::git::{GITHUB, Remotes};
use cor_code::plane::{ContainerPlane as _, MemoryPlane};
use cor_code::secrets::Secrets;
use cor_code::store::Owner;
use cor_code::store::{ChatStore, Manifest, NewChat};
use cor_code::ui;

const IMAGE: &str = "ghcr.io/corvous/corcode-workspace:2026-08-05";

#[tokio::test]
async fn the_status_line_reads_the_pool_the_parked_count_and_the_tag_off_live_state() {
    let now = Utc::now();
    let dataset = Dataset::fresh();
    let holding = dataset.chat("Resume ladder", now - Duration::minutes(3));
    dataset.chat("Sweep", now - Duration::hours(2));
    let plane = MemoryPlane::default();
    dataset.spawn(&plane, &holding).await;
    let chats = dataset.chats(plane);

    let fragment = ui::status_line(&chats.status(now).await.expect("status should read"));

    assert!(
        fragment.contains(
            "<summary>pool 1/2 · parked 1 · img 2026-08-05 · sweep not yet run</summary>"
        ),
        "the status line does not read as ADR-0008 asks: {fragment}"
    );
    assert!(
        fragment.contains("Resume ladder · 3m"),
        "the slot does not carry its chat's idle time: {fragment}"
    );
}

#[tokio::test]
async fn every_slot_carries_its_own_idle_time() {
    let now = Utc::now();
    let dataset = Dataset::fresh();
    let fresh = dataset.chat("Fresh", now - Duration::seconds(4));
    let stale = dataset.chat("Stale", now - Duration::hours(26));
    let plane = MemoryPlane::default();
    dataset.spawn(&plane, &fresh).await;
    dataset.spawn(&plane, &stale).await;
    let chats = dataset.chats(plane);

    let fragment = ui::status_line(&chats.status(now).await.expect("status should read"));

    assert!(
        fragment.contains("Fresh · 4s") && fragment.contains("Stale · 1d"),
        "the slots do not each carry their own idle time: {fragment}"
    );
    assert!(
        fragment.contains("pool 2/2"),
        "the slots are miscounted: {fragment}"
    );
}

#[tokio::test]
async fn a_sweep_that_found_nothing_says_so_until_one_finds_something() {
    let now = Utc::now();
    let dataset = Dataset::fresh();
    dataset.chat("Resume ladder", now);
    let chats = dataset.chats(MemoryPlane::default());

    chats.sweep().await;
    let clean = ui::status_line(&chats.status(now).await.expect("status should read"));
    dataset.orphan_workspace("01K1ORPHANWORKSPACE00000");
    chats.sweep().await;
    let swept = ui::status_line(&chats.status(now).await.expect("status should read"));

    assert!(
        clean.contains("sweep ok"),
        "a sweep that found nothing does not read as ok: {clean}"
    );
    assert!(
        swept.contains("sweep removed 1") && swept.contains("01K1ORPHANWORKSPACE00000"),
        "the sweep does not name what it removed: {swept}"
    );
}

/// A dataset on disk, plus the chats vertical that reads it.
struct Dataset {
    dir: TempDir,
}

impl Dataset {
    fn fresh() -> Self {
        let dir = TempDir::new().expect("temp dir should be creatable");
        ChatStore::new(dir.path())
            .prepare()
            .expect("the dataset should prepare, as serving does");
        Self { dir }
    }

    fn store(&self) -> ChatStore {
        ChatStore::new(self.dir.path())
    }

    /// One open chat whose last turn was at `last_active_at`.
    fn chat(&self, title: &str, last_active_at: DateTime<Utc>) -> String {
        let store = self.store();
        let manifest = store
            .create_chat(NewChat {
                title: title.to_owned(),
                repo: "CorVous/CorCode".to_owned(),
                branch: format!("chat/2026-08-05-{title}"),
                base_branch: "main".to_owned(),
            })
            .expect("fixture chat should be created");
        let chat_id = manifest.chat_id.clone();
        store
            .write_manifest(&Manifest {
                last_active_at,
                ..manifest
            })
            .expect("the fixture's last turn should be datable");
        chat_id
    }

    /// A working tree no chat claims, for the sweep to find.
    fn orphan_workspace(&self, chat_id: &str) {
        fs::create_dir_all(self.dir.path().join("workspaces").join(chat_id))
            .expect("an orphan working tree should be creatable");
    }

    async fn spawn(&self, plane: &MemoryPlane, chat_id: &str) {
        plane
            .spawn(
                chat_id,
                &self.store().workspace_dir(chat_id),
                &self.store().claude_dir(chat_id),
                &BTreeMap::new(),
            )
            .await
            .expect("the fixture chat should take a container");
    }

    fn chats(&self, plane: MemoryPlane) -> Chats<MemoryPlane, ScriptedAdapter> {
        let config = config(self.dir.path());
        let secrets = Arc::new(Secrets::from_config(&config));
        Chats::new(
            &config,
            Owner::of(&config.data_dir).expect("we own the dataset we just made"),
            plane,
            ScriptedAdapter::silent(),
            Remotes::new(GITHUB),
            secrets,
        )
    }
}

fn config(data_dir: &Path) -> Config {
    Config {
        data_dir: data_dir.to_path_buf(),
        host_data_dir: data_dir.to_path_buf(),
        bind_addr: "127.0.0.1:0".parse().expect("valid address"),
        username: "cassidy".to_owned(),
        password_hash: "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA".to_owned(),
        workspace_image: IMAGE.to_owned(),
        container_memory_mb: DEFAULT_CONTAINER_MEMORY_MB,
        container_cpus: DEFAULT_CONTAINER_CPUS,
        warm_pool: DEFAULT_WARM_POOL,
        registry: None,
        repos: vec!["CorVous/CorCode".to_owned()],
        github_token: None,
        anthropic_api_key: None,
    }
}
