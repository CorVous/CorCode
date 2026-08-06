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
