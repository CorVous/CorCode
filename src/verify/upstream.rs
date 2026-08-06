//! Putting a credential to the service it opens: one cheap authenticated call,
//! of which nothing is kept but whether it was taken and who by.
//!
//! TLS is rustls over the roots compiled into the binary, so a check does not
//! depend on the runtime image carrying a certificate bundle.

use std::time::Duration;

use log::warn;
use serde::Deserialize;
use ureq::http::header::{ACCEPT, AUTHORIZATION, HeaderValue, InvalidHeaderValue, USER_AGENT};
use ureq::http::{Request, Response};
use ureq::{Agent, Body};

use crate::secrets::Secret;

use super::{Verified, VerifyClient};

/// How long a service gets to answer before the panel reports silence.
const PATIENCE: Duration = Duration::from_secs(8);

/// Far more than an account name needs; the rest of an answer is never read.
const ENOUGH: u64 = 64 * 1024;

/// GitHub turns away a request that does not say who is asking.
const ASKING: &str = concat!("CorCode/", env!("CARGO_PKG_VERSION"));

/// Who the token belongs to: the cheapest authenticated call GitHub has.
const GITHUB_ACCOUNT: &str = "https://api.github.com/user";

/// Which scopes a classic token was issued with. A fine-grained one does not
/// come back with this header at all.
const GITHUB_SCOPES: &str = "x-oauth-scopes";

/// The scope a token needs to clone the private repositories a chat is cut
/// from (ADR-0005).
const REPO_SCOPE: &str = "repo";

/// The cheapest authenticated call the Anthropic API has.
const ANTHROPIC_MODELS: &str = "https://api.anthropic.com/v1/models";

/// Checks credentials against the real services.
#[derive(Clone)]
pub struct UpstreamVerifier {
    agent: Agent,
}

impl UpstreamVerifier {
    #[must_use]
    pub fn new() -> Self {
        Self {
            agent: Agent::new_with_config(
                Agent::config_builder()
                    .timeout_global(Some(PATIENCE))
                    .http_status_as_error(false)
                    .build(),
            ),
        }
    }

    fn ask(&self, secret: Secret, value: &str) -> Verified {
        let named = secret.name();
        let Ok(request) = request(secret, value) else {
            warn!("the {named} cannot be spelled as a header, so it was not sent");
            return Verified::Silent;
        };
        match self.agent.run(request) {
            Ok(mut answer) => read(secret, &mut answer),
            Err(failure) => {
                warn!("checking the {named} got no answer: {failure}");
                Verified::Silent
            }
        }
    }
}

impl Default for UpstreamVerifier {
    fn default() -> Self {
        Self::new()
    }
}

impl VerifyClient for UpstreamVerifier {
    /// The call itself blocks on its socket, so it is made off the runtime's
    /// worker threads.
    async fn verify(&self, secret: Secret, value: &str) -> Verified {
        let checking = self.clone();
        let value = value.to_owned();
        tokio::task::spawn_blocking(move || checking.ask(secret, &value))
            .await
            .unwrap_or_else(|panicked| {
                warn!("checking the {} came apart: {panicked}", secret.name());
                Verified::Silent
            })
    }
}

impl std::fmt::Debug for UpstreamVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamVerifier").finish_non_exhaustive()
    }
}

/// The one request that checks `secret`, with `value` in the header the
/// service reads it from.
fn request(secret: Secret, value: &str) -> Result<Request<()>, ureq::http::Error> {
    match secret {
        Secret::GithubToken => Request::get(GITHUB_ACCOUNT)
            .header(USER_AGENT, ASKING)
            .header(ACCEPT, "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28")
            .header(AUTHORIZATION, credential(&format!("Bearer {value}"))?)
            .body(()),
        Secret::AnthropicKey => Request::get(ANTHROPIC_MODELS)
            .header(USER_AGENT, ASKING)
            .header("anthropic-version", "2023-06-01")
            .header("x-api-key", credential(value)?)
            .body(()),
    }
}

/// A header value marked sensitive, so that whatever prints the request prints
/// everything but this.
fn credential(value: &str) -> Result<HeaderValue, InvalidHeaderValue> {
    let mut value = HeaderValue::from_str(value)?;
    value.set_sensitive(true);
    Ok(value)
}

/// What the service made of the credential. Nothing it said is repeated: a
/// status is the app's own word for it.
fn read(secret: Secret, answer: &mut Response<Body>) -> Verified {
    let status = answer.status();
    if !status.is_success() {
        return Verified::Refused(status.as_u16());
    }
    match secret {
        Secret::GithubToken => {
            let scopes = answer
                .headers()
                .get(GITHUB_SCOPES)
                .and_then(|scopes| scopes.to_str().ok())
                .map(str::to_owned);
            Verified::Accepted {
                without_repo_scope: short_of_repo_scope(scopes.as_deref()),
                login: login(answer),
            }
        }
        Secret::AnthropicKey => Verified::Accepted {
            login: None,
            without_repo_scope: false,
        },
    }
}

/// The account GitHub authenticated as, if it named one.
fn login(answer: &mut Response<Body>) -> Option<String> {
    #[derive(Deserialize)]
    struct Account {
        login: String,
    }

    let said = answer
        .body_mut()
        .with_config()
        .limit(ENOUGH)
        .read_to_string()
        .ok()?;
    serde_json::from_str::<Account>(&said)
        .inspect_err(|_| warn!("GitHub took the token and named no account"))
        .ok()
        .map(|account| account.login)
}

/// Whether a token's scopes stop short of `repo`. A token that lists no scopes
/// at all is fine-grained and makes no such claim either way.
fn short_of_repo_scope(scopes: Option<&str>) -> bool {
    scopes.is_some_and(|scopes| !scopes.split(',').any(|scope| scope.trim() == REPO_SCOPE))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::secrets::Secret;

    /// Distinctive enough that a leak into any rendering is unmistakable.
    const CREDENTIAL: &str = "sk-a-credential-that-must-not-be-printed";

    fn asked(secret: Secret) -> Request<()> {
        request(secret, CREDENTIAL).expect("an ordinary credential should be spellable")
    }

    fn header(request: &Request<()>, name: &str) -> String {
        let value = request
            .headers()
            .get(name)
            .unwrap_or_else(|| panic!("the request carries no {name} header"));
        String::from_utf8(value.as_bytes().to_vec()).expect("a header this app wrote")
    }

    /// Left to itself ureq waits forever, and a check that waits forever holds
    /// a blocking thread with nothing to show for it.
    #[test]
    fn a_service_that_never_answers_is_given_up_on() {
        let agent = UpstreamVerifier::new().agent;

        assert_eq!(agent.config().timeouts().global, Some(PATIENCE));
    }

    /// Being turned away is what a check is for, so it must come back as a
    /// status and not as a failure to reach anyone.
    #[test]
    fn a_refusal_comes_back_as_an_answer() {
        let agent = UpstreamVerifier::new().agent;

        assert!(!agent.config().http_status_as_error());
    }

    #[test]
    fn the_github_check_asks_the_api_who_the_token_belongs_to() {
        let request = asked(Secret::GithubToken);

        assert_eq!(request.method(), "GET");
        assert_eq!(request.uri().to_string(), "https://api.github.com/user");
        assert_eq!(
            header(&request, "authorization"),
            format!("Bearer {CREDENTIAL}")
        );
        assert_eq!(header(&request, "accept"), "application/vnd.github+json");
    }

    #[test]
    fn the_anthropic_check_carries_the_key_in_the_header_the_api_asks_for() {
        let request = asked(Secret::AnthropicKey);

        assert_eq!(request.method(), "GET");
        assert_eq!(
            request.uri().to_string(),
            "https://api.anthropic.com/v1/models"
        );
        assert_eq!(header(&request, "x-api-key"), CREDENTIAL);
        assert_eq!(header(&request, "anthropic-version"), "2023-06-01");
        assert!(
            request.headers().get("authorization").is_none(),
            "the key is spelled in a second place as well"
        );
    }

    /// GitHub turns away a request that does not say who is asking.
    #[test]
    fn every_check_says_which_program_is_asking() {
        for secret in Secret::ALL {
            let said = header(&asked(secret), "user-agent");

            assert!(said.starts_with("CorCode/"), "asked as {said}");
        }
    }

    #[test]
    fn no_check_ever_prints_the_credential_it_carries() {
        for secret in Secret::ALL {
            let printed = format!("{:?}", asked(secret));

            assert!(
                !printed.contains(CREDENTIAL),
                "the credential leaked: {printed}"
            );
        }
    }

    #[test]
    fn a_credential_no_header_could_carry_is_never_sent() {
        assert!(request(Secret::AnthropicKey, "sk-\u{0}-not-a-header").is_err());
    }

    /// A classic token says what it may reach, and a private clone needs `repo`
    /// (ADR-0005). A fine-grained token states no scopes at all, and so is
    /// never short of one.
    #[test]
    fn only_a_token_that_lists_its_scopes_can_be_short_of_repo() {
        assert!(!short_of_repo_scope(None));
        assert!(!short_of_repo_scope(Some("gist, repo, workflow")));
        assert!(short_of_repo_scope(Some("public_repo, gist")));
        assert!(short_of_repo_scope(Some("")));
    }
}
