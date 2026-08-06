//! The chats the console reads and the vertical that cuts a new one
//! (ADR-0005, ADR-0006).

use std::collections::{BTreeMap, HashSet};

use anyhow::Result;
use chrono::Utc;
use log::{error, info, warn};
use serde_json::json;
use thiserror::Error;
use ulid::Ulid;

use crate::acp::{AcpError, AcpTransport, Adapter, Connections, Held, Turn};
use crate::config::Config;
use crate::git::{self, GitError, Remotes};
use crate::plane::ContainerPlane;
use crate::pool;
use crate::store::{
    ChatState, ChatStore, ContainerLiveness, Event, Manifest, NewChat, runtime_status,
};
use crate::sweep::{self, Sweep};
use crate::ui;

/// The variable the agent reads its Anthropic credentials from (ADR-0001).
const API_KEY: &str = "ANTHROPIC_API_KEY";

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
    #[error("this deployment does not offer {repo}")]
    UnknownRepo { repo: String },
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
    #[error("this chat has no live connection")]
    NotConnected,
    #[error("this chat is still answering the last prompt")]
    Busy,
    #[error("the turn could not be written down")]
    Unrecorded(#[source] anyhow::Error),
    #[error("the turn broke")]
    Broke(#[source] anyhow::Error),
}

impl PromptError {
    /// Whether the chat was turned away rather than the turn failing. A
    /// refusal is worth a line in the log; a failure is worth the operator's
    /// server log.
    const fn is_refusal(&self) -> bool {
        matches!(self, Self::NotConnected | Self::Busy)
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

/// What a refusal calls itself in a chat's own log (ADR-0006).
const REFUSAL: &str = "refusal";

/// What an archive that got nothing onto the remote calls itself there.
const PUSH_FAILURE: &str = "push_failure";

/// The right to decide who gives a container up, held by one capping at a
/// time.
type ParkingLock = tokio::sync::Mutex<()>;

/// Every chat in one dataset: who is live, what they hold, and how a new one
/// comes to exist.
pub struct Chats<P, T: AcpTransport> {
    store: ChatStore,
    plane: P,
    adapter: Adapter<T>,
    connections: Connections<T::Channel>,
    remotes: Remotes,
    repos: Vec<String>,
    anthropic_api_key: Option<String>,
    workspace_image: String,
    warm_pool: usize,
    parking: ParkingLock,
}

impl<P, T: AcpTransport + Sync> Chats<P, T> {
    /// Serve the dataset `config` names, over `plane` and the adapters
    /// `transport` reaches, from the repositories `remotes` holds.
    pub fn new(config: &Config, plane: P, transport: T, remotes: Remotes) -> Self {
        Self {
            store: ChatStore::new(&config.data_dir),
            plane,
            adapter: Adapter::new(transport),
            connections: Connections::default(),
            remotes,
            repos: config.repos.clone(),
            anthropic_api_key: config.anthropic_api_key.clone(),
            workspace_image: config.workspace_image.clone(),
            warm_pool: config.warm_pool,
            parking: ParkingLock::default(),
        }
    }
}

impl<P, T> Chats<P, T>
where
    P: ContainerPlane + ContainerLiveness + Sync,
    T: AcpTransport + Sync,
{
    /// The pinned image every chat runs (ADR-0004).
    #[must_use]
    pub fn workspace_image(&self) -> &str {
        &self.workspace_image
    }

    /// How many containers this deployment keeps warm (ADR-0002 rule 2).
    #[must_use]
    pub const fn warm_pool(&self) -> usize {
        self.warm_pool
    }

    /// The repositories a new chat may be cut from, first one default.
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
            Ok(()) => {
                self.touch(&chat_id);
                self.cap_the_pool().await;
            }
            Err(refusal) if refusal.is_refusal() => self.note_refusal(&chat_id, refusal),
            Err(_) => {}
        }
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
    /// the chat it followed is already done.
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
            &self.connections.busy_chat_ids(),
            self.warm_pool,
        ))
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
        let pushed = match self.push_everything(&manifest).await {
            Ok(pushed) => pushed,
            Err(failure) => {
                self.note(&chat_id, PUSH_FAILURE, &format!("Nothing was archived: {failure}. The chat is still open and can be archived again."));
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
        for chat_id in &swept.orphaned {
            match self.store.remove_workspace(chat_id) {
                Ok(()) => info!("{chat_id} left a workspace no chat claims: removed"),
                Err(stubborn) => warn!("{chat_id} left a workspace that will not go: {stubborn:#}"),
            }
        }
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
        Ok(sweep::reconcile(&self.store.workspace_ids()?, &open, &live))
    }

    /// Get the chat's workspace onto the remote, whole. Git blocks, so it
    /// runs off the runtime.
    async fn push_everything(&self, manifest: &Manifest) -> Result<git::Pushed, GitError> {
        let origin = self.remotes.origin(&manifest.repo);
        let workspace = self.store.workspace_dir(&manifest.chat_id);
        let branch = manifest.branch.clone();
        tokio::task::spawn_blocking(move || git::push_for_archive(&origin, &workspace, &branch))
            .await
            .expect("the git task should not panic")
    }

    async fn turn(&self, chat_id: &str, said: &str) -> Result<(), PromptError> {
        let connection = self.connected(chat_id)?;
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

    /// The live connection a prompt goes over. A chat that has none is where
    /// ADR-0007's ladder — spawn, then resume — will land; until it exists,
    /// a prompt into such a chat wakes nothing and says so.
    fn connected(&self, chat_id: &str) -> Result<Held<T::Channel>, PromptError> {
        self.connections
            .of(chat_id)
            .ok_or(PromptError::NotConnected)
    }

    /// Cut a new chat whole: both trees, a clone of the repository at its
    /// base branch, the chat's own branch, a container, and the ACP session
    /// they will talk over. A step that fails leaves what came before it on
    /// disk for the operator to look at (ADR-0007 rule 5).
    pub async fn create(&self, wanted: WantedChat) -> Result<String, CreateError> {
        let typed = wanted.slug.trim();
        let slug = git::slugify(typed);
        if slug.is_empty() {
            return Err(CreateError::Unnamed);
        }
        if !self.repos.contains(&wanted.repo) {
            return Err(CreateError::UnknownRepo { repo: wanted.repo });
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
        let container = self.spawn(&chat_id).await?;
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

    /// Clone the repository into the chat's workspace and stand on the branch
    /// the chat works from. Git blocks, so it runs off the runtime.
    async fn check_out(
        &self,
        chat_id: &str,
        wanted: &WantedChat,
        branch: &str,
    ) -> Result<(), CreateError> {
        let origin = self.remotes.origin(&wanted.repo);
        let workspace = self.store.workspace_dir(chat_id);
        let base_branch = wanted.base_branch.clone();
        let to_cut = (!wanted.direct_on_base).then(|| branch.to_owned());
        tokio::task::spawn_blocking(move || {
            git::clone_at(&origin, &base_branch, &workspace)?;
            to_cut.map_or(Ok(()), |branch| git::create_branch(&workspace, &branch))
        })
        .await
        .expect("the git task should not panic")
        .map_err(broke)
    }

    /// Start the chat's container over both of its directories (ADR-0006),
    /// answering with the container's name.
    async fn spawn(&self, chat_id: &str) -> Result<String, CreateError> {
        let mut env = BTreeMap::new();
        if let Some(key) = &self.anthropic_api_key {
            env.insert(API_KEY.to_owned(), key.clone());
        }
        self.plane
            .spawn(
                chat_id,
                &self.store.workspace_dir(chat_id),
                &self.store.claude_dir(chat_id),
                &env,
            )
            .await
            .map(|container| container.name)
            .map_err(broke)
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
        self.connections.hold(&manifest.chat_id, connection);
        Ok(())
    }
}
