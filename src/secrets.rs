//! The two operational secrets on disk: the token that clones and pushes
//! (ADR-0005), and the key the agent is spawned with (ADR-0001).
//!
//! Every read goes to the file, so a secret written now is in force for the
//! next clone, push or spawn without a restart. Nothing is cached: secrets are
//! read at the rate chats take turns, which a file read is never the cost of.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use thiserror::Error;
use ulid::Ulid;

use crate::config::Config;

/// Directory under the data root the secrets are kept in.
const SECRETS_DIR: &str = "secrets";

/// Extension a secret lands under before it is moved into place, so a crash
/// mid-write cannot leave half a credential behind.
const PENDING: &str = "tmp";

/// One operational secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Secret {
    /// Opens the private repositories a chat is cut from (ADR-0005).
    GithubToken,
    /// Handed to the agent as `ANTHROPIC_API_KEY` (ADR-0001).
    AnthropicKey,
}

impl Secret {
    /// The file this secret is kept in, under the secrets directory.
    const fn file(self) -> &'static str {
        match self {
            Self::GithubToken => "github_token",
            Self::AnthropicKey => "anthropic_key",
        }
    }
}

/// Something the secrets on disk would not do. No variant can carry a secret:
/// a failure names the file it happened to and nothing else.
#[derive(Debug, Error)]
pub enum SecretsError {
    #[error("{} could not be read", path.display())]
    Read { path: PathBuf, source: io::Error },
    #[error("{} could not be written", path.display())]
    Write { path: PathBuf, source: io::Error },
    #[error("{} could not be taken away", path.display())]
    Clear { path: PathBuf, source: io::Error },
    #[error("{} cannot be set to nothing: clear it instead", path.display())]
    Empty { path: PathBuf },
}

/// The operational secrets of one deployment: what is on disk, over what the
/// environment bootstrapped.
pub struct Secrets {
    dir: PathBuf,
    from_env: BTreeMap<Secret, String>,
}

impl Secrets {
    /// Secrets kept under `data_dir`, falling back to `from_env` for the ones
    /// nothing has been written for.
    #[must_use]
    pub fn new(data_dir: &Path, from_env: impl IntoIterator<Item = (Secret, String)>) -> Self {
        Self {
            dir: data_dir.join(SECRETS_DIR),
            from_env: from_env.into_iter().collect(),
        }
    }

    /// The secrets of the deployment `config` was read for: the environment is
    /// where a first boot gets them, and this is the only thing that reads
    /// them from it.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        let from_env = [
            (Secret::GithubToken, config.github_token.clone()),
            (Secret::AnthropicKey, config.anthropic_api_key.clone()),
        ];
        Self::new(
            &config.data_dir,
            from_env
                .into_iter()
                .filter_map(|(secret, value)| value.map(|value| (secret, value))),
        )
    }

    /// What `secret` is worth right now: what was last written for it, else
    /// what the environment bootstrapped, else nothing.
    pub fn read(&self, secret: Secret) -> Result<Option<String>, SecretsError> {
        Ok(self
            .written(secret)?
            .or_else(|| self.from_env.get(&secret).cloned()))
    }

    /// Put `value` in force for every read after this one, without the
    /// whitespace around it. A value that is nothing but whitespace is
    /// refused: a secret is unset by [`Secrets::clear`], not by emptying it.
    pub fn write(&self, secret: Secret, value: &str) -> Result<(), SecretsError> {
        let path = self.path(secret);
        let value = value.trim();
        if value.is_empty() {
            return Err(SecretsError::Empty { path });
        }
        self.make_dir()?;
        let pending = self.pending(secret);
        write_owner_only(value, &pending)?;
        fs::rename(&pending, &path).map_err(|source| SecretsError::Write { path, source })
    }

    /// Take the written value away, leaving whatever the environment
    /// bootstrapped in force again. A secret nothing was written for is
    /// already clear.
    pub fn clear(&self, secret: Secret) -> Result<(), SecretsError> {
        let path = self.path(secret);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(SecretsError::Clear { path, source }),
        }
    }

    /// What the secret's own file holds, if it holds anything. The whitespace
    /// around it is not part of it: a file written by hand on the dataset
    /// ends in the newline whatever wrote it put there.
    fn written(&self, secret: Secret) -> Result<Option<String>, SecretsError> {
        let path = self.path(secret);
        match fs::read_to_string(&path) {
            Ok(held) => Ok(Some(held.trim().to_owned()).filter(|held| !held.is_empty())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(SecretsError::Read { path, source }),
        }
    }

    fn path(&self, secret: Secret) -> PathBuf {
        self.dir.join(secret.file())
    }

    /// Where one write stages its secret: a name of its own, so two writes at
    /// once cannot publish half of each other.
    fn pending(&self, secret: Secret) -> PathBuf {
        self.dir
            .join(format!("{}.{}.{PENDING}", secret.file(), Ulid::generate()))
    }

    /// The secrets directory, made if it is not there yet, open to nobody but
    /// the account the core runs as.
    fn make_dir(&self) -> Result<(), SecretsError> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder
            .create(&self.dir)
            .map_err(|source| SecretsError::Write {
                path: self.dir.clone(),
                source,
            })
    }
}

/// Write a secret where only its owner can read it back, and make sure it has
/// reached the disk.
fn write_owner_only(value: &str, path: &Path) -> Result<(), SecretsError> {
    let mut options = File::options();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
        .open(path)
        .and_then(|mut file| {
            file.write_all(value.as_bytes())
                .and_then(|()| file.sync_all())
        })
        .map_err(|source| SecretsError::Write {
            path: path.to_owned(),
            source,
        })
}

impl fmt::Debug for Secrets {
    /// Says where the secrets are and which of them the environment carried,
    /// never what any of them is.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Secrets")
            .field("dir", &self.dir)
            .field("from_env", &self.from_env.keys())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use tempfile::{TempDir, tempdir};

    use super::*;

    const TOKEN: &str = "ghs-clone-secret";
    const ROTATED: &str = "ghs-rotated-secret";
    const FROM_ENV: &str = "ghs-bootstrapped";

    #[test]
    fn a_secret_says_whether_it_is_set_and_what_set_it() {
        let (_dir, secrets) = bootstrapped();

        assert_eq!(source(&secrets, Secret::AnthropicKey), Source::Unset);
        assert_eq!(source(&secrets, Secret::GithubToken), Source::Environment);

        secrets
            .write(Secret::GithubToken, TOKEN)
            .expect("a secret should be writable");
        assert_eq!(source(&secrets, Secret::GithubToken), Source::Settings);

        secrets
            .clear(Secret::GithubToken)
            .expect("a secret should be clearable");
        assert_eq!(source(&secrets, Secret::GithubToken), Source::Environment);
    }

    #[test]
    fn every_secret_is_spelled_the_same_way_wherever_one_is_named() {
        for secret in Secret::ALL {
            assert_eq!(Secret::named(secret.name()), Some(secret));
        }

        assert_eq!(Secret::named("password"), None);
    }

    #[test]
    fn a_secret_nothing_holds_is_nothing() {
        let (_dir, secrets) = bare();

        assert_eq!(read(&secrets, Secret::GithubToken), None);
        assert_eq!(read(&secrets, Secret::AnthropicKey), None);
    }

    #[test]
    fn an_unwritten_secret_is_the_one_the_environment_bootstrapped() {
        let (_dir, secrets) = bootstrapped();

        assert_eq!(
            read(&secrets, Secret::GithubToken).as_deref(),
            Some(FROM_ENV)
        );
    }

    #[test]
    fn a_written_secret_is_the_one_in_force() {
        let (_dir, secrets) = bootstrapped();

        secrets
            .write(Secret::GithubToken, TOKEN)
            .expect("a secret should be writable");

        assert_eq!(read(&secrets, Secret::GithubToken).as_deref(), Some(TOKEN));
    }

    #[test]
    fn a_rotated_secret_is_in_force_for_the_next_read() {
        let (_dir, secrets) = bootstrapped();
        secrets
            .write(Secret::GithubToken, TOKEN)
            .expect("a secret should be writable");
        assert_eq!(read(&secrets, Secret::GithubToken).as_deref(), Some(TOKEN));

        secrets
            .write(Secret::GithubToken, ROTATED)
            .expect("a secret should be rotatable");

        assert_eq!(
            read(&secrets, Secret::GithubToken).as_deref(),
            Some(ROTATED)
        );
    }

    #[test]
    fn a_cleared_secret_leaves_the_environment_in_force_again() {
        let (_dir, secrets) = bootstrapped();
        secrets
            .write(Secret::GithubToken, TOKEN)
            .expect("a secret should be writable");

        secrets
            .clear(Secret::GithubToken)
            .expect("a secret should be clearable");

        assert_eq!(
            read(&secrets, Secret::GithubToken).as_deref(),
            Some(FROM_ENV)
        );
    }

    #[test]
    fn clearing_a_secret_nothing_was_written_for_is_no_failure() {
        let (_dir, secrets) = bootstrapped();

        secrets
            .clear(Secret::AnthropicKey)
            .expect("clearing nothing should be no failure");

        assert_eq!(read(&secrets, Secret::AnthropicKey), None);
    }

    #[test]
    fn each_secret_is_kept_in_a_file_of_its_own() {
        let (dir, secrets) = bare();

        secrets
            .write(Secret::GithubToken, TOKEN)
            .expect("a secret should be writable");
        secrets
            .write(Secret::AnthropicKey, ROTATED)
            .expect("a secret should be writable");

        let kept = dir.path().join("secrets");
        assert_eq!(
            fs::read_to_string(kept.join("github_token")).expect("the token should be on disk"),
            TOKEN
        );
        assert_eq!(
            fs::read_to_string(kept.join("anthropic_key")).expect("the key should be on disk"),
            ROTATED
        );
    }

    #[test]
    fn a_write_leaves_no_half_written_secret_behind() {
        let (dir, secrets) = bare();

        secrets
            .write(Secret::GithubToken, TOKEN)
            .expect("a secret should be writable");

        let leftovers: Vec<PathBuf> = fs::read_dir(dir.path().join("secrets"))
            .expect("the secrets dir should be readable")
            .map(|entry| entry.expect("entry should be readable").path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == PENDING)
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    /// These files are the operator's to write by hand on the dataset, and a
    /// shell puts a newline on the end of everything it echoes.
    #[test]
    fn a_hand_written_secret_file_is_read_without_the_whitespace_around_it() {
        let (dir, secrets) = bootstrapped();
        let kept = dir.path().join("secrets");
        fs::create_dir_all(&kept).expect("the secrets dir should be creatable");

        fs::write(kept.join("github_token"), format!("{TOKEN}\n"))
            .expect("a secret file should be writable");
        assert_eq!(read(&secrets, Secret::GithubToken).as_deref(), Some(TOKEN));

        fs::write(kept.join("github_token"), "  \n").expect("a secret file should be writable");
        assert_eq!(
            read(&secrets, Secret::GithubToken).as_deref(),
            Some(FROM_ENV),
            "a file with nothing in it is not a value"
        );
    }

    /// A secret stored as nothing reads as the environment's, which is a
    /// rotation quietly undone. Unsetting one is [`Secrets::clear`]'s to do.
    #[test]
    fn a_secret_cannot_be_written_as_nothing() {
        let (dir, secrets) = bootstrapped();

        let refused = secrets
            .write(Secret::GithubToken, "  \n")
            .expect_err("writing nothing should be refused");

        assert!(matches!(refused, SecretsError::Empty { .. }));
        assert!(!dir.path().join("secrets").join("github_token").exists());
        assert_eq!(
            read(&secrets, Secret::GithubToken).as_deref(),
            Some(FROM_ENV)
        );
    }

    /// What a read takes off a hand-written file, a write never puts on.
    #[test]
    fn a_secret_is_stored_without_the_whitespace_around_it() {
        let (dir, secrets) = bare();

        secrets
            .write(Secret::GithubToken, &format!("  {TOKEN}\n"))
            .expect("a secret should be writable");

        assert_eq!(
            fs::read_to_string(dir.path().join("secrets").join("github_token"))
                .expect("the token should be on disk"),
            TOKEN
        );
    }

    /// Two writes in flight at once must not stage over each other: half of
    /// one credential and half of the other is a credential nobody holds.
    #[test]
    fn no_two_writes_stage_a_secret_under_the_same_name() {
        let (_dir, secrets) = bare();

        assert_ne!(
            secrets.pending(Secret::GithubToken),
            secrets.pending(Secret::GithubToken)
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_secrets_are_kept_where_only_their_owner_can_look() {
        use std::os::unix::fs::PermissionsExt as _;

        let (dir, secrets) = bare();

        secrets
            .write(Secret::GithubToken, TOKEN)
            .expect("a secret should be writable");

        let mode = fs::metadata(dir.path().join("secrets"))
            .expect("the secrets dir should be on disk")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn a_secret_that_cannot_be_taken_away_says_so() {
        let (dir, secrets) = bare();
        fs::create_dir_all(dir.path().join("secrets").join("github_token"))
            .expect("a directory should be creatable");

        let failure = secrets
            .clear(Secret::GithubToken)
            .expect_err("clearing a directory should fail");

        assert!(matches!(failure, SecretsError::Clear { .. }));
        let said = format!("{failure}");
        assert!(
            said.contains("github_token"),
            "failure should name the file, got: {said}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_written_secret_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt as _;

        let (dir, secrets) = bare();

        secrets
            .write(Secret::GithubToken, TOKEN)
            .expect("a secret should be writable");

        let mode = fs::metadata(dir.path().join("secrets").join("github_token"))
            .expect("the token should be on disk")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn a_failure_names_the_file_and_never_the_secret() {
        let (dir, secrets) = bare();
        let kept = dir.path().join("secrets");
        fs::create_dir_all(kept.join("github_token")).expect("a directory should be creatable");

        let unwritable = secrets
            .write(Secret::GithubToken, TOKEN)
            .expect_err("writing over a directory should fail");
        let unreadable = secrets
            .read(Secret::GithubToken)
            .expect_err("reading a directory should fail");

        for failure in [&unwritable, &unreadable] {
            let said = format!("{failure}");
            assert!(
                said.contains("github_token"),
                "failure should name the file, got: {said}"
            );
            assert!(!said.contains(TOKEN), "the secret leaked: {said}");
        }
    }

    #[test]
    fn debug_says_where_the_secrets_are_and_never_what_they_are() {
        let (dir, secrets) = bootstrapped();
        secrets
            .write(Secret::GithubToken, TOKEN)
            .expect("a secret should be writable");

        let debug = format!("{secrets:?}");

        assert!(debug.contains(&dir.path().display().to_string()));
        assert!(!debug.contains(TOKEN), "the written secret leaked: {debug}");
        assert!(
            !debug.contains(FROM_ENV),
            "the bootstrapped secret leaked: {debug}"
        );
    }

    #[test]
    fn the_environment_is_bootstrapped_from_the_config_and_nowhere_else() {
        let dir = tempdir().expect("temp dir should be creatable");
        let config = config(dir.path());

        let secrets = Secrets::from_config(&config);

        assert_eq!(read(&secrets, Secret::GithubToken).as_deref(), Some(TOKEN));
        assert_eq!(
            read(&secrets, Secret::AnthropicKey).as_deref(),
            Some("sk-ant-bootstrapped")
        );
    }

    fn read(secrets: &Secrets, secret: Secret) -> Option<String> {
        secrets.read(secret).expect("a secret should be readable")
    }

    fn source(secrets: &Secrets, secret: Secret) -> Source {
        secrets.source(secret).expect("a secret should be readable")
    }

    /// Secrets over an empty dataset, with nothing bootstrapped.
    fn bare() -> (TempDir, Secrets) {
        let dir = tempdir().expect("temp dir should be creatable");
        let secrets = Secrets::new(dir.path(), []);
        (dir, secrets)
    }

    /// Secrets over an empty dataset, with a token the environment carried in.
    fn bootstrapped() -> (TempDir, Secrets) {
        let dir = tempdir().expect("temp dir should be creatable");
        let secrets = Secrets::new(dir.path(), [(Secret::GithubToken, FROM_ENV.to_owned())]);
        (dir, secrets)
    }

    /// A deployment whose environment carried both secrets.
    fn config(data_dir: &Path) -> Config {
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
            ("CORCODE_GITHUB_TOKEN", TOKEN),
            ("CORCODE_ANTHROPIC_API_KEY", "sk-ant-bootstrapped"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()));
        Config::from_vars(vars).expect("a credentialled environment should load")
    }
}
