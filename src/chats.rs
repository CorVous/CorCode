//! The chats the console reads and the vertical that cuts a new one
//! (ADR-0005, ADR-0006).

use std::collections::BTreeMap;

use anyhow::Result;
use log::warn;
use thiserror::Error;
use ulid::Ulid;

use crate::acp::{AcpTransport, Adapter};
use crate::config::Config;
use crate::git::{self, Remotes};
use crate::plane::ContainerPlane;
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

/// Every chat in one dataset: who is live, what they hold, and how a new one
/// comes to exist.
pub struct Chats<P, T> {
    store: ChatStore,
    plane: P,
    adapter: Adapter<T>,
    remotes: Remotes,
    repos: Vec<String>,
    anthropic_api_key: Option<String>,
    workspace_image: String,
}

impl<P, T: AcpTransport + Sync> Chats<P, T> {
    /// Serve the dataset `config` names, over `plane` and the adapters
    /// `transport` reaches, from the repositories `remotes` holds.
    pub fn new(config: &Config, plane: P, transport: T, remotes: Remotes) -> Self {
        Self {
            store: ChatStore::new(&config.data_dir),
            plane,
            adapter: Adapter::new(transport),
            remotes,
            repos: config.repos.clone(),
            anthropic_api_key: config.anthropic_api_key.clone(),
            workspace_image: config.workspace_image.clone(),
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
            Ok(()) => Ok(chat_id),
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

    /// Open the ACP session and write its id into the manifest, which is the
    /// only trace a new session leaves: it is not an event (ADR-0006).
    async fn record_session(
        &self,
        mut manifest: Manifest,
        container: &str,
    ) -> Result<(), CreateError> {
        let session_id = self.adapter.open_session(container).await.map_err(broke)?;
        manifest.acp_session_id = Some(session_id);
        self.store.write_manifest(&manifest).map_err(broke)
    }
}
