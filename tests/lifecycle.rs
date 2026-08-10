//! Integration tests for the session lifecycle: the warm pool's cap, the
//! archive gate that empties a workspace onto the remote, and the sweep that
//! keeps `workspaces/` honest (ADR-0002, ADR-0005).

use std::fs;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use argon2::password_hash::{PasswordHasher as _, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use reqwest::{Client, StatusCode, redirect::Policy};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use cor_code::acp::ScriptedAdapter;
use cor_code::chats::Chats;
use cor_code::config::{
    Config, DEFAULT_CONTAINER_CPUS, DEFAULT_CONTAINER_MEMORY_MB, DEFAULT_SCRATCH_MB,
    DEFAULT_WARM_POOL,
};
use cor_code::git::Remotes;
use cor_code::plane::{ContainerPlane as _, MemoryPlane, StopGrace, Teardown};
use cor_code::secrets::Secrets;
use cor_code::server;
use cor_code::settings::Settings;
use cor_code::store::{ChatStore, Owner};
use cor_code::verify::ScriptedVerifier;

const USERNAME: &str = "cassidy";
const PASSWORD: &str = "correct horse battery staple";
const REPO: &str = "CorVous/fixture";
const BARE: &str = "CorVous/fixture.git";
const SESSION: &str = "3f2b1c4d-0000-4000-8000-000000000001";

/// What one line out of a slow agent costs.
const DAWDLE: Duration = Duration::from_millis(200);

#[tokio::test]
async fn a_chat_beyond_the_pool_gives_up_its_container_and_keeps_its_workspace() {
    let app = TestApp::start().await;
    let first = app.create_chat("first").await;
    let second = app.create_chat("second").await;

    let third = app.create_chat("third").await;

    let console = app.body("/").await;
    assert_eq!(group(&console, &first), "Parked");
    assert_eq!(group(&console, &second), "Live");
    assert_eq!(group(&console, &third), "Live");
    assert!(
        app.workspace(&first).join("README.md").is_file(),
        "parking took the workspace with it"
    );
    assert_eq!(
        app.manifest(&first)["state"],
        "open",
        "parking is not a state on disk (ADR-0002)"
    );
}

/// Opening a chat is a read (ADR-0007 rule 1): the page is the event log off
/// disk, and looking at a parked chat must not start anything.
#[tokio::test]
async fn opening_a_parked_chat_leaves_it_parked() {
    let app = TestApp::start().await;
    let parked = app.create_chat("first").await;
    app.create_chat("second").await;
    app.create_chat("third").await;

    app.body(&format!("/chats/{parked}")).await;
    app.body(&format!("/chats/{parked}/events")).await;

    assert_eq!(
        group(&app.body("/").await, &parked),
        "Parked",
        "reading a chat woke it"
    );
}

#[tokio::test]
async fn a_prompt_into_a_parked_chat_puts_its_container_back_up() {
    let app = TestApp::start().await;
    let parked = app.create_chat("first").await;
    app.create_chat("second").await;
    app.create_chat("third").await;

    let response = app.prompt(&parked, "still there?").await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        group(&app.body("/").await, &parked),
        "Live",
        "the chat took a turn without a container of its own"
    );
}

#[tokio::test]
async fn a_completed_turn_moves_its_chat_to_the_front_of_the_pool() {
    let app = TestApp::start().await;
    app.create_chat("first").await;
    let second = app.create_chat("second").await;
    let third = app.create_chat("third").await;

    let turn = app.prompt(&second, "ship the ladder").await;
    let fourth = app.create_chat("fourth").await;

    assert_eq!(turn.status(), StatusCode::OK);
    let console = app.body("/").await;
    assert_eq!(
        group(&console, &third),
        "Parked",
        "the chat that took a turn was parked ahead of an idler: {console}"
    );
    assert_eq!(group(&console, &second), "Live");
    assert_eq!(group(&console, &fourth), "Live");
}

/// The evicted container's keep-alive discards the signal, so every second
/// of grace is a second the request that ordered the eviction waits for a
/// kill it could have asked for at once (issue #40).
#[tokio::test]
async fn parking_a_chat_stops_its_container_without_grace() {
    let app = TestApp::start().await;
    let parked = app.create_chat("first").await;
    app.create_chat("second").await;

    app.create_chat("third").await;

    assert_eq!(
        app.teardowns(),
        vec![Teardown {
            chat_id: parked,
            grace: StopGrace::Zero,
        }]
    );
}

/// Connections live only in memory, so every open chat a restarted core
/// inherits holds a running container and nothing to talk to it over. That
/// container is a keep-alive like any parked one, and archiving it must not
/// buy it a grace nothing in it can spend (issue #40).
#[tokio::test]
async fn archiving_a_chat_whose_container_outlived_its_connection_stops_it_without_grace() {
    let app = TestApp::start().await;
    let chat = app.create_chat("first").await;
    app.create_chat("second").await;
    app.create_chat("third").await;
    app.spawn_container_for(&chat).await;

    app.archive(&chat).await;

    assert_eq!(
        app.teardowns().last(),
        Some(&Teardown {
            chat_id: chat,
            grace: StopGrace::Zero,
        }),
        "the container the archive stopped is the one spawned without a connection"
    );
}

/// A chat still holding its adapter is torn down with the plane's whole
/// grace: what is in the container is the agent's, not a keep-alive's.
#[tokio::test]
async fn archiving_a_live_chat_leaves_its_container_the_whole_grace() {
    let app = TestApp::start().await;
    let chat = app.create_chat("first").await;

    app.archive(&chat).await;

    assert_eq!(
        app.teardowns(),
        vec![Teardown {
            chat_id: chat,
            grace: StopGrace::Full,
        }]
    );
}

#[tokio::test]
async fn archiving_a_clean_chat_pushes_its_branch_and_empties_its_workspace() {
    let app = TestApp::start().await;
    let chat = app.create_chat("first").await;
    let semantic = app.commit_in_workspace(&chat, "the agent's own commit");

    let response = app.archive(&chat).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        app.origin_says(&["rev-parse", &app.branch(&chat)]),
        semantic,
        "the chat branch did not reach the remote"
    );
    let manifest = app.manifest(&chat);
    assert_eq!(manifest["state"], "archived");
    assert_eq!(manifest["last_pushed_commit"], semantic);
    assert_eq!(manifest["checkpoint_branch"], Value::Null);
    assert!(
        !app.workspace(&chat).exists(),
        "an archived chat kept its workspace (ADR-0002 rule 1)"
    );
    assert_eq!(group(&app.body("/").await, &chat), "Archived");
}

#[tokio::test]
async fn archiving_a_dirty_chat_checkpoints_it_and_leaves_the_branch_where_the_agent_left_it() {
    let app = TestApp::start().await;
    let chat = app.create_chat("first").await;
    let semantic = app.commit_in_workspace(&chat, "the agent's own commit");
    fs::write(app.workspace(&chat).join("scratch.txt"), "work in flight")
        .expect("a dirty file should be writable");

    let response = app.archive(&chat).await;

    assert_eq!(response.status(), StatusCode::OK);
    let checkpoint = app.manifest(&chat)["checkpoint_branch"]
        .as_str()
        .expect("a dirty tree should be archived onto a checkpoint branch")
        .to_owned();
    assert_eq!(
        app.origin_says(&["rev-parse", &app.branch(&chat)]),
        semantic,
        "the chat branch left the agent's last semantic commit"
    );
    assert_eq!(
        app.origin_says(&["show", &format!("{checkpoint}:scratch.txt")]),
        "work in flight",
        "the work in flight never reached the remote"
    );
}

#[tokio::test]
async fn a_chat_whose_push_fails_keeps_its_container_and_says_so() {
    let app = TestApp::start().await;
    let chat = app.create_chat("first").await;
    fs::write(app.workspace(&chat).join("scratch.txt"), "work in flight")
        .expect("a dirty file should be writable");
    app.lose_the_remote();

    let response = app.archive(&chat).await;

    assert!(
        response.status().is_server_error(),
        "a failed archive answered {}",
        response.status()
    );
    assert_eq!(app.manifest(&chat)["state"], "open");
    assert!(
        app.workspace(&chat).join("scratch.txt").is_file(),
        "the work nobody pushed was deleted anyway"
    );
    assert_eq!(group(&app.body("/").await, &chat), "Live");
    assert!(
        app.events(&chat).contains("push_failure"),
        "the failure was not written down: {}",
        app.events(&chat)
    );
    let log = app.body(&format!("/chats/{chat}/events")).await;
    assert!(
        log.contains("<blockquote"),
        "the operator is told nothing when they next look: {log}"
    );
}

/// The checkpoint is pushed before the chat's own branch, so the second push
/// can fail with the operator's work already safe on the remote. The notice
/// has to say so, or they will read "nothing was archived" and go looking for
/// work that is right there.
#[tokio::test]
async fn a_checkpoint_that_landed_is_named_when_the_chat_branch_will_not_go() {
    let app = TestApp::start().await;
    let chat = app.create_chat("first").await;
    fs::write(app.workspace(&chat).join("scratch.txt"), "work in flight")
        .expect("a dirty file should be writable");
    app.refuse_the_chat_branch();

    let response = app.archive(&chat).await;

    assert!(
        response.status().is_server_error(),
        "a half-finished archive answered {}",
        response.status()
    );
    let landed = app
        .origin_says(&["for-each-ref", "--format=%(refname:short)", "refs/heads/"])
        .lines()
        .find(|branch| branch.contains("-chkpt-"))
        .expect("the checkpoint should reach the remote before the branch is refused")
        .to_owned();
    assert!(
        app.events(&chat).contains(&landed),
        "the notice does not name the checkpoint that landed: {}",
        app.events(&chat)
    );
    let manifest = app.manifest(&chat);
    assert_eq!(manifest["state"], "open");
    assert_eq!(
        manifest["checkpoint_branch"],
        Value::Null,
        "the manifest was written for an archive that did not happen"
    );
    assert!(app.workspace(&chat).join("scratch.txt").is_file());
    assert_eq!(group(&app.body("/").await, &chat), "Live");

    app.allow_the_chat_branch();
    assert_eq!(
        app.archive(&chat).await.status(),
        StatusCode::OK,
        "the operator could not archive again once the remote took the branch"
    );
}

/// A remote that has moved past the chat refuses the chat's branch, and a
/// chat nobody can close is worse than a branch nobody expected: the work goes
/// onto a rescue branch of its own and the archive finishes (issue #50).
#[tokio::test]
async fn a_chat_branch_the_remote_refuses_is_archived_onto_a_rescue_branch() {
    let app = TestApp::start().await;
    let chat = app.create_chat("first").await;
    let semantic = app.commit_in_workspace(&chat, "the agent's own commit");
    let outsider = app.push_over_the_chat_branch(&app.branch(&chat));

    let response = app.archive(&chat).await;

    assert_eq!(response.status(), StatusCode::OK);
    let rescue = app
        .origin_branches()
        .into_iter()
        .find(|branch| branch.contains("-rescue-"))
        .expect("the refused work should reach the remote on a rescue branch");
    assert_eq!(
        app.origin_says(&["rev-parse", &rescue]),
        semantic,
        "the rescue branch does not carry the work the chat could not push"
    );
    assert_eq!(app.manifest(&chat)["state"], "archived");
    assert!(
        !app.workspace(&chat).exists(),
        "a rescued archive left the chat holding its workspace"
    );
    let told = app.told(&chat);
    let note = told
        .iter()
        .find(|line| line["corcode"] == "rescue_branch")
        .and_then(|line| line["text"].as_str())
        .unwrap_or_else(|| panic!("nothing tells the operator where their work went: {told:?}"));
    assert!(
        note.contains(&rescue),
        "the notice does not name the branch that holds the work: {note}"
    );
    assert_eq!(
        app.origin_says(&["rev-parse", &app.branch(&chat)]),
        outsider,
        "the chat's branch was forced over what the remote already had"
    );
}

/// A dirty tree and a refused branch at once: the checkpoint goes under its
/// own name, the chat's commits go onto a rescue branch, and the manifest
/// records both of them (issue #50).
#[tokio::test]
async fn a_dirty_chat_the_remote_refuses_is_checkpointed_and_rescued_at_once() {
    let app = TestApp::start().await;
    let chat = app.create_chat("first").await;
    let semantic = app.commit_in_workspace(&chat, "the agent's own commit");
    fs::write(app.workspace(&chat).join("scratch.txt"), "work in flight")
        .expect("a dirty file should be writable");
    app.push_over_the_chat_branch(&app.branch(&chat));

    let response = app.archive(&chat).await;

    assert_eq!(response.status(), StatusCode::OK);
    let manifest = app.manifest(&chat);
    let checkpoint = manifest["checkpoint_branch"]
        .as_str()
        .expect("a dirty tree should be archived onto a checkpoint branch");
    assert_eq!(
        app.origin_says(&["show", &format!("{checkpoint}:scratch.txt")]),
        "work in flight",
        "the work in flight never reached the remote"
    );
    assert_eq!(
        manifest["last_pushed_commit"], semantic,
        "the manifest does not name the commit the rescue branch carries"
    );
    let rescue = app
        .origin_branches()
        .into_iter()
        .find(|branch| branch.contains("-rescue-"))
        .expect("the refused work should reach the remote on a rescue branch");
    assert_eq!(app.origin_says(&["rev-parse", &rescue]), semantic);
}

/// The rescue can land and the archive fail after it, on the manifest write.
/// The retry finds the chat's work already rescued and says so: one rescue
/// branch on the remote however often the operator tries (issue #82).
#[tokio::test]
async fn an_archive_retried_after_its_rescue_landed_mints_no_second_rescue() {
    let app = TestApp::start().await;
    let chat = app.create_chat("first").await;
    let semantic = app.commit_in_workspace(&chat, "the agent's own commit");
    app.push_over_the_chat_branch(&app.branch(&chat));
    app.seal_the_chat_dir(&chat);
    let stopped = app.archive(&chat).await;
    app.unseal_the_chat_dir(&chat);
    assert!(
        stopped.status().is_server_error(),
        "an unrecorded archive answered {}",
        stopped.status()
    );
    let landed = app.rescue_branches();
    assert_eq!(
        landed.len(),
        1,
        "the archive that failed did not leave a rescue behind: {landed:?}"
    );

    let retried = app.archive(&chat).await;

    assert_eq!(retried.status(), StatusCode::OK);
    assert_eq!(
        app.rescue_branches(),
        landed,
        "the retry did not close the chat on the rescue its work was already on"
    );
    assert_eq!(app.origin_says(&["rev-parse", &landed[0]]), semantic);
    assert!(
        app.last_rescue_note(&chat).contains(&landed[0]),
        "the notice does not name the branch that holds the work: {}",
        app.last_rescue_note(&chat)
    );
}

/// The rescue a retry finds is the one holding the work it is pushing, not
/// whatever rescue the chat has been through before: an agent that committed
/// again between the attempts is rescued again, onto a branch of its own
/// (issue #82).
#[tokio::test]
async fn work_committed_after_a_rescue_landed_is_rescued_onto_a_branch_of_its_own() {
    let app = TestApp::start().await;
    let chat = app.create_chat("first").await;
    app.commit_in_workspace(&chat, "the agent's own commit");
    app.push_over_the_chat_branch(&app.branch(&chat));
    app.seal_the_chat_dir(&chat);
    let stopped = app.archive(&chat).await;
    app.unseal_the_chat_dir(&chat);
    assert!(stopped.status().is_server_error());
    let stale = app.rescue_branches();
    let later = app.commit_in_workspace(&chat, "what the agent did next");

    let retried = app.archive(&chat).await;

    assert_eq!(retried.status(), StatusCode::OK);
    let rescues = app.rescue_branches();
    assert_eq!(
        rescues.len(),
        2,
        "the later commit was not rescued onto a branch of its own: {rescues:?}"
    );
    let fresh = rescues
        .iter()
        .find(|rescue| !stale.contains(rescue))
        .expect("the retry should mint a rescue for the work it is pushing");
    assert_eq!(
        app.origin_says(&["rev-parse", fresh]),
        later,
        "the fresh rescue does not carry the commit the retry pushed"
    );
    assert!(
        app.last_rescue_note(&chat).contains(fresh),
        "the notice sends the operator to a rescue without their latest work: {}",
        app.last_rescue_note(&chat)
    );
}

/// The gate is not the last thing that can fail: the manifest is written
/// after it. Whatever the operator retries from has to be a workspace on the
/// chat's own branch.
#[tokio::test]
async fn an_archive_the_dataset_cannot_record_leaves_the_workspace_on_the_chat_branch() {
    let app = TestApp::start().await;
    let chat = app.create_chat("first").await;
    fs::write(app.workspace(&chat).join("scratch.txt"), "work in flight")
        .expect("a dirty file should be writable");
    app.seal_the_chat_dir(&chat);

    let response = app.archive(&chat).await;
    app.unseal_the_chat_dir(&chat);

    assert!(
        response.status().is_server_error(),
        "an unrecorded archive answered {}",
        response.status()
    );
    assert!(
        app.workspace(&chat).exists(),
        "the workspace went even though the chat is still open (ADR-0002 rule 1)"
    );
    assert_eq!(
        says(
            &app.workspace(&chat),
            &["rev-parse", "--abbrev-ref", "HEAD"]
        ),
        app.branch(&chat),
        "the retry would start from a checkpoint branch"
    );
}

/// The gate stages and commits the whole working tree, and then kills the
/// container: doing that to an agent mid-turn destroys work git never saw
/// (ADR-0002 rule 3).
#[tokio::test]
async fn an_archive_over_a_running_turn_is_refused_and_tears_nothing_down() {
    let app = TestApp::dawdling().await;
    let chat = app.create_chat("first").await;

    let in_flight = app.spawn_prompt(&chat, "ship the ladder");
    tokio::time::sleep(DAWDLE).await;
    let refused = app.archive(&chat).await;

    assert_eq!(refused.status(), StatusCode::CONFLICT);
    assert_eq!(app.manifest(&chat)["state"], "open");
    assert!(app.workspace(&chat).join("README.md").is_file());
    assert_eq!(group(&app.body("/").await, &chat), "Live");
    assert_eq!(
        in_flight.await.expect("the turn should not panic").status(),
        StatusCode::OK,
        "the refused archive interrupted the turn"
    );
}

/// A parked chat holds no connection, so an archive that only looks for one
/// finds a chat it believes is doing nothing while a prompt is mid-wake — and
/// tears down the container that wake just started, workspace and all
/// (issue #93).
#[tokio::test]
async fn an_archive_over_a_chat_being_woken_is_refused_and_tears_nothing_down() {
    let app = TestApp::dawdling().await;
    let parked = app.create_chat("first").await;
    app.create_chat("second").await;
    app.create_chat("third").await;

    let waking = app.spawn_prompt(&parked, "still there?");
    tokio::time::sleep(DAWDLE).await;
    let refused = app.archive(&parked).await;

    assert_eq!(refused.status(), StatusCode::CONFLICT);
    let told = refused.text().await.expect("a refusal says why");
    assert!(
        told.contains("being woken"),
        "the archive was refused over a turn nobody is taking: {told}"
    );
    assert_eq!(app.manifest(&parked)["state"], "open");
    assert!(
        app.workspace(&parked).join("README.md").is_file(),
        "the workspace went out from under a waking chat (ADR-0002 rule 3)"
    );
    assert_eq!(
        waking.await.expect("the turn should not panic").status(),
        StatusCode::OK,
        "the refused archive interrupted the wake"
    );
    assert_eq!(group(&app.body("/").await, &parked), "Live");
}

/// htmx drops the answer to a refused archive, so the 409 reaches nobody: the
/// chat's own log is the only place the operator, who is looking at the chat
/// page, can read why the Archive button did nothing (issue #102).
#[tokio::test]
async fn a_refused_archive_says_why_in_the_chats_own_log() {
    let app = TestApp::dawdling().await;
    let parked = app.create_chat("first").await;
    app.create_chat("second").await;
    app.create_chat("third").await;

    let waking = app.spawn_prompt(&parked, "still there?");
    tokio::time::sleep(DAWDLE).await;
    let refused = app.archive(&parked).await;

    assert_eq!(refused.status(), StatusCode::CONFLICT);
    let told = app.told(&parked);
    let refusal = told
        .iter()
        .filter(|line| line["corcode"] == "refusal")
        .filter_map(|line| line["text"].as_str())
        .next_back()
        .unwrap_or_else(|| panic!("the refused archive was never told of: {told:?}"));
    assert!(
        refusal.contains("being woken"),
        "the refusal does not say what the chat is doing: {refusal}"
    );
    let page = app.body(&format!("/chats/{parked}/events")).await;
    assert!(
        page.contains(refusal),
        "the chat page does not show the refusal the operator is waiting on: {page}"
    );
    assert_eq!(
        waking.await.expect("the turn should not panic").status(),
        StatusCode::OK
    );
}

/// The other order: the archive is already committing and deleting a working
/// tree when the prompt arrives. Waking the chat now would put a container over
/// a workspace that is about to go, so the prompt is turned away and told what
/// the chat is doing (issue #93).
#[tokio::test]
async fn a_prompt_arriving_mid_archive_is_turned_away_rather_than_waking_the_chat() {
    let app = TestApp::start().await;
    let closing = app.create_chat("first").await;
    app.create_chat("second").await;
    app.create_chat("third").await;
    app.commit_in_workspace(&closing, "the agent's own commit");
    app.slow_the_remote();

    let archiving = app.spawn_archive(&closing);
    tokio::time::sleep(DAWDLE).await;
    let refused = app.prompt(&closing, "still there?").await;

    assert_eq!(refused.status(), StatusCode::CONFLICT);
    assert_eq!(
        archiving
            .await
            .expect("the archive should not panic")
            .status(),
        StatusCode::OK
    );
    assert_eq!(app.manifest(&closing)["state"], "archived");
    assert!(!app.workspace(&closing).exists());
    assert!(
        app.plane.mounts_of(&closing).is_none(),
        "the prompt left a container running over a deleted workspace"
    );
    let told = app.told(&closing);
    let refusal = told
        .iter()
        .filter(|line| line["corcode"] == "refusal")
        .filter_map(|line| line["text"].as_str())
        .next_back()
        .unwrap_or_else(|| panic!("the turned-away prompt was never told so: {told:?}"));
    assert!(
        refusal.contains("archiv"),
        "the refusal does not say what the chat is doing: {refusal}"
    );
}

/// Two archives over one chat: the second is turned away by the same claim,
/// and is told the archive it lost to rather than a turn nobody is taking
/// (issue #93).
#[tokio::test]
async fn a_second_archive_arriving_mid_archive_is_told_the_first_one_is_running() {
    let app = TestApp::start().await;
    let closing = app.create_chat("first").await;
    app.commit_in_workspace(&closing, "the agent's own commit");
    app.slow_the_remote();

    let archiving = app.spawn_archive(&closing);
    tokio::time::sleep(DAWDLE).await;
    let refused = app.archive(&closing).await;

    assert_eq!(refused.status(), StatusCode::CONFLICT);
    let told = refused.text().await.expect("a refusal says why");
    assert!(
        told.contains("already being archived"),
        "the second archive was refused over a turn nobody is taking: {told}"
    );
    assert_eq!(
        archiving
            .await
            .expect("the archive should not panic")
            .status(),
        StatusCode::OK
    );
    assert_eq!(app.manifest(&closing)["state"], "archived");
}

/// `last_active_at` is only written when a turn ends, so the chat that has
/// been answering longest looks like the stalest chat there is.
#[tokio::test]
async fn a_chat_in_the_middle_of_a_turn_is_not_parked_by_another_chats_arrival() {
    let app = TestApp::dawdling().await;
    let answering = app.create_chat("first").await;
    app.create_chat("second").await;

    let in_flight = app.spawn_prompt(&answering, "ship the ladder");
    tokio::time::sleep(DAWDLE).await;
    app.create_chat("third").await;

    assert_eq!(
        group(&app.body("/").await, &answering),
        "Live",
        "the pool parked a chat mid-stream"
    );
    assert_eq!(
        in_flight.await.expect("the turn should not panic").status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn archiving_an_archived_chat_runs_no_git_and_says_it_is_already_done() {
    let app = TestApp::start().await;
    let chat = app.create_chat("first").await;
    app.archive(&chat).await;
    let archived = app.events(&chat);

    let again = app.archive(&chat).await;

    assert_eq!(again.status(), StatusCode::CONFLICT);
    assert_eq!(app.manifest(&chat)["state"], "archived");
    assert_eq!(
        app.events(&chat),
        archived,
        "the second archive wrote into a finished transcript"
    );
}

#[tokio::test]
async fn archiving_a_chat_also_sweeps_the_working_trees_nothing_claims() {
    let app = TestApp::start().await;
    let chat = app.create_chat("first").await;
    let orphan = app.stray_workspace();

    app.archive(&chat).await;

    assert!(
        !orphan.exists(),
        "the sweep left a pile behind (ADR-0002 rule 4)"
    );
}

/// Which of the console's headings a chat is rendered under.
fn group(console: &str, chat_id: &str) -> String {
    let at = console
        .find(chat_id)
        .unwrap_or_else(|| panic!("{chat_id} is not on the console: {console}"));
    ["Live", "Parked", "Archived"]
        .into_iter()
        .rfind(|heading| {
            console
                .find(&format!("<h2>{heading}</h2>"))
                .is_some_and(|found| found < at)
        })
        .expect("every chat sits under a heading")
        .to_owned()
}

struct TestApp {
    address: SocketAddr,
    cookie: String,
    data_dir: TempDir,
    plane: MemoryPlane,
    origin: TempDir,
    _server: JoinHandle<anyhow::Result<()>>,
    _shutdown: oneshot::Sender<()>,
}

impl TestApp {
    async fn start() -> Self {
        Self::serving(ScriptedAdapter::answering(SESSION, &[update("on it")])).await
    }

    /// An app whose agent answers in many slow lines, so that a turn stays in
    /// flight long enough for another request to be made against it.
    async fn dawdling() -> Self {
        let unhurried: Vec<Value> = (0..8).map(|line| update(&format!("line {line}"))).collect();
        Self::serving(ScriptedAdapter::dawdling(SESSION, &unhurried, DAWDLE)).await
    }

    async fn serving(adapter: ScriptedAdapter) -> Self {
        let data_dir = TempDir::new().expect("temp dir should be creatable");
        let (origin, remotes) = seeded_repository();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral port should bind");
        let address = listener.local_addr().expect("listener reports its address");
        let config = test_config(data_dir.path().to_path_buf());
        ChatStore::new(data_dir.path())
            .prepare()
            .expect("the dataset should prepare, as serving does");
        let secrets = Arc::new(Secrets::from_config(&config));
        let plane = MemoryPlane::default();
        let chats = Chats::new(
            &config,
            Owner::of(&config.data_dir).expect("we own the dataset we just made"),
            plane.clone(),
            adapter,
            remotes,
            Arc::clone(&secrets),
        );
        let router = server::router(
            &config,
            chats,
            Settings::new(secrets, ScriptedVerifier::default()),
        )
        .expect("router should build");
        let (shutdown, shutdown_rx) = oneshot::channel();
        let server = tokio::spawn(server::serve(listener, router, async {
            shutdown_rx.await.ok();
        }));
        Self {
            cookie: sign_in(address).await,
            address,
            data_dir,
            plane,
            origin,
            _server: server,
            _shutdown: shutdown,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    async fn create_chat(&self, slug: &str) -> String {
        let response = client()
            .post(self.url("/chats"))
            .header("cookie", &self.cookie)
            .form(&[("repo", REPO), ("base_branch", "main"), ("slug", slug)])
            .send()
            .await
            .expect("request");
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "chat not created");
        response
            .headers()
            .get("location")
            .expect("a created chat redirects to itself")
            .to_str()
            .expect("a location is text")
            .strip_prefix("/chats/")
            .expect("the redirect points at a chat")
            .to_owned()
    }

    async fn prompt(&self, chat_id: &str, said: &str) -> reqwest::Response {
        client()
            .post(self.url(&format!("/chats/{chat_id}/prompt")))
            .header("cookie", &self.cookie)
            .form(&[("prompt", said)])
            .send()
            .await
            .expect("request")
    }

    /// A turn put in flight, to be awaited once the test has looked at what
    /// happens while it runs.
    fn spawn_prompt(&self, chat_id: &str, said: &str) -> JoinHandle<reqwest::Response> {
        let request = client()
            .post(self.url(&format!("/chats/{chat_id}/prompt")))
            .header("cookie", &self.cookie)
            .form(&[("prompt", said)]);
        tokio::spawn(async move { request.send().await.expect("request") })
    }

    /// An archive put in flight, to be awaited once the test has looked at
    /// what happens while it runs.
    fn spawn_archive(&self, chat_id: &str) -> JoinHandle<reqwest::Response> {
        let request = client()
            .post(self.url(&format!("/chats/{chat_id}/archive")))
            .header("cookie", &self.cookie);
        tokio::spawn(async move { request.send().await.expect("request") })
    }

    async fn archive(&self, chat_id: &str) -> reqwest::Response {
        client()
            .post(self.url(&format!("/chats/{chat_id}/archive")))
            .header("cookie", &self.cookie)
            .send()
            .await
            .expect("request")
    }

    async fn body(&self, path: &str) -> String {
        let response = client()
            .get(self.url(path))
            .header("cookie", &self.cookie)
            .send()
            .await
            .expect("request");
        assert_eq!(response.status(), StatusCode::OK, "{path} did not answer");
        response.text().await.expect("body")
    }

    /// Every container the plane behind this app stopped, in order.
    fn teardowns(&self) -> Vec<Teardown> {
        self.plane.teardowns()
    }

    /// Put a container under a chat that has none, the way a restarted core
    /// finds one: running, and with no connection into it.
    async fn spawn_container_for(&self, chat_id: &str) {
        self.plane
            .spawn(
                chat_id,
                &self.workspace(chat_id),
                &self.chat_dir(chat_id).join("claude"),
                &std::collections::BTreeMap::new(),
            )
            .await
            .expect("a chat with no container should take one");
    }

    fn workspace(&self, chat_id: &str) -> PathBuf {
        self.data_dir.path().join("workspaces").join(chat_id)
    }

    fn manifest(&self, chat_id: &str) -> Value {
        let path = self.chat_dir(chat_id).join("manifest.json");
        serde_json::from_str(&fs::read_to_string(&path).expect("the manifest should be readable"))
            .expect("the manifest should be json")
    }

    fn events(&self, chat_id: &str) -> String {
        fs::read_to_string(self.chat_dir(chat_id).join("events.jsonl"))
            .expect("the event log should be readable")
    }

    /// The payloads the chat's log holds, out of the stamps they are written
    /// under.
    fn told(&self, chat_id: &str) -> Vec<Value> {
        self.events(chat_id)
            .lines()
            .map(|line| {
                let written: Value = serde_json::from_str(line).expect("a line should be json");
                written["event"].clone()
            })
            .collect()
    }

    fn chat_dir(&self, chat_id: &str) -> PathBuf {
        self.data_dir.path().join("chats").join(chat_id)
    }

    fn branch(&self, chat_id: &str) -> String {
        self.manifest(chat_id)["branch"]
            .as_str()
            .expect("a chat names its branch")
            .to_owned()
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
            run(&workspace, &args);
        }
        says(&workspace, &["rev-parse", "HEAD"])
    }

    /// What the remote says, once whatever was pushed has reached it.
    fn origin_says(&self, args: &[&str]) -> String {
        says(&self.origin.path().join(BARE), args)
    }

    /// Every branch the remote carries.
    fn origin_branches(&self) -> Vec<String> {
        self.origin_says(&["for-each-ref", "--format=%(refname:short)", "refs/heads/"])
            .lines()
            .map(ToOwned::to_owned)
            .collect()
    }

    /// Every rescue branch the remote carries, in the order it lists them.
    fn rescue_branches(&self) -> Vec<String> {
        self.origin_branches()
            .into_iter()
            .filter(|branch| branch.contains("-rescue-"))
            .collect()
    }

    /// The last thing the chat's log told the operator about a rescue.
    fn last_rescue_note(&self, chat_id: &str) -> String {
        let told = self.told(chat_id);
        told.iter()
            .filter(|line| line["corcode"] == "rescue_branch")
            .filter_map(|line| line["text"].as_str())
            .next_back()
            .unwrap_or_else(|| panic!("nothing tells the operator where their work went: {told:?}"))
            .to_owned()
    }

    /// Somebody else's commit on the chat's own branch, as a push from outside
    /// this core leaves it: the chat's own push is now a non-fast-forward the
    /// remote will refuse.
    fn push_over_the_chat_branch(&self, branch: &str) -> String {
        let work = self.origin.path().join("seed");
        fs::write(work.join("elsewhere.txt"), "somebody else's work")
            .expect("a file should be writable");
        run(&work, &["add", "."]);
        run(&work, &["commit", "-m", "somebody else's work"]);
        run(
            &work,
            &["push", "origin", &format!("HEAD:refs/heads/{branch}")],
        );
        says(&work, &["rev-parse", "HEAD"])
    }

    /// A working tree left behind by a chat this dataset has never heard of,
    /// as a crash mid-teardown would leave one.
    fn stray_workspace(&self) -> PathBuf {
        let stray = self.workspace("01KSTRAYWORKSPACE00000000");
        fs::create_dir_all(&stray).expect("a stray workspace should be creatable");
        stray
    }

    /// Take the remote away, so the next push has nowhere to land.
    fn lose_the_remote(&self) {
        fs::remove_dir_all(self.origin.path().join(BARE)).expect("the remote should be removable");
    }

    /// Make the remote refuse every branch but a checkpoint, as a protected
    /// branch does, so that half of a gate's pushes land.
    fn refuse_the_chat_branch(&self) {
        self.hook_the_remote("#!/bin/sh\ncase \"$1\" in *chkpt*) exit 0;; esac\nexit 1\n");
    }

    /// Put `shell` in the remote's update hook, so that every branch pushed to
    /// it goes through whatever the test wants to happen first.
    fn hook_the_remote(&self, shell: &str) {
        let hook = self.remote_hook();
        fs::write(&hook, shell).expect("the hook should be writable");
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))
            .expect("the hook should be runnable");
    }

    fn remote_hook(&self) -> PathBuf {
        self.origin.path().join(BARE).join("hooks").join("update")
    }

    /// Make the remote take its time over every branch, so that an archive can
    /// be caught with its gate still open.
    fn slow_the_remote(&self) {
        self.hook_the_remote("#!/bin/sh\nsleep 1\nexit 0\n");
    }

    fn allow_the_chat_branch(&self) {
        fs::remove_file(self.remote_hook()).expect("the hook should be removable");
    }

    /// Make the chat's own directory unwritable, so that the archive's next
    /// write to the dataset fails the way a full disk would.
    fn seal_the_chat_dir(&self, chat_id: &str) {
        self.chmod_chat_dir(chat_id, 0o555);
    }

    fn unseal_the_chat_dir(&self, chat_id: &str) {
        self.chmod_chat_dir(chat_id, 0o755);
    }

    fn chmod_chat_dir(&self, chat_id: &str, mode: u32) {
        fs::set_permissions(self.chat_dir(chat_id), fs::Permissions::from_mode(mode))
            .expect("the chat directory should be chmodable");
    }
}

/// One `session/update` notification's params, as the adapter sends them.
fn update(said: &str) -> Value {
    json!({
        "sessionId": SESSION,
        "update": {
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": said},
        },
    })
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
    says(cwd, args);
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

async fn sign_in(address: SocketAddr) -> String {
    let response = client()
        .post(format!("http://{address}/login"))
        .form(&[("username", USERNAME), ("password", PASSWORD)])
        .send()
        .await
        .expect("request");
    response
        .headers()
        .get("set-cookie")
        .expect("a correct login hands out a cookie")
        .to_str()
        .expect("cookie should be text")
        .split(';')
        .next()
        .expect("a cookie has a value")
        .to_owned()
}

fn test_config(data_dir: PathBuf) -> Config {
    Config {
        host_data_dir: data_dir.clone(),
        data_dir,
        bind_addr: "127.0.0.1:0".parse().expect("valid address"),
        username: USERNAME.to_owned(),
        password_hash: password_hash(PASSWORD),
        workspace_image: "ghcr.io/corvous/corcode-workspace:2026-08-05".to_owned(),
        container_memory_mb: DEFAULT_CONTAINER_MEMORY_MB,
        container_cpus: DEFAULT_CONTAINER_CPUS,
        scratch_mb: DEFAULT_SCRATCH_MB,
        warm_pool: DEFAULT_WARM_POOL,
        registry: None,
        repos: vec![REPO.to_owned()],
        github_token: None,
        anthropic_api_key: None,
    }
}

/// A deliberately cheap argon2 hash: these tests verify plenty of them.
fn password_hash(password: &str) -> String {
    let params = Params::new(Params::MIN_M_COST, 1, 1, None).expect("valid argon2 parameters");
    let salt = SaltString::from_b64("c2FsdHNhbHRzYWx0c2FsdA").expect("valid salt");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password(password.as_bytes(), &salt)
        .expect("password should hash")
        .to_string()
}

fn client() -> Client {
    Client::builder()
        .redirect(Policy::none())
        .build()
        .expect("client should build")
}
