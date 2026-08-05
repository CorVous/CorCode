//! Git as the git command line sees it: clone a base branch, cut the chat's
//! branch off it (ADR-0005).

use std::fmt;
use std::io;
use std::path::Path;
use std::process::{Command, Output};

use thiserror::Error;

use crate::config::REDACTED;

/// Where the repositories named in `CORCODE_REPOS` actually live.
pub const GITHUB: &str = "https://github.com";

/// The user half of a token-bearing https clone URL, as GitHub wants it.
const TOKEN_USER: &str = "x-access-token";

/// Git is never given a terminal to ask for credentials on: a chat that
/// cannot be cloned must fail, not hang holding a request open.
const NO_PROMPTS: (&str, &str) = ("GIT_TERMINAL_PROMPT", "0");

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

    /// Where one `owner/name` repository is cloned from. A token rides only
    /// on https, so no local or ssh URL can carry it out of the process.
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

/// One repository's clone URL, credentials and all. Nothing but `git` ever
/// sees the whole of it: it prints, and errors, as the URL without them.
pub struct Origin {
    url: String,
    token: Option<String>,
}

impl Origin {
    fn url(&self) -> &str {
        &self.url
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

/// Clone `origin` at `base_branch` into `dest`, history and all: ADR-0005's
/// commits and ADR-0002's checkpoints are cut from it.
pub fn clone_at(origin: &Origin, base_branch: &str, dest: &Path) -> Result<(), GitError> {
    let doing = format!("clone {origin} at {base_branch}");
    let dest = dest.to_string_lossy();
    let output = git(
        &doing,
        &["clone", "--branch", base_branch, "--", origin.url(), &dest],
    )?;
    judge(&doing, &output, |said| origin.scrub(said))
}

/// Cut `branch` off whatever `workspace` has checked out and stand on it.
/// Nothing is pushed: the branch reaches the remote with its first commit
/// (ADR-0005).
pub fn create_branch(workspace: &Path, branch: &str) -> Result<(), GitError> {
    let doing = format!("cut branch {branch}");
    let workspace = workspace.to_string_lossy();
    let output = git(&doing, &["-C", &workspace, "checkout", "-b", branch])?;
    judge(&doing, &output, ToOwned::to_owned)
}

fn git(doing: &str, args: &[&str]) -> Result<Output, GitError> {
    Command::new("git")
        .args(args)
        .env(NO_PROMPTS.0, NO_PROMPTS.1)
        .output()
        .map_err(|source| GitError::Unusable {
            doing: doing.to_owned(),
            source,
        })
}

/// Turn a non-zero exit into the failure it is, in git's own words.
fn judge(doing: &str, output: &Output, scrub: impl FnOnce(&str) -> String) -> Result<(), GitError> {
    if output.status.success() {
        return Ok(());
    }
    Err(GitError::Refused {
        doing: doing.to_owned(),
        complaint: scrub(String::from_utf8_lossy(&output.stderr).trim()),
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;

    const TOKEN: &str = "ghs-clone-secret";

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

    /// A bare repository with two commits on `main`, reachable over `file://`
    /// so that no test needs the network.
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
        for (file, message) in [("README.md", "first"), ("second.txt", "second")] {
            std::fs::write(work.join(file), message).expect("seed file should be writable");
            run(&work, &["add", "."]);
            run(&work, &["commit", "-m", message]);
        }
        run(&work, &["remote", "add", "origin", &spelled(&bare)]);
        run(&work, &["push", "origin", "main"]);
        let served_from = format!("file://{}", spelled(dir.path()));
        (dir, Remotes::new(served_from, None))
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
