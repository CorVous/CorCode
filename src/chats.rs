//! The chats the console reads and the vertical that cuts a new one
//! (ADR-0005, ADR-0006).

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Display;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use log::{error, info, warn};
use serde_json::json;
use thiserror::Error;
use ulid::Ulid;

use crate::acp::{AcpError, AcpTransport, Adapter, Connection, Connections, Held, Turn};
use crate::config::Config;
use crate::failure::with_causes;
use crate::git::{self, Remotes};
use crate::plane::{
    ContainerPlane, PlaneError, ScriptRun, StopGrace, container_name, managed_default_mode,
};
use crate::pool;
use crate::resume::{self, Attempt, Rung, Step};
use crate::secrets::{AnthropicCredential, Secret, Secrets, SecretsError};
use crate::status::{Containers, Slot, Status};
use crate::store::{
    self, ChatState, ChatStore, ContainerLiveness, Event, Manifest, NewChat, Owner, RuntimeStatus,
    StoreError, runtime_status,
};
use crate::sweep::{self, Sweep, Swept};
use crate::ui;

/// One pass of the dataset, and whether the plane took part in it.
///
/// With the containers unknown every open chat reads as parked, which is why
/// the console shows the statuses only where they were answered for
/// (issue #25).
pub struct Survey {
    pub chats: Vec<ui::Chat>,
    pub containers: Containers,
}

/// What the form asks for, before any of it is believed.
pub struct WantedChat {
    pub repo: String,
    pub base_branch: String,
    pub slug: String,
    pub direct_on_base: bool,
    /// Custom environment for the agent container, already parsed and validated
    /// at the form (issue #14). System credentials still win a name clash at
    /// spawn.
    pub env: BTreeMap<String, String>,
    /// A shell to run in the container once it is ready, or nothing.
    pub startup_script: Option<String>,
}

/// Why a chat was not created. A refusal is the request's fault and says so;
/// anything else is this deployment's, and the operator reads it in the log.
#[derive(Debug, Error)]
pub enum CreateError {
    #[error("that leaves no slug a branch could carry")]
    Unnamed,
    #[error("{repo} is neither an owner/name repository nor an https clone URL")]
    UnusableRepo { repo: String },
    #[error("{branch} is not a branch name")]
    UnusableBranch { branch: String },
    #[error("the chat could not be built")]
    Broke(#[source] anyhow::Error),
}

impl CreateError {
    /// Whether the request was turned down rather than the work failing.
    #[must_use]
    pub const fn is_refusal(&self) -> bool {
        !matches!(self, Self::Broke(_))
    }
}

fn broke(failure: impl Into<anyhow::Error>) -> CreateError {
    CreateError::Broke(failure.into())
}

/// Why a prompt never reached the agent, or never finished. Neither refusal is
/// the request's fault, so both say what the chat is doing instead.
#[derive(Debug, Error)]
pub enum PromptError {
    #[error("this chat could not be woken")]
    Unwoken(#[source] anyhow::Error),
    #[error("this chat is still answering the last prompt")]
    Busy,
    #[error("this chat is being woken for another prompt")]
    Waking,
    #[error("this chat is being archived")]
    Archiving,
    #[error("the turn could not be written down")]
    Unrecorded(#[source] anyhow::Error),
    #[error("the turn broke")]
    Broke(#[source] anyhow::Error),
}

impl PromptError {
    /// Whether the chat was turned away rather than the turn failing. A
    /// refusal is worth a line in the log; a failure is worth the operator's
    /// server log. A chat that could not be woken is told in its own log by
    /// whoever tried, which is the only place the reason is known.
    const fn is_refusal(&self) -> bool {
        matches!(self, Self::Busy | Self::Waking | Self::Archiving)
    }
}

/// Why a chat was not archived. Whichever this is, the chat is as it was:
/// nothing is torn down until everything in the workspace is on the remote
/// (ADR-0002 rule 3).
#[derive(Debug, Error)]
pub enum ArchiveError {
    #[error("this dataset holds no such chat")]
    NoSuchChat,
    #[error("this chat was archived already")]
    AlreadyArchived,
    #[error("this chat is in the middle of a turn")]
    Busy,
    #[error("this chat is being woken for a prompt")]
    Waking,
    #[error("this chat is already being archived")]
    Archiving,
    #[error("nothing reached the remote, so nothing was torn down")]
    NotPushed(#[source] anyhow::Error),
    #[error("the chat could not be archived")]
    Broke(#[source] anyhow::Error),
}

impl ArchiveError {
    /// Whether the request was turned down over what the chat is doing rather
    /// than the archive failing. A refusal touched nothing.
    #[must_use]
    pub const fn is_refusal(&self) -> bool {
        matches!(
            self,
            Self::AlreadyArchived | Self::Busy | Self::Waking | Self::Archiving
        )
    }

    /// Whether the chat's own log is where this refusal belongs. It is the
    /// only place the operator reads a refused archive, since the browser
    /// throws the answer away (issue #102) — but a chat that is archived
    /// already says so on its own page, and its log is the record of a chat
    /// that is closed, not a place to write down what was asked of it after.
    const fn is_worth_noting(&self) -> bool {
        self.is_refusal() && !matches!(self, Self::AlreadyArchived)
    }
}

fn unarchived(failure: impl Into<anyhow::Error>) -> ArchiveError {
    ArchiveError::Broke(failure.into())
}

/// What the chat's log is told about an archive that stopped part way. Every
/// branch that landed is named: that is where the operator's work is, and
/// nothing else will tell them (ADR-0006).
fn half_pushed(failure: &git::PushFailure) -> String {
    let retry = "The chat is still open and can be archived again.";
    let landed = failure.landed.branches();
    if landed.is_empty() {
        return format!("Nothing was archived: {failure}. {retry}");
    }
    format!(
        "The archive stopped part way: {failure}. \
         The work that got there is on the remote, on {}. {retry}",
        landed.join(" and ")
    )
}

/// What the chat's log is told when the remote would not take its branch and
/// the work went onto a rescue branch instead (issue #50). The archive is
/// done; this is the only place the operator can read where their work is.
fn rescued(branch: &str, rescue: &str) -> String {
    format!(
        "The remote would not take {branch}, so this chat's work was pushed to {rescue} instead. \
         Nothing on {branch} was overwritten: what the remote has there still stands."
    )
}

/// What a refusal calls itself in a chat's own log (ADR-0006).
const REFUSAL: &str = "refusal";

/// What a refused prompt says went nowhere.
const PROMPT_NOT_SENT: &str = "Prompt not sent";

/// What a refused archive says went nowhere (issue #102).
const NOT_ARCHIVED: &str = "Chat not archived";

/// What an archive that got nothing onto the remote calls itself there.
const PUSH_FAILURE: &str = "push_failure";

/// What an archive that had to put the chat's work somewhere else calls itself
/// there (issue #50).
const RESCUE_BRANCH: &str = "rescue_branch";

/// What waking a chat costs it calls itself in its log: memory the agent could
/// not get back, or a workspace that came back as a fresh clone (ADR-0006).
const RESET_NOTICE: &str = "reset_notice";

/// What a chat revived behind the branch it works on calls itself there
/// (issue #50).
const DRIFT_NOTICE: &str = "drift_notice";

/// What a prompt that never got as far as an agent calls itself there.
const WAKE_FAILURE: &str = "wake_failure";

/// What a session running in a mode nobody asked for calls itself in a chat's
/// log (ADR-0006, issue #58).
const MODE_NOTICE: &str = "mode_notice";

/// Claude Code's interactive mode, and the one the adapter clamps a session
/// down to: every call the model is unsure of is an ask, and this client
/// answers no to every ask it is put (ADR-0001).
const ASKING_MODE: &str = "default";

/// What the chat is told when its session opened in some other mode than the
/// one the image's managed settings ask for: the adapter clamps silently, and
/// nothing else the operator can read says which mode they are watching
/// (ADR-0001).
///
/// What that costs the chat is only said of the mode it costs anything in: a
/// session clamped into asking reads as a mute agent, and one in any other
/// mode is named and left at that rather than described wrongly.
fn clamped_mode(mode: &str) -> String {
    let asked = if mode == ASKING_MODE {
        " The agent asks before acting, every such ask is declined, \
         and so it can look as though it is doing nothing."
    } else {
        ""
    };
    format!(
        "This session opened in permission mode {mode}, not the {} this deployment asks for.{asked}",
        managed_default_mode(),
    )
}

/// What a startup script's outcome calls itself in a chat's log (issue #14).
const STARTUP_SCRIPT: &str = "startup_script";

/// How much of a startup script's output the transcript keeps: enough to read,
/// not so much it buries the chat (issue #14).
const STARTUP_OUTPUT_LIMIT: usize = 16 * 1024;

/// What the chat's log is told a startup script did: its exit code always, and
/// its output when it produced any, cut to a readable length (issue #14). A
/// clean run is a brief line; a failure names the code the operator must act on.
fn startup_summary(run: &ScriptRun) -> String {
    let mut told = if run.exit_code == 0 {
        "The startup script finished (exit 0).".to_owned()
    } else {
        format!("The startup script exited {}.", run.exit_code)
    };
    let output = run.output.trim();
    if !output.is_empty() {
        told.push_str("\n\n");
        told.push_str(&truncated(output));
    }
    told
}

/// The line the operator reads when a container refuses to go: which chat,
/// what it was being let go for, and everything the daemon said under the
/// runtime's own summary (issue #41).
fn stubborn_teardown(chat_id: &str, why: &str, stubborn: &PlaneError) -> String {
    format!(
        "{chat_id} ({why}) would not stop: {}",
        with_causes(stubborn)
    )
}

/// The line the operator reads when a working tree stays on disk: which
/// chat, what was left of it, and everything the filesystem said under the
/// store's own summary (issue #81). Only the cause says whether the operator
/// has a permission to fix or a mount to unbusy.
fn stubborn_workspace(chat_id: &str, what: &str, stubborn: &StoreError) -> String {
    format!(
        "{chat_id} left {what} that will not go: {}",
        with_causes(stubborn)
    )
}

/// The line the operator reads when a turn goes undated: which chat, and
/// everything the filesystem said under the store's own summary (issue #81).
/// The warm pool's order is only as good as these writes, so the cause is
/// what says whether the pool is about to park the wrong chats.
fn undated_turn(chat_id: &str, failure: &StoreError) -> String {
    format!(
        "{chat_id} took a turn that could not be dated: {}",
        with_causes(failure)
    )
}

/// The line the operator reads when the core's own voice does not reach a
/// chat's log: which kind of line was lost, and everything the filesystem
/// said under the store's own summary (issue #81). The chat's log is where
/// this would otherwise have been read, so this line is all there is.
fn unwritten_line(kind: &str, failure: &StoreError) -> String {
    format!(
        "a {kind} line could not be written down: {}",
        with_causes(failure)
    )
}

/// The line the operator reads when a chat's connection is dropped: which
/// chat, and everything under the ACP summary that says what it was dropped
/// after. Without it a disconnect leaves no trace on this side at all (issue
/// #66). A refusal drops the connection too, and the adapter that gave one is
/// alive — so the line says what followed, not that anything broke.
fn lost_the_connection(chat_id: &str, failure: &AcpError) -> String {
    format!(
        "{chat_id} dropped its adapter connection after: {}",
        with_causes(failure)
    )
}

/// What a turn that ended in a failure rather than an answer calls itself in
/// the chat's own log (issue #65).
const CONNECTION_LOST: &str = "connection_lost";

/// What the chat is told when its turn ended in a failure: what went wrong,
/// what became of the connection it went over, that the prompt can simply be
/// put again, and that the agent is not necessarily idle — the container is
/// still running whatever the turn was in the middle of.
///
/// Without this line the log ends at the prompt and the page shows a chat that
/// is still thinking, for as long as the operator is willing to believe it
/// (issue #65).
fn turn_lost(failure: &AcpError) -> String {
    match failure {
        AcpError::Garbled { sample, .. } => channel_corrupted(sample),
        AcpError::Unrecorded { source } => turn_unrecorded(&format!("{source:#}")),
        spent => connection_dropped(&with_causes(spent)),
    }
}

/// What the chat is told when the adapter's connection went with its turn: the
/// next prompt climbs ADR-0007's ladder over a fresh one, so it is a clean
/// thing to put again.
fn connection_dropped(why: &str) -> String {
    format!(
        "The turn ended without an answer: {why}. The connection to the agent was \
         dropped, so the prompt can be sent again. The agent may have kept working \
         inside its container meanwhile."
    )
}

/// What the chat is told when the turn failed at this end and left the
/// connection standing: the agent was never told to stop, so the rest of the
/// turn is still coming down a pipe nobody is reading, and the next prompt
/// reads it as its own reply. The core promises no clean re-send here rather
/// than draining a stream it cannot know the end of (issue #65).
fn turn_unrecorded(why: &str) -> String {
    format!(
        "The turn was abandoned because it could not be written down: {why}. The \
         connection to the agent is still open, but what the agent went on saying \
         is still in the pipe, so the next prompt may see the tail of this turn \
         before its own reply. The agent may have kept working inside its \
         container meanwhile."
    )
}

/// What the chat is told when its turn ended because the protocol channel
/// stopped carrying protocol: which is not a pipe that broke but a program the
/// agent started writing where the messages go, and the operator can act on the
/// difference. A container that keeps garbling every turn is one to archive and
/// revive, since the thing writing over the channel is inside it (issue #65).
fn channel_corrupted(sample: &str) -> String {
    format!(
        "Something the agent ran wrote over its own protocol channel, so what \
         reached the core stopped being messages (last read: {sample}). The turn \
         was abandoned and the connection dropped, but the agent may still be \
         running whatever it started inside its container. The prompt can be sent \
         again; if the chat stays stuck, archiving and reviving it gives the agent \
         a container of its own again."
    )
}

/// `output` cut to the transcript's limit on a character boundary, marked when
/// anything was dropped.
fn truncated(output: &str) -> String {
    if output.len() <= STARTUP_OUTPUT_LIMIT {
        return output.to_owned();
    }
    let mut end = STARTUP_OUTPUT_LIMIT;
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n(truncated)", &output[..end])
}

/// The names an agent's tooling reads the GitHub token from: `gh` — which the
/// image's credential helper answers over — prefers the first, and everything
/// else looks for the second. An agent pushes its own work (ADR-0005), so it
/// is handed the same token the core clones and archives over.
const GITHUB_TOKEN_VARIABLES: [&str; 2] = ["GH_TOKEN", "GITHUB_TOKEN"];

/// Why a chat's custom env block could not be believed, refused before any
/// chat is cut so a container is never spawned with an env it could not honour
/// (issue #14). Each names what to fix.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum EnvError {
    #[error("line {line} is not NAME=VALUE: every variable needs an '='")]
    NoEquals { line: usize },
    #[error("line {line} names no variable before its '='")]
    NoName { line: usize },
    #[error(
        "{name} is not a usable variable name: use letters, digits and underscore, and do not start with a digit"
    )]
    BadName { name: String },
    #[error("{name} is set by CorCode itself and cannot be overridden")]
    Reserved { name: String },
}

/// Every variable name the core sets for itself, so a form can turn a colliding
/// user var away before it would be dropped at spawn (ADR-0001). Derived from
/// the one place each name is spelled, so the guard cannot drift from the merge.
fn reserved_env_names() -> HashSet<String> {
    let mut names: HashSet<String> = GITHUB_TOKEN_VARIABLES
        .iter()
        .map(|&n| n.to_owned())
        .collect();
    names.insert(AnthropicCredential::OauthToken.variable().to_owned());
    names.insert(AnthropicCredential::ApiKey.variable().to_owned());
    names
}

/// Whether `name` is a shell variable name: a letter or underscore, then
/// letters, digits and underscores.
fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|char| char.is_ascii_alphanumeric() || char == '_')
}

/// Parse the form's env block into the variables a container is spawned with.
///
/// One `NAME=VALUE` per line, blank lines and lines beginning with `#` ignored,
/// the value everything after the first `=` kept verbatim. A name that is
/// missing, malformed, or one the core sets itself is refused (issue #14).
pub fn parse_env(raw: &str) -> Result<BTreeMap<String, String>, EnvError> {
    let reserved = reserved_env_names();
    let mut env = BTreeMap::new();
    for (index, line) in raw.lines().enumerate() {
        let line_number = index + 1;
        let line = line.strip_suffix('\r').unwrap_or(line);
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let (name, value) = line
            .split_once('=')
            .ok_or(EnvError::NoEquals { line: line_number })?;
        if name.is_empty() {
            return Err(EnvError::NoName { line: line_number });
        }
        if !is_env_name(name) {
            return Err(EnvError::BadName {
                name: name.to_owned(),
            });
        }
        if reserved.contains(name) {
            return Err(EnvError::Reserved {
                name: name.to_owned(),
            });
        }
        env.insert(name.to_owned(), value.to_owned());
    }
    Ok(env)
}

/// The right to decide who gives a container up, held by one capping at a
/// time.
type ParkingLock = tokio::sync::Mutex<()>;

/// What each claimed chat is in the middle of, which is the whole of what the
/// request turned away can be told (ADR-0006).
#[derive(Clone, Copy)]
enum Doing {
    Waking,
    Archiving,
}

/// The chats whose container is being decided right now, and by what. Waking
/// clones a workspace, starts a container and climbs a ladder; archiving
/// commits that workspace, stops the container and deletes the tree. Neither
/// may run twice at once over one chat, neither may run while the other does,
/// and no sweep or capping may cut across either (issue #93).
type Claims = std::sync::Mutex<HashMap<String, Doing>>;

/// What the last orphan sweep came to, kept for the status line to read
/// (ADR-0008). Nothing until one has run.
type LastSweep = std::sync::Mutex<Option<Swept>>;

/// One chat, given to whoever asked for it first and given back when whatever
/// took it is over however it ends.
struct Claim<'a> {
    claims: &'a Claims,
    chat_id: String,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.claims
            .lock()
            .expect("no holder of the lock panics")
            .remove(&self.chat_id);
    }
}

/// A chat ready to be prompted: the connection the turn goes over and, where
/// this prompt woke the chat, the claim it was woken under. The claim outlives
/// the wake and ends where the turn lock begins, so a woken chat is never for
/// an instant one an archive can read as idle (issue #101).
struct Woken<'a, C> {
    connection: Held<C>,
    claim: Option<Claim<'a>>,
}

/// Where a wake found its agent: a container that was already up, or one this
/// wake started and is therefore this wake's to give back.
struct Housing {
    container: String,
    spawned: bool,
}

/// What a climb up ADR-0007's ladder came to: a connection to prompt over, and
/// whether the agent on the other end of it remembers this chat.
struct Climbed<C> {
    connection: Connection<C>,
    forgot_everything: bool,
}

/// Every chat in one dataset: who is live, what they hold, and how a new one
/// comes to exist.
pub struct Chats<P, T: AcpTransport> {
    store: ChatStore,
    plane: P,
    adapter: Adapter<T>,
    connections: Connections<T::Channel>,
    remotes: Remotes,
    secrets: Arc<Secrets>,
    repos: Vec<String>,
    workspace_image: String,
    warm_pool: usize,
    parking: ParkingLock,
    claims: Claims,
    last_sweep: LastSweep,
}

impl<P, T: AcpTransport + Sync> Chats<P, T> {
    /// Serve the dataset `config` names, over `plane` and the adapters
    /// `transport` reaches, from the repositories `remotes` holds, on the
    /// credentials `secrets` holds, for the `agent` every workspace is handed
    /// to (ADR-0001).
    pub fn new(
        config: &Config,
        agent: Owner,
        plane: P,
        transport: T,
        remotes: Remotes,
        secrets: Arc<Secrets>,
    ) -> Self {
        Self {
            store: ChatStore::mounted(&config.data_dir, &config.host_data_dir)
                .handing_trees_to(agent),
            plane,
            adapter: Adapter::new(transport),
            connections: Connections::default(),
            remotes,
            secrets,
            repos: config.repos.clone(),
            workspace_image: config.workspace_image.clone(),
            warm_pool: config.warm_pool,
            parking: ParkingLock::default(),
            claims: Claims::default(),
            last_sweep: LastSweep::default(),
        }
    }
}

impl<P, T> Chats<P, T>
where
    P: ContainerPlane + ContainerLiveness + Sync,
    T: AcpTransport + Sync,
{
    /// The container picture as of `now`, over one pass of the dataset
    /// (ADR-0008).
    pub async fn status(&self, now: DateTime<Utc>) -> Result<Status> {
        Ok(self.status_of(&self.survey_or_unknown().await?, now))
    }

    /// The same picture read off a survey the caller already has, so that
    /// rendering the whole console costs one pass and not two.
    #[must_use]
    pub fn status_of(&self, survey: &Survey, now: DateTime<Utc>) -> Status {
        let chats = &survey.chats;
        Status {
            containers: survey.containers.clone(),
            pool: chats
                .iter()
                .filter(|(_, status)| *status == RuntimeStatus::Live)
                .map(|(manifest, _)| Slot {
                    title: manifest.title.clone(),
                    idle: now - manifest.last_active_at,
                })
                .collect(),
            warm_pool: self.warm_pool,
            parked: chats
                .iter()
                .filter(|(_, status)| *status == RuntimeStatus::Parked)
                .count(),
            image: self.workspace_image.clone(),
            sweep: self
                .last_sweep
                .lock()
                .expect("no holder of the lock panics")
                .clone(),
        }
    }

    /// The repositories the new-chat form suggests, first one default. A chat
    /// can be cut from any repository [`git::names_a_repository`] takes.
    #[must_use]
    pub fn repos(&self) -> &[String] {
        &self.repos
    }

    /// Every chat on disk paired with the status it has right now.
    pub async fn survey(&self) -> Result<Vec<ui::Chat>> {
        let live = self.plane.live_chat_ids().await?;
        self.scanned(&live)
    }

    /// The same pass, taken whether or not the plane will say which chats
    /// hold a container: a dead daemon costs the live/parked distinction and
    /// not the dataset (issue #25). Only the chats on disk are the plane's to
    /// take away, and it holds none of them.
    pub async fn survey_or_unknown(&self) -> Result<Survey> {
        match self.plane.live_chat_ids().await {
            Ok(live) => Ok(Survey {
                chats: self.scanned(&live)?,
                containers: Containers::Known,
            }),
            Err(quiet) => {
                let quiet = with_causes(quiet.as_ref());
                warn!("the container plane would not say what is live: {quiet}");
                Ok(Survey {
                    chats: self.scanned(&HashSet::new())?,
                    containers: Containers::Unknown(quiet),
                })
            }
        }
    }

    /// The dataset as it stands, each chat's status read against `live`.
    fn scanned(&self, live: &HashSet<String>) -> Result<Vec<ui::Chat>> {
        Ok(self
            .store
            .scan()?
            .into_iter()
            .map(|manifest| {
                let status = runtime_status(&manifest, live);
                (manifest, status)
            })
            .collect())
    }

    /// One chat and its whole event log, or nothing if the dataset holds no
    /// such chat. A pure read: opening a chat neither touches a container nor
    /// asks after one, so it stands whatever the plane is doing (ADR-0006,
    /// issue #25).
    pub fn open(&self, chat_id: &Ulid) -> Result<Option<(Manifest, Vec<Event>)>> {
        let chat_id = chat_id.to_string();
        let manifest = match self.store.read_manifest(&chat_id) {
            Ok(manifest) => manifest,
            Err(failure) if failure.is_missing() => return Ok(None),
            Err(failure) => return Err(failure.into()),
        };
        let events = self.store.read_events(&chat_id)?;
        Ok(Some((manifest, events)))
    }

    /// One chat's event log, or nothing if the dataset holds no such chat.
    /// The whole of what the UI renders, and cheap enough to poll: a file
    /// read, no container, no connection (ADR-0006).
    pub fn events(&self, chat_id: &Ulid) -> Result<Option<Vec<Event>>> {
        match self.store.read_events(&chat_id.to_string()) {
            Ok(events) => Ok(Some(events)),
            Err(failure) if failure.is_missing() => Ok(None),
            Err(failure) => Err(failure.into()),
        }
    }

    /// Put one prompt to a chat's agent, writing the turn down as it happens:
    /// the prompt before it goes out, then every update the agent streams
    /// back (ADR-0006). Returns once the agent has ended the turn.
    pub async fn prompt(&self, chat_id: &Ulid, said: &str) -> Result<(), PromptError> {
        let chat_id = chat_id.to_string();
        let outcome = self.turn(&chat_id, said).await;
        match &outcome {
            Ok(()) => self.touch(&chat_id),
            Err(refusal) if refusal.is_refusal() => {
                self.note_refusal(&chat_id, PROMPT_NOT_SENT, refusal);
            }
            Err(_) => {}
        }
        self.cap_the_pool().await;
        outcome
    }

    /// Say that a chat was just active. This is the whole of the warm pool's
    /// order, written once per completed turn rather than per event
    /// (ADR-0006).
    fn touch(&self, chat_id: &str) {
        let touched = self.store.read_manifest(chat_id).and_then(|manifest| {
            self.store.write_manifest(&Manifest {
                last_active_at: Utc::now(),
                ..manifest
            })
        });
        if let Err(failure) = touched {
            warn!("{}", undated_turn(chat_id, &failure));
        }
    }

    /// Bring the pool back inside its cap. A pool that cannot be read is the
    /// operator's to hear about, not this request's to fail on: the turn or
    /// the chat it followed is already done, whichever way it went.
    ///
    /// One capping at a time: two that weigh the same pool before either of
    /// them parks anything would each pick the same victims and cut the pool
    /// below its cap.
    async fn cap_the_pool(&self) {
        let _capping = self.parking.lock().await;
        match self.overflowing().await {
            Ok(overflowing) => {
                for chat_id in overflowing {
                    self.park(&chat_id).await;
                }
            }
            Err(failure) => warn!("the warm pool could not be weighed: {failure:#}"),
        }
    }

    /// The chats holding a container the pool has no room for (ADR-0002).
    async fn overflowing(&self) -> Result<Vec<String>> {
        let chats = self.store.scan()?;
        let live = self.plane.live_chat_ids().await?;
        Ok(pool::beyond_the_pool(
            &chats,
            &live,
            &self.occupied(),
            self.warm_pool,
        ))
    }

    /// The chats no capping may take a container from: the ones taking a turn,
    /// and the ones a wake or an archive is deciding a container for.
    fn occupied(&self) -> HashSet<String> {
        let mut occupied = self.connections.busy_chat_ids();
        occupied.extend(self.claimed());
        occupied
    }

    /// The chats a wake or an archive holds right now.
    fn claimed(&self) -> HashSet<String> {
        self.claims
            .lock()
            .expect("no holder of the lock panics")
            .keys()
            .cloned()
            .collect()
    }

    /// Take `chat_id` for `doing`, for as long as the caller keeps what comes
    /// back. A chat somebody else already holds has nothing left to give, and
    /// what they are doing with it is what comes back instead.
    ///
    /// Nothing waits here: a claim is taken or refused, so no path that holds
    /// one can be behind a path that wants one.
    fn claim(&self, chat_id: &str, doing: Doing) -> Result<Claim<'_>, Doing> {
        match self
            .claims
            .lock()
            .expect("no holder of the lock panics")
            .entry(chat_id.to_owned())
        {
            Entry::Occupied(held) => Err(*held.get()),
            Entry::Vacant(slot) => {
                slot.insert(doing);
                Ok(Claim {
                    claims: &self.claims,
                    chat_id: chat_id.to_owned(),
                })
            }
        }
    }

    /// Park one chat: the container goes, the workspace and the agent's
    /// memory stay where they are, and nothing at all is committed
    /// (ADR-0002 rule 2, ADR-0005).
    ///
    /// Every eviction stops with no grace, an idle adapter in the container
    /// included: PID 1 never passed the signal on, so the grace only ever
    /// bought the capping request wall-clock ahead of the same kill (#40).
    async fn park(&self, chat_id: &str) {
        self.release(chat_id, "parked, workspace kept", StopGrace::Zero)
            .await;
    }

    /// Give a chat's container up under `grace`. What is left on disk is the
    /// caller's to decide: parking keeps the workspace, the archive gate
    /// deletes it once everything in it is on the remote.
    async fn release(&self, chat_id: &str, why: &str, grace: StopGrace) {
        match self.plane.teardown(chat_id, grace).await {
            Ok(()) => {
                self.connections.forget(chat_id);
                info!("{chat_id} {why}: container torn down");
            }
            Err(stubborn) => warn!("{}", stubborn_teardown(chat_id, why, &stubborn)),
        }
    }

    /// Close a chat for good: everything in the workspace onto the remote,
    /// then the container down and the workspace deleted (ADR-0002 rule 3).
    ///
    /// The push is the gate. A dirty tree goes onto a checkpoint branch of
    /// its own so the chat's branch keeps only the agent's own commits
    /// (ADR-0005); if any of it fails to land, the chat is left exactly as it
    /// was, told about in its own log, and the operator can try again.
    ///
    /// The chat's claim and its turn lock are both held throughout: the gate
    /// commits and then deletes the whole working tree, which is the one thing
    /// that must never happen under an agent that is writing into it — and a
    /// chat holding no connection is one a prompt can wake into a container of
    /// its own in the time the gate takes, which the claim is what stops
    /// (issue #93).
    ///
    /// A chat holding no connection has nothing in its container but the
    /// keep-alive, so the stop asks for no grace and the archive costs the
    /// request nothing (issue #40).
    ///
    /// A refusal is written into the chat's own log, which is where the
    /// operator is looking: the browser throws the answer to an archive away
    /// (issue #102).
    pub async fn archive(&self, chat_id: &Ulid) -> Result<(), ArchiveError> {
        let chat_id = chat_id.to_string();
        let outcome = self.close(&chat_id).await;
        if let Err(refusal) = &outcome {
            if refusal.is_worth_noting() {
                self.note_refusal(&chat_id, NOT_ARCHIVED, refusal);
            }
        }
        outcome
    }

    /// The archive itself, from the claim to the emptied workspace.
    async fn close(&self, chat_id: &str) -> Result<(), ArchiveError> {
        let _claim = self
            .claim(chat_id, Doing::Archiving)
            .map_err(|held| match held {
                Doing::Waking => ArchiveError::Waking,
                Doing::Archiving => ArchiveError::Archiving,
            })?;
        let manifest = match self.store.read_manifest(chat_id) {
            Ok(manifest) => manifest,
            Err(failure) if failure.is_missing() => return Err(ArchiveError::NoSuchChat),
            Err(failure) => return Err(unarchived(failure)),
        };
        if manifest.state != ChatState::Open {
            return Err(ArchiveError::AlreadyArchived);
        }
        let _turn = self.idle(chat_id)?;
        let origin = self.origin(&manifest.repo).map_err(unarchived)?;
        let pushed = match self.push_everything(origin, &manifest).await {
            Ok(pushed) => pushed,
            Err(failure) => {
                self.note(chat_id, PUSH_FAILURE, &half_pushed(&failure));
                return Err(ArchiveError::NotPushed(failure.into()));
            }
        };
        if let Some(rescue) = &pushed.rescue_branch {
            self.note(chat_id, RESCUE_BRANCH, &rescued(&manifest.branch, rescue));
        }
        self.store
            .write_manifest(&Manifest {
                state: ChatState::Archived,
                last_pushed_commit: Some(pushed.tip),
                checkpoint_branch: pushed.checkpoint_branch,
                ..manifest
            })
            .map_err(unarchived)?;
        self.release(chat_id, "archived", self.grace_owed(chat_id))
            .await;
        self.store.remove_workspace(chat_id).map_err(unarchived)?;
        self.sweep().await;
        Ok(())
    }

    /// What a chat's container is owed when it goes down: one the chat is
    /// still holding a connection to has an adapter in it, one that is parked
    /// has nothing but its keep-alive (issue #40).
    fn grace_owed(&self, chat_id: &str) -> StopGrace {
        if self.connections.of(chat_id).is_some() {
            StopGrace::Full
        } else {
            StopGrace::Zero
        }
    }

    /// Take the chat's connection for as long as the caller keeps what comes
    /// back, so that no turn can run over it meanwhile. A chat holding no
    /// connection has nothing to take and nothing running.
    fn idle(&self, chat_id: &str) -> Result<Option<Turn<T::Channel>>, ArchiveError> {
        self.connections.of(chat_id).map_or(Ok(None), |connection| {
            connection
                .try_lock_owned()
                .map(Some)
                .map_err(|_| ArchiveError::Busy)
        })
    }

    /// Reconcile `workspaces/` against the chats that claim a working tree,
    /// deleting the ones nothing does (ADR-0002 rule 4). A dataset that
    /// cannot be read is the operator's to hear about: a sweep repairs, it
    /// does not gate.
    pub async fn sweep(&self) {
        let swept = match self.reconciled().await {
            Ok(swept) => swept,
            Err(failure) => return warn!("workspaces/ could not be read: {failure:#}"),
        };
        for chat_id in &swept.held {
            error!("{chat_id} is not open but its container is up: its workspace is left alone");
        }
        let mut outcome = Swept {
            held: swept.held,
            ..Swept::default()
        };
        for chat_id in swept.orphaned {
            match self.store.remove_workspace(&chat_id) {
                Ok(()) => {
                    info!("{chat_id} left a workspace no chat claims: removed");
                    outcome.removed.push(chat_id);
                }
                Err(stubborn) => {
                    warn!("{}", stubborn_workspace(&chat_id, "a workspace", &stubborn));
                    outcome.stubborn.push(chat_id);
                }
            }
        }
        *self
            .last_sweep
            .lock()
            .expect("no holder of the lock panics") = Some(outcome);
    }

    /// Which working trees on disk no open chat claims, and which of those a
    /// container is still holding.
    async fn reconciled(&self) -> Result<Sweep> {
        let open: HashSet<String> = self
            .store
            .scan()?
            .into_iter()
            .filter(|manifest| manifest.state == ChatState::Open)
            .map(|manifest| manifest.chat_id)
            .collect();
        let live = self.plane.live_chat_ids().await?;
        Ok(sweep::reconcile(
            &self.store.workspace_ids()?,
            &open,
            &live,
            &self.claimed(),
        ))
    }

    /// Where a chat's repository is reached, over the credential as it stands
    /// right now: a token rotated since the last operation is the one this
    /// operation goes out on. Only github.com is handed it (`crate::git`).
    fn origin(&self, repo: &str) -> Result<git::Origin, SecretsError> {
        let token = self.secrets.read(Secret::GithubToken)?;
        Ok(self.remotes.origin(repo, token.as_deref()))
    }

    /// Get the chat's workspace onto the remote, whole. Git blocks, so it
    /// runs off the runtime.
    async fn push_everything(
        &self,
        origin: git::Origin,
        manifest: &Manifest,
    ) -> Result<git::Pushed, git::PushFailure> {
        let workspace = self.store.workspace_dir(&manifest.chat_id);
        let branch = manifest.branch.clone();
        tokio::task::spawn_blocking(move || git::push_for_archive(&origin, &workspace, &branch))
            .await
            .expect("the git task should not panic")
    }

    /// Take one turn over the chat's connection, waking it first if it holds
    /// none. A wake hands its claim over with the connection, and the turn lets
    /// go of it only once it holds the connection itself: between the two there
    /// is no instant in which the chat reads as idle to an archive (issue #101).
    async fn turn(&self, chat_id: &str, said: &str) -> Result<(), PromptError> {
        let woken = match self.connections.of(chat_id) {
            Some(connection) => Woken {
                connection,
                claim: None,
            },
            None => self.wake(chat_id).await?,
        };
        let turn = {
            let mut agent = woken.connection.try_lock().map_err(|_| PromptError::Busy)?;
            drop(woken.claim);
            agent
                .take_turn(said, &mut |payload| {
                    self.store.append_event(chat_id, payload)?;
                    Ok(())
                })
                .await
        };
        turn.map_err(|failure| self.ended(chat_id, failure))
    }

    /// A turn that failed, read for what it says about the connection it went
    /// over: one the adapter spent is dropped, so the next prompt climbs
    /// ADR-0007's ladder rather than going over a pipe nobody is holding.
    fn ended(&self, chat_id: &str, failure: AcpError) -> PromptError {
        self.note(chat_id, CONNECTION_LOST, &turn_lost(&failure));
        if failure.spent_the_connection() {
            warn!("{}", lost_the_connection(chat_id, &failure));
            self.connections.forget(chat_id);
            PromptError::Broke(failure.into())
        } else {
            PromptError::Unrecorded(failure.into())
        }
    }

    /// A refusal in the chat's own log, under what it turned away. The page
    /// renders nothing else, so this is the only place the operator can read
    /// why what they asked for went nowhere; the next poll brings it
    /// (ADR-0006).
    fn note_refusal(&self, chat_id: &str, turned_away: &str, refusal: &dyn Display) {
        self.note(chat_id, REFUSAL, &format!("{turned_away}: {refusal}."));
    }

    /// Say in the chat's log when its session runs in some mode other than the
    /// one this deployment asks for. A session whose adapter names no mode
    /// says nothing: an unknown mode is not a wrong one.
    fn note_the_mode(&self, chat_id: &str, connection: &Connection<T::Channel>) {
        let clamped = connection
            .current_mode()
            .filter(|mode| *mode != managed_default_mode());
        if let Some(mode) = clamped {
            warn!("{chat_id} opened a session in permission mode {mode}");
            self.note(chat_id, MODE_NOTICE, &clamped_mode(mode));
        }
    }

    /// A line in the core's own voice in a chat's log, which is the only
    /// place the UI can say anything the agent did not (ADR-0006).
    fn note(&self, chat_id: &str, kind: &str, text: &str) {
        let line = json!({"corcode": kind, "text": text});
        if let Err(failure) = self.store.append_event(chat_id, &line) {
            warn!("{}", unwritten_line(kind, &failure));
        }
    }

    /// Bring a chat back to something a prompt can go over, and say in the
    /// chat's own log when it cannot be (ADR-0007).
    ///
    /// A wake that fails leaves the chat as it was — nothing is repaired on a
    /// guess (rule 5) — so the prompt can simply be put again.
    ///
    /// One wake at a time: a second prompt arriving mid-wake is turned away
    /// rather than left to clone over the first one's workspace.
    ///
    /// The claim goes back with the connection rather than ending here: a chat
    /// woken but not yet taking a turn is one an archive would read as idle and
    /// tear the fresh container from (issue #101).
    async fn wake(&self, chat_id: &str) -> Result<Woken<'_, T::Channel>, PromptError> {
        let claim = self
            .claim(chat_id, Doing::Waking)
            .map_err(|held| match held {
                Doing::Waking => PromptError::Waking,
                Doing::Archiving => PromptError::Archiving,
            })?;
        match self.woken(chat_id).await {
            Ok(connection) => Ok(Woken {
                connection,
                claim: Some(claim),
            }),
            Err(failure) => {
                self.note(
                    chat_id,
                    WAKE_FAILURE,
                    &format!("The prompt was not sent: {failure:#}. It can be sent again."),
                );
                Err(PromptError::Unwoken(failure))
            }
        }
    }

    /// A chat with a workspace, a container and an agent that has been asked
    /// for its memory back, in that order (ADR-0007). Whatever the waking cost
    /// the chat is in its log by the time a prompt goes out.
    ///
    /// Nothing is cloned, started or written for a chat that could not be
    /// prompted anyway: the manifest is read for everything the wake needs
    /// before the wake touches anything.
    async fn woken(&self, chat_id: &str) -> Result<Held<T::Channel>> {
        let manifest = self.store.read_manifest(chat_id)?;
        let remembered = manifest
            .acp_session_id
            .clone()
            .context("this chat has no session recorded to come back to")?;
        let manifest = if manifest.state == ChatState::Archived {
            self.revive(manifest).await?
        } else {
            self.still_has_its_workspace(chat_id)?;
            self.hand_the_workspace_back(chat_id).await?;
            manifest
        };
        let climbed = self.climbed_in_a_container(chat_id, &remembered).await?;
        self.note_the_mode(chat_id, &climbed.connection);
        if climbed.forgot_everything {
            self.store.write_manifest(&Manifest {
                acp_session_id: Some(climbed.connection.session_id().to_owned()),
                ..manifest
            })?;
            self.note(chat_id, RESET_NOTICE, resume::MEMORY_RESET);
        }
        Ok(self.connections.hold(chat_id, climbed.connection)?)
    }

    /// Climb ADR-0007's ladder in the chat's container, starting one if the
    /// chat is not holding one. A climb that gets nowhere hands back a
    /// container it started: the chat is parked again, exactly as the prompt
    /// found it.
    async fn climbed_in_a_container(
        &self,
        chat_id: &str,
        remembered: &str,
    ) -> Result<Climbed<T::Channel>> {
        let housing = self.container_for(chat_id).await?;
        match self.climb(&housing.container, remembered).await {
            Ok(climbed) => Ok(climbed),
            Err(failure) => {
                if housing.spawned {
                    self.release(chat_id, "woken but out of reach", StopGrace::Full)
                        .await;
                }
                Err(failure.into())
            }
        }
    }

    /// An open chat has a working tree, always (ADR-0002 rule 1). One that
    /// does not is a dataset that is not mounted as often as it is a chat that
    /// lost its files, so nothing is cloned over the top of it.
    fn still_has_its_workspace(&self, chat_id: &str) -> Result<()> {
        let workspace = self.store.workspace_dir(chat_id);
        anyhow::ensure!(
            workspace.is_dir(),
            "this chat is open but has no workspace at {}",
            workspace.display()
        );
        Ok(())
    }

    /// Give the workspace back to the agent before a container opens on it
    /// again. The core writes in an open chat's tree itself — an archive
    /// commits and pushes as the core, down to the refs and the index — and
    /// one entry the agent does not own is enough to stop it committing. Walking
    /// a tree blocks, so it runs off the runtime.
    async fn hand_the_workspace_back(&self, chat_id: &str) -> Result<()> {
        let workspace = self.store.workspace_dir(chat_id);
        let agent = self.store.agent();
        tokio::task::spawn_blocking(move || store::hand_tree_to(&workspace, agent))
            .await
            .expect("the handover task should not panic")?;
        Ok(())
    }

    /// Bring an archived chat's files back as a fresh clone and open it again
    /// (ADR-0002 rule 5), answering with the manifest as it now stands.
    ///
    /// The clone is the whole of the revival: one that fails takes its own
    /// half-written workspace with it and leaves the chat archived and
    /// readable, to be tried again once whatever is missing is back. What was
    /// on disk before the revival began is nobody's to clone over or to
    /// delete, so a workspace already there stops the revival dead: an
    /// archived chat is one with no working tree (ADR-0002 rule 1).
    async fn revive(&self, manifest: Manifest) -> Result<Manifest> {
        let last_pushed = manifest
            .last_pushed_commit
            .clone()
            .context("this chat was archived without a commit to come back to")?;
        let workspace = self.store.workspace_dir(&manifest.chat_id);
        anyhow::ensure!(
            !workspace.exists(),
            "this chat is archived but something is already in its workspace at {}",
            workspace.display()
        );
        let standing = match self.clone_back(&manifest, &last_pushed).await {
            Ok(standing) => standing,
            Err(failure) => {
                self.wipe_the_half_clone(&manifest.chat_id);
                return Err(failure);
            }
        };
        let revived = Manifest {
            state: ChatState::Open,
            ..manifest
        };
        self.store.write_manifest(&revived)?;
        self.note(
            &revived.chat_id,
            RESET_NOTICE,
            &resume::workspace_reset(&revived.branch, &standing.standing_at, &last_pushed),
        );
        if standing.behind_the_tip() {
            self.note(
                &revived.chat_id,
                DRIFT_NOTICE,
                &resume::remote_drift(&revived.branch, &standing.standing_at, &standing.tip),
            );
        }
        Ok(revived)
    }

    /// Take away what this revival got as far as writing — there was nothing
    /// there before it started — so the chat is left archived with nothing on
    /// disk, exactly as it was. A tree that will not go is the operator's to
    /// hear about: the chat is already as safe as it can be made.
    fn wipe_the_half_clone(&self, chat_id: &str) {
        if let Err(stubborn) = self.store.remove_workspace(chat_id) {
            warn!("{}", stubborn_workspace(chat_id, "half a clone", &stubborn));
        }
    }

    /// Clone the chat's branch back into its workspace at `commit` and hand
    /// the clone to the agent that will work in it: a revived workspace is as
    /// new as a created one. Git blocks, so it runs off the runtime.
    async fn clone_back(&self, manifest: &Manifest, commit: &str) -> Result<git::Revived> {
        let origin = self.origin(&manifest.repo)?;
        let workspace = self.store.workspace_dir(&manifest.chat_id);
        let branch = manifest.branch.clone();
        let commit = commit.to_owned();
        let agent = self.store.agent();
        tokio::task::spawn_blocking(move || -> Result<git::Revived> {
            let standing = git::revive_at(&origin, &branch, &commit, &workspace)?;
            store::hand_tree_to(&workspace, agent)?;
            Ok(standing)
        })
        .await
        .expect("the git task should not panic")
    }

    /// The container this chat's adapter is reached in. One that is still up
    /// is connected to again rather than replaced: the agent's memory is
    /// inside it, and that is the whole of what rung 1 asks for.
    async fn container_for(&self, chat_id: &str) -> Result<Housing> {
        if self.plane.live_chat_ids().await?.contains(chat_id) {
            return Ok(Housing {
                container: container_name(chat_id),
                spawned: false,
            });
        }
        Ok(Housing {
            container: self.spawn(chat_id).await?,
            spawned: true,
        })
    }

    /// Climb ADR-0007's ladder in `container` until a rung answers: the
    /// session the chat remembers resumed, or replayed, or given up on for one
    /// that remembers nothing.
    async fn climb(
        &self,
        container: &str,
        remembered: &str,
    ) -> Result<Climbed<T::Channel>, AcpError> {
        let mut greeting = self.adapter.greet(container).await?;
        let mut session_id = remembered.to_owned();
        let mut rung = resume::FIRST;
        loop {
            let reached = match rung {
                Rung::Resume => greeting.resume(&session_id).await,
                Rung::Load => greeting.load(&session_id).await,
                Rung::Fresh => greeting.open().await.map(|fresh| session_id = fresh),
            };
            let attempt = match &reached {
                Ok(()) => Attempt::Restored,
                Err(failure) if failure.answered() => Attempt::Refused,
                Err(_) => Attempt::Broken,
            };
            let forgot_everything = match resume::after(rung, attempt) {
                Step::Prompt => false,
                Step::PromptWithoutMemory => true,
                Step::Climb(next) => {
                    rung = next;
                    continue;
                }
                Step::GiveUp => {
                    return Err(reached.expect_err("a climb gives up only on a failure"));
                }
            };
            return Ok(Climbed {
                connection: greeting.over(session_id),
                forgot_everything,
            });
        }
    }

    /// Cut a new chat whole: both trees, a clone of the repository at its
    /// base branch, the chat's own branch, a container, and the ACP session
    /// they will talk over. A step that fails leaves what came before it on
    /// disk for the operator to look at (ADR-0007 rule 5).
    ///
    /// The repository is trimmed the way `CORCODE_REPOS` entries are, and the
    /// manifest holds what is left: a URL pasted out of a browser carries
    /// whitespace nobody typed and no repository wants.
    pub async fn create(&self, wanted: WantedChat) -> Result<String, CreateError> {
        let wanted = WantedChat {
            repo: wanted.repo.trim().to_owned(),
            ..wanted
        };
        let typed = wanted.slug.trim();
        let slug = git::slugify(typed);
        if slug.is_empty() {
            return Err(CreateError::Unnamed);
        }
        if !git::names_a_repository(&wanted.repo) {
            return Err(CreateError::UnusableRepo { repo: wanted.repo });
        }
        if !git::names_a_branch(&wanted.base_branch) {
            return Err(CreateError::UnusableBranch {
                branch: wanted.base_branch,
            });
        }
        let branch = if wanted.direct_on_base {
            wanted.base_branch.clone()
        } else {
            git::chat_branch(&slug)
        };
        let manifest = self
            .store
            .create_chat(NewChat {
                title: typed.to_owned(),
                repo: wanted.repo.clone(),
                branch: branch.clone(),
                base_branch: wanted.base_branch.clone(),
                env: wanted.env.clone(),
                startup_script: wanted.startup_script.clone(),
            })
            .map_err(broke)?;
        let chat_id = manifest.chat_id.clone();
        self.check_out(&chat_id, &wanted, &branch).await?;
        let container = self.spawn(&chat_id).await.map_err(broke)?;
        match self.record_session(manifest, &container).await {
            Ok(()) => {
                self.cap_the_pool().await;
                Ok(chat_id)
            }
            Err(failure) => {
                if let Err(stubborn) = self.plane.teardown(&chat_id, StopGrace::Full).await {
                    warn!(
                        "{}",
                        stubborn_teardown(&chat_id, "never opened a session", &stubborn)
                    );
                }
                Err(failure)
            }
        }
    }

    /// Clone the repository into the chat's workspace, stand on the branch the
    /// chat works from, and hand the clone to the agent that will work in it:
    /// git wrote it as the core. Git blocks, so it runs off the runtime.
    async fn check_out(
        &self,
        chat_id: &str,
        wanted: &WantedChat,
        branch: &str,
    ) -> Result<(), CreateError> {
        let origin = self.origin(&wanted.repo).map_err(broke)?;
        let workspace = self.store.workspace_dir(chat_id);
        let base_branch = wanted.base_branch.clone();
        let to_cut = (!wanted.direct_on_base).then(|| branch.to_owned());
        let agent = self.store.agent();
        tokio::task::spawn_blocking(move || -> Result<()> {
            git::clone_at(&origin, &base_branch, &workspace)?;
            if let Some(branch) = to_cut {
                git::create_branch(&workspace, &branch)?;
            }
            Ok(store::hand_tree_to(&workspace, agent)?)
        })
        .await
        .expect("the git task should not panic")
        .map_err(broke)
    }

    /// Start the chat's container over both of its directories (ADR-0006),
    /// answering with the container's name.
    ///
    /// Every credential is read here rather than held, so the one written a
    /// moment ago is the one the next container is spawned with, and a
    /// deployment holding none hands the agent none.
    async fn spawn(&self, chat_id: &str) -> Result<String> {
        let manifest = self.store.read_manifest(chat_id)?;
        let env = self.spawn_env(&manifest.env)?;
        let container = self
            .plane
            .spawn(
                chat_id,
                &self.store.host_workspace_dir(chat_id),
                &self.store.host_claude_dir(chat_id),
                &env,
            )
            .await?
            .name;
        if let Some(script) = &manifest.startup_script {
            self.run_startup_script(chat_id, script, &env).await;
        }
        Ok(container)
    }

    /// Run the chat's startup script in the container this spawn just started,
    /// over the same env, and write what it did to the chat's log — every spawn
    /// path runs it, since containers are ephemeral (issue #14).
    ///
    /// A script that fails does not fail the spawn: the chat still opens, and
    /// the exit code and output are the operator's to read in the transcript.
    async fn run_startup_script(
        &self,
        chat_id: &str,
        script: &str,
        env: &BTreeMap<String, String>,
    ) {
        match self.plane.run_startup_script(chat_id, script, env).await {
            Ok(run) => self.note(chat_id, STARTUP_SCRIPT, &startup_summary(&run)),
            Err(failure) => {
                warn!(
                    "{chat_id} could not run its startup script: {}",
                    with_causes(&failure)
                );
                self.note(
                    chat_id,
                    STARTUP_SCRIPT,
                    &format!("The startup script could not be run: {failure}."),
                );
            }
        }
    }

    /// The container's environment: the credentials the core holds now, then
    /// the chat's own custom variables, each added only where it names nothing
    /// the core already set. System credentials always win a name clash, so a
    /// user var can never point the agent at a key the operator typed
    /// (ADR-0001).
    fn spawn_env(&self, custom: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
        let mut env = BTreeMap::new();
        if let Some(credential) = self.secrets.read(Secret::AnthropicKey)? {
            let variable = AnthropicCredential::of(&credential).variable();
            env.insert(variable.to_owned(), credential);
        }
        if let Some(token) = self.secrets.read(Secret::GithubToken)? {
            for variable in GITHUB_TOKEN_VARIABLES {
                env.insert(variable.to_owned(), token.clone());
            }
        }
        for (name, value) in custom {
            env.entry(name.clone()).or_insert_with(|| value.clone());
        }
        Ok(env)
    }

    /// Open the ACP session, write its id into the manifest — the only trace
    /// a new session leaves, since it is not an event (ADR-0006) — and keep
    /// the connection for the prompts that follow.
    async fn record_session(
        &self,
        mut manifest: Manifest,
        container: &str,
    ) -> Result<(), CreateError> {
        let connection = self.adapter.open_session(container).await.map_err(broke)?;
        self.note_the_mode(&manifest.chat_id, &connection);
        manifest.acp_session_id = Some(connection.session_id().to_owned());
        self.store.write_manifest(&manifest).map_err(broke)?;
        self.connections
            .hold(&manifest.chat_id, connection)
            .map_err(broke)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::acp::ScriptedAdapter;
    use crate::plane::MemoryPlane;

    /// The session a woken chat comes back to.
    const REMEMBERED: &str = "session-remembered";

    /// A dataset the tests own outright, over an adapter that resumes at
    /// ADR-0007's first rung and a plane whose containers cost nothing.
    fn a_parked_chat() -> (TempDir, Chats<MemoryPlane, ScriptedAdapter>, String) {
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let config = a_config(dir.path());
        let store = ChatStore::new(dir.path());
        store.prepare().expect("the dataset should prepare");
        let manifest = store
            .create_chat(NewChat {
                title: "parked".to_owned(),
                repo: "CorVous/CorCode".to_owned(),
                branch: "chat/parked".to_owned(),
                base_branch: "main".to_owned(),
                env: BTreeMap::new(),
                startup_script: None,
            })
            .expect("a chat should be created");
        let chat_id = manifest.chat_id.clone();
        store
            .write_manifest(&Manifest {
                acp_session_id: Some(REMEMBERED.to_owned()),
                ..manifest
            })
            .expect("the session should be recorded");
        let secrets = Arc::new(Secrets::from_config(&config));
        let chats = Chats::new(
            &config,
            Owner::of(&config.data_dir).expect("we own the dataset we just made"),
            MemoryPlane::default(),
            ScriptedAdapter::resuming(REMEMBERED, &[]),
            Remotes::new("file:///nowhere"),
            secrets,
        );
        (dir, chats, chat_id)
    }

    fn a_config(data_dir: &std::path::Path) -> Config {
        let vars = [
            ("CORCODE_DATA_DIR", data_dir.to_str().expect("spellable")),
            ("CORCODE_USERNAME", "cassidy"),
            (
                "CORCODE_PASSWORD_HASH",
                "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA",
            ),
            (
                "CORCODE_WORKSPACE_IMAGE",
                "ghcr.io/corvous/corcode-workspace:2026-08-05",
            ),
            ("CORCODE_REPOS", "CorVous/CorCode"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()));
        Config::from_vars(vars).expect("a test environment should load")
    }

    /// A wake pays for a container and hands the connection to the turn that
    /// asked for it. The claim is the whole of what stops an archive taking
    /// that container away, so it has to outlive the wake: a chat that is
    /// woken but not yet turning is in neither `busy_chat_ids` nor `claimed`,
    /// and an archive landing there finds an idle chat and tears down what the
    /// wake just paid for (issue #101).
    #[tokio::test]
    async fn a_woken_chat_stays_claimed_until_its_turn_holds_the_connection() {
        let (_dataset, chats, chat_id) = a_parked_chat();

        let woken = chats
            .wake(&chat_id)
            .await
            .expect("a parked chat with a session to come back to wakes");

        assert!(
            chats.claimed().contains(&chat_id),
            "the woken chat is unclaimed before its turn begins: an archive \
             landing here reads it as idle and tears its container down"
        );
        let connection = chats
            .connections
            .of(&chat_id)
            .expect("a woken chat holds the connection its turn goes over");
        let _turn = connection
            .try_lock_owned()
            .expect("nothing else is taking a turn");
        drop(woken);
        assert!(
            chats.connections.busy_chat_ids().contains(&chat_id),
            "the turn that took over the claim is not visible as a busy chat"
        );
        assert!(
            !chats.claimed().contains(&chat_id),
            "the claim outlived the turn it handed the chat over to"
        );
    }

    /// The shape the operator actually met: a teardown the daemon refused,
    /// with its refusal one level under the summary.
    fn a_teardown_the_daemon_refused() -> PlaneError {
        PlaneError::Runtime {
            action: "tear down the container of chat 01K1TESTCHATID0000000000".to_owned(),
            source: bollard::errors::Error::DockerResponseServerError {
                status_code: 409,
                message: "removal of container is already in progress".to_owned(),
            },
        }
    }

    #[test]
    fn a_container_that_would_not_stop_is_logged_with_what_the_daemon_said() {
        let logged = stubborn_teardown(
            "01K1TESTCHATID0000000000",
            "parked, workspace kept",
            &a_teardown_the_daemon_refused(),
        );

        assert_eq!(
            logged,
            "01K1TESTCHATID0000000000 (parked, workspace kept) would not stop: \
             the container runtime failed to tear down the container of chat \
             01K1TESTCHATID0000000000: \
             Docker responded with status code 409: \
             removal of container is already in progress",
            "the summary alone names nothing the operator can act on"
        );
    }

    #[test]
    fn displaying_the_stubborn_container_would_lose_the_daemons_complaint() {
        let stubborn = a_teardown_the_daemon_refused();

        assert_eq!(
            stubborn.to_string(),
            "the container runtime failed to tear down the container of chat \
             01K1TESTCHATID0000000000",
            "plain Display is what these warn sites used to get"
        );
        assert_ne!(with_causes(&stubborn), stubborn.to_string());
    }

    /// The shape the operator actually met: a write to `path` the filesystem
    /// refused, with its refusal one level under the store's summary.
    fn a_write_the_filesystem_refused(path: &str) -> StoreError {
        StoreError::Write {
            path: path.into(),
            source: std::io::ErrorKind::PermissionDenied.into(),
        }
    }

    #[test]
    fn a_workspace_that_will_not_go_is_logged_with_what_the_filesystem_said() {
        let logged = stubborn_workspace(
            "01K1TESTCHATID0000000000",
            "a workspace",
            &a_write_the_filesystem_refused("/srv/corcode/workspaces/01K1TESTCHATID0000000000"),
        );

        assert_eq!(
            logged,
            "01K1TESTCHATID0000000000 left a workspace that will not go: \
             /srv/corcode/workspaces/01K1TESTCHATID0000000000 could not be written: \
             permission denied",
            "the summary names the path but not why it stayed"
        );
    }

    #[test]
    fn a_turn_that_could_not_be_dated_is_logged_with_what_the_filesystem_said() {
        let logged = undated_turn(
            "01K1TESTCHATID0000000000",
            &a_write_the_filesystem_refused(
                "/srv/corcode/chats/01K1TESTCHATID0000000000/manifest.json",
            ),
        );

        assert_eq!(
            logged,
            "01K1TESTCHATID0000000000 took a turn that could not be dated: \
             /srv/corcode/chats/01K1TESTCHATID0000000000/manifest.json could not be written: \
             permission denied",
            "a warm pool that stops being ordered is read from this line alone"
        );
    }

    #[test]
    fn a_line_that_could_not_be_written_down_is_logged_with_what_the_filesystem_said() {
        let logged = unwritten_line(
            REFUSAL,
            &a_write_the_filesystem_refused(
                "/srv/corcode/chats/01K1TESTCHATID0000000000/events.jsonl",
            ),
        );

        assert_eq!(
            logged,
            "a refusal line could not be written down: \
             /srv/corcode/chats/01K1TESTCHATID0000000000/events.jsonl could not be written: \
             permission denied",
            "the chat's own log just failed, so this line is all the operator gets"
        );
    }

    #[test]
    fn displaying_the_stubborn_workspace_would_lose_the_filesystems_complaint() {
        let stubborn =
            a_write_the_filesystem_refused("/srv/corcode/workspaces/01K1TESTCHATID0000000000");

        assert_eq!(
            format!("{stubborn:#}"),
            "/srv/corcode/workspaces/01K1TESTCHATID0000000000 could not be written",
            "the alternate flag buys a typed error nothing"
        );
        assert_ne!(with_causes(&stubborn), format!("{stubborn:#}"));
    }

    #[test]
    fn a_connection_the_adapter_spent_is_logged_with_what_broke_it() {
        let logged = lost_the_connection(
            "01K1TESTCHATID0000000000",
            &AcpError::Broken {
                doing: "reading the adapter's answer".to_owned(),
                source: std::io::ErrorKind::BrokenPipe.into(),
            },
        );

        assert_eq!(
            logged,
            "01K1TESTCHATID0000000000 dropped its adapter connection after: \
             the adapter's channel broke while reading the adapter's answer: \
             broken pipe",
            "a disconnect nobody logs is the incident of issue #66"
        );
    }

    #[test]
    fn a_connection_dropped_after_a_refusal_is_not_called_a_broken_one() {
        let logged = lost_the_connection(
            "01K1TESTCHATID0000000000",
            &AcpError::Refused {
                method: "session/prompt".to_owned(),
                complaint: "unknown session".to_owned(),
            },
        );

        assert_eq!(
            logged,
            "01K1TESTCHATID0000000000 dropped its adapter connection after: \
             the adapter refused session/prompt: unknown session",
            "an adapter that answered is alive, whatever became of the connection"
        );
    }

    #[test]
    fn a_turn_that_spent_its_connection_tells_the_chat_the_prompt_can_go_again() {
        let told = turn_lost(&AcpError::Broken {
            doing: "reading the adapter's answer".to_owned(),
            source: std::io::ErrorKind::BrokenPipe.into(),
        });

        assert_eq!(
            told,
            "The turn ended without an answer: the adapter's channel broke while \
             reading the adapter's answer: broken pipe. The connection to the agent \
             was dropped, so the prompt can be sent again. The agent may have kept \
             working inside its container meanwhile.",
            "a turn that dies saying nothing here leaves the page frozen (issue #65)"
        );
    }

    #[test]
    fn a_turn_nothing_could_write_down_promises_no_clean_re_send() {
        let told = turn_lost(&AcpError::Unrecorded {
            source: anyhow::anyhow!("no space left on device"),
        });

        assert_eq!(
            told,
            "The turn was abandoned because it could not be written down: no space \
             left on device. The connection to the agent is still open, but what \
             the agent went on saying is still in the pipe, so the next prompt may \
             see the tail of this turn before its own reply. The agent may have \
             kept working inside its container meanwhile.",
            "the connection this failure leaves alone is not a clean one to prompt \
             over, and saying so would be a promise the core cannot keep"
        );
    }

    #[test]
    fn a_turn_lost_to_a_corrupted_channel_is_not_told_as_an_ordinary_drop() {
        let told = turn_lost(&AcpError::Garbled {
            method: "session/prompt".to_owned(),
            sample: "\u{fffd}\u{fffd} building: 42%".to_owned(),
        });

        assert_eq!(
            told,
            "Something the agent ran wrote over its own protocol channel, so what \
             reached the core stopped being messages (last read: \u{fffd}\u{fffd} \
             building: 42%). The turn was abandoned and the connection dropped, but \
             the agent may still be running whatever it started inside its \
             container. The prompt can be sent again; if the chat stays stuck, \
             archiving and reviving it gives the agent a container of its own again.",
            "a corrupted channel is a different thing from a pipe that broke, and \
             the operator can act on the difference (issue #65)"
        );
        assert_ne!(told, turn_lost(&AcpError::Closed));
    }

    const CHECKPOINT: &str = "chat/2026-08-09-archived-chkpt-20260809T214033172";
    const RESCUE: &str = "chat/2026-08-09-archived-rescue-20260809T214033173";

    fn a_gate_that_stopped(landed: git::Landed) -> git::PushFailure {
        git::PushFailure {
            landed,
            source: git::GitError::Refused {
                doing: "stand the workspace back on its branch".to_owned(),
                complaint: "fatal: unable to check out".to_owned(),
            },
        }
    }

    /// A rescue branch that reached the remote before the gate stopped holds
    /// the chat's whole work, and the log is the only place it is named
    /// (issue #50).
    #[test]
    fn an_archive_that_stopped_names_every_branch_that_got_there() {
        let told = half_pushed(&a_gate_that_stopped(git::Landed {
            checkpoint: Some(CHECKPOINT.to_owned()),
            rescue: Some(RESCUE.to_owned()),
        }));

        assert!(
            told.contains(CHECKPOINT) && told.contains(RESCUE),
            "the operator is sent looking for work that is on the remote: {told}"
        );
    }

    #[test]
    fn an_archive_that_got_nowhere_names_no_branch_at_all() {
        let told = half_pushed(&a_gate_that_stopped(git::Landed::default()));

        assert!(
            told.contains("Nothing was archived"),
            "a gate that got nowhere reads as a half-finished one: {told}"
        );
    }

    #[test]
    fn a_session_clamped_into_asking_is_told_what_that_costs_the_chat() {
        let notice = clamped_mode("default");

        assert!(
            notice.contains("default") && notice.contains(managed_default_mode()),
            "the notice names neither mode: {notice}"
        );
        assert!(
            notice.contains("declined"),
            "an agent whose every ask is declined looks mute, and the chat is not told: {notice}"
        );
    }

    /// A mode that asks for nothing is not a mode that asks and is refused, so
    /// the ask story would be a wrong one told confidently.
    #[test]
    fn a_session_in_some_other_mode_is_named_without_the_ask_story() {
        let notice = clamped_mode("bypassPermissions");

        assert!(
            notice.contains("bypassPermissions") && notice.contains(managed_default_mode()),
            "the notice names neither mode: {notice}"
        );
        assert!(
            !notice.contains("declined"),
            "a mode that declines nothing is described as one that does: {notice}"
        );
    }

    #[test]
    fn a_key_value_line_becomes_one_variable() {
        let env = parse_env("EDITOR=helix").expect("a plain line should parse");

        assert_eq!(
            env,
            BTreeMap::from([("EDITOR".to_owned(), "helix".to_owned())])
        );
    }

    #[test]
    fn blank_lines_and_comments_are_ignored() {
        let env = parse_env("\n# a note\n\nFOO=bar\n   \n# BAZ=qux\n")
            .expect("comments and blanks should be skipped");

        assert_eq!(env, BTreeMap::from([("FOO".to_owned(), "bar".to_owned())]));
    }

    #[test]
    fn the_value_keeps_every_equals_and_space_after_the_first() {
        let env = parse_env("FLAGS=--set a=b  c=d ").expect("a rich value should parse");

        assert_eq!(env["FLAGS"], "--set a=b  c=d ");
    }

    #[test]
    fn a_carriage_return_ending_leaves_no_stray_byte_in_key_or_value() {
        let env = parse_env("A=one\r\nB=two\r").expect("both endings should parse");

        assert_eq!(
            env,
            BTreeMap::from([
                ("A".to_owned(), "one".to_owned()),
                ("B".to_owned(), "two".to_owned()),
            ])
        );
    }

    #[test]
    fn a_line_with_no_equals_is_refused() {
        assert_eq!(
            parse_env("EDITOR helix"),
            Err(EnvError::NoEquals { line: 1 })
        );
    }

    #[test]
    fn a_line_with_no_name_before_the_equals_is_refused() {
        assert_eq!(parse_env("=orphan"), Err(EnvError::NoName { line: 1 }));
    }

    #[test]
    fn a_name_that_breaks_the_shape_is_refused() {
        assert_eq!(
            parse_env("1BADNAME=x"),
            Err(EnvError::BadName {
                name: "1BADNAME".to_owned()
            })
        );
        assert!(matches!(
            parse_env("has space=x"),
            Err(EnvError::BadName { .. })
        ));
    }

    #[test]
    fn a_name_the_core_sets_itself_is_refused() {
        for reserved in [
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "ANTHROPIC_API_KEY",
            "CLAUDE_CODE_OAUTH_TOKEN",
        ] {
            assert_eq!(
                parse_env(&format!("{reserved}=mine")),
                Err(EnvError::Reserved {
                    name: reserved.to_owned()
                }),
                "{reserved} was not guarded"
            );
        }
    }
}
