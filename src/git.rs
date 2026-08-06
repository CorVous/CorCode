//! Git as the git command line sees it: clone a base branch, cut the chat's
//! branch off it (ADR-0005).

use std::fmt;
use std::io;
use std::iter;
use std::path::Path;
use std::process::Command;

use chrono::Utc;
use log::warn;
use thiserror::Error;

use crate::config::REDACTED;

/// Where the repositories named in `CORCODE_REPOS` actually live.
pub const GITHUB: &str = "https://github.com";

/// The user half of a token-bearing https clone URL, as GitHub wants it.
const TOKEN_USER: &str = "x-access-token";

/// Git is never given a terminal to ask for credentials on: a chat that
/// cannot be cloned must fail, not hang holding a request open.
const NO_PROMPTS: (&str, &str) = ("GIT_TERMINAL_PROMPT", "0");

/// Who the core commits as on the one commit it authors itself (ADR-0005).
const CORE_COMMITTER: (&str, &str) = ("CorCode core", "corcode@local");

/// What that commit says it is.
const CHECKPOINT_MESSAGE: &str = "Checkpoint uncommitted work at archive";

/// The site the chats' repositories are cloned from, and the token that
/// opens the private ones.
#[derive(Clone)]
pub struct Remotes {
    base: String,
    token: Option<String>,
}

impl Remotes {
    /// Serve `owner/name` repositories from under `base`, which is
    /// [`GITHUB`] in production and a directory of bare repositories in
    /// tests.
    pub fn new(base: impl Into<String>, token: Option<String>) -> Self {
        Self {
            base: base.into(),
            token,
        }
    }

    /// Where one `owner/name` repository is cloned from. A token is put into
    /// the URL only for https, so no local or ssh URL can carry one.
    #[must_use]
    pub fn origin(&self, repo: &str) -> Origin {
        let base = &self.base;
        let token = self
            .token
            .as_ref()
            .filter(|_| base.starts_with("https://"))
            .cloned();
        let authority = token.as_ref().map_or_else(
            || base.clone(),
            |token| base.replacen("https://", &format!("https://{TOKEN_USER}:{token}@"), 1),
        );
        Origin {
            url: format!("{authority}/{repo}.git"),
            token,
        }
    }
}

/// One repository's clone URL, credentials and all.
///
/// The credential is carried only for the length of a clone: it prints, and
/// errors, as [`Origin::scrub`] leaves it, and the workspace git wrote it
/// into is set back to [`Origin::tokenless`] before anyone else can read it.
pub struct Origin {
    url: String,
    token: Option<String>,
}

impl Origin {
    fn url(&self) -> &str {
        &self.url
    }

    /// The same URL with the credential taken out rather than covered up:
    /// still a URL git can fetch and push over.
    fn tokenless(&self) -> String {
        self.token.as_ref().map_or_else(
            || self.url.clone(),
            |token| self.url.replace(&format!("{TOKEN_USER}:{token}@"), ""),
        )
    }

    /// Whatever git said, with the token taken out of it.
    fn scrub(&self, said: &str) -> String {
        self.token
            .as_ref()
            .map_or_else(|| said.to_owned(), |token| said.replace(token, REDACTED))
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.scrub(&self.url))
    }
}

impl fmt::Debug for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Origin")
            .field("url", &self.scrub(&self.url))
            .finish_non_exhaustive()
    }
}

/// Something git would not do. Neither variant can carry a token: every
/// message git wrote is scrubbed on the way in.
#[derive(Debug, Error)]
pub enum GitError {
    #[error("git could not be run to {doing}")]
    Unusable { doing: String, source: io::Error },
    #[error("git failed to {doing}: {complaint}")]
    Refused { doing: String, complaint: String },
}

/// The dated branch a chat's work is cut onto (ADR-0005). An empty slug
/// spells the prefix alone, which is what the console previews.
#[must_use]
pub fn chat_branch(slug: &str) -> String {
    format!("chat/{}-{slug}", Utc::now().format("%Y-%m-%d"))
}

/// What typed text becomes on a branch name.
///
/// Lowercase ASCII alphanumerics with single hyphens between them and none at
/// either end. The console's preview says the same thing in JavaScript; this
/// is the one that binds.
#[must_use]
pub fn slugify(typed: &str) -> String {
    let mut slug = String::with_capacity(typed.len());
    for character in typed.chars() {
        let lowered = character.to_ascii_lowercase();
        if lowered.is_ascii_alphanumeric() {
            slug.push(lowered);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_owned()
}

/// Whether git would take `branch` as a branch name — and whether a command
/// line would take it as a name rather than as another option.
#[must_use]
pub fn names_a_branch(branch: &str) -> bool {
    !branch.is_empty()
        && !branch.starts_with('-')
        && !branch.contains("..")
        && branch
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_./".contains(&byte))
}

/// Clone `origin` at `base_branch` into `dest`, history and all: ADR-0005's
/// commits and ADR-0002's checkpoints are cut from it.
///
/// The clone records the URL it was given, so the remote is set back to the
/// tokenless one before returning: the workspace is the agent's to read
/// (ADR-0001).
pub fn clone_at(origin: &Origin, base_branch: &str, dest: &Path) -> Result<(), GitError> {
    let spelled_dest = dest.to_string_lossy();
    run(
        &format!("clone {origin} at {base_branch}"),
        &[
            "clone",
            "--branch",
            base_branch,
            "--",
            origin.url(),
            &spelled_dest,
        ],
        |said| origin.scrub(said),
    )?;
    let tokenless = origin.tokenless();
    run(
        &format!("take the credential back out of {}", dest.display()),
        &[
            "-C",
            &spelled_dest,
            "remote",
            "set-url",
            "origin",
            &tokenless,
        ],
        |said| origin.scrub(said),
    )
    .map(drop)
}

/// Cut `branch` off whatever `workspace` has checked out and stand on it.
/// Nothing is pushed: the branch reaches the remote with its first commit
/// (ADR-0005).
pub fn create_branch(workspace: &Path, branch: &str) -> Result<(), GitError> {
    run(
        &format!("cut branch {branch}"),
        &["-C", &workspace.to_string_lossy(), "checkout", "-b", branch],
        ToOwned::to_owned,
    )
    .map(drop)
}

/// The branch a chat's unpushed work is checkpointed onto (ADR-0005).
///
/// The chat's own branch, stamped to the second, so that two archives moments
/// apart cannot name the same branch.
#[must_use]
pub fn checkpoint_branch(branch: &str) -> String {
    format!("{branch}-chkpt-{}", Utc::now().format("%Y%m%dT%H%M%S"))
}

/// What the archive gate got onto the remote (ADR-0005).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pushed {
    /// Where `branch` stands now: the agent's last semantic commit.
    pub tip: String,
    /// The branch the unpushed work went onto, if there was any.
    pub checkpoint_branch: Option<String>,
}

/// A gate that stopped, and what of the workspace was already on the remote
/// when it did.
///
/// The checkpoint goes first, so the chat branch can be the push that fails
/// with the operator's work already safe. Saying nothing about that would
/// send them looking for work that is right there.
#[derive(Debug, Error)]
#[error("{source}")]
pub struct PushFailure {
    /// The checkpoint branch that reached the remote before this happened.
    pub landed: Option<String>,
    pub source: GitError,
}

/// A failure that happened before anything could reach the remote.
const fn unpushed(source: GitError) -> PushFailure {
    PushFailure {
        landed: None,
        source,
    }
}

/// Where a workspace stands: the commit HEAD is on, and what to check out to
/// put it back there — its branch, or the commit itself when HEAD is
/// detached.
struct Standing {
    head: String,
    at: String,
}

/// Get everything in `workspace` onto the remote so the directory can be
/// deleted (ADR-0002 rule 3, ADR-0005).
///
/// Anything `branch` does not already carry — a dirty tree, commits made off
/// the branch — is committed onto a checkpoint branch of its own, leaving
/// `branch` at the agent's last semantic commit; both branches are then
/// pushed. Nothing here is destructive: the workspace is left standing on
/// `branch` either way, so whatever the caller does next can be retried from
/// it.
pub fn push_for_archive(
    origin: &Origin,
    workspace: &Path,
    branch: &str,
) -> Result<Pushed, PushFailure> {
    let standing = standing(workspace).map_err(unpushed)?;
    let checkpoint = checkpoint_unpushed_work(workspace, branch, &standing).map_err(unpushed)?;
    match push_branches(origin, workspace, branch, checkpoint.as_deref()) {
        Ok(tip) => {
            stand_on(workspace, branch).map_err(|source| PushFailure {
                landed: checkpoint.clone(),
                source,
            })?;
            Ok(Pushed {
                tip,
                checkpoint_branch: checkpoint,
            })
        }
        Err(failure) => {
            if let Some(checkpoint) = &checkpoint
                && let Err(stubborn) = undo_checkpoint(workspace, &standing, checkpoint)
            {
                warn!("{checkpoint} could not be rolled back: {stubborn}");
            }
            Err(failure)
        }
    }
}

/// Where `workspace` has HEAD right now.
fn standing(workspace: &Path) -> Result<Standing, GitError> {
    let spelled = workspace.to_string_lossy();
    let doing = format!("read where {} stands", workspace.display());
    let head = run(
        &doing,
        &["-C", &spelled, "rev-parse", "HEAD"],
        ToOwned::to_owned,
    )?;
    let branch = run(
        &doing,
        &["-C", &spelled, "branch", "--show-current"],
        ToOwned::to_owned,
    )?;
    Ok(Standing {
        at: if branch.is_empty() {
            head.clone()
        } else {
            branch
        },
        head,
    })
}

/// Commit whatever `branch` would not carry onto a checkpoint branch, and say
/// which branch that was.
///
/// A workspace standing somewhere else holds commits `branch` never saw, and
/// the gate is the last look anyone gets at them: they are checkpointed even
/// though the tree is clean.
fn checkpoint_unpushed_work(
    workspace: &Path,
    branch: &str,
    standing: &Standing,
) -> Result<Option<String>, GitError> {
    let dirty = dirty(workspace)?;
    if !dirty && standing.at == branch {
        return Ok(None);
    }
    let checkpoint = checkpoint_branch(branch);
    cut_checkpoint(workspace, &checkpoint)?;
    if dirty {
        commit_everything(workspace, &checkpoint)?;
    }
    Ok(Some(checkpoint))
}

/// Whether `workspace` holds anything git has not been told about.
fn dirty(workspace: &Path) -> Result<bool, GitError> {
    let said = run(
        &format!("read the state of {}", workspace.display()),
        &["-C", &workspace.to_string_lossy(), "status", "--porcelain"],
        ToOwned::to_owned,
    )?;
    Ok(!said.is_empty())
}

/// Name the commit the workspace stands on, so what follows can be pushed.
fn cut_checkpoint(workspace: &Path, checkpoint: &str) -> Result<(), GitError> {
    run(
        &format!("cut {checkpoint} off the working tree"),
        &[
            "-C",
            &workspace.to_string_lossy(),
            "checkout",
            "-b",
            checkpoint,
        ],
        ToOwned::to_owned,
    )
    .map(drop)
}

/// Commit the whole working tree onto the checkpoint branch, in the core's
/// own name: these are the one kind of commit no agent authored (ADR-0005).
fn commit_everything(workspace: &Path, checkpoint: &str) -> Result<(), GitError> {
    let workspace = workspace.to_string_lossy();
    let doing = |what: &str| format!("{what} on {checkpoint}");
    run(
        &doing("stage the working tree"),
        &["-C", &workspace, "add", "-A"],
        ToOwned::to_owned,
    )?;
    run(
        &doing("commit the working tree"),
        &[
            "-C",
            &workspace,
            "-c",
            &format!("user.name={}", CORE_COMMITTER.0),
            "-c",
            &format!("user.email={}", CORE_COMMITTER.1),
            "commit",
            "--message",
            CHECKPOINT_MESSAGE,
        ],
        ToOwned::to_owned,
    )
    .map(drop)
}

/// Push the checkpoint branch, then the chat's own, answering with where the
/// chat's branch stands. Whatever fails, the failure carries the checkpoint
/// that got there first.
fn push_branches(
    origin: &Origin,
    workspace: &Path,
    branch: &str,
    checkpoint: Option<&str>,
) -> Result<String, PushFailure> {
    let spelled = workspace.to_string_lossy();
    let mut landed = None;
    for pushing in checkpoint.into_iter().chain(iter::once(branch)) {
        push_one(origin, &spelled, pushing).map_err(|source| PushFailure {
            landed: landed.clone(),
            source,
        })?;
        landed = checkpoint.map(ToOwned::to_owned);
    }
    run(
        &format!("read where {branch} stands"),
        &["-C", &spelled, "rev-parse", branch],
        ToOwned::to_owned,
    )
    .map_err(|source| PushFailure { landed, source })
}

/// Put one branch on the remote under its own name.
fn push_one(origin: &Origin, workspace: &str, pushing: &str) -> Result<(), GitError> {
    run(
        &format!("push {pushing} to {origin}"),
        &[
            "-C",
            workspace,
            "push",
            "--",
            origin.url(),
            &format!("{pushing}:refs/heads/{pushing}"),
        ],
        |said| origin.scrub(said),
    )
    .map(drop)
}

/// Stand the workspace on the branch its chat works from, which is where
/// anyone who retries the archive expects to find it.
fn stand_on(workspace: &Path, branch: &str) -> Result<(), GitError> {
    run(
        &format!("stand {} back on {branch}", workspace.display()),
        &["-C", &workspace.to_string_lossy(), "checkout", branch],
        ToOwned::to_owned,
    )
    .map(drop)
}

/// Put a checkpoint nobody could push back the way it was found: the commit
/// undone, its files dirty again, the workspace standing where it was, the
/// branch gone.
fn undo_checkpoint(
    workspace: &Path,
    standing: &Standing,
    checkpoint: &str,
) -> Result<(), GitError> {
    let workspace = workspace.to_string_lossy();
    let doing = format!("roll back {checkpoint}");
    for args in [
        vec!["-C", &workspace, "reset", "--mixed", &standing.head],
        vec!["-C", &workspace, "checkout", &standing.at],
        vec!["-C", &workspace, "branch", "-d", checkpoint],
    ] {
        run(&doing, &args, ToOwned::to_owned)?;
    }
    Ok(())
}

/// Run one git command, answering with what it wrote on its way out. A
/// non-zero exit is the failure it is, in git's own words with `scrub`
/// applied to them.
fn run(doing: &str, args: &[&str], scrub: impl FnOnce(&str) -> String) -> Result<String, GitError> {
    let output = Command::new("git")
        .args(args)
        .env(NO_PROMPTS.0, NO_PROMPTS.1)
        .output()
        .map_err(|source| GitError::Unusable {
            doing: doing.to_owned(),
            source,
        })?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned());
    }
    Err(GitError::Refused {
        doing: doing.to_owned(),
        complaint: scrub(String::from_utf8_lossy(&output.stderr).trim()),
    })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;

    const TOKEN: &str = "ghs-clone-secret";

    #[test]
    fn a_typed_title_becomes_a_slug_a_branch_can_carry() {
        for (typed, slug) in [
            ("Resume Ladder!", "resume-ladder"),
            ("  spaced  out  ", "spaced-out"),
            ("ALL/CAPS", "all-caps"),
            ("émigré", "migr"),
            ("!!!", ""),
        ] {
            assert_eq!(slugify(typed), slug, "{typed} slugified wrong");
        }
    }

    #[test]
    fn a_chat_branch_is_dated_and_prefixed() {
        let today = Utc::now().format("%Y-%m-%d");

        assert_eq!(chat_branch("a-slug"), format!("chat/{today}-a-slug"));
        assert_eq!(chat_branch(""), format!("chat/{today}-"));
    }

    #[test]
    fn a_branch_name_that_git_would_read_as_an_option_is_not_a_branch_name() {
        for named in ["main", "release/2026-08", "a_branch.name"] {
            assert!(names_a_branch(named), "{named} is a branch name");
        }
        for unnamed in ["", "--upload-pack=evil", "-x", "a..b", "a branch", "a;b"] {
            assert!(!names_a_branch(unnamed), "{unnamed} is not a branch name");
        }
    }

    #[test]
    fn a_clone_url_names_the_repository_on_github() {
        let origin = Remotes::new(GITHUB, None).origin("CorVous/CorCode");

        assert_eq!(origin.url(), "https://github.com/CorVous/CorCode.git");
    }

    #[test]
    fn a_token_rides_in_the_url_and_nowhere_else() {
        let origin = Remotes::new(GITHUB, Some(TOKEN.to_owned())).origin("CorVous/CorCode");

        assert_eq!(
            origin.url(),
            format!("https://x-access-token:{TOKEN}@github.com/CorVous/CorCode.git")
        );
        assert!(
            !format!("{origin}{origin:?}").contains(TOKEN),
            "the token is spoken out loud: {origin:?}"
        );
        assert_eq!(
            origin.tokenless(),
            "https://github.com/CorVous/CorCode.git",
            "what the workspace is left pointing at should still be fetchable"
        );
    }

    #[test]
    fn whatever_git_says_comes_back_without_the_token() {
        let origin = Remotes::new(GITHUB, Some(TOKEN.to_owned())).origin("CorVous/CorCode");

        let scrubbed = origin.scrub(&format!(
            "fatal: unable to access 'https://x-access-token:{TOKEN}@github.com/CorVous/CorCode.git/': 404"
        ));

        assert!(!scrubbed.contains(TOKEN), "the token leaked: {scrubbed}");
        assert!(scrubbed.contains(crate::config::REDACTED));
    }

    #[test]
    fn a_clone_brings_the_base_branch_and_its_whole_history() {
        let (_origin_dir, remotes) = seeded_repository();
        let into = TempDir::new().expect("clone target should be created");
        let dest = into.path().join("workspace");

        clone_at(&remotes.origin(REPO), "main", &dest).expect("the seeded repo should clone");

        assert_eq!(
            git_says(&dest, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "main"
        );
        assert_eq!(
            git_says(&dest, &["rev-list", "--count", "HEAD"]),
            "2",
            "a shallow clone would strand the commits ADR-0005 builds on"
        );
        assert!(dest.join("README.md").is_file());
    }

    /// The workspace is bind-mounted into the agent's container (ADR-0001),
    /// so anything git writes into `.git/config` is the agent's to read.
    #[test]
    fn a_clone_leaves_no_credential_behind_in_the_workspace() {
        let (_origin_dir, origin) = credentialed_repository();
        let into = TempDir::new().expect("clone target should be created");
        let dest = into.path().join("workspace");

        clone_at(&origin, "main", &dest).expect("the credentialed repo should clone");

        let config = std::fs::read_to_string(dest.join(".git").join("config"))
            .expect("a clone writes a config");
        assert!(
            !config.contains(TOKEN),
            "the token was left where the agent can read it: {config}"
        );
        assert!(
            git_says(&dest, &["remote", "get-url", "origin"]).contains("github.com"),
            "the workspace was left pointing at nowhere it could push"
        );
    }

    #[test]
    fn a_clone_of_a_branch_that_is_not_there_fails_loudly() {
        let (_origin_dir, remotes) = seeded_repository();
        let into = TempDir::new().expect("clone target should be created");
        let dest = into.path().join("workspace");

        let error = clone_at(&remotes.origin(REPO), "no-such-branch", &dest)
            .expect_err("cloning a missing branch should fail");

        assert!(
            format!("{error}").contains("no-such-branch"),
            "error should say which branch, got: {error}"
        );
    }

    #[test]
    fn a_new_branch_is_checked_out_and_pushed_nowhere() {
        let (origin_dir, remotes) = seeded_repository();
        let into = TempDir::new().expect("clone target should be created");
        let dest = into.path().join("workspace");
        clone_at(&remotes.origin(REPO), "main", &dest).expect("the seeded repo should clone");

        create_branch(&dest, "chat/2026-08-05-a-slug").expect("the chat branch should be cut");

        assert_eq!(
            git_says(&dest, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "chat/2026-08-05-a-slug"
        );
        assert_eq!(
            git_says(
                &origin_dir.path().join(BARE),
                &["branch", "--format=%(refname:short)"]
            ),
            "main",
            "the chat branch reached the remote before its first commit"
        );
    }

    #[test]
    fn cutting_a_branch_that_is_already_there_fails_loudly() {
        let (_origin_dir, remotes) = seeded_repository();
        let into = TempDir::new().expect("clone target should be created");
        let dest = into.path().join("workspace");
        clone_at(&remotes.origin(REPO), "main", &dest).expect("the seeded repo should clone");

        let error =
            create_branch(&dest, "main").expect_err("cutting an existing branch should fail");

        assert!(
            format!("{error}").contains("main"),
            "error should say which branch, got: {error}"
        );
    }

    const REPO: &str = "CorVous/fixture";
    const BARE: &str = "CorVous/fixture.git";
    const CHAT_BRANCH: &str = "chat/2026-08-05-archived";

    /// Two archives of one chat a few seconds apart must not name the same
    /// branch: the second push would be refused as a non-fast-forward.
    #[test]
    fn a_checkpoint_branch_is_stamped_to_the_second() {
        let stamp = Utc::now().format("%Y%m%dT%H%M%S");

        assert_eq!(
            checkpoint_branch(CHAT_BRANCH),
            format!("{CHAT_BRANCH}-chkpt-{stamp}")
        );
    }

    #[test]
    fn a_clean_workspace_pushes_its_branch_and_cuts_no_checkpoint() {
        let (origin_dir, remotes) = seeded_repository();
        let (_into, workspace) = chat_workspace(&remotes.origin(REPO));
        let semantic = commit_in(&workspace, "third.txt", "the agent's own commit");

        let pushed = push_for_archive(&remotes.origin(REPO), &workspace, CHAT_BRANCH)
            .expect("a clean workspace should push");

        assert_eq!(pushed.checkpoint_branch, None);
        assert_eq!(pushed.tip, semantic);
        assert_eq!(
            git_says(&origin_dir.path().join(BARE), &["rev-parse", CHAT_BRANCH]),
            semantic,
            "the chat branch did not reach the remote"
        );
    }

    #[test]
    fn a_dirty_workspace_checkpoints_onto_a_branch_of_its_own() {
        let (origin_dir, remotes) = seeded_repository();
        let (_into, workspace) = chat_workspace(&remotes.origin(REPO));
        let semantic = commit_in(&workspace, "third.txt", "the agent's own commit");
        std::fs::write(workspace.join("scratch.txt"), "work in flight")
            .expect("a dirty file should be writable");
        let bare = origin_dir.path().join(BARE);

        let pushed = push_for_archive(&remotes.origin(REPO), &workspace, CHAT_BRANCH)
            .expect("a dirty workspace should push");

        let checkpoint = pushed
            .checkpoint_branch
            .expect("a dirty tree should cut a checkpoint branch");
        assert!(
            checkpoint.starts_with(&format!("{CHAT_BRANCH}-chkpt-")),
            "the checkpoint branch is not named after the chat's: {checkpoint}"
        );
        assert_eq!(
            pushed.tip, semantic,
            "the chat branch left the agent's last semantic commit"
        );
        assert_eq!(git_says(&bare, &["rev-parse", CHAT_BRANCH]), semantic);
        assert_eq!(
            git_says(&bare, &["show", &format!("{checkpoint}:scratch.txt")]),
            "work in flight",
            "the dirty state never reached the remote"
        );
        assert_eq!(
            git_says(&bare, &["log", "-1", "--format=%cn <%ce>", &checkpoint]),
            "CorCode core <corcode@local>",
            "the checkpoint was committed as somebody else"
        );
    }

    /// An agent that wanders off its branch — a detached rebase, a stray
    /// checkout — leaves commits nothing else would push. The gate is the last
    /// look anyone gets at the workspace, so it takes them too.
    #[test]
    fn a_clean_workspace_standing_off_its_chat_branch_is_checkpointed_all_the_same() {
        let (origin_dir, remotes) = seeded_repository();
        let (_into, workspace) = chat_workspace(&remotes.origin(REPO));
        commit_in(&workspace, "third.txt", "the agent's own commit");
        run(&workspace, &["checkout", "--detach"]);
        let stray = commit_in(
            &workspace,
            "fourth.txt",
            "a commit the chat branch never saw",
        );

        let pushed = push_for_archive(&remotes.origin(REPO), &workspace, CHAT_BRANCH)
            .expect("a workspace off its branch should push");

        let checkpoint = pushed
            .checkpoint_branch
            .expect("work off the chat branch should be checkpointed");
        assert_eq!(
            git_says(&origin_dir.path().join(BARE), &["rev-parse", &checkpoint]),
            stray,
            "the commit the chat branch never saw was left to be deleted"
        );
    }

    /// Whatever the caller does after the gate — write the manifest, tear the
    /// container down — can fail, and it retries from this workspace.
    #[test]
    fn a_gate_that_pushed_everything_leaves_the_workspace_on_the_chat_branch() {
        let (_origin_dir, remotes) = seeded_repository();
        let (_into, workspace) = chat_workspace(&remotes.origin(REPO));
        commit_in(&workspace, "third.txt", "the agent's own commit");
        std::fs::write(workspace.join("scratch.txt"), "work in flight")
            .expect("a dirty file should be writable");

        push_for_archive(&remotes.origin(REPO), &workspace, CHAT_BRANCH)
            .expect("a dirty workspace should push");

        assert_eq!(
            git_says(&workspace, &["rev-parse", "--abbrev-ref", "HEAD"]),
            CHAT_BRANCH,
            "the workspace was left standing on the checkpoint branch"
        );
    }

    #[test]
    fn a_push_that_fails_leaves_the_workspace_as_it_was() {
        let (_origin_dir, remotes) = seeded_repository();
        let (_into, workspace) = chat_workspace(&remotes.origin(REPO));
        commit_in(&workspace, "third.txt", "the agent's own commit");
        std::fs::write(workspace.join("scratch.txt"), "work in flight")
            .expect("a dirty file should be writable");
        let unreachable = Remotes::new("file:///nowhere/at/all", None);

        let error = push_for_archive(&unreachable.origin(REPO), &workspace, CHAT_BRANCH)
            .expect_err("a push to nowhere should fail");

        assert!(
            format!("{error}").contains("push"),
            "error should say what it was doing, got: {error}"
        );
        assert_eq!(
            git_says(&workspace, &["rev-parse", "--abbrev-ref", "HEAD"]),
            CHAT_BRANCH,
            "the workspace was left standing somewhere else"
        );
        assert_eq!(
            git_says(&workspace, &["status", "--porcelain"]),
            "?? scratch.txt",
            "the work the operator can still retry from was swallowed"
        );
        assert_eq!(
            git_says(&workspace, &["branch", "--list", "*chkpt*"]),
            "",
            "an unpushed checkpoint branch was left where the next try will not find it"
        );
    }

    #[test]
    fn the_gate_leaves_no_credential_behind_in_the_workspace() {
        let (_origin_dir, origin) = credentialed_repository();
        let (_into, workspace) = chat_workspace(&origin);
        std::fs::write(workspace.join("scratch.txt"), "work in flight")
            .expect("a dirty file should be writable");

        push_for_archive(&origin, &workspace, CHAT_BRANCH).expect("a credentialed push should go");

        let config = std::fs::read_to_string(workspace.join(".git").join("config"))
            .expect("a clone writes a config");
        assert!(
            !config.contains(TOKEN),
            "the token was left where the agent can read it: {config}"
        );
    }

    /// A cloned workspace standing on its chat branch, ready to commit as the
    /// agent would.
    fn chat_workspace(origin: &Origin) -> (TempDir, PathBuf) {
        let into = TempDir::new().expect("clone target should be created");
        let workspace = into.path().join("workspace");
        clone_at(origin, "main", &workspace).expect("the seeded repo should clone");
        create_branch(&workspace, CHAT_BRANCH).expect("the chat branch should be cut");
        run(
            &workspace,
            &["config", "user.email", "agent@example.invalid"],
        );
        run(&workspace, &["config", "user.name", "Agent"]);
        (into, workspace)
    }

    /// One agent-authored commit, answering with the sha it landed on.
    fn commit_in(workspace: &Path, file: &str, message: &str) -> String {
        std::fs::write(workspace.join(file), message).expect("a file should be writable");
        run(workspace, &["add", "."]);
        run(workspace, &["commit", "-m", message]);
        git_says(workspace, &["rev-parse", "HEAD"])
    }

    /// A bare repository with two commits on `main`, reachable over `file://`
    /// so that no test needs the network.
    fn seeded_repository() -> (TempDir, Remotes) {
        let dir = TempDir::new().expect("origin dir should be created");
        seed(dir.path(), &dir.path().join(BARE));
        let served_from = format!("file://{}", spelled(dir.path()));
        (dir, Remotes::new(served_from, None))
    }

    /// A repository whose clone URL carries a credential the way GitHub's
    /// does. The credential is spelled into a path a real clone can reach, so
    /// what git writes into the workspace is the real thing.
    fn credentialed_repository() -> (TempDir, Origin) {
        let dir = TempDir::new().expect("origin dir should be created");
        let bare = dir
            .path()
            .join(format!("{TOKEN_USER}:{TOKEN}@github.com"))
            .join(BARE);
        seed(dir.path(), &bare);
        let origin = Origin {
            url: format!("file://{}", spelled(&bare)),
            token: Some(TOKEN.to_owned()),
        };
        (dir, origin)
    }

    /// Two commits on `main`, pushed into a bare repository at `bare`.
    fn seed(dir: &Path, bare: &Path) {
        let work = dir.join("seed");
        run(
            dir,
            &["init", "--bare", "--initial-branch=main", &spelled(bare)],
        );
        run(dir, &["init", "--initial-branch=main", &spelled(&work)]);
        run(&work, &["config", "user.email", "seed@example.invalid"]);
        run(&work, &["config", "user.name", "Seed"]);
        for (file, message) in [("README.md", "first"), ("second.txt", "second")] {
            std::fs::write(work.join(file), message).expect("seed file should be writable");
            run(&work, &["add", "."]);
            run(&work, &["commit", "-m", message]);
        }
        run(&work, &["remote", "add", "origin", &spelled(bare)]);
        run(&work, &["push", "origin", "main"]);
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

    fn git_says(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git should run");
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }
}
