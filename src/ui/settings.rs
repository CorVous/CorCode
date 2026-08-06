//! The settings panel: the two operational secrets, set and checked from the
//! one screen (ADR-0008).
//!
//! No function here is ever handed a secret's value, which is how none of
//! them can render one.

use crate::secrets::{AnthropicCredential, Secret, Source, Standing};
use crate::settings::Outcome;
use crate::verify::Verified;

use super::{secret_clear_path, secret_path, secret_verify_path, text};

/// A classic token that authenticates perfectly well and cannot clone the
/// private repositories a chat is cut from (ADR-0005).
const SHORT_OF_REPO_SCOPE: &str =
    "<p role=\"alert\">This token has no repo scope: private repositories are closed to it.</p>";

/// The whole panel, as the console draws it with nothing just done to it.
#[must_use]
pub fn settings_panel(secrets: &[(Secret, Standing)]) -> String {
    let sections: String = secrets
        .iter()
        .map(|&(secret, standing)| secret_settings(secret, standing, &Outcome::Untouched))
        .collect();
    format!("<details id=\"settings\"><summary>Settings</summary>{sections}</details>")
}

/// One secret's section, which is also what every action on it swaps back in.
///
/// Only the section is replaced: the panel around it holds whether the reader
/// has it open.
#[must_use]
pub fn secret_settings(secret: Secret, standing: Standing, outcome: &Outcome) -> String {
    let name = secret.name();
    let swap = format!(
        "hx-target=\"#{}\" hx-swap=\"outerHTML\"",
        section_id(secret)
    );
    format!(
        "<section id=\"{}\"><h3>{}</h3><p>{}</p>{}\
         <form hx-post=\"{}\" {swap}><p><label>New value \
         <input type=\"password\" name=\"value\" autocomplete=\"off\"></label> \
         <button type=\"submit\">Save</button></p></form>\
         <form hx-post=\"{}\" {swap}><p><button type=\"submit\">Clear</button></p></form>\
         <form hx-post=\"{}\" {swap}><p><button type=\"submit\">Verify</button></p></form>\
         </section>",
        section_id(secret),
        label(secret),
        reads(standing),
        told(outcome),
        secret_path(name),
        secret_clear_path(name),
        secret_verify_path(name),
    )
}

/// The element every action on one secret targets.
fn section_id(secret: Secret) -> String {
    format!("secret-{}", secret.name())
}

/// What the panel calls each secret out loud.
const fn label(secret: Secret) -> &'static str {
    match secret {
        Secret::GithubToken => "GitHub token",
        Secret::AnthropicKey => "Anthropic API key",
    }
}

/// How a secret's status line reads.
fn reads(standing: Standing) -> String {
    match standing.source {
        Source::Unset => "not set",
        Source::Environment => "set (from environment)",
        Source::Settings => "set (from settings)",
    }
    .to_owned()
}

/// What the last action came to, or nothing at all if there has not been one.
fn told(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Untouched => String::new(),
        Outcome::Saved => said("saved"),
        Outcome::NothingGiven => said("nothing given, nothing changed"),
        Outcome::Cleared => said("cleared"),
        Outcome::NothingToVerify => said("not set: nothing to check"),
        Outcome::Verified(verified) => verdict(verified),
    }
}

fn said(what: &str) -> String {
    format!("<p role=\"status\">{what}</p>")
}

/// What the service made of the credential, in the app's own words: a status
/// and a generic reason, never a line the upstream wrote.
fn verdict(verified: &Verified) -> String {
    match verified {
        Verified::Accepted {
            login,
            without_repo_scope,
        } => {
            let mut line = said(&format!("ok{}", authenticated_as(login.as_deref())));
            if *without_repo_scope {
                line.push_str(SHORT_OF_REPO_SCOPE);
            }
            line
        }
        Verified::Refused(status) => said(&format!("{} ({status})", refusal(*status))),
        Verified::Silent => said("the service could not be reached"),
    }
}

/// Who the service says the credential belongs to, where it says so at all.
fn authenticated_as(login: Option<&str>) -> String {
    login.map_or_else(String::new, |login| {
        format!(" — authenticated as {}", text(login))
    })
}

/// Why a credential was turned away, in as much detail as a status alone can
/// honestly carry.
const fn refusal(status: u16) -> &'static str {
    match status {
        401 => "invalid or expired",
        _ => "not accepted",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_says_whether_it_is_set_and_what_set_it() {
        for (source, reads) in [
            (Source::Unset, "not set"),
            (Source::Environment, "set (from environment)"),
            (Source::Settings, "set (from settings)"),
        ] {
            let rendered = untouched(source);

            assert!(
                rendered.contains(reads),
                "a {source:?} secret does not read as {reads}: {rendered}"
            );
        }
    }

    /// The Anthropic slot takes either kind, and the two are opened with in
    /// different ways. Which one is in there is the operator's to see, from
    /// wherever it came.
    #[test]
    fn a_slot_that_takes_more_than_one_kind_of_credential_names_the_one_in_it() {
        for (source, set) in [
            (Source::Settings, "set (from settings)"),
            (Source::Environment, "set (from environment)"),
        ] {
            for (kind, named) in [
                (AnthropicCredential::OauthToken, "OAuth token"),
                (AnthropicCredential::ApiKey, "API key"),
            ] {
                let rendered = secret_settings(
                    Secret::AnthropicKey,
                    Standing {
                        source,
                        kind: Some(kind),
                    },
                    &Outcome::Untouched,
                );

                assert!(
                    rendered.contains(&format!("{set} — {named}")),
                    "a {kind:?} from {source:?} does not read as one: {rendered}"
                );
            }
        }
    }

    #[test]
    fn a_secret_that_stands_for_no_kind_of_credential_names_none() {
        let rendered = untouched(Source::Settings);

        assert!(rendered.contains("<p>set (from settings)</p>"), "{rendered}");
    }

    #[test]
    fn the_box_a_secret_is_typed_into_never_carries_one_back() {
        let rendered = untouched(Source::Settings);

        assert!(
            rendered.contains("type=\"password\""),
            "the secret is typed in the clear: {rendered}"
        );
        assert!(
            !rendered.contains("value=\""),
            "the box would carry a value back to the browser: {rendered}"
        );
    }

    #[test]
    fn setting_checking_and_clearing_one_secret_each_post_to_a_path_of_their_own() {
        let rendered = untouched(Source::Settings);

        for path in [
            secret_path("github_token"),
            secret_clear_path("github_token"),
            secret_verify_path("github_token"),
        ] {
            assert!(
                rendered.contains(&format!("hx-post=\"{path}\"")),
                "nothing posts to {path}: {rendered}"
            );
        }
        assert!(
            rendered.contains("hx-target=\"#secret-github_token\""),
            "an action would swap something other than its own secret: {rendered}"
        );
    }

    /// Checking a credential costs a call to somebody else's service. Only a
    /// click may spend it, so the panel carries no trigger of its own.
    #[test]
    fn nothing_in_the_panel_checks_a_credential_on_a_clock() {
        let rendered = settings_panel(&[
            (Secret::GithubToken, stands(Source::Settings)),
            (Secret::AnthropicKey, stands(Source::Environment)),
        ]);

        assert!(
            !rendered.contains("hx-trigger"),
            "the panel would act without a click: {rendered}"
        );
    }

    #[test]
    fn the_panel_holds_every_secret_this_deployment_keeps() {
        let rendered = settings_panel(&[
            (Secret::GithubToken, stands(Source::Settings)),
            (Secret::AnthropicKey, stands(Source::Unset)),
        ]);

        assert!(
            rendered.contains("GitHub token") && rendered.contains("Anthropic API key"),
            "the panel is missing a secret: {rendered}"
        );
    }

    #[test]
    fn a_blank_save_says_that_nothing_was_given_and_nothing_changed() {
        let rendered = after(&Outcome::NothingGiven);

        assert!(
            rendered.contains("nothing given, nothing changed"),
            "a blank save reads as something having happened: {rendered}"
        );
    }

    #[test]
    fn saving_and_clearing_each_say_what_they_did() {
        assert!(after(&Outcome::Saved).contains("saved"));
        assert!(after(&Outcome::Cleared).contains("cleared"));
    }

    #[test]
    fn a_working_token_names_who_it_authenticated_as() {
        let rendered = after(&Outcome::Verified(Verified::Accepted {
            login: Some("cassidy".to_owned()),
            without_repo_scope: false,
        }));

        assert!(
            rendered.contains("ok — authenticated as cassidy"),
            "a working token does not name its account: {rendered}"
        );
        assert!(
            !rendered.contains("repo scope"),
            "a token that may reach private repositories is warned about: {rendered}"
        );
    }

    /// A classic token whose scopes stop short of `repo` cannot clone the
    /// private repositories a chat is cut from (ADR-0005), and it authenticates
    /// perfectly well right up until it tries.
    #[test]
    fn a_token_that_cannot_reach_private_repositories_is_flagged_as_working_anyway() {
        let rendered = after(&Outcome::Verified(Verified::Accepted {
            login: Some("cassidy".to_owned()),
            without_repo_scope: true,
        }));

        assert!(rendered.contains("ok — authenticated as cassidy"));
        assert!(
            rendered.contains("repo scope"),
            "a token short of the repo scope passes without a word: {rendered}"
        );
    }

    #[test]
    fn a_service_that_names_nobody_just_reads_ok() {
        let rendered = after(&Outcome::Verified(Verified::Accepted {
            login: None,
            without_repo_scope: false,
        }));

        assert!(rendered.contains("ok"));
        assert!(
            !rendered.contains("authenticated as"),
            "a service that named nobody is quoted naming somebody: {rendered}"
        );
    }

    #[test]
    fn a_refused_credential_reads_as_its_status_and_nothing_the_service_said() {
        assert!(
            after(&Outcome::Verified(Verified::Refused(401))).contains("invalid or expired (401)")
        );
        assert!(after(&Outcome::Verified(Verified::Refused(500))).contains("(500)"));
    }

    #[test]
    fn a_service_that_did_not_answer_says_so() {
        assert!(
            after(&Outcome::Verified(Verified::Silent)).contains("could not be reached"),
            "a silent service reads as something else"
        );
    }

    #[test]
    fn a_secret_nothing_holds_has_nothing_to_check() {
        assert!(after(&Outcome::NothingToVerify).contains("nothing to check"));
    }

    #[test]
    fn an_untouched_panel_reports_no_action_at_all() {
        assert!(
            !untouched(Source::Settings).contains("role=\"status\""),
            "a freshly drawn panel claims something just happened"
        );
    }

    #[test]
    fn an_account_name_cannot_smuggle_markup_into_the_panel() {
        let rendered = after(&Outcome::Verified(Verified::Accepted {
            login: Some("<img src=x onerror=alert(1)>".to_owned()),
            without_repo_scope: false,
        }));

        assert!(
            !rendered.contains("<img"),
            "the account name escaped into markup: {rendered}"
        );
    }

    /// One secret's section, with nothing just done to it.
    fn untouched(source: Source) -> String {
        secret_settings(Secret::GithubToken, stands(source), &Outcome::Untouched)
    }

    /// One secret's section, reporting what was just done to it.
    fn after(outcome: &Outcome) -> String {
        secret_settings(Secret::GithubToken, stands(Source::Settings), outcome)
    }

    /// How a secret that names no kind of credential stands.
    const fn stands(source: Source) -> Standing {
        Standing { source, kind: None }
    }
}
