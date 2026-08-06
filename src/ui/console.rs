//! The one screen: status line, new-chat form, grouped chat list (ADR-0008).

use std::fmt::Write as _;

use crate::git::chat_branch;
use crate::store::RuntimeStatus;

use super::{CHATS_PATH, Chat, HTMX_PATH, LOGOUT_PATH, last_push, page, status_word, text};

/// The branch a chat is cut from when the operator says nothing else.
const DEFAULT_BASE_BRANCH: &str = "main";

/// Orphan-sweep result, held here until a sweep actually runs (ADR-0008).
const SWEEP: &str = "ok";

/// The groups the chat list is stacked in, top to bottom (ADR-0002).
const GROUPS: [RuntimeStatus; 3] = [
    RuntimeStatus::Live,
    RuntimeStatus::Parked,
    RuntimeStatus::Archived,
];

/// The whole console: status line, the collapsed new-chat form over the
/// repositories this deployment offers, and the grouped chat list.
#[must_use]
pub fn console_page(
    chats: &[Chat],
    workspace_image: &str,
    repos: &[String],
    warm_pool: usize,
) -> String {
    page(
        "CorCode",
        &format!(
            "{}{}{}\
             <form method=\"post\" action=\"{LOGOUT_PATH}\">\
             <p><button type=\"submit\">Log out everywhere</button></p></form>\
             <script src=\"{HTMX_PATH}\" defer></script>",
            status_line(chats, workspace_image, warm_pool),
            new_chat_form(repos),
            chat_list(chats),
        ),
    )
}

/// The chat list on its own, so htmx can swap it in without the page.
#[must_use]
pub fn chat_list(chats: &[Chat]) -> String {
    let mut list = format!(
        "<section id=\"chats\">\
         <p><button hx-get=\"{CHATS_PATH}\" hx-target=\"#chats\" hx-swap=\"outerHTML\">\
         Refresh</button></p>"
    );
    for status in GROUPS {
        write!(
            list,
            "<h2>{}</h2>{}",
            status_word(status),
            group(chats, status)
        )
        .expect("writing to a string cannot fail");
    }
    list.push_str("</section>");
    list
}

/// The container picture in one line, expanding in place (ADR-0008).
fn status_line(chats: &[Chat], workspace_image: &str, warm_pool: usize) -> String {
    let live = count(chats, RuntimeStatus::Live);
    let parked = count(chats, RuntimeStatus::Parked);
    format!(
        "<details>\
         <summary>pool {live}/{warm_pool} · parked {parked} · img {} · sweep {SWEEP}</summary>\
         <dl><dt>Pool</dt><dd>{}</dd>\
         <dt>Parked</dt><dd>{parked} chats — workspace kept, container torn down</dd>\
         <dt>Image</dt><dd>{}</dd>\
         <dt>Sweep</dt><dd>{SWEEP}</dd></dl></details>",
        text(image_tag(workspace_image)),
        pool(chats),
        text(workspace_image),
    )
}

/// The new chat this form cuts. The preview slugifies as you type so it reads
/// as the branch name it would become; the server slugifies again, and that
/// is the one that binds.
fn new_chat_form(repos: &[String]) -> String {
    let prefix = chat_branch("");
    format!(
        "<details><summary>New chat</summary>\
         <form method=\"post\" action=\"{CHATS_PATH}\">\
         <p><label>Repository <select name=\"repo\">{}</select></label></p>\
         <p><label>Base branch <input name=\"base_branch\" value=\"{DEFAULT_BASE_BRANCH}\">\
         </label><br><small>Typed, not listed: naming a repository's branches would cost \
         a call to GitHub on every page.</small></p>\
         <p><label>Slug <input name=\"slug\" \
         oninput=\"document.getElementById('branch').textContent=\
         '{prefix}'+this.value.toLowerCase().replace(/[^a-z0-9]+/g,'-')\
         .replace(/^-|-$/g,'')\"></label></p>\
         <p>Branch: <output id=\"branch\">{prefix}</output></p>\
         <p><label><input type=\"checkbox\" name=\"direct_on_base\"> \
         Work directly on the base branch</label></p>\
         <p><button type=\"submit\">Create</button></p></form></details>",
        offered(repos),
    )
}

/// One `<option>` per repository this deployment was configured with, in the
/// order it was given them: the first is the default (ADR-0005).
fn offered(repos: &[String]) -> String {
    repos.iter().fold(String::new(), |mut markup, repo| {
        write!(markup, "<option>{}</option>", text(repo)).expect("writing to a string cannot fail");
        markup
    })
}

/// The chats in one group, most recently active first as the store scanned
/// them.
fn in_group(chats: &[Chat], status: RuntimeStatus) -> impl Iterator<Item = &Chat> {
    chats
        .iter()
        .filter(move |(_, chat_status)| *chat_status == status)
}

fn group(chats: &[Chat], status: RuntimeStatus) -> String {
    let rows: String = in_group(chats, status).map(row).collect();
    if rows.is_empty() {
        "<p>None.</p>".to_owned()
    } else {
        format!("<ul>{rows}</ul>")
    }
}

fn row((manifest, _): &Chat) -> String {
    format!(
        "<li><a href=\"{CHATS_PATH}/{}\">{}</a><br>\
         <small>{} · push {}</small></li>",
        text(&manifest.chat_id),
        text(&manifest.title),
        text(&manifest.branch),
        text(last_push(manifest)),
    )
}

fn count(chats: &[Chat], status: RuntimeStatus) -> usize {
    in_group(chats, status).count()
}

/// Who holds a warm-pool slot right now.
fn pool(chats: &[Chat]) -> String {
    let holders: Vec<String> = in_group(chats, RuntimeStatus::Live)
        .map(|(manifest, _)| text(&manifest.title).to_string())
        .collect();
    if holders.is_empty() {
        "no containers running".to_owned()
    } else {
        holders.join("<br>")
    }
}

/// The dated half of a pinned image reference (ADR-0004).
fn image_tag(workspace_image: &str) -> &str {
    workspace_image
        .rsplit_once(':')
        .map_or(workspace_image, |(_, tag)| tag)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use crate::store::{ChatState, MANIFEST_SCHEMA, Manifest};

    use super::super::status_word;
    use super::*;

    const IMAGE: &str = "ghcr.io/corvous/corcode-workspace:2026-08-05";

    /// A warm pool sized like no default, so a status line that reads it
    /// cannot pass by reading a constant.
    const POOL: usize = 3;

    /// What a deployment offers, in the order it was given them.
    fn repos() -> Vec<String> {
        vec![
            "CorVous/CorCode".to_owned(),
            "CorVous/zenni-tools".to_owned(),
        ]
    }

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
            checkpoint_branch: None,
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
        let rendered = console_page(&[], &status(), &repos());

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
    fn the_status_line_reports_the_configured_pool_size_the_parked_count_and_the_image_tag() {
        let rendered = console_page(&every_state(), &status(), &repos());

        assert!(
            rendered.contains("<summary>pool 1/3 · parked 1 · img 2026-08-05 · sweep ok</summary>"),
            "the status line does not read as ADR-0008 asks: {rendered}"
        );
        assert!(
            rendered.contains("<details><summary>pool"),
            "the status line does not expand in place: {rendered}"
        );
    }

    #[test]
    fn the_status_line_polls_itself_while_the_console_is_open() {
        let fragment = status_line(&status());

        assert!(
            fragment.starts_with("<section id=\"status\""),
            "the status line is not a fragment htmx can swap: {fragment}"
        );
        assert!(
            fragment.contains(&format!("hx-get=\"{STATUS_PATH}\""))
                && fragment.contains("hx-trigger=\"every 5s\""),
            "the status line does not poll itself: {fragment}"
        );
        assert!(
            console_page(&every_state(), &status(), &repos()).contains(&fragment),
            "the console and the fragment render the status differently"
        );
    }

    #[test]
    fn the_expanded_picture_names_every_slot_the_parked_count_the_image_and_the_sweep() {
        let fragment = status_line(&Status {
            pool: vec![
                slot("running", TimeDelta::minutes(3)),
                slot("waiting", TimeDelta::hours(2)),
            ],
            sweep: Some(Sweep {
                orphaned: vec!["01K1ORPHAN".to_owned()],
                held: Vec::new(),
            }),
            ..status()
        });

        assert!(
            fragment.contains("running · 3m") && fragment.contains("waiting · 2h"),
            "the slots do not carry their idle times: {fragment}"
        );
        assert!(fragment.contains(IMAGE), "the pinned image is missing: {fragment}");
        assert!(
            fragment.contains("01K1ORPHAN"),
            "the sweep does not name what it removed: {fragment}"
        );
    }

    #[test]
    fn an_empty_pool_says_so_rather_than_showing_nothing() {
        let fragment = status_line(&Status {
            pool: Vec::new(),
            ..status()
        });

        assert!(
            fragment.contains("no containers running"),
            "an empty pool renders as a blank: {fragment}"
        );
    }

    #[test]
    fn an_idle_time_reads_in_the_largest_unit_it_fills() {
        for (idle, reads) in [
            (TimeDelta::seconds(4), "4s"),
            (TimeDelta::seconds(59), "59s"),
            (TimeDelta::minutes(3), "3m"),
            (TimeDelta::hours(2), "2h"),
            (TimeDelta::hours(26), "1d"),
        ] {
            assert_eq!(idle_for(idle), reads);
        }
    }

    /// A clock that has slipped backwards is not a chat from the future.
    #[test]
    fn an_idle_time_that_ran_backwards_reads_as_no_time_at_all() {
        assert_eq!(idle_for(TimeDelta::seconds(-30)), "0s");
    }

    fn slot(title: &str, idle: TimeDelta) -> Slot {
        Slot {
            title: title.to_owned(),
            idle,
        }
    }

    /// A pool holding the one live chat of [`every_state`], swept clean.
    fn status() -> Status {
        Status {
            pool: vec![slot("running", TimeDelta::zero())],
            warm_pool: POOL,
            parked: 1,
            image: IMAGE.to_owned(),
            sweep: Some(Sweep::default()),
        }
    }

    #[test]
    fn the_new_chat_form_offers_the_repositories_this_deployment_was_given() {
        let rendered = console_page(&every_state(), &status(), &repos());

        assert!(
            rendered
                .contains("<option>CorVous/CorCode</option><option>CorVous/zenni-tools</option>"),
            "the repositories are not offered in the order given: {rendered}"
        );
        assert!(
            rendered.contains("name=\"base_branch\" value=\"main\""),
            "the base branch does not default to main: {rendered}"
        );
    }

    #[test]
    fn the_new_chat_form_previews_the_branch_it_would_cut() {
        let rendered = console_page(&[], &status(), &repos());
        let today = Utc::now().format("%Y-%m-%d");

        assert!(
            rendered.contains(&format!("chat/{today}-")),
            "the branch preview is missing: {rendered}"
        );
    }

    #[test]
    fn the_branch_preview_slugifies_what_you_type() {
        let rendered = console_page(&[], &status(), &repos());

        assert!(
            rendered.contains("toLowerCase()") && rendered.contains("[^a-z0-9]+"),
            "the preview would show a slug no branch could carry: {rendered}"
        );
    }

    #[test]
    fn the_new_chat_form_posts_itself_to_the_chats_path() {
        let rendered = console_page(&every_state(), &status(), &repos());

        assert!(
            rendered.contains(&format!("<form method=\"post\" action=\"{CHATS_PATH}\">")),
            "the new-chat form posts nowhere: {rendered}"
        );
        assert!(
            rendered.contains("<button type=\"submit\">Create</button>"),
            "the new-chat form cannot be submitted: {rendered}"
        );
    }

    #[test]
    fn the_chat_list_refreshes_itself_through_htmx() {
        let rendered = console_page(&[], &status(), &repos());

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
