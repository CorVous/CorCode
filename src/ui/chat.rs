//! The chat view: `events.jsonl` rendered as an event log (ADR-0006).

use crate::store::{Event, Manifest, RuntimeStatus};

/// One chat, top to bottom: what it is, everything that has happened to it,
/// and the prompt box waiting at the end.
#[must_use]
pub fn chat_page(_manifest: &Manifest, _status: RuntimeStatus, _events: &[Event]) -> String {
    String::new()
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

    #[test]
    fn prompts_and_agent_text_render_in_the_order_they_happened() {
        let events = log(&[
            json!({"sessionUpdate": "user_message_chunk", "text": "ship the ladder"}),
            json!({"sessionUpdate": "agent_message_chunk", "text": "on it"}),
        ]);

        let rendered = chat_page(&manifest(RuntimeStatus::Live), RuntimeStatus::Live, &events);

        assert!(
            position(&rendered, "ship the ladder") < position(&rendered, "on it"),
            "the log is out of order: {rendered}"
        );
    }

    #[test]
    fn a_tool_call_renders_as_a_small_inline_line() {
        let events = log(&[json!({"sessionUpdate": "tool_call", "title": "git commit"})]);

        let rendered = chat_page(&manifest(RuntimeStatus::Live), RuntimeStatus::Live, &events);

        assert!(
            rendered.contains("<small>git commit</small>"),
            "the tool call is not a small inline line: {rendered}"
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
        let rendered = chat_page(
            &manifest(RuntimeStatus::Parked),
            RuntimeStatus::Parked,
            &[],
        );

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
    fn the_prompt_box_is_inert() {
        let rendered = chat_page(&manifest(RuntimeStatus::Live), RuntimeStatus::Live, &[]);

        assert!(
            rendered.contains("<button type=\"submit\" disabled>Send</button>"),
            "the prompt box would submit somewhere: {rendered}"
        );
    }

    #[test]
    fn agent_text_cannot_smuggle_markup_into_the_log() {
        let events = log(&[
            json!({"sessionUpdate": "agent_message_chunk", "text": "<script>alert(1)</script>"}),
        ]);

        let rendered = chat_page(&manifest(RuntimeStatus::Live), RuntimeStatus::Live, &events);

        assert!(
            !rendered.contains("<script>"),
            "agent text escaped into markup: {rendered}"
        );
    }
}
