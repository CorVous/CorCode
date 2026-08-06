//! Deployment configuration, read from the environment.

use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context as _, Result, bail};

/// Address served when `CORCODE_BIND_ADDR` is unset.
pub const DEFAULT_BIND_ADDR: &str = "0.0.0.0:8080";
/// Memory ceiling of a workspace container when `CORCODE_CONTAINER_MEMORY_MB`
/// is unset.
pub const DEFAULT_CONTAINER_MEMORY_MB: u32 = 4096;
/// CPU ceiling of a workspace container when `CORCODE_CONTAINER_CPUS` is
/// unset.
pub const DEFAULT_CONTAINER_CPUS: u32 = 2;
/// How many chats keep a container when `CORCODE_WARM_POOL` is unset
/// (ADR-0002: a cap, and no TTL behind it).
pub const DEFAULT_WARM_POOL: usize = 2;

/// What a secret reads as anywhere it would otherwise be spelled out.
pub const REDACTED: &str = "<redacted>";

/// A secret, said out loud without being said.
const fn hidden<T>(_secret: &T) -> &'static str {
    REDACTED
}

/// A registry login for the lazy image pull (ADR-0009).
#[derive(Clone)]
pub struct RegistryCredentials {
    pub user: String,
    pub token: String,
}

impl fmt::Debug for RegistryCredentials {
    /// Redacts `token`: it is a registry password in all but name.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegistryCredentials")
            .field("user", &self.user)
            .field("token", &hidden(&self.token))
            .finish()
    }
}

/// Everything the core needs from its deployment environment.
#[derive(Clone)]
pub struct Config {
    /// Root of the NAS dataset holding chats and workspaces (ADR-0006).
    pub data_dir: PathBuf,
    /// Address the HTTP server binds.
    pub bind_addr: SocketAddr,
    /// The single account's name (ADR-0003).
    pub username: String,
    /// The single account's argon2 password hash (ADR-0003).
    pub password_hash: String,
    /// Active workspace image tag, pulled lazily at spawn (ADR-0004, ADR-0009).
    pub workspace_image: String,
    /// Memory ceiling of one workspace container (ADR-0001).
    pub container_memory_mb: u32,
    /// CPU ceiling of one workspace container (ADR-0001).
    pub container_cpus: u32,
    /// How many chats keep a live container at once (ADR-0002).
    pub warm_pool: usize,
    /// Login for the registry holding the workspace image (ADR-0009).
    pub registry: Option<RegistryCredentials>,
    /// The repositories a new chat may be cut from, first one default
    /// (ADR-0005).
    pub repos: Vec<String>,
    /// Clones the private repositories when set; a GitHub token in all but
    /// name.
    pub github_token: Option<String>,
    /// Handed to the agent as `ANTHROPIC_API_KEY` (ADR-0001).
    pub anthropic_api_key: Option<String>,
}

impl Config {
    /// Load the configuration from the process environment.
    pub fn from_env() -> Result<Self> {
        Self::from_vars(std::env::vars())
    }

    /// Load the configuration from an arbitrary set of variables.
    pub fn from_vars<I>(vars: I) -> Result<Self>
    where
        I: IntoIterator<Item = (String, String)>,
    {
        let vars: HashMap<String, String> = vars.into_iter().collect();
        let bind_addr = optional(&vars, "CORCODE_BIND_ADDR").unwrap_or(DEFAULT_BIND_ADDR);
        Ok(Self {
            data_dir: PathBuf::from(required(&vars, "CORCODE_DATA_DIR")?),
            bind_addr: bind_addr
                .parse()
                .with_context(|| format!("CORCODE_BIND_ADDR is not an address: {bind_addr}"))?,
            username: required(&vars, "CORCODE_USERNAME")?.to_owned(),
            password_hash: required(&vars, "CORCODE_PASSWORD_HASH")?.to_owned(),
            workspace_image: required(&vars, "CORCODE_WORKSPACE_IMAGE")?.to_owned(),
            container_memory_mb: number(
                &vars,
                "CORCODE_CONTAINER_MEMORY_MB",
                DEFAULT_CONTAINER_MEMORY_MB,
            )?,
            container_cpus: number(&vars, "CORCODE_CONTAINER_CPUS", DEFAULT_CONTAINER_CPUS)?,
            warm_pool: number(&vars, "CORCODE_WARM_POOL", DEFAULT_WARM_POOL)?,
            registry: registry(&vars)?,
            repos: repos(&vars)?,
            github_token: optional(&vars, "CORCODE_GITHUB_TOKEN").map(ToOwned::to_owned),
            anthropic_api_key: optional(&vars, "CORCODE_ANTHROPIC_API_KEY").map(ToOwned::to_owned),
        })
    }
}

/// Look up a variable, treating "set to nothing" as unset.
fn optional<'a>(vars: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    vars.get(key)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
}

/// Look up a variable that has no safe default.
fn required<'a>(vars: &'a HashMap<String, String>, key: &str) -> Result<&'a str> {
    match optional(vars, key) {
        Some(value) => Ok(value),
        None => bail!("{key} must be set"),
    }
}

/// Look up a variable that falls back to a sane number.
fn number<T>(vars: &HashMap<String, String>, key: &str, default: T) -> Result<T>
where
    T: FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    optional(vars, key).map_or(Ok(default), |value| {
        value
            .parse()
            .with_context(|| format!("{key} is not a whole number: {value}"))
    })
}

/// The repositories the new-chat form offers. Each is checked here rather
/// than where it becomes a URL: a boot is the right time to learn about a
/// typo.
fn repos(vars: &HashMap<String, String>) -> Result<Vec<String>> {
    required(vars, "CORCODE_REPOS")?
        .split(',')
        .map(str::trim)
        .map(|repo| {
            if names_a_repository(repo) {
                Ok(repo.to_owned())
            } else {
                bail!("CORCODE_REPOS holds {repo}, which is not an owner/name repository")
            }
        })
        .collect()
}

/// `owner/name`, both halves there and neither of them a path trick.
fn names_a_repository(repo: &str) -> bool {
    let mut halves = repo.split('/');
    let named = |half: Option<&str>| {
        half.is_some_and(|half| {
            !half.is_empty()
                && !half.bytes().all(|byte| byte == b'.')
                && half
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        })
    };
    named(halves.next()) && named(halves.next()) && halves.next().is_none()
}

/// The registry login is optional, but half of one is a misconfiguration.
fn registry(vars: &HashMap<String, String>) -> Result<Option<RegistryCredentials>> {
    match (
        optional(vars, "CORCODE_REGISTRY_USER"),
        optional(vars, "CORCODE_REGISTRY_TOKEN"),
    ) {
        (None, None) => Ok(None),
        (Some(user), Some(token)) => Ok(Some(RegistryCredentials {
            user: user.to_owned(),
            token: token.to_owned(),
        })),
        _ => bail!("CORCODE_REGISTRY_USER and CORCODE_REGISTRY_TOKEN must be set together"),
    }
}

impl fmt::Debug for Config {
    /// Redacts `password_hash`: this often ends up in logs and error messages.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("data_dir", &self.data_dir)
            .field("bind_addr", &self.bind_addr)
            .field("username", &self.username)
            .field("password_hash", &hidden(&self.password_hash))
            .field("workspace_image", &self.workspace_image)
            .field("container_memory_mb", &self.container_memory_mb)
            .field("container_cpus", &self.container_cpus)
            .field("warm_pool", &self.warm_pool)
            .field("registry", &self.registry)
            .field("repos", &self.repos)
            .field("github_token", &self.github_token.as_ref().map(hidden))
            .field(
                "anthropic_api_key",
                &self.anthropic_api_key.as_ref().map(hidden),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::path::Path;

    use super::*;

    fn required_vars() -> Vec<(String, String)> {
        [
            ("CORCODE_DATA_DIR", "/mnt/tank/corcode"),
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
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
    }

    fn with(vars: &[(&str, &str)]) -> Result<Config> {
        let mut all = required_vars();
        all.retain(|(key, _)| !vars.iter().any(|(named, _)| named == key));
        all.extend(
            vars.iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned())),
        );
        Config::from_vars(all)
    }

    #[test]
    fn the_offerable_repositories_load_in_the_order_they_were_given() {
        let config = with(&[("CORCODE_REPOS", "CorVous/CorCode, CorVous/zenni-tools")])
            .expect("a repository list should load");

        assert_eq!(config.repos, ["CorVous/CorCode", "CorVous/zenni-tools"]);
    }

    #[test]
    fn a_repository_that_is_not_owner_slash_name_fails_loudly() {
        for spelling in ["CorCode", "CorVous/CorCode/extra", "CorVous/", "/CorCode"] {
            let error = with(&[("CORCODE_REPOS", spelling)])
                .expect_err("an unusable repository should fail");

            assert!(
                format!("{error:#}").contains(spelling),
                "error should quote the offending repository, got: {error:#}"
            );
        }
    }

    #[test]
    fn an_empty_repository_list_fails_loudly() {
        let error = required_vars()
            .into_iter()
            .filter(|(key, _)| key != "CORCODE_REPOS")
            .collect::<Vec<_>>();
        let error = Config::from_vars(error).expect_err("no repositories should fail");

        assert!(
            format!("{error:#}").contains("CORCODE_REPOS"),
            "error should name the missing variable, got: {error:#}"
        );
    }

    #[test]
    fn the_tokens_are_optional() {
        let config =
            Config::from_vars(required_vars()).expect("a tokenless environment should load");

        assert_eq!(config.github_token, None);
        assert_eq!(config.anthropic_api_key, None);
    }

    #[test]
    fn debug_redacts_every_token_it_carries() {
        let config = with(&[
            ("CORCODE_GITHUB_TOKEN", "ghs-clone-secret"),
            ("CORCODE_ANTHROPIC_API_KEY", "sk-ant-secret"),
        ])
        .expect("a credentialled environment should load");

        let debug = format!("{config:?}");

        assert_eq!(config.github_token.as_deref(), Some("ghs-clone-secret"));
        assert_eq!(config.anthropic_api_key.as_deref(), Some("sk-ant-secret"));
        assert!(!debug.contains("ghs-clone-secret"), "token leaked: {debug}");
        assert!(!debug.contains("sk-ant-secret"), "key leaked: {debug}");
    }

    #[test]
    fn reads_every_field_from_vars() {
        let mut vars = required_vars();
        vars.push(("CORCODE_BIND_ADDR".to_owned(), "127.0.0.1:9000".to_owned()));

        let config = Config::from_vars(vars).expect("complete environment should load");

        assert_eq!(config.data_dir, Path::new("/mnt/tank/corcode"));
        assert_eq!(config.username, "cassidy");
        assert_eq!(
            config.password_hash,
            "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA"
        );
        assert_eq!(
            config.workspace_image,
            "ghcr.io/corvous/corcode-workspace:2026-08-05"
        );
        assert_eq!(
            config.bind_addr,
            "127.0.0.1:9000".parse::<SocketAddr>().expect("valid addr")
        );
    }

    #[test]
    fn bind_addr_defaults_when_unset() {
        let config = Config::from_vars(required_vars()).expect("defaulted environment should load");

        assert_eq!(
            config.bind_addr,
            DEFAULT_BIND_ADDR.parse::<SocketAddr>().expect("valid addr")
        );
    }

    #[test]
    fn the_warm_pool_defaults_to_the_cap_the_adr_set_and_can_be_tuned() {
        let defaulted =
            Config::from_vars(required_vars()).expect("defaulted environment should load");
        let tuned = with(&[("CORCODE_WARM_POOL", "3")]).expect("a tuned pool should load");

        assert_eq!(defaulted.warm_pool, DEFAULT_WARM_POOL);
        assert_eq!(tuned.warm_pool, 3);
    }

    #[test]
    fn container_limits_default_when_unset() {
        let config = Config::from_vars(required_vars()).expect("defaulted environment should load");

        assert_eq!(config.container_memory_mb, DEFAULT_CONTAINER_MEMORY_MB);
        assert_eq!(config.container_cpus, DEFAULT_CONTAINER_CPUS);
        assert!(config.registry.is_none());
    }

    #[test]
    fn container_limits_come_from_the_environment() {
        let mut vars = required_vars();
        vars.push(("CORCODE_CONTAINER_MEMORY_MB".to_owned(), "8192".to_owned()));
        vars.push(("CORCODE_CONTAINER_CPUS".to_owned(), "6".to_owned()));

        let config = Config::from_vars(vars).expect("tuned environment should load");

        assert_eq!(config.container_memory_mb, 8192);
        assert_eq!(config.container_cpus, 6);
    }

    #[test]
    fn an_unparseable_container_limit_names_the_variable() {
        let mut vars = required_vars();
        vars.push(("CORCODE_CONTAINER_CPUS".to_owned(), "half".to_owned()));

        let error = Config::from_vars(vars).expect_err("a non-numeric limit should fail");

        assert!(
            format!("{error:#}").contains("CORCODE_CONTAINER_CPUS"),
            "error should name the offending variable, got: {error:#}"
        );
    }

    #[test]
    fn registry_credentials_load_as_a_pair() {
        let mut vars = required_vars();
        vars.push(("CORCODE_REGISTRY_USER".to_owned(), "CorVous".to_owned()));
        vars.push(("CORCODE_REGISTRY_TOKEN".to_owned(), "ghp-secret".to_owned()));

        let config = Config::from_vars(vars).expect("credentialled environment should load");

        let registry = config.registry.expect("credentials should be read");
        assert_eq!(registry.user, "CorVous");
        assert_eq!(registry.token, "ghp-secret");
    }

    #[test]
    fn half_a_registry_credential_fails_loudly() {
        let mut vars = required_vars();
        vars.push(("CORCODE_REGISTRY_USER".to_owned(), "CorVous".to_owned()));

        let error = Config::from_vars(vars).expect_err("a lone registry user should fail");

        assert!(
            format!("{error:#}").contains("CORCODE_REGISTRY_TOKEN"),
            "error should name the missing variable, got: {error:#}"
        );
    }

    #[test]
    fn debug_redacts_the_registry_token() {
        let mut vars = required_vars();
        vars.push(("CORCODE_REGISTRY_USER".to_owned(), "CorVous".to_owned()));
        vars.push(("CORCODE_REGISTRY_TOKEN".to_owned(), "ghp-secret".to_owned()));
        let config = Config::from_vars(vars).expect("credentialled environment should load");

        let debug = format!("{config:?}");

        assert!(!debug.contains("ghp-secret"), "token leaked: {debug}");
        assert!(debug.contains("CorVous"));
    }

    #[test]
    fn missing_required_var_names_the_variable() {
        let vars = required_vars()
            .into_iter()
            .filter(|(key, _)| key != "CORCODE_DATA_DIR");

        let error = Config::from_vars(vars).expect_err("missing data dir should fail");

        assert!(
            format!("{error:#}").contains("CORCODE_DATA_DIR"),
            "error should name the missing variable, got: {error:#}"
        );
    }

    #[test]
    fn debug_redacts_password_hash() {
        let config = Config::from_vars(required_vars()).expect("complete environment should load");

        let debug = format!("{config:?}");

        assert!(!debug.contains(&config.password_hash));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn unparseable_bind_addr_names_the_variable() {
        let mut vars = required_vars();
        vars.push(("CORCODE_BIND_ADDR".to_owned(), "not-an-address".to_owned()));

        let error = Config::from_vars(vars).expect_err("bad bind address should fail");

        assert!(
            format!("{error:#}").contains("CORCODE_BIND_ADDR"),
            "error should name the offending variable, got: {error:#}"
        );
    }
}
