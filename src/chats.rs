//! The chats the console reads and the vertical that cuts a new one
//! (ADR-0005, ADR-0006).

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use log::{error, info, warn};
use serde_json::json;
use thiserror::Error;
use ulid::Ulid;

use crate::acp::{AcpError, AcpTransport, Adapter, Connection, Connections, Held, Turn};
use crate::config::Config;
use crate::git::{self, Remotes};
use crate::plane::{ContainerPlane, container_name};
use crate::pool;
use crate::resume::{self, Attempt, Rung, Step};
use crate::secrets::{AnthropicCredential, Secret, Secrets, SecretsError};
use crate::status::{Slot, Status};
use crate::store::{
    self, ChatState, ChatStore, ContainerLiveness, Event, Manifest, NewChat, RuntimeStatus,
    runtime_status,
};
use crate::sweep::{self, Sweep, Swept};
use crate::ui;

/// What the form asks for, before any of it is believed.
pub struct WantedChat {
    pub repo: String,
    pub base_branch: String,
    pub slug: String,
    pub direct_on_base: bool,
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
        matches!(self, Self::Busy | Self::Waking)
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
        matches!(self, Self::AlreadyArchived | Self::Busy)
    }
}

fn unarchived(failure: impl Into<anyhow::Error>) -> ArchiveError {
    ArchiveError::Broke(failure.into())
}

/// What the chat's log is told about an archive that stopped part way. A
/// checkpoint that landed is named: it is where the operator's work is, and
/// nothing else will tell them (ADR-0006).
fn half_pushed(failure: &git::PushFailure) -> String {
    let retry = "The chat is still open and can be archived again.";
    failure.landed.as_ref().map_or_else(
        || format!("Nothing was archived: {failure}. {retry}"),
        |landed| {
            format!(
                "The archive stopped part way: {failure}. \
                 The work in flight is on the remote, on {landed}. {retry}"
            )
        },
    )
}

/// What a refusal calls itself in a chat's own log (ADR-0006).
const REFUSAL: &str = "refusal";

/// What an archive that got nothing onto the remote calls itself there.
const PUSH_FAILURE: &str = "push_failure";

/// What waking a chat costs it calls itself in its log: memory the agent could
/// not get back, or a workspace that came back as a fresh clone (ADR-0006).
const RESET_NOTICE: &str = "reset_notice";

/// What a prompt that never got as far as an agent calls itself there.
const WAKE_FAILURE: &str = "wake_failure";

/// The right to decide who gives a container up, held by one capping at a
/// time.
type ParkingLock = tokio::sync::Mutex<()>;

/// The chats being woken right now. Waking clones a workspace, starts a
/// container and climbs a ladder, none of which two prompts may do at once for
/// one chat, and none of which a sweep or a capping may cut across.
type Waking = std::sync::Mutex<HashSet<String>>;

/// What the last orphan sweep came to, kept for the status line to read
/// (ADR-0008). Nothing until one has run.
type LastSweep = std::sync::Mutex<Option<Swept>>;

/// One chat's wake, given to whoever asked for it first and given back when
/// the wake is over however it ends.
struct Claim<'a> {
    waking: &'a Waking,
    chat_id: String,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.waking
            .lock()
            .expect("no holder of the lock panics")
            .remove(&self.chat_id);
    }
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
    waking: Waking,
    last_sweep: LastSweep,
}

impl<P, T: AcpTransport + Sync> Chats<P, T> {
    /// Serve the dataset `config` names, over `plane` and the adapters
    /// `transport` reaches, from the repositories `remotes` holds, on the
    /// credentials `secrets` holds.
    pub fn new(
        config: &Config,
        plane: P,
        transport: T,
        remotes: Remotes,
        secrets: Arc<Secrets>,
    ) -> Self {
        Self {
            store: ChatStore::mounted(&config.data_dir, &config.host_data_dir),
            plane,
            adapter: Adapter::new(transport),
            connections: Connections::default(),
            remotes,
            secrets,
            repos: config.repos.clone(),
            workspace_image: config.workspace_image.clone(),
            warm_pool: config.warm_pool,
            parking: ParkingLock::default(),
            waking: Waking::default(),
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
        Ok(self.status_of(&self.survey().await?, now))
    }

    /// The same picture read off a survey the caller already has, so that
    /// rendering the whole console costs one pass and not two.
    #[must_use]
    pub fn status_of(&self, chats: &[ui::Chat], now: DateTime<Utc>) -> Status {
        Status {
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
        Ok(self
            .store
            .scan()?
            .into_iter()
            .map(|manifest| {
                let status = runtime_status(&manifest, &live);
                (manifest, status)
            })
            .collect())
    }

    /// One chat and its whole event log, or nothing if the dataset holds no
    /// such chat. A pure read: opening a chat never touches a container
    /// (ADR-0007).
    pub async fn open(&self, chat_id: &Ulid) -> Result<Option<(ui::Chat, Vec<Event>)>> {
        let chat_id = chat_id.to_string();
        let manifest = match self.store.read_manifest(&chat_id) {
            Ok(manifest) => manifest,
            Err(failure) if failure.is_missing() => return Ok(None),
            Err(failure) => return Err(failure.into()),
        };
        let events = self.store.read_events(&chat_id)?;
        let live = self.plane.live_chat_ids().await?;
        let status = runtime_status(&manifest, &live);
        Ok(Some(((manifest, status), events)))
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
            Err(refusal) if refusal.is_refusal() => self.note_refusal(&chat_id, refusal),
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
            warn!("{chat_id} took a turn that could not be dated: {failure:#}");
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
    /// and the ones a wake is starting a container for.
    fn occupied(&self) -> HashSet<String> {
        let mut occupied = self.connections.busy_chat_ids();
        occupied.extend(self.claimed());
        occupied
    }

    /// The chats being woken right now.
    fn claimed(&self) -> HashSet<String> {
        self.waking
            .lock()
            .expect("no holder of the lock panics")
            .clone()
    }

    /// Park one chat: the container goes, the workspace and the agent's
    /// memory stay where they are, and nothing at all is committed
    /// (ADR-0002 rule 2, ADR-0005).
    async fn park(&self, chat_id: &str) {
        self.release(chat_id, "parked, workspace kept").await;
    }

    /// Give a chat's container up. What is left on disk is the caller's to
    /// decide: parking keeps the workspace, the archive gate deletes it once
    /// everything in it is on the remote.
    async fn release(&self, chat_id: &str, why: &str) {
        match self.plane.teardown(chat_id).await {
            Ok(()) => {
                self.connections.forget(chat_id);
                info!("{chat_id} {why}: container torn down");
            }
            Err(stubborn) => warn!("{chat_id} ({why}) would not stop: {stubborn}"),
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
    /// The turn lock is held throughout: the gate commits and then deletes the
    /// whole working tree, which is the one thing that must never happen under
    /// an agent that is writing into it.
    pub async fn archive(&self, chat_id: &Ulid) -> Result<(), ArchiveError> {
        let chat_id = chat_id.to_string();
        let manifest = match self.store.read_manifest(&chat_id) {
            Ok(manifest) => manifest,
            Err(failure) if failure.is_missing() => return Err(ArchiveError::NoSuchChat),
            Err(failure) => return Err(unarchived(failure)),
        };
        if manifest.state != ChatState::Open {
            return Err(ArchiveError::AlreadyArchived);
        }
        let _turn = self.idle(&chat_id)?;
        let origin = self.origin(&manifest.repo).map_err(unarchived)?;
        let pushed = match self.push_everything(origin, &manifest).await {
            Ok(pushed) => pushed,
            Err(failure) => {
                self.note(&chat_id, PUSH_FAILURE, &half_pushed(&failure));
                return Err(ArchiveError::NotPushed(failure.into()));
            }
        };
        self.store
            .write_manifest(&Manifest {
                state: ChatState::Archived,
                last_pushed_commit: Some(pushed.tip),
                checkpoint_branch: pushed.checkpoint_branch,
                ..manifest
            })
            .map_err(unarchived)?;
        self.release(&chat_id, "archived").await;
        self.store.remove_workspace(&chat_id).map_err(unarchived)?;
        self.sweep().await;
        Ok(())
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
                    warn!("{chat_id} left a workspace that will not go: {stubborn:#}");
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

    async fn turn(&self, chat_id: &str, said: &str) -> Result<(), PromptError> {
        let connection = match self.connections.of(chat_id) {
            Some(held) => held,
            None => self.wake(chat_id).await?,
        };
        let turn = {
            let mut agent = connection.try_lock().map_err(|_| PromptError::Busy)?;
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
        if failure.spent_the_connection() {
            self.connections.forget(chat_id);
            PromptError::Broke(failure.into())
        } else {
            PromptError::Unrecorded(failure.into())
        }
    }

    /// A refusal in the chat's own log. The page renders nothing else, so
    /// this is the only place the operator can read why their prompt went
    /// nowhere; the next poll brings it (ADR-0006).
    fn note_refusal(&self, chat_id: &str, refusal: &PromptError) {
        self.note(chat_id, REFUSAL, &format!("Prompt not sent: {refusal}."));
    }

    /// A line in the core's own voice in a chat's log, which is the only
    /// place the UI can say anything the agent did not (ADR-0006).
    fn note(&self, chat_id: &str, kind: &str, text: &str) {
        let line = json!({"corcode": kind, "text": text});
        if let Err(failure) = self.store.append_event(chat_id, &line) {
            warn!("a {kind} line could not be written down: {failure:#}");
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
    async fn wake(&self, chat_id: &str) -> Result<Held<T::Channel>, PromptError> {
        let _claim = self.claim_the_wake(chat_id)?;
        match self.woken(chat_id).await {
            Ok(held) => Ok(held),
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

    /// The wake of `chat_id`, for as long as the caller keeps it. A chat
    /// already being woken has nothing left to give.
    fn claim_the_wake(&self, chat_id: &str) -> Result<Claim<'_>, PromptError> {
        if self
            .waking
            .lock()
            .expect("no holder of the lock panics")
            .insert(chat_id.to_owned())
        {
            Ok(Claim {
                waking: &self.waking,
                chat_id: chat_id.to_owned(),
            })
        } else {
            Err(PromptError::Waking)
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
            manifest
        };
        let climbed = self.climbed_in_a_container(chat_id, &remembered).await?;
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
                    self.release(chat_id, "woken but out of reach").await;
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
        let standing_at = match self.clone_back(&manifest, &last_pushed).await {
            Ok(standing_at) => standing_at,
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
            &resume::workspace_reset(&revived.branch, &standing_at, &last_pushed),
        );
        Ok(revived)
    }

    /// Take away what this revival got as far as writing — there was nothing
    /// there before it started — so the chat is left archived with nothing on
    /// disk, exactly as it was. A tree that will not go is the operator's to
    /// hear about: the chat is already as safe as it can be made.
    fn wipe_the_half_clone(&self, chat_id: &str) {
        if let Err(stubborn) = self.store.remove_workspace(chat_id) {
            warn!("{chat_id} left half a clone that will not go: {stubborn:#}");
        }
    }

    /// Clone the chat's branch back into its workspace at `commit` and hand
    /// the clone to the agent that will work in it: a revived workspace is as
    /// new as a created one. Git blocks, so it runs off the runtime.
    async fn clone_back(&self, manifest: &Manifest, commit: &str) -> Result<String> {
        let origin = self.origin(&manifest.repo)?;
        let workspace = self.store.workspace_dir(&manifest.chat_id);
        let branch = manifest.branch.clone();
        let commit = commit.to_owned();
        tokio::task::spawn_blocking(move || -> Result<String> {
            let standing_at = git::revive_at(&origin, &branch, &commit, &workspace)?;
            store::hand_tree_to(&workspace, store::Owner::AGENT)?;
            Ok(standing_at)
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
                if let Err(stubborn) = self.plane.teardown(&chat_id).await {
                    warn!("{chat_id} never opened a session and would not stop: {stubborn}");
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
        tokio::task::spawn_blocking(move || -> Result<()> {
            git::clone_at(&origin, &base_branch, &workspace)?;
            if let Some(branch) = to_cut {
                git::create_branch(&workspace, &branch)?;
            }
            Ok(store::hand_tree_to(&workspace, store::Owner::AGENT)?)
        })
        .await
        .expect("the git task should not panic")
        .map_err(broke)
    }

    /// Start the chat's container over both of its directories (ADR-0006),
    /// answering with the container's name.
    async fn spawn(&self, chat_id: &str) -> Result<String> {
        let mut env = BTreeMap::new();
        if let Some(credential) = self.secrets.read(Secret::AnthropicKey)? {
            let variable = AnthropicCredential::of(&credential).variable();
            env.insert(variable.to_owned(), credential);
        }
        Ok(self
            .plane
            .spawn(
                chat_id,
                &self.store.host_workspace_dir(chat_id),
                &self.store.host_claude_dir(chat_id),
                &env,
            )
            .await?
            .name)
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
        manifest.acp_session_id = Some(connection.session_id().to_owned());
        self.store.write_manifest(&manifest).map_err(broke)?;
        self.connections
            .hold(&manifest.chat_id, connection)
            .map_err(broke)?;
        Ok(())
    }
}
