//! The one-screen console: semantic HTML on browser defaults (ADR-0008).

mod chat;
mod console;
mod escape;
mod settings;

use crate::store::{Manifest, RuntimeStatus};

pub use chat::{chat_page, event_log};
pub use console::{chat_list, console_page, status_line, status_picture};
pub use settings::{secret_settings, settings_panel};

use escape::text;

/// The chat list, both as a section of the console and as a fragment htmx
/// swaps in on its own.
pub const CHATS_PATH: &str = "/chats";

/// The status line, both as the head of the console and as the fragment htmx
/// polls to keep it true (ADR-0008).
///
/// Every poll costs one scan of `chats/` and one liveness call to the daemon,
/// per console tab left open. At a household's handful of chats and tabs that
/// is nothing; it is the reason the interval is seconds rather than sub-second.
pub const STATUS_PATH: &str = "/status";

/// Where the vendored htmx bundle is served from; nothing reaches a CDN.
pub const HTMX_PATH: &str = "/assets/htmx.js";

/// Where the key-rotating sign-out posts (ADR-0003).
pub const LOGOUT_PATH: &str = "/logout-all";

/// Under which every settings action on every secret is spelled.
pub const SETTINGS_PATH: &str = "/settings";

/// The exact htmx build compiled into the binary.
pub const HTMX: &str = include_str!("../../assets/htmx-2.0.10.min.js");

/// The whole stylesheet (ADR-0008).
///
/// `color-scheme` so default link and control colours follow the system, 16px
/// controls so iOS Safari does not zoom on focus, padding, and a wrap guard
/// for long branch names.
pub const CSS: &str = "\
:root{color-scheme:light dark;}\
body{padding:1rem;overflow-wrap:break-word;}\
input,select,button{font-size:16px;}";

/// A chat paired with the runtime status it has right now (ADR-0002).
pub type Chat = (Manifest, RuntimeStatus);

/// One chat's page. The router spells its routes with these too, passing the
/// path parameter as the id, so no path is written down twice.
#[must_use]
pub fn chat_path(chat_id: &str) -> String {
    format!("{CHATS_PATH}/{chat_id}")
}

/// One chat's event log on its own, as htmx polls it.
#[must_use]
pub fn chat_events_path(chat_id: &str) -> String {
    format!("{}/events", chat_path(chat_id))
}

/// Where one chat's prompt box posts.
#[must_use]
pub fn chat_prompt_path(chat_id: &str) -> String {
    format!("{}/prompt", chat_path(chat_id))
}

/// Where one chat's archive button posts (ADR-0002 rule 3).
#[must_use]
pub fn chat_archive_path(chat_id: &str) -> String {
    format!("{}/archive", chat_path(chat_id))
}

/// Where one secret's Save posts. The router spells its routes with these
/// too, passing the path parameter as the name, so no path is written down
/// twice.
#[must_use]
pub fn secret_path(secret: &str) -> String {
    format!("{SETTINGS_PATH}/{secret}")
}

/// Where one secret's Clear posts.
#[must_use]
pub fn secret_clear_path(secret: &str) -> String {
    format!("{}/clear", secret_path(secret))
}

/// Where one secret's Verify posts.
#[must_use]
pub fn secret_verify_path(secret: &str) -> String {
    format!("{}/verify", secret_path(secret))
}

/// A semantic HTML document on browser defaults (ADR-0008).
#[must_use]
pub fn page(title: &str, body: &str) -> String {
    let title = text(title);
    format!(
        "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <style>{CSS}</style><title>{title}</title></head>\
         <body><h1>{title}</h1>{body}</body></html>"
    )
}

/// How a chat's state is named wherever the UI says it out loud.
const fn status_word(status: RuntimeStatus) -> &'static str {
    match status {
        RuntimeStatus::Live => "Live",
        RuntimeStatus::Parked => "Parked",
        RuntimeStatus::Archived => "Archived",
    }
}

/// The commit a chat last got onto the remote, or that it has none yet.
fn last_push(manifest: &Manifest) -> &str {
    manifest.last_pushed_commit.as_deref().unwrap_or("never")
}

#[cfg(test)]
mod tests {
    use crate::secrets::{Secret, Source};
    use crate::status::Status;

    use super::*;

    /// ADR-0008 budgets styling at "on the order of a dozen lines". Past this
    /// the UI is being restyled, which is a new decision, not an increment.
    const MAX_DECLARATIONS: usize = 16;

    #[test]
    fn the_stylesheet_stays_inside_the_adr_budget() {
        let declarations = CSS.matches(';').count();

        assert!(
            declarations <= MAX_DECLARATIONS,
            "the stylesheet has grown to {declarations} declarations: {CSS}"
        );
    }

    #[test]
    fn the_stylesheet_spends_its_budget_on_the_adr_items() {
        for item in ["color-scheme", "16px", "padding", "overflow"] {
            assert!(
                CSS.contains(item),
                "the stylesheet is missing {item}: {CSS}"
            );
        }
    }

    /// ADR-0008's budget only means anything if the stylesheet is the whole
    /// of the styling: one `<style>` block and no inline attribute anywhere.
    pub fn assert_styling_is_only_the_stylesheet(rendered: &str) {
        assert_eq!(
            rendered.matches("<style>").count(),
            1,
            "styling is not one block: {rendered}"
        );
        assert!(
            !rendered.contains("style=\""),
            "an inline style slipped past the budget: {rendered}"
        );
    }

    #[test]
    fn the_console_styles_itself_only_from_the_stylesheet() {
        assert_styling_is_only_the_stylesheet(&console_page(
            &[],
            &Status {
                pool: Vec::new(),
                warm_pool: 2,
                parked: 0,
                image: "ghcr.io/corvous/x:2026-08-05".to_owned(),
                sweep: None,
            },
            &["CorVous/CorCode".to_owned()],
            &[
                (Secret::GithubToken, Source::Environment.into()),
                (Secret::AnthropicKey, Source::Unset.into()),
            ],
        ));
    }

    #[test]
    fn every_page_asks_for_the_mobile_viewport() {
        assert!(
            page("CorCode", "").contains(
                "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">"
            )
        );
    }

    #[test]
    fn a_title_cannot_smuggle_markup_into_the_page() {
        let rendered = page("<script>alert(1)</script>", "");

        assert!(
            !rendered.contains("<script>"),
            "the title escaped into markup: {rendered}"
        );
    }
}
