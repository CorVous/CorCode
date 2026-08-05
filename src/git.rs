//! Git as the git command line sees it: clone a base branch, cut the chat's
//! branch off it (ADR-0005).

use std::fmt;
use std::io;
use std::path::Path;
use std::process::{Command, Output};

use chrono::Utc;
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
    let doing = format!("clone {origin} at {base_branch}");
    let scrub = |said: &str| origin.scrub(said);
    let spelled_dest = dest.to_string_lossy();
    let output = git(
        &doing,
        &[
            "clone",
            "--branch",
            base_branch,
            "--",
            origin.url(),
            &spelled_dest,
        ],
    )?;
    judge(&doing, &output, scrub)?;
    let doing = format!("take the credential back out of {}", dest.display());
    let output = git(
        &doing,
        &[
            "-C",
            &spelled_dest,
            "remote",
            "set-url",
            "origin",
            &origin.tokenless(),
        ],
    )?;
    judge(&doing, &output, scrub)
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
