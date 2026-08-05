//! Deployment configuration, read from the environment.

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
