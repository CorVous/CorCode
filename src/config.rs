//! Deployment configuration, read from the environment.

use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};

use crate::plane::RegistryCredentials;

/// Address served when `CORCODE_BIND_ADDR` is unset.
pub const DEFAULT_BIND_ADDR: &str = "0.0.0.0:8080";
/// Memory ceiling of a workspace container when `CORCODE_CONTAINER_MEMORY_MB`
/// is unset.
pub const DEFAULT_CONTAINER_MEMORY_MB: u32 = 4096;
/// CPU ceiling of a workspace container when `CORCODE_CONTAINER_CPUS` is
/// unset.
pub const DEFAULT_CONTAINER_CPUS: u32 = 2;

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
    /// Login for the registry holding the workspace image (ADR-0009).
    pub registry: Option<RegistryCredentials>,
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
        let bind_addr = vars
            .get("CORCODE_BIND_ADDR")
            .map_or(DEFAULT_BIND_ADDR, String::as_str);
        Ok(Self {
            data_dir: PathBuf::from(required(&vars, "CORCODE_DATA_DIR")?),
            bind_addr: bind_addr
                .parse()
                .with_context(|| format!("CORCODE_BIND_ADDR is not an address: {bind_addr}"))?,
            username: required(&vars, "CORCODE_USERNAME")?.to_owned(),
            password_hash: required(&vars, "CORCODE_PASSWORD_HASH")?.to_owned(),
            workspace_image: required(&vars, "CORCODE_WORKSPACE_IMAGE")?.to_owned(),
            container_memory_mb: DEFAULT_CONTAINER_MEMORY_MB,
            container_cpus: DEFAULT_CONTAINER_CPUS,
            registry: None,
        })
    }
}

/// Look up a variable that has no safe default.
fn required<'a>(vars: &'a HashMap<String, String>, key: &str) -> Result<&'a str> {
    match vars.get(key) {
        Some(value) if !value.is_empty() => Ok(value),
        _ => bail!("{key} must be set"),
    }
}

impl fmt::Debug for Config {
    /// Redacts `password_hash`: this often ends up in logs and error messages.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("data_dir", &self.data_dir)
            .field("bind_addr", &self.bind_addr)
            .field("username", &self.username)
            .field("password_hash", &"<redacted>")
            .field("workspace_image", &self.workspace_image)
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
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
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
