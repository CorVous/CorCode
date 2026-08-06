//! The operational secrets as the console sets them: what one action on one
//! secret came to, and nothing of what the secret is.

use std::sync::Arc;

use crate::secrets::{Secret, Secrets, SecretsError, Source};
use crate::verify::{Verified, VerifyClient};

/// The two operational secrets as the settings panel acts on them: what is on
/// disk, and where a credential is put to find out whether it works.
#[derive(Debug)]
pub struct Settings<V> {
    secrets: Arc<Secrets>,
    client: V,
}

impl<V: VerifyClient + Sync> Settings<V> {
    /// The secrets `secrets` keeps, checked against their services through
    /// `client`.
    pub const fn new(secrets: Arc<Secrets>, client: V) -> Self {
        Self { secrets, client }
    }

    /// Every secret this deployment holds and where each one's value comes
    /// from, in the order the console lists them.
    pub fn statuses(&self) -> Result<Vec<(Secret, Source)>, SecretsError> {
        Secret::ALL
            .into_iter()
            .map(|secret| Ok((secret, self.source(secret)?)))
            .collect()
    }

    /// Where the value in force for `secret` comes from.
    pub fn source(&self, secret: Secret) -> Result<Source, SecretsError> {
        self.secrets.source(secret)
    }

    /// Put `value` in force for `secret`. A blank box is a no-op: S1 made
    /// writing nothing a failure so that a secret can only be unset through
    /// [`Settings::clear`], and this is the panel answering before it gets
    /// there.
    pub fn save(&self, secret: Secret, value: &str) -> Result<Outcome, SecretsError> {
        if value.trim().is_empty() {
            return Ok(Outcome::NothingGiven);
        }
        self.secrets.write(secret, value)?;
        Ok(Outcome::Saved)
    }

    /// Take the set value away, leaving whatever the environment bootstrapped
    /// in force again.
    pub fn clear(&self, secret: Secret) -> Result<Outcome, SecretsError> {
        self.secrets.clear(secret)?;
        Ok(Outcome::Cleared)
    }

    /// Put the value in force for `secret` right now to the service it opens.
    /// The value is read afresh, so a rotation between two checks is what the
    /// second one checks.
    pub async fn verify(&self, secret: Secret) -> Result<Outcome, SecretsError> {
        let Some(value) = self.secrets.read(secret)? else {
            return Ok(Outcome::NothingToVerify);
        };
        Ok(Outcome::Verified(self.client.verify(secret, &value).await))
    }
}

/// What was just done to one secret, as its panel reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing: the panel as it is drawn with the page around it.
    Untouched,
    /// A value was given and is in force from the next read on.
    Saved,
    /// A blank submission. Unsetting a secret is [`Outcome::Cleared`]'s to do,
    /// so a save with nothing in it changes nothing.
    NothingGiven,
    /// The value on disk was taken away, leaving the environment's in force.
    Cleared,
    /// Nothing is set, so there was nothing to put to the service.
    NothingToVerify,
    /// The service was asked, and this is what it made of the credential.
    Verified(Verified),
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use tempfile::{TempDir, tempdir};

    use crate::secrets::{Secret, Secrets, Source};
    use crate::verify::ScriptedVerifier;

    use super::*;

    const TOKEN: &str = "ghs-set-from-the-console";
    const ROTATED: &str = "ghs-set-again";
    const FROM_ENV: &str = "ghs-bootstrapped";

    /// S1 made writing nothing a failure so that a secret can only be unset
    /// through Clear. A blank box is not an attempt to unset one.
    #[tokio::test]
    async fn a_blank_save_changes_nothing_and_says_so() {
        let (dir, settings, _) = bootstrapped();

        assert_eq!(save(&settings, "   \n"), Outcome::NothingGiven);

        assert!(
            !kept(&dir).join("github_token").exists(),
            "a blank save reached the disk"
        );
        assert_eq!(source(&settings), Source::Environment);
    }

    #[tokio::test]
    async fn a_saved_secret_is_the_one_the_panel_reports() {
        let (_dir, settings, _) = bootstrapped();

        assert_eq!(save(&settings, TOKEN), Outcome::Saved);

        assert_eq!(source(&settings), Source::Settings);
    }

    #[tokio::test]
    async fn a_cleared_secret_falls_back_to_the_one_the_environment_carried() {
        let (_dir, settings, _) = bootstrapped();
        save(&settings, TOKEN);

        assert_eq!(clear(&settings), Outcome::Cleared);

        assert_eq!(source(&settings), Source::Environment);
    }

    #[tokio::test]
    async fn clearing_the_only_value_there_ever_was_leaves_the_secret_unset() {
        let (_dir, settings, _) = bare();
        save(&settings, TOKEN);

        clear(&settings);

        assert_eq!(source(&settings), Source::Unset);
    }

    #[tokio::test]
    async fn a_check_puts_the_value_in_force_and_not_the_one_the_environment_carried() {
        let (_dir, settings, client) = bootstrapped();
        save(&settings, TOKEN);

        verify(&settings).await;

        assert_eq!(
            client.heard(),
            vec![(Secret::GithubToken, TOKEN.to_owned())]
        );
    }

    #[tokio::test]
    async fn a_secret_rotated_between_two_checks_is_checked_as_it_now_stands() {
        let (_dir, settings, client) = bootstrapped();

        save(&settings, TOKEN);
        verify(&settings).await;
        save(&settings, ROTATED);
        verify(&settings).await;

        assert_eq!(
            client.heard(),
            vec![
                (Secret::GithubToken, TOKEN.to_owned()),
                (Secret::GithubToken, ROTATED.to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn a_secret_nothing_holds_is_never_put_to_its_service() {
        let (_dir, settings, client) = bare();

        assert_eq!(verify(&settings).await, Outcome::NothingToVerify);

        assert!(
            client.heard().is_empty(),
            "an unset secret was spent on a call"
        );
    }

    #[tokio::test]
    async fn what_the_service_made_of_a_credential_is_what_the_panel_is_told() {
        for verified in [
            Verified::Accepted {
                login: Some("cassidy".to_owned()),
                without_repo_scope: true,
            },
            Verified::Refused(401),
            Verified::Silent,
        ] {
            let dir = tempdir().expect("temp dir should be creatable");
            let settings = Settings::new(
                Arc::new(Secrets::new(dir.path(), [])),
                ScriptedVerifier::answering(verified.clone()),
            );
            save(&settings, TOKEN);

            assert_eq!(verify(&settings).await, Outcome::Verified(verified));
        }
    }

    #[tokio::test]
    async fn every_secret_is_listed_with_where_its_value_comes_from() {
        let (_dir, settings, _) = bootstrapped();

        assert_eq!(
            settings.statuses().expect("the secrets should be readable"),
            vec![
                (Secret::GithubToken, Source::Environment),
                (Secret::AnthropicKey, Source::Unset),
            ]
        );
    }

    fn save(settings: &Settings<ScriptedVerifier>, value: &str) -> Outcome {
        settings
            .save(Secret::GithubToken, value)
            .expect("a secret should be settable")
    }

    fn clear(settings: &Settings<ScriptedVerifier>) -> Outcome {
        settings
            .clear(Secret::GithubToken)
            .expect("a secret should be clearable")
    }

    async fn verify(settings: &Settings<ScriptedVerifier>) -> Outcome {
        settings
            .verify(Secret::GithubToken)
            .await
            .expect("a secret should be readable")
    }

    fn source(settings: &Settings<ScriptedVerifier>) -> Source {
        settings
            .source(Secret::GithubToken)
            .expect("a secret should be readable")
    }

    fn kept(dir: &TempDir) -> std::path::PathBuf {
        dir.path().join("secrets")
    }

    /// Settings over an empty dataset, with nothing bootstrapped.
    fn bare() -> (TempDir, Settings<ScriptedVerifier>, ScriptedVerifier) {
        over(&[])
    }

    /// Settings over an empty dataset, with a token the environment carried in.
    fn bootstrapped() -> (TempDir, Settings<ScriptedVerifier>, ScriptedVerifier) {
        over(&[(Secret::GithubToken, FROM_ENV)])
    }

    fn over(
        from_env: &[(Secret, &str)],
    ) -> (TempDir, Settings<ScriptedVerifier>, ScriptedVerifier) {
        let dir = tempdir().expect("temp dir should be creatable");
        let secrets = secrets(dir.path(), from_env);
        let client = ScriptedVerifier::default();
        let settings = Settings::new(secrets, client.clone());
        (dir, settings, client)
    }

    fn secrets(data_dir: &Path, from_env: &[(Secret, &str)]) -> Arc<Secrets> {
        Arc::new(Secrets::new(
            data_dir,
            from_env
                .iter()
                .map(|&(secret, value)| (secret, value.to_owned())),
        ))
    }
}
