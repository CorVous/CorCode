//! The one screen: status line, new-chat form, grouped chat list (ADR-0008).

use crate::store::RuntimeStatus;

use super::Chat;

/// The whole console: status line, the collapsed new-chat form, and the
/// grouped chat list.
#[must_use]
pub fn console_page(_chats: &[Chat], _workspace_image: &str) -> String {
    String::new()
}

/// The chat list on its own, so htmx can swap it in without the page.
#[must_use]
pub fn chat_list(_chats: &[Chat]) -> String {
    String::new()
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use crate::store::{ChatState, MANIFEST_SCHEMA, Manifest};

    use super::super::status_word;
    use super::*;

    const IMAGE: &str = "ghcr.io/corvous/corcode-workspace:2026-08-05";

    fn chat(title: &str, status: RuntimeStatus) -> Chat {
        let now = Utc::now();
        let state = match status {
            RuntimeStatus::Archived => ChatState::Archived,
            RuntimeStatus::Live | RuntimeStatus::Parked => ChatState::Open,
        };
        let manifest = Manifest {
            schema: MANIFEST_SCHEMA,
            chat_id: format!("01K1{}", title.to_uppercase()),
            title: title.to_owned(),
            state,
            repo: "CorVous/CorCode".to_owned(),
            branch: format!("chat/2026-08-05-{title}"),
            base_branch: "main".to_owned(),
            last_pushed_commit: Some("abc1234".to_owned()),
            acp_session_id: None,
            created_at: now,
            last_active_at: now,
        };
        (manifest, status)
    }

    /// Where `needle` starts, so that tests can assert on document order.
    fn position(rendered: &str, needle: &str) -> usize {
        rendered
            .find(needle)
            .unwrap_or_else(|| panic!("{needle} is missing from: {rendered}"))
    }

    fn every_state() -> Vec<Chat> {
        vec![
            chat("running", RuntimeStatus::Live),
            chat("resting", RuntimeStatus::Parked),
            chat("finished", RuntimeStatus::Archived),
        ]
    }

    #[test]
    fn each_chat_sits_under_the_heading_of_its_runtime_state() {
        let rendered = chat_list(&every_state());

        for (title, status) in [
            ("running", RuntimeStatus::Live),
            ("resting", RuntimeStatus::Parked),
            ("finished", RuntimeStatus::Archived),
        ] {
            let heading = position(&rendered, &format!("<h2>{}</h2>", status_word(status)));
            let row = position(&rendered, title);
            assert!(
                heading < row,
                "{title} is not under its {status:?} heading: {rendered}"
            );
        }
        assert!(
            position(&rendered, "running") < position(&rendered, "resting")
                && position(&rendered, "resting") < position(&rendered, "finished"),
            "the groups are out of order: {rendered}"
        );
    }

    #[test]
    fn a_row_links_to_the_chat_and_shows_its_branch_and_last_push() {
        let chats = vec![chat("running", RuntimeStatus::Live)];

        let rendered = chat_list(&chats);

        assert!(
            rendered.contains("href=\"/chats/01K1RUNNING\""),
            "the row does not link to the chat: {rendered}"
        );
        assert!(
            rendered.contains("chat/2026-08-05-running"),
            "the row does not show the branch: {rendered}"
        );
        assert!(
            rendered.contains("push abc1234"),
            "the row does not show the last push: {rendered}"
        );
    }

    #[test]
    fn a_chat_that_has_never_pushed_says_so() {
        let mut chats = vec![chat("running", RuntimeStatus::Live)];
        chats[0].0.last_pushed_commit = None;

        assert!(chat_list(&chats).contains("push never"));
    }

    #[test]
    fn an_empty_dataset_still_renders_every_group() {
        let rendered = console_page(&[], IMAGE);

        for status in [
            RuntimeStatus::Live,
            RuntimeStatus::Parked,
            RuntimeStatus::Archived,
        ] {
            assert!(
                rendered.contains(&format!("<h2>{}</h2>", status_word(status))),
                "the {status:?} group is missing: {rendered}"
            );
        }
    }

    #[test]
    fn the_status_line_reports_the_parked_count_and_the_pinned_image_tag() {
        let rendered = console_page(&every_state(), IMAGE);

        assert!(
            rendered.contains("<summary>pool 1/2 · parked 1 · img 2026-08-05 · sweep ok</summary>"),
            "the status line does not read as ADR-0008 asks: {rendered}"
        );
        assert!(
            rendered.contains("<details><summary>pool"),
            "the status line does not expand in place: {rendered}"
        );
    }

    #[test]
    fn the_new_chat_form_offers_the_repositories_already_in_use() {
        let mut chats = every_state();
        chats[0].0.repo = "CorVous/zenni-tools".to_owned();

        let rendered = console_page(&chats, IMAGE);

        assert!(rendered.contains("<option>CorVous/CorCode</option>"));
        assert!(rendered.contains("<option>CorVous/zenni-tools</option>"));
        assert!(
            rendered.contains("<option>main</option>"),
            "the base branch select is empty: {rendered}"
        );
    }

    #[test]
    fn the_new_chat_form_previews_the_branch_it_would_cut() {
        let rendered = console_page(&[], IMAGE);
        let today = Utc::now().format("%Y-%m-%d");

        assert!(
            rendered.contains(&format!("chat/{today}-")),
            "the branch preview is missing: {rendered}"
        );
    }

    #[test]
    fn nothing_on_the_console_can_be_submitted_yet() {
        let rendered = console_page(&every_state(), IMAGE);

        assert!(
            rendered.contains("<button type=\"submit\" disabled>Create</button>"),
            "the new-chat form is not inert: {rendered}"
        );
    }

    #[test]
    fn the_chat_list_refreshes_itself_through_htmx() {
        let rendered = console_page(&[], IMAGE);

        assert!(
            rendered.contains(&format!("src=\"{}\"", super::super::HTMX_PATH)),
            "htmx is not loaded: {rendered}"
        );
        assert!(
            chat_list(&[]).contains("hx-get=\"/chats\""),
            "the chat list cannot refresh itself: {}",
            chat_list(&[])
        );
    }

    #[test]
    fn a_chat_title_cannot_smuggle_markup_into_a_row() {
        let chats = vec![chat("<img src=x onerror=alert(1)>", RuntimeStatus::Live)];

        let rendered = chat_list(&chats);

        assert!(
            !rendered.contains("<img"),
            "the title escaped into markup: {rendered}"
        );
        assert!(rendered.contains("&lt;img"));
    }
}
