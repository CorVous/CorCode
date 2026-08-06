//! The chat view: `events.jsonl` rendered as an event log (ADR-0006).

use serde_json::Value;

use crate::store::{Event, Manifest, RuntimeStatus};

use super::{HTMX_PATH, chat_events_path, chat_prompt_path, last_push, page, status_word, text};

/// What an event calls itself when it carries no ACP discriminator.
const UNTYPED: &str = "event";

/// The key on a line the core wrote in its own voice rather than relaying.
const CORE_LINE: &str = "corcode";

/// How often an open chat asks for the log again. A turn streams for minutes,
/// so this is what "live" means here: polling, not a second connection.
const POLL_SECONDS: u32 = 2;

/// One chat, top to bottom: what it is, everything that has happened to it,
/// and the prompt box waiting at the end.
#[must_use]
pub fn chat_page(manifest: &Manifest, status: RuntimeStatus, events: &[Event]) -> String {
    page(
        &manifest.title,
        &format!(
            "<p><a href=\"/\">← chats</a></p>\
             <p><small>{} · {} · push {} · {}</small></p>\
             {}{}\
             <form hx-post=\"{}\" hx-target=\"#log\" hx-swap=\"outerHTML\">\
             <p><input name=\"prompt\" aria-label=\"Prompt\" placeholder=\"prompt\"> \
             <button type=\"submit\">Send</button></p></form>\
             <script src=\"{HTMX_PATH}\" defer></script>",
            text(&manifest.repo),
            text(&manifest.branch),
            text(last_push(manifest)),
            status_word(status),
            event_log(&manifest.chat_id, events),
            first_prompt_hint(status),
            text(&chat_prompt_path(&manifest.chat_id)),
        ),
    )
}

/// The event log alone, polling itself for whatever a turn has added since.
///
/// It is rendered from `events.jsonl` and never from the connection, so it
/// reads the same whether the chat is live or was live last week (ADR-0006).
#[must_use]
pub fn event_log(chat_id: &str, events: &[Event]) -> String {
    let lines: String = events.iter().map(|event| line(&event.event)).collect();
    format!(
        "<section id=\"log\" hx-get=\"{}\" hx-trigger=\"every {POLL_SECONDS}s\" \
         hx-swap=\"outerHTML\">{lines}</section>",
        text(&chat_events_path(chat_id)),
    )
}

/// What a first prompt would do to a chat with no live container (ADR-0007).
const fn first_prompt_hint(status: RuntimeStatus) -> &'static str {
    match status {
        RuntimeStatus::Live => "",
        RuntimeStatus::Parked => {
            "<p><small>A prompt re-spins the container over the kept workspace.</small></p>"
        }
        RuntimeStatus::Archived => {
            "<p><small>A prompt revives the chat: a fresh clone at the last pushed \
             commit.</small></p>"
        }
    }
}

/// One line of the log, in the on-disk shapes ADR-0006 fixes.
fn line(event: &Value) -> String {
    match entry(event) {
        Entry::Prompt(said) => format!("<p><b>you:</b> {}</p>", text(&said)),
        Entry::AgentText(said) => format!("<p>{}</p>", text(&said)),
        Entry::Notice(said) => format!("<blockquote>{}</blockquote>", text(said)),
        Entry::Aside(said) => format!("<p><small>{}</small></p>", text(said)),
    }
}

/// How a log line reads once its shape is known.
enum Entry<'a> {
    Prompt(String),
    AgentText(String),
    Notice(&'a str),
    Aside(&'a str),
}

/// The store has already refused anything unreadable, so a shape this build
/// does not know is a newer ACP, not damage: it names itself as an aside
/// rather than disappearing.
fn entry(event: &Value) -> Entry<'_> {
    if let Some(said) = event.get("prompt").and_then(blocks_text) {
        return Entry::Prompt(said);
    }
    if let Some(kind) = field(event, CORE_LINE) {
        return Entry::Notice(field(event, "text").unwrap_or(kind));
    }
    let kind = field(event, "sessionUpdate").unwrap_or(UNTYPED);
    match (kind, event.get("content").and_then(blocks_text)) {
        ("user_message_chunk", Some(said)) => Entry::Prompt(said),
        ("agent_message_chunk" | "agent_thought_chunk", Some(said)) => Entry::AgentText(said),
        ("tool_call" | "tool_call_update", _) => {
            Entry::Aside(field(event, "title").unwrap_or(kind))
        }
        _ => Entry::Aside(kind),
    }
}

/// The words in one content block or a run of them; blocks that carry no text
/// (resource links, images) contribute nothing to read.
fn blocks_text(content: &Value) -> Option<String> {
    let said = match content.as_array() {
        Some(blocks) => blocks.iter().filter_map(block_text).collect(),
        None => block_text(content)?.to_owned(),
    };
    (!said.is_empty()).then_some(said)
}

fn block_text(block: &Value) -> Option<&str> {
    (field(block, "type") == Some("text")).then(|| field(block, "text"))?
}

fn field<'a>(event: &'a Value, name: &str) -> Option<&'a str> {
    event.get(name).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::{Value, json};

    use crate::store::{ChatState, MANIFEST_SCHEMA};

    use super::*;

    fn manifest(status: RuntimeStatus) -> Manifest {
        let now = Utc::now();
        Manifest {
            schema: MANIFEST_SCHEMA,
            chat_id: "01K1TESTCHATID0000000000".to_owned(),
            title: "Resume ladder".to_owned(),
            state: match status {
                RuntimeStatus::Archived => ChatState::Archived,
                RuntimeStatus::Live | RuntimeStatus::Parked => ChatState::Open,
            },
            repo: "CorVous/CorCode".to_owned(),
            branch: "chat/2026-08-05-resume-ladder".to_owned(),
            base_branch: "main".to_owned(),
            checkpoint_branch: None,
            last_pushed_commit: Some("abc1234".to_owned()),
            acp_session_id: None,
            created_at: now,
            last_active_at: now,
        }
    }

    fn log(payloads: &[Value]) -> Vec<Event> {
        payloads
            .iter()
            .map(|payload| Event {
                ts: Utc::now(),
                event: payload.clone(),
            })
            .collect()
    }

    fn position(rendered: &str, needle: &str) -> usize {
        rendered
            .find(needle)
            .unwrap_or_else(|| panic!("{needle} is missing from: {rendered}"))
    }

    /// The outbound `session/prompt` the core records for the user (ADR-0006).
    fn outbound_prompt(said: &str) -> Value {
        json!({
            "sessionId": "3f2b1c4d-0000-4000-8000-000000000001",
            "prompt": [{"type": "text", "text": said}],
        })
    }

    /// An inbound `sessionUpdate` chunk, text inside a content block.
    fn chunk(update: &str, said: &str) -> Value {
        json!({"sessionUpdate": update, "content": {"type": "text", "text": said}})
    }

    #[test]
    fn an_outbound_prompt_renders_as_the_users_own_block() {
        let events = log(&[outbound_prompt("ship the ladder")]);

        let rendered = chat_page(&manifest(RuntimeStatus::Live), RuntimeStatus::Live, &events);

        assert!(
            rendered.contains("<b>you:</b> ship the ladder"),
            "the prompt is not the user's own block: {rendered}"
        );
    }

    #[test]
    fn a_prompt_of_several_content_blocks_reads_as_one() {
        let events = log(&[json!({
            "sessionId": "s",
            "prompt": [
                {"type": "text", "text": "ship "},
                {"type": "resource_link", "uri": "file:///workspace/src/ui/chat.rs"},
                {"type": "text", "text": "the ladder"},
            ],
        })]);

        let rendered = chat_page(&manifest(RuntimeStatus::Live), RuntimeStatus::Live, &events);

        assert!(
            rendered.contains("<b>you:</b> ship the ladder"),
            "the prompt's text blocks did not join up: {rendered}"
        );
    }

    #[test]
    fn agent_text_comes_out_of_its_content_block() {
        let events = log(&[chunk("agent_message_chunk", "on it")]);

        let rendered = chat_page(&manifest(RuntimeStatus::Live), RuntimeStatus::Live, &events);

        assert!(
            rendered.contains("<p>on it</p>"),
            "the agent's words vanished: {rendered}"
        );
    }

    #[test]
    fn a_replayed_user_chunk_reads_as_the_user_too() {
        let events = log(&[chunk("user_message_chunk", "ship the ladder")]);

        let rendered = chat_page(&manifest(RuntimeStatus::Live), RuntimeStatus::Live, &events);

        assert!(rendered.contains("<b>you:</b> ship the ladder"));
    }

    #[test]
    fn prompts_and_agent_text_render_in_the_order_they_happened() {
        let events = log(&[
            outbound_prompt("ship the ladder"),
            chunk("agent_message_chunk", "on it"),
        ]);

        let rendered = chat_page(&manifest(RuntimeStatus::Live), RuntimeStatus::Live, &events);

        assert!(
            position(&rendered, "ship the ladder") < position(&rendered, "on it"),
            "the log is out of order: {rendered}"
        );
    }

    #[test]
    fn a_tool_call_renders_as_a_small_inline_line() {
        let events = log(&[json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_1",
            "title": "git commit",
            "kind": "execute",
            "status": "pending",
        })]);

        let rendered = chat_page(&manifest(RuntimeStatus::Live), RuntimeStatus::Live, &events);

        assert!(
            rendered.contains("<small>git commit</small>"),
            "the tool call is not a small inline line: {rendered}"
        );
    }

    #[test]
    fn a_core_injected_reset_notice_renders_as_a_block_quote() {
        let events = log(&[json!({
            "corcode": "reset_notice",
            "text": "Agent memory was reset; the log below is the whole record.",
        })]);

        let rendered = chat_page(&manifest(RuntimeStatus::Live), RuntimeStatus::Live, &events);

        assert!(
            rendered.contains("<blockquote>Agent memory was reset"),
            "the reset notice is not a block quote: {rendered}"
        );
    }

    #[test]
    fn an_event_shape_this_build_does_not_know_still_renders() {
        let events = log(&[json!({"sessionUpdate": "plan", "entries": []})]);

        let rendered = chat_page(&manifest(RuntimeStatus::Live), RuntimeStatus::Live, &events);

        assert!(
            rendered.contains("<small>plan</small>"),
            "an unknown event vanished instead of naming itself: {rendered}"
        );
    }

    #[test]
    fn a_parked_chat_says_a_prompt_would_re_spin_its_container() {
        let rendered = chat_page(&manifest(RuntimeStatus::Parked), RuntimeStatus::Parked, &[]);

        assert!(
            rendered.contains("re-spins"),
            "the parked chat gives no first-prompt hint: {rendered}"
        );
    }

    #[test]
    fn an_archived_chat_says_a_prompt_would_revive_it() {
        let rendered = chat_page(
            &manifest(RuntimeStatus::Archived),
            RuntimeStatus::Archived,
            &[],
        );

        assert!(
            rendered.contains("revives"),
            "the archived chat gives no first-prompt hint: {rendered}"
        );
    }

    #[test]
    fn a_live_chat_needs_no_first_prompt_hint() {
        let rendered = chat_page(&manifest(RuntimeStatus::Live), RuntimeStatus::Live, &[]);

        assert!(!rendered.contains("re-spins") && !rendered.contains("revives"));
    }

    #[test]
    fn the_chat_view_heads_with_its_branch_last_push_and_state() {
        let rendered = chat_page(&manifest(RuntimeStatus::Live), RuntimeStatus::Live, &[]);

        assert!(rendered.contains("chat/2026-08-05-resume-ladder"));
        assert!(rendered.contains("push abc1234"));
        assert!(rendered.contains("Live"));
        assert!(
            rendered.contains("href=\"/\""),
            "there is no way back to the console: {rendered}"
        );
    }

    #[test]
    fn the_chat_view_styles_itself_only_from_the_stylesheet() {
        let events = log(&[chunk("agent_message_chunk", "on it")]);

        let rendered = chat_page(&manifest(RuntimeStatus::Live), RuntimeStatus::Live, &events);

        crate::ui::tests::assert_styling_is_only_the_stylesheet(&rendered);
    }

    #[test]
    fn the_prompt_box_posts_the_prompt_and_swaps_the_log_it_lands_in() {
        let manifest = manifest(RuntimeStatus::Live);

        let rendered = chat_page(&manifest, RuntimeStatus::Live, &[]);

        assert!(
            rendered.contains(&format!(
                "hx-post=\"{}\"",
                chat_prompt_path(&manifest.chat_id)
            )),
            "the prompt box posts nowhere: {rendered}"
        );
        assert!(
            rendered.contains("hx-target=\"#log\"") && rendered.contains("name=\"prompt\""),
            "the prompt would not land in the log: {rendered}"
        );
        assert!(
            !rendered.contains("disabled"),
            "the prompt box is still inert: {rendered}"
        );
    }

    #[test]
    fn the_log_polls_itself_while_the_page_is_open() {
        let manifest = manifest(RuntimeStatus::Live);

        let fragment = event_log(
            &manifest.chat_id,
            &log(&[chunk("agent_message_chunk", "on it")]),
        );

        assert!(
            fragment.starts_with("<section id=\"log\""),
            "the log is not a fragment htmx can swap: {fragment}"
        );
        assert!(
            fragment.contains(&format!(
                "hx-get=\"{}\"",
                chat_events_path(&manifest.chat_id)
            )) && fragment.contains("hx-trigger=\"every 2s\""),
            "the log does not poll itself: {fragment}"
        );
        assert!(fragment.contains("<p>on it</p>"));
    }

    #[test]
    fn the_chat_page_carries_the_log_and_the_script_that_polls_it() {
        let manifest = manifest(RuntimeStatus::Live);
        let events = log(&[chunk("agent_message_chunk", "on it")]);

        let rendered = chat_page(&manifest, RuntimeStatus::Live, &events);

        assert!(
            rendered.contains(&event_log(&manifest.chat_id, &events)),
            "the page and the fragment render the log differently: {rendered}"
        );
        assert!(
            rendered.contains(&format!("src=\"{}\"", crate::ui::HTMX_PATH)),
            "htmx is not loaded, so nothing polls: {rendered}"
        );
    }

    #[test]
    fn agent_text_cannot_smuggle_markup_into_the_log() {
        let events = log(&[chunk("agent_message_chunk", "<script>alert(1)</script>")]);

        let rendered = chat_page(&manifest(RuntimeStatus::Live), RuntimeStatus::Live, &events);

        assert!(
            !rendered.contains("<script>"),
            "agent text escaped into markup: {rendered}"
        );
    }
}
