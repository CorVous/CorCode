//! The chats the console reads and the vertical that cuts a new one
//! (ADR-0005, ADR-0006).

use std::collections::BTreeMap;

use anyhow::Result;
use chrono::Utc;
use log::{info, warn};
use serde_json::json;
use thiserror::Error;
use ulid::Ulid;

use crate::acp::{AcpError, AcpTransport, Adapter, Connections, Held};
use crate::config::Config;
use crate::git::{self, Remotes};
use crate::plane::ContainerPlane;
use crate::pool;
use crate::store::{ChatStore, ContainerLiveness, Event, Manifest, NewChat, runtime_status};
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

/// What a refusal calls itself in a chat's own log (ADR-0006).
const REFUSAL: &str = "refusal";

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
    async fn cap_the_pool(&self) {
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
        Ok(pool::beyond_the_pool(&chats, &live, self.warm_pool))
    }

    /// Park one chat: the container goes, the workspace and the agent's
    /// memory stay where they are, and nothing at all is committed
    /// (ADR-0002 rule 2, ADR-0005).
    async fn park(&self, chat_id: &str) {
        match self.plane.teardown(chat_id).await {
            Ok(()) => {
                self.connections.forget(chat_id);
                info!("{chat_id} parked: workspace kept, container torn down");
            }
            Err(stubborn) => warn!("{chat_id} is past the pool and would not stop: {stubborn}"),
        }
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
        let line = json!({"corcode": REFUSAL, "text": format!("Prompt not sent: {refusal}.")});
        if let Err(failure) = self.store.append_event(chat_id, &line) {
            warn!("a refusal could not be written down: {failure:#}");
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
