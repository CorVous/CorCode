//! Integration tests for ADR-0007's resume flow: the first prompt into a chat
//! with no live connection climbs the reconnect ladder, and one into an
//! archived chat brings its workspace back from the remote.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use serde_json::{Value, json};
use tempfile::TempDir;
use ulid::Ulid;

use cor_code::acp::ScriptedAdapter;
use cor_code::chats::{Chats, PromptError, WantedChat};
use cor_code::config::{
    Config, DEFAULT_CONTAINER_CPUS, DEFAULT_CONTAINER_MEMORY_MB, DEFAULT_SCRATCH_MB,
};
use cor_code::git::Remotes;
use cor_code::plane::MemoryPlane;
use cor_code::secrets::Secrets;
use cor_code::store::{ChatStore, Owner, RuntimeStatus};
use cor_code::ui;

const REPO: &str = "CorVous/fixture";
const BARE: &str = "CorVous/fixture.git";

/// The session the adapter opens whenever it is asked for a new one.
const SESSION: &str = "3f2b1c4d-0000-4000-8000-000000000001";

/// The session a chat remembers from before whatever was holding it went
/// away, which is the one the ladder asks about.
const FORGOTTEN: &str = "3f2b1c4d-0000-4000-8000-00000000dead";

const SAID: &str = "ship the ladder";

/// The mode a session the adapter clamped comes back in: Claude Code's
/// interactive default, where every call is an ask this client declines.
const CLAMPED: &str = "default";
/// The only user that can give a tree away, and so the only one this suite can
/// watch a handover under.
#[cfg(unix)]
const ROOT: Owner = Owner { uid: 0, gid: 0 };

const RESUME_SESSION: &str = "session/resume";
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

/// A rung the adapter has never heard of is answered -32601 and the ladder
/// climbs on, so the chat resumes either way and only the price says which
/// rung paid for it. The methods one wake put on the wire are pinned here,
/// where a wrong name cannot hide behind the fallthrough (issue #56).
#[tokio::test]
async fn a_wake_asks_for_the_resume_method_the_adapter_answers_and_stops_there() {
    let dataset = Dataset::of(ScriptedAdapter::resuming(
        SESSION,
        &[update(FORGOTTEN, "on it")],
    ));
    let chat = dataset.create("resumed").await;
    dataset.forget_the_session(&chat);
    dataset.park(&chat).await;

    dataset
        .prompt(&chat, SAID)
        .await
        .expect("a parked chat should wake");

    assert_eq!(
        dataset.methods_asked_over(FORGOTTEN),
        ["session/resume", "session/prompt"],
        "the wake did not resume at rung 1 and then prompt"
    );
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

/// A session comes back in a mode of its own on the rung that answered, not
/// only on a session/new: a chat woken into asking declines every ask and
/// reads as a mute agent unless the wake says so (ADR-0001, issue #58).
#[tokio::test]
async fn a_session_the_wake_brought_back_clamped_is_said_so_in_the_chats_own_log() {
    let dataset = Dataset::of(ScriptedAdapter::loading_in_mode(SESSION, CLAMPED));
    let chat = dataset.create("clamped on the way back").await;
    dataset.park(&chat).await;

    dataset
        .prompt(&chat, SAID)
        .await
        .expect("a parked chat should wake");

    let events = dataset.events(&chat);
    assert_eq!(
        events[0]["corcode"], "mode_notice",
        "a clamped wake left the chat nothing to read: {events:?}"
    );
    assert!(
        events[0]["text"]
            .as_str()
            .is_some_and(|text| text.contains(CLAMPED)),
        "the notice does not say what mode the session came back in: {events:?}"
    );
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
        dataset.rendered_log(&chat).contains("<blockquote"),
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
    assert!(dataset.rendered_log(&chat).contains("<blockquote"));
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

/// A chat that comes back behind its branch will have its next archive
/// refused and rescued (issue #50). The drift is said out loud on the way in,
/// so the rescue is not the first anyone hears of it.
#[tokio::test]
async fn an_archived_chat_the_remote_has_moved_past_is_told_it_is_behind() {
    let dataset = Dataset::of(ScriptedAdapter::resuming(
        SESSION,
        &[update(SESSION, "on it")],
    ));
    let chat = dataset.create("behind").await;
    let pushed = dataset.commit_in_workspace(&chat, "the agent's own commit");
    dataset.archive(&chat).await;
    let ahead = dataset.move_the_branch_past(&chat, &pushed);

    dataset
        .prompt(&chat, SAID)
        .await
        .expect("a chat behind the tip should still revive");

    assert_eq!(
        says(&dataset.workspace(&chat), &["rev-parse", "HEAD"]),
        pushed,
        "the revival did not come back where the chat was archived"
    );
    let told = dataset.events(&chat);
    let drift = told
        .iter()
        .find(|line| line["corcode"] == "drift_notice")
        .and_then(|line| line["text"].as_str())
        .unwrap_or_else(|| panic!("a chat behind its branch is told nothing: {told:?}"));
    assert!(
        drift.contains(&ahead) && drift.contains(&dataset.branch(&chat)),
        "the notice does not say what this chat is behind: {drift}"
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

/// A workspace where an archived chat's should not be is somebody else's, or
/// the last revival's, and either way it is not this one's to clone over or to
/// delete (ADR-0007 rule 5).
#[tokio::test]
async fn an_archived_chat_whose_workspace_is_already_on_disk_is_left_untouched() {
    let dataset = Dataset::of(ScriptedAdapter::resuming(
        SESSION,
        &[update(SESSION, "on it")],
    ));
    let chat = dataset.create("leftover").await;
    dataset.commit_in_workspace(&chat, "the agent's own commit");
    dataset.archive(&chat).await;
    let leftover = dataset.workspace(&chat).join("not-ours.txt");
    fs::create_dir_all(dataset.workspace(&chat)).expect("a workspace should be creatable");
    fs::write(&leftover, "somebody else's work").expect("a file should be writable");

    dataset
        .prompt(&chat, SAID)
        .await
        .expect_err("a chat cannot be revived over a workspace that is already there");

    assert!(
        leftover.is_file(),
        "a revival deleted a workspace it did not create"
    );
    assert_eq!(dataset.manifest(&chat)["state"], "archived");
    assert!(
        dataset
            .events(&chat)
            .last()
            .and_then(|line| line["text"].as_str())
            .is_some_and(|text| text.contains("workspace")),
        "the operator is told nothing about the workspace in the way"
    );
}

/// Rule 5 before anything else: a chat whose manifest cannot say what to come
/// back to is not worth cloning for, so nothing is cloned and nothing is said
/// about a reset that never happened.
#[tokio::test]
async fn an_archived_chat_with_no_session_recorded_fails_before_it_clones_anything() {
    let dataset = Dataset::of(ScriptedAdapter::resuming(
        SESSION,
        &[update(SESSION, "on it")],
    ));
    let chat = dataset.create("sessionless").await;
    dataset.commit_in_workspace(&chat, "the agent's own commit");
    dataset.archive(&chat).await;
    dataset.forget_there_was_a_session(&chat);

    dataset
        .prompt(&chat, SAID)
        .await
        .expect_err("a chat with no session recorded cannot be woken");

    assert_eq!(dataset.manifest(&chat)["state"], "archived");
    assert!(
        !dataset.workspace(&chat).exists(),
        "a chat that could never be woken was cloned back anyway"
    );
    let told = dataset.events(&chat);
    assert!(
        told.iter().all(|line| line["corcode"] != "reset_notice"),
        "a chat was told its workspace came back when it never did: {told:?}"
    );
    assert_eq!(
        told.len(),
        1,
        "one failed wake should leave one line: {told:?}"
    );
}

/// Waking is a clone, a container and a ladder, none of which two prompts may
/// do at once. The loser is told rather than left to race.
#[tokio::test]
async fn two_prompts_racing_to_wake_one_chat_leave_one_of_them_turned_away() {
    let dataset = Dataset::of(ScriptedAdapter::resuming(
        SESSION,
        &[update(FORGOTTEN, "on it")],
    ));
    let chat = dataset.create("raced").await;
    dataset.forget_the_session(&chat);
    dataset.park(&chat).await;

    let (first, second) = tokio::join!(
        dataset.prompt(&chat, SAID),
        dataset.prompt(&chat, "and again")
    );

    assert_eq!(
        [first.is_ok(), second.is_ok()]
            .into_iter()
            .filter(|won| *won)
            .count(),
        1,
        "two prompts woke one chat: {first:?} and {second:?}"
    );
    assert_eq!(dataset.status(&chat).await, RuntimeStatus::Live);
    assert_eq!(
        dataset.asked_to(RESUME_SESSION),
        [FORGOTTEN],
        "the ladder was climbed twice over one chat"
    );
    let told = dataset.events(&chat);
    assert_eq!(
        told.iter()
            .filter(|line| line["corcode"] == "refusal")
            .count(),
        1,
        "the prompt that was turned away was never told so: {told:?}"
    );
}

/// A wake that started a container and then could not reach an agent in it
/// gives the container back: the chat is parked, as it was before the prompt.
#[tokio::test]
async fn a_wake_whose_adapter_never_answers_leaves_no_container_behind() {
    let dataset = Dataset::of(ScriptedAdapter::resuming(
        SESSION,
        &[update(SESSION, "on it")],
    ));
    let chat = dataset.create("unreachable").await;
    dataset.park(&chat).await;
    dataset.adapter.dies();

    let refused = dataset
        .prompt(&chat, SAID)
        .await
        .expect_err("a chat whose adapter is dead cannot be woken");

    assert!(
        matches!(refused, PromptError::Unwoken(_)),
        "a dead adapter mid ladder was not reported as a failed wake: {refused:?}"
    );
    assert_eq!(
        dataset.status(&chat).await,
        RuntimeStatus::Parked,
        "a wake that failed kept the container it started"
    );
    assert!(
        dataset
            .events(&chat)
            .last()
            .and_then(|line| line["corcode"].as_str())
            .is_some_and(|kind| kind == "wake_failure"),
        "the operator is never told the wake failed"
    );
}

/// The container a wake starts counts against the pool like any other, even
/// when the turn it was started for breaks (ADR-0002 rule 2).
#[tokio::test]
async fn a_wake_brings_the_pool_back_inside_its_cap_whatever_the_turn_does() {
    let dataset = Dataset::of(ScriptedAdapter::dying_mid_turn(
        SESSION,
        &[update(SESSION, "on i")],
    ));
    let woken = dataset.create("woken").await;
    dataset.park(&woken).await;

    dataset
        .prompt(&woken, SAID)
        .await
        .expect_err("this adapter dies mid turn");

    assert_eq!(
        dataset.live_count().await,
        1,
        "the woken chat's container was never counted against the pool"
    );
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

/// A revived workspace is as new as a created one: the core clones it back as
/// itself and the container opens on it as the agent (ADR-0001), so the clone
/// changes hands before anything runs in it (issue #46). Only a core that can
/// hand a tree to somebody else can be watched doing it.
#[cfg(unix)]
#[tokio::test]
async fn the_workspace_a_revived_chat_comes_back_in_belongs_to_the_agent() {
    let dataset = Dataset::handing_trees_to(
        ScriptedAdapter::resuming(SESSION, &[update(SESSION, "on it")]),
        Some(Owner::AGENT),
    );
    if dataset.ours() != ROOT {
        eprintln!(
            "SKIPPED the_workspace_a_revived_chat_comes_back_in_belongs_to_the_agent: {} can \
             hand a tree to nobody but itself",
            dataset.ours()
        );
        return;
    }
    let chat = dataset.create("revived").await;
    dataset.archive(&chat).await;

    dataset
        .prompt(&chat, SAID)
        .await
        .expect("an archived chat should revive");

    let workspace = dataset.workspace(&chat);
    for tree in [&workspace, &workspace.join(".git")] {
        assert_eq!(
            Owner::of(tree).expect("the clone should be there"),
            Owner::AGENT,
            "{tree:?} came back where the agent can change nothing"
        );
    }
}

/// The core writes in an open chat's own workspace — archiving it leaves a
/// commit message and refs in `.git` that git wrote as the core — so the tree
/// goes back into the agent's hands before a container opens on it again. A
/// tree only half handed over is one the agent cannot commit from, and no
/// prompt is worth leaving it in.
#[cfg(unix)]
#[tokio::test]
async fn a_parked_chat_whose_workspace_will_not_change_hands_is_not_woken() {
    use std::os::unix::fs::PermissionsExt as _;

    let dataset = Dataset::of(ScriptedAdapter::resuming(
        SESSION,
        &[update(SESSION, "on it")],
    ));
    let chat = dataset.create("unhandable").await;
    dataset.park(&chat).await;
    let sealed = dataset.workspace(&chat).join(".git");
    fs::set_permissions(&sealed, fs::Permissions::from_mode(0o000))
        .expect("a dir should be sealable");

    let woken = dataset.prompt(&chat, SAID).await;

    fs::set_permissions(&sealed, fs::Permissions::from_mode(0o755))
        .expect("a dir should be unsealable");
    woken.expect_err("a workspace that will not change hands cannot be woken");
    assert_eq!(
        dataset.status(&chat).await,
        RuntimeStatus::Parked,
        "a container was started over a workspace the agent cannot work in"
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
        Self::handing_trees_to(adapter, None)
    }

    /// A dataset whose chats are handed to `agent`, or to whoever runs this
    /// suite when the trees are to stay where they are.
    fn handing_trees_to(adapter: ScriptedAdapter, agent: Option<Owner>) -> Self {
        let data_dir = TempDir::new().expect("temp dir should be creatable");
        let (origin, remotes) = seeded_repository();
        let config = test_config(data_dir.path().to_path_buf());
        ChatStore::new(data_dir.path())
            .prepare()
            .expect("the dataset should prepare, as serving does");
        let agent = agent.unwrap_or_else(|| {
            Owner::of(&config.data_dir).expect("we own the dataset we just made")
        });
        let chats = Chats::new(
            &config,
            agent,
            MemoryPlane::default(),
            adapter.clone(),
            remotes,
            Arc::new(Secrets::from_config(&config)),
        );
        Self {
            chats,
            adapter,
            data_dir,
            origin,
        }
    }

    /// Whoever this suite is running as, and so what it can hand a tree to.
    fn ours(&self) -> Owner {
        Owner::of(self.data_dir.path()).expect("we own the dataset we just made")
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

    async fn prompt(&self, chat_id: &str, said: &str) -> Result<(), PromptError> {
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
        self.rewrite_manifest(chat_id, json!(FORGOTTEN));
    }

    /// Leave the chat remembering no session at all, as one whose creation
    /// stopped half way does.
    fn forget_there_was_a_session(&self, chat_id: &str) {
        self.rewrite_manifest(chat_id, Value::Null);
    }

    fn rewrite_manifest(&self, chat_id: &str, acp_session_id: Value) {
        let path = self.chat_dir(chat_id).join("manifest.json");
        let mut manifest = self.manifest(chat_id);
        manifest["acp_session_id"] = acp_session_id;
        fs::write(&path, manifest.to_string()).expect("the manifest should be rewritable");
    }

    /// How many chats in the dataset are holding a container.
    async fn live_count(&self) -> usize {
        self.chats
            .survey()
            .await
            .expect("the dataset should survey")
            .into_iter()
            .filter(|(_, status)| *status == RuntimeStatus::Live)
            .count()
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

    /// The methods the adapter was asked about one session, in order.
    fn methods_asked_over(&self, session_id: &str) -> Vec<String> {
        self.adapter
            .requests()
            .iter()
            .filter(|request| request["params"]["sessionId"] == session_id)
            .map(|request| request["method"].to_string().replace('"', ""))
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
            .handing_trees_to(
                Owner::of(self.data_dir.path()).expect("we own the dataset we just made"),
            )
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

    /// Somebody else's commit on top of the chat's own, as a push from outside
    /// this core leaves the branch: what the chat archived is still there, and
    /// the branch has moved past it. Answers with the new tip.
    fn move_the_branch_past(&self, chat_id: &str, commit: &str) -> String {
        let tree = self.origin_says(&["rev-parse", &format!("{commit}^{{tree}}")]);
        let ahead = self.origin_says(&[
            "-c",
            "user.name=Someone Else",
            "-c",
            "user.email=else@example.invalid",
            "commit-tree",
            &tree,
            "-p",
            commit,
            "-m",
            "somebody else's work",
        ]);
        self.move_the_branch(chat_id, &ahead);
        ahead
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
    (dir, Remotes::new(served_from))
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
