//! The one-screen console: semantic HTML on browser defaults (ADR-0008).

mod chat;
mod console;
mod escape;

use crate::store::{Manifest, RuntimeStatus};

pub use chat::chat_page;
pub use console::{chat_list, console_page};

use escape::text;

/// The chat list, both as a section of the console and as a fragment htmx
/// swaps in on its own.
pub const CHATS_PATH: &str = "/chats";

/// Where the vendored htmx bundle is served from; nothing reaches a CDN.
pub const HTMX_PATH: &str = "/assets/htmx.js";

/// Where the key-rotating sign-out posts (ADR-0003).
pub const LOGOUT_PATH: &str = "/logout-all";

/// The exact htmx build compiled into the binary.
pub const HTMX: &str = include_str!("../../assets/htmx-2.0.10.min.js");

/// The whole stylesheet (ADR-0008): `color-scheme` so default link and
/// control colours follow the system, 16px controls so iOS Safari does not
/// zoom on focus, padding, and a wrap guard for long branch names.
pub const CSS: &str = "";

/// ADR-0008 budgets styling at "on the order of a dozen lines". Past this the
/// UI is being restyled, which is a new decision rather than an increment.
const MAX_DECLARATIONS: usize = 16;

/// A chat paired with the runtime status it has right now (ADR-0002).
pub type Chat = (Manifest, RuntimeStatus);

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
    use super::*;

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
            assert!(CSS.contains(item), "the stylesheet is missing {item}: {CSS}");
        }
    }

    #[test]
    fn every_page_asks_for_the_mobile_viewport() {
        assert!(page("CorCode", "").contains(
            "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">"
        ));
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
