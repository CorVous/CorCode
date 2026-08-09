//! The chat view: `events.jsonl` rendered as an event log (ADR-0006).
//!
//! The log is read rather than replayed: a run of chunks is one message, a
//! tool call is one line however many updates it took, and bookkeeping is left
//! out. One rendering serves both the page and the fragment htmx polls, so a
//! chat reads the same on load as it does while it streams.

use serde_json::Value;

use crate::store::{Event, Manifest, RuntimeStatus};

use super::{
    HTMX_PATH, chat_archive_path, chat_events_path, chat_prompt_path, last_push, page, status_word,
    text,
};

/// What an event calls itself when it carries no ACP discriminator.
const UNTYPED: &str = "event";

/// The key on a line the core wrote in its own voice rather than relaying.
const CORE_LINE: &str = "corcode";

/// What a tool call that names neither itself nor an id is called.
const UNNAMED_TOOL: &str = "tool call";

/// How an adapter wraps a tool's output for a markdown reader. The transcript
/// is not one, so the wrapper would be read out as itself.
const FENCE: &str = "```";

/// Updates the agent keeps its client's accounting with. They carry no words
/// for the operator, so the transcript is quieter without them.
const BOOKKEEPING: [&str; 3] = [
    "usage_update",
    "available_commands_update",
    "session_info_update",
];

/// How often an open chat asks for the log again. A turn streams for minutes,
/// so this is what "live" means here: polling, not a second connection.
const POLL_SECONDS: u32 = 2;

/// How often the whole log is sent again. The tail is polled from a cursor,
/// which nothing moves between resyncs: this is what puts the settled lines
/// back where they belong and starts the tail over from the end of them.
const RESYNC_SECONDS: u32 = 30;

/// One chat, top to bottom: what it is, everything that has happened to it,
/// and the prompt box waiting at the end.
#[must_use]
pub fn chat_page(manifest: &Manifest, status: RuntimeStatus, events: &[Event]) -> String {
    page(
        &manifest.title,
        &format!(
            "<p><a href=\"/\">← chats</a></p>\
             <p><small>{} · {} · push {} · {}</small></p>\
             {}{}{}\
             <form hx-post=\"{}\" hx-target=\"#log\" hx-swap=\"outerHTML\" \
             hx-on::after-request=\"if(event.detail.successful)this.reset()\">\
             <p><input name=\"prompt\" aria-label=\"Prompt\" placeholder=\"prompt\"> \
             <button type=\"submit\">Send</button></p></form>\
             <script src=\"{HTMX_PATH}\" defer></script>",
            text(&manifest.repo),
            text(&manifest.branch),
            text(last_push(manifest)),
            status_word(status),
            archive_button(&manifest.chat_id, status),
            event_log(&manifest.chat_id, events),
            first_prompt_hint(status),
            text(&chat_prompt_path(&manifest.chat_id)),
        ),
    )
}

/// The whole event log: everything settled, and an empty region at the end
/// polling on from where it stops.
///
/// It is rendered from `events.jsonl` and never from the connection, so it
/// reads the same whether the chat is live or was live last week (ADR-0006).
/// The section asks for itself again slowly, which is what settles a turn the
/// tail has been carrying and starts the tail over after it.
#[must_use]
pub fn event_log(chat_id: &str, events: &[Event]) -> String {
    let lines: String = blocks(events).iter().map(line).collect();
    format!(
        "<section id=\"log\" hx-get=\"{}\" hx-trigger=\"every {RESYNC_SECONDS}s\" \
         hx-swap=\"outerHTML\"><div id=\"log-history\">{lines}</div>{}</section>",
        text(&chat_events_path(chat_id)),
        hot_log(chat_id, events, events.len()),
    )
}

/// The log from an event on, polling from that same event again.
///
/// Every poll re-sends everything since `from`, so what the region carries
/// grows with the turn until whoever renders it moves the cursor up.
#[must_use]
pub fn hot_log(chat_id: &str, events: &[Event], from: usize) -> String {
    let lines: String = blocks(events.get(from..).unwrap_or(&[]))
        .iter()
        .map(line)
        .collect();
    format!(
        "<div id=\"log-hot\" hx-get=\"{}?from={from}\" hx-trigger=\"every {POLL_SECONDS}s\" \
         hx-swap=\"outerHTML\">{lines}</div>",
        text(&chat_events_path(chat_id)),
    )
}

/// The gate that pushes a chat's work and gives its workspace back, offered
/// wherever a workspace is still held — a parked chat has given up only its
/// container (ADR-0002 rule 2). A success re-renders the page rather than
/// swapping a fragment: archiving changes the whole of what the chat is.
fn archive_button(chat_id: &str, status: RuntimeStatus) -> String {
    if status == RuntimeStatus::Archived {
        return String::new();
    }
    format!(
        "<form hx-post=\"{}\" hx-swap=\"none\"><p><button type=\"submit\">Archive</button>\
         </p></form>",
        text(&chat_archive_path(chat_id)),
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

/// The log as it is read rather than as it arrived: a run of chunks is one
/// message, and a tool call is one line however many updates it took.
fn blocks(events: &[Event]) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    for event in events.iter().map(|event| &event.event) {
        match entry(event) {
            Entry::Bookkeeping => {}
            Entry::Turn(said) => blocks.push(Block::Turn(said)),
            Entry::Chunk(voice, said) => join(&mut blocks, voice, said),
            Entry::Notice(said) => blocks.push(Block::Notice(said.to_owned())),
            Entry::Aside(said) => blocks.push(Block::Aside(said.to_owned())),
            Entry::Tool(call) => amend(&mut blocks, call),
        }
    }
    blocks
}

/// A chunk continues the run it belongs to, seam and all: the adapter splits
/// its text mid-word, so a boundary is not a space and not a break.
fn join(blocks: &mut Vec<Block>, voice: Voice, said: String) {
    match blocks.last_mut() {
        Some(Block::Run(run, text)) if *run == voice => text.push_str(&said),
        _ => blocks.push(Block::Run(voice, said)),
    }
}

/// A tool call keeps the line it first took, wherever the log has since gone;
/// an update replaces what it says rather than adding a line of its own.
fn amend(blocks: &mut Vec<Block>, call: ToolCall) {
    for block in blocks.iter_mut().rev() {
        if let Block::Tool(shown) = block
            && shown.is_amended_by(&call)
        {
            shown.amend(call);
            return;
        }
    }
    blocks.push(Block::Tool(call));
}

/// One block of the log as it will be read.
enum Block {
    Turn(String),
    Run(Voice, String),
    Notice(String),
    Aside(String),
    Tool(ToolCall),
}

/// Whose words a run carries. A message and a thought read the same but are
/// separate utterances, so a run of one never swallows the other.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Voice {
    User,
    Agent,
    Thought,
}

/// One tool call as the transcript shows it: what to call it and where it has
/// got to (ADR-0006).
struct ToolCall {
    id: Option<String>,
    title: Option<String>,
    status: Option<String>,
    result: Option<String>,
}

impl ToolCall {
    /// Whether a later event is another word on this same call. A call the
    /// adapter left unidentified can be nobody else's line.
    fn is_amended_by(&self, later: &Self) -> bool {
        self.id.is_some() && self.id == later.id
    }

    /// Fold a later word on the same call in: an update carries only what
    /// changed, so a field it leaves out is not a field it cleared.
    fn amend(&mut self, later: Self) {
        self.title = later.title.or_else(|| self.title.take());
        self.status = later.status.or_else(|| self.status.take());
        self.result = later.result.or_else(|| self.result.take());
    }

    /// What the operator can recognise the call by.
    fn name(&self) -> &str {
        self.title
            .as_deref()
            .or(self.id.as_deref())
            .unwrap_or(UNNAMED_TOOL)
    }
}

/// How a log line reads once its shape is known.
enum Entry<'a> {
    Turn(String),
    Chunk(Voice, String),
    Notice(&'a str),
    Aside(&'a str),
    Tool(ToolCall),
    Bookkeeping,
}

/// The store has already refused anything unreadable, so a shape this build
/// does not know is a newer ACP, not damage: it names itself as an aside
/// rather than disappearing. A shape this build knows carries nothing to read
/// — bookkeeping, or a chunk of pictures rather than words — is the exception.
fn entry(event: &Value) -> Entry<'_> {
    if let Some(said) = event.get("prompt").and_then(blocks_text) {
        return Entry::Turn(said);
    }
    if let Some(kind) = field(event, CORE_LINE) {
        return Entry::Notice(field(event, "text").unwrap_or(kind));
    }
    let kind = field(event, "sessionUpdate").unwrap_or(UNTYPED);
    if BOOKKEEPING.contains(&kind) {
        return Entry::Bookkeeping;
    }
    if let Some(voice) = voice(kind) {
        return event
            .get("content")
            .and_then(blocks_text)
            .map_or(Entry::Bookkeeping, |said| Entry::Chunk(voice, said));
    }
    match kind {
        "tool_call" | "tool_call_update" => Entry::Tool(ToolCall {
            id: owned(event, "toolCallId"),
            title: owned(event, "title"),
            status: owned(event, "status"),
            result: tool_result_text(event),
        }),
        _ => Entry::Aside(kind),
    }
}

/// Whose words a chunk of this kind carries, if it is a chunk at all.
fn voice(kind: &str) -> Option<Voice> {
    match kind {
        "user_message_chunk" => Some(Voice::User),
        "agent_message_chunk" => Some(Voice::Agent),
        "agent_thought_chunk" => Some(Voice::Thought),
        _ => None,
    }
}

/// One block of the log as HTML. The agent's message is what the transcript
/// is read for; every other line stands back from it (ADR-0008).
fn line(block: &Block) -> String {
    match block {
        Block::Turn(said) | Block::Run(Voice::User, said) => {
            dimmed("p", &format!("<b>you:</b> {}", said_html(said)))
        }
        Block::Run(Voice::Agent, said) => format!("<p>{}</p>", said_html(said)),
        Block::Run(Voice::Thought, said) => dimmed("p", &said_html(said)),
        Block::Notice(said) => dimmed("blockquote", &said_html(said)),
        Block::Aside(said) => dimmed("p", &format!("<small>{}</small>", text(said))),
        Block::Tool(call) => format!(
            "{}{}",
            dimmed("p", &format!("<small>{}</small>", tool_line(call))),
            call.result.as_deref().map(printed_html).unwrap_or_default(),
        ),
    }
}

/// One line set back from the agent's message, in the element it reads as.
/// What the class costs is the stylesheet's to say (ADR-0008 §3).
fn dimmed(tag: &str, inner: &str) -> String {
    format!("<{tag} class=\"dim\">{inner}</{tag}>")
}

/// A tool call in one line: what it is, and where it last got to.
fn tool_line(call: &ToolCall) -> String {
    let status = call
        .status
        .as_deref()
        .map(|status| format!(" · {}", text(status)))
        .unwrap_or_default();
    format!("{}{status}", text(call.name()))
}

/// What a tool printed, kept as it was printed: the element holds the line
/// breaks and the columns, and the stylesheet keeps a wide line in its own box.
fn printed_html(printed: &str) -> String {
    dimmed("pre", &text(printed).to_string())
}

/// Said words as HTML: escaped, keeping the line breaks the speaker meant.
fn said_html(said: &str) -> String {
    text(said).to_string().replace('\n', "<br>")
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

/// What a tool call printed, out of the wrapper the adapter puts it in: a
/// result block holds its text one content deeper than a message chunk does,
/// and blocks of anything else (diffs, terminals) carry nothing to read here.
fn tool_result_text(event: &Value) -> Option<String> {
    let printed: String = event
        .get("content")?
        .as_array()?
        .iter()
        .filter_map(|block| block.get("content").and_then(block_text))
        .collect();
    let printed = unfenced(&printed);
    (!printed.is_empty()).then(|| printed.to_owned())
}

/// Output with the one code fence around it taken off, so the transcript reads
/// what the tool printed rather than how it was marked up.
fn unfenced(printed: &str) -> &str {
    let Some((opening, body)) = printed.split_once('\n') else {
        return printed;
    };
    if !opening.starts_with(FENCE) {
        return printed;
    }
    body.trim_end()
        .strip_suffix(FENCE)
        .map_or(printed, |inner| inner.strip_suffix('\n').unwrap_or(inner))
}

fn block_text(block: &Value) -> Option<&str> {
    (field(block, "type") == Some("text")).then(|| field(block, "text"))?
}

fn field<'a>(event: &'a Value, name: &str) -> Option<&'a str> {
    event.get(name).and_then(Value::as_str)
}

fn owned(event: &Value, name: &str) -> Option<String> {
    field(event, name).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::{Value, json};

    use crate::store::{ChatState, MANIFEST_SCHEMA};

    use super::*;

    /// The chat every test in this module renders.
    const CHAT_ID: &str = "01K1TESTCHATID0000000000";

    fn manifest(status: RuntimeStatus) -> Manifest {
        let now = Utc::now();
        Manifest {
            schema: MANIFEST_SCHEMA,
            chat_id: CHAT_ID.to_owned(),
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
            env: std::collections::BTreeMap::new(),
            startup_script: None,
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

    /// A tool call as the adapter first announces it (ADR-0006).
    fn tool_call(id: &str, title: &str) -> Value {
        json!({
            "sessionUpdate": "tool_call",
            "toolCallId": id,
            "title": title,
            "kind": "execute",
            "status": "pending",
        })
    }

    /// A later word on the same tool call, carrying only what changed.
    fn tool_update(id: &str, status: &str) -> Value {
        json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": status})
    }

    /// The word on a tool call that carries what it printed: the text sits in
    /// a content block of its own, fenced for a markdown reader (ADR-0006).
    fn tool_result(id: &str, printed: &str) -> Value {
        json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": id,
            "status": "completed",
            "content": [{"type": "content", "content": {"type": "text", "text": printed}}],
        })
    }

    /// An update the agent keeps its client's accounting with.
    fn usage_update() -> Value {
        json!({"sessionUpdate": "usage_update", "usage": {"inputTokens": 12}})
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

    /// The adapter splits a sentence mid-word ("I" + "'m at"), so a chunk
    /// boundary must contribute nothing at all — no space, no break.
    #[test]
    fn a_run_of_agent_chunks_reads_as_one_message() {
        let events = log(&[
            chunk("agent_message_chunk", "I"),
            chunk("agent_message_chunk", "'m at"),
            chunk("agent_message_chunk", " the ladder"),
        ]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains("<p>I&#39;m at the ladder</p>"),
            "the chunks did not join into one message: {rendered}"
        );
        assert_eq!(
            rendered.matches("<p>").count(),
            1,
            "the message is still broken into blocks: {rendered}"
        );
    }

    /// A chunk can carry no words at all, and one that carries none says
    /// nothing — least of all its own name.
    #[test]
    fn a_wordless_chunk_neither_prints_nor_breaks_the_run() {
        for wordless in [
            chunk("agent_message_chunk", ""),
            json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png"},
            }),
        ] {
            let events = log(&[
                chunk("agent_message_chunk", "I"),
                wordless.clone(),
                chunk("agent_message_chunk", "'m at it"),
            ]);

            let rendered = event_log(CHAT_ID, &events);

            assert!(
                rendered.contains("<p>I&#39;m at it</p>"),
                "{wordless} broke the sentence in two: {rendered}"
            );
            assert!(
                !rendered.contains("agent_message_chunk"),
                "{wordless} was read out as its own name: {rendered}"
            );
        }
    }

    #[test]
    fn bookkeeping_between_chunks_does_not_break_the_run() {
        let events = log(&[
            chunk("agent_message_chunk", "I"),
            usage_update(),
            chunk("agent_message_chunk", "'m at it"),
        ]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains("<p>I&#39;m at it</p>"),
            "an update nobody sees still broke the sentence: {rendered}"
        );
    }

    #[test]
    fn the_user_and_the_agent_never_share_a_run() {
        let events = log(&[
            chunk("user_message_chunk", "ship it"),
            chunk("agent_message_chunk", "on it"),
        ]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains("<p class=\"dim\"><b>you:</b> ship it</p>")
                && rendered.contains("<p>on it</p>"),
            "the two voices ran into one: {rendered}"
        );
    }

    #[test]
    fn a_thought_and_a_message_stay_two_blocks() {
        let events = log(&[
            chunk("agent_thought_chunk", "weighing it up"),
            chunk("agent_message_chunk", "on it"),
        ]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains("<p class=\"dim\">weighing it up</p>")
                && rendered.contains("<p>on it</p>"),
            "a thought swallowed the message after it: {rendered}"
        );
    }

    #[test]
    fn a_newline_the_agent_wrote_still_breaks_the_line() {
        let events = log(&[chunk("agent_message_chunk", "first\nsecond")]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains("first<br>second"),
            "the agent's own line break was swallowed: {rendered}"
        );
    }

    #[test]
    fn an_event_between_chunks_ends_the_message_it_interrupts() {
        let events = log(&[
            chunk("agent_message_chunk", "before"),
            tool_call("call_1", "git commit"),
            chunk("agent_message_chunk", "after"),
        ]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains("<p>before</p>") && rendered.contains("<p>after</p>"),
            "the run did not end where the tool call broke it: {rendered}"
        );
    }

    #[test]
    fn two_turns_of_the_user_stay_two_blocks() {
        let events = log(&[
            outbound_prompt("ship the ladder"),
            outbound_prompt("now push"),
        ]);

        let rendered = event_log(CHAT_ID, &events);

        assert_eq!(
            rendered.matches("<b>you:</b>").count(),
            2,
            "two turns ran into one: {rendered}"
        );
    }

    #[test]
    fn updates_to_one_tool_call_coalesce_into_its_line() {
        let events = log(&[
            tool_call("call_1", "git commit"),
            tool_update("call_1", "in_progress"),
            tool_update("call_1", "completed"),
        ]);

        let rendered = event_log(CHAT_ID, &events);

        assert_eq!(
            rendered.matches("git commit").count(),
            1,
            "the tool call took a line per update: {rendered}"
        );
        assert!(
            rendered.contains("<small>git commit · completed</small>"),
            "the tool call's line does not carry its last status: {rendered}"
        );
        assert!(
            !rendered.contains("pending") && !rendered.contains("in_progress"),
            "a superseded status is still on the page: {rendered}"
        );
    }

    #[test]
    fn two_tool_calls_keep_a_line_each() {
        let events = log(&[
            tool_call("call_1", "git commit"),
            tool_call("call_2", "cargo test"),
            tool_update("call_1", "completed"),
        ]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains("<small>git commit · completed</small>")
                && rendered.contains("<small>cargo test · pending</small>"),
            "the two calls did not keep their own lines: {rendered}"
        );
    }

    #[test]
    fn what_a_tool_printed_comes_out_of_the_fence_the_adapter_wrapped_it_in() {
        let printed = tool_result_text(&tool_result(
            "call_1",
            "```console\n1 file changed, 2 insertions(+)\n```",
        ));

        assert_eq!(printed.as_deref(), Some("1 file changed, 2 insertions(+)"));
    }

    #[test]
    fn a_tool_call_with_nothing_to_read_has_no_result() {
        for content in [
            json!([]),
            json!([{"type": "diff", "path": "src/ui/mod.rs"}]),
        ] {
            let event = json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_1",
                "status": "completed",
                "content": content,
            });

            assert!(
                tool_result_text(&event).is_none(),
                "a call that printed nothing carries a result: {event}"
            );
        }
    }

    #[test]
    fn what_a_tool_printed_lands_under_the_line_it_belongs_to() {
        let events = log(&[
            tool_call("call_1", "git commit"),
            tool_result("call_1", "```console\nnothing to commit\n```"),
        ]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains(
                "<p class=\"dim\"><small>git commit · completed</small></p>\
                 <pre class=\"dim\">nothing to commit</pre>"
            ),
            "what the tool printed is not under its own line: {rendered}"
        );
        assert_eq!(
            rendered.matches("git commit").count(),
            1,
            "the result took a line of its own: {rendered}"
        );
    }

    /// The transcript is the record of the run: a long result is worth reading
    /// to the end, and the element it sits in is what holds its lines apart.
    #[test]
    fn what_a_tool_printed_reaches_the_page_whole_and_broken_where_it_broke() {
        let printed = (0..200u8)
            .map(|line| {
                format!(
                    "line {}{}",
                    char::from(b'a' + line / 26),
                    char::from(b'a' + line % 26)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let events = log(&[
            tool_call("call_1", "git diff"),
            tool_result("call_1", &format!("```console\n{printed}\n```")),
        ]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains(&format!("<pre class=\"dim\">{printed}</pre>")),
            "what the tool printed did not reach the page whole ({} chars rendered)",
            rendered.len()
        );
        assert!(
            !rendered.contains("<br>"),
            "the line breaks were rewritten as markup: {rendered}"
        );
    }

    /// A tool prints whatever it was pointed at, so its output is the least
    /// trusted string on the page.
    #[test]
    fn what_a_tool_printed_cannot_smuggle_markup_into_the_log() {
        let events = log(&[
            tool_call("call_1", "cat x"),
            tool_result("call_1", "```console\n<script>alert(1)</script>\n```"),
        ]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            !rendered.contains("<script>"),
            "a tool's output let markup through: {rendered}"
        );
    }

    #[test]
    fn a_link_in_tool_output_reads_as_a_link() {
        assert_eq!(
            colorize("see https://corvous.dev/x?a=1 now"),
            "see <span class=\"tok-url\">https://corvous.dev/x?a=1</span> now"
        );
    }

    #[test]
    fn a_path_is_one_token_whether_or_not_it_names_a_line() {
        assert_eq!(
            colorize("src/ui/chat.rs"),
            "<span class=\"tok-path\">src/ui/chat.rs</span>"
        );
        assert_eq!(
            colorize("src/ui/chat.rs:47"),
            "<span class=\"tok-path\">src/ui/chat.rs:47</span>"
        );
    }

    #[test]
    fn a_count_reads_as_a_count() {
        assert_eq!(
            colorize("47 files, 1.5 s"),
            "<span class=\"tok-num\">47</span> files, <span class=\"tok-num\">1.5</span> s"
        );
    }

    #[test]
    fn a_diff_mark_reads_as_what_it_adds_or_takes_away() {
        let coloured = colorize("chat.rs | 6 ++++--\n+ kept\n- gone");

        for marked in [
            "<span class=\"tok-add\">++++</span>",
            "<span class=\"tok-del\">--</span>",
            "<span class=\"tok-add\">+</span> kept",
            "<span class=\"tok-del\">-</span> gone",
        ] {
            assert!(
                coloured.contains(marked),
                "{marked} is not marked as a change: {coloured}"
            );
        }
    }

    #[test]
    fn a_tick_and_a_cross_read_as_pass_and_fail() {
        assert_eq!(
            colorize("✓ ✗"),
            "<span class=\"tok-ok\">✓</span> <span class=\"tok-err\">✗</span>"
        );
    }

    /// Colouring runs over text that is already escaped, so an escaped
    /// character has to come through it exactly as it went in — a span opened
    /// inside one would put the raw character back on the page.
    #[test]
    fn colouring_never_breaks_an_escaped_character() {
        let escaped = text("<script>alert(1)</script> it's 39").to_string();

        let coloured = colorize(&escaped);

        assert!(
            !coloured.contains("<script>"),
            "colouring let markup back through: {coloured}"
        );
        for entity in ["&lt;script&gt;", "&lt;/script&gt;", "&#39;"] {
            assert!(
                coloured.contains(entity),
                "{entity} was broken open: {coloured}"
            );
        }
    }

    /// A token is claimed once: what a path is made of is not read again as a
    /// number, or the path comes apart into pieces on the page.
    #[test]
    fn a_path_with_digits_in_it_is_one_token_and_not_several() {
        for path in ["src/v2/a.rs:47", "2026-08-09/report.md:12"] {
            let coloured = colorize(path);

            assert_eq!(
                coloured.matches("<span").count(),
                1,
                "the path came apart: {coloured}"
            );
            assert!(
                !coloured.contains("tok-num"),
                "a number was read inside the path: {coloured}"
            );
        }
    }

    #[test]
    fn what_a_tool_printed_carries_its_tokens_inside_the_dimmed_block() {
        let events = log(&[
            tool_call("call_1", "git status"),
            tool_result("call_1", "```console\nsrc/ui/chat.rs\n```"),
        ]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains(
                "<pre class=\"dim\"><span class=\"tok-path\">src/ui/chat.rs</span></pre>"
            ),
            "the tokens are not inside the dimmed block: {rendered}"
        );
    }

    /// A token class only colours anything if the one stylesheet says what it
    /// is worth, and says it twice: once for each scheme (ADR-0008 §3).
    #[test]
    fn what_colours_a_token_stands_in_the_stylesheet() {
        for rule in [
            "--tok-num:#b8791b;",
            "--tok-path:#2f6bd8;",
            "--tok-url:#1c7a86;",
            "--tok-add:#1a7f37;",
            "--tok-del:#cf222e;",
            ".tok-num{color:var(--tok-num);}",
            ".tok-path{color:var(--tok-path);}",
            ".tok-url{color:var(--tok-url);}",
            ".tok-add,.tok-ok{color:var(--tok-add);}",
            ".tok-del,.tok-err{color:var(--tok-del);}",
        ] {
            assert!(
                crate::ui::CSS.contains(rule),
                "the stylesheet is missing {rule}: {}",
                crate::ui::CSS
            );
        }
        assert!(
            position(crate::ui::CSS, "@media(prefers-color-scheme:dark)")
                < position(crate::ui::CSS, "--tok-num:#f5a742;"),
            "the dark palette is not behind the scheme it is for: {}",
            crate::ui::CSS
        );
    }

    #[test]
    fn a_later_word_on_a_call_does_not_wipe_what_it_printed() {
        let events = log(&[
            tool_call("call_1", "git commit"),
            tool_result("call_1", "```console\nnothing to commit\n```"),
            tool_update("call_1", "failed"),
        ]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains("<pre class=\"dim\">nothing to commit</pre>"),
            "a later update carrying no output cleared the output: {rendered}"
        );
    }

    #[test]
    fn a_tool_call_that_printed_nothing_stays_one_line() {
        let events = log(&[tool_call("call_1", "git commit")]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            !rendered.contains("<pre"),
            "a call with nothing to show opened a block anyway: {rendered}"
        );
    }

    #[test]
    fn bookkeeping_updates_are_left_out_of_the_transcript() {
        let events = log(&[
            usage_update(),
            json!({"sessionUpdate": "available_commands_update", "availableCommands": []}),
        ]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            !rendered.contains("usage") && !rendered.contains("available_commands"),
            "bookkeeping is being read out as if it said something: {rendered}"
        );
    }

    #[test]
    fn the_title_the_adapter_settles_on_mid_turn_neither_speaks_nor_breaks_the_message() {
        let events = log(&[
            chunk("agent_message_chunk", "ship "),
            json!({
                "sessionUpdate": "session_info_update",
                "title": "Ship the ladder",
                "updatedAt": "2026-08-07T09:41:00.000Z",
            }),
            chunk("agent_message_chunk", "the ladder"),
        ]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            !rendered.contains("session_info_update") && !rendered.contains("Ship the ladder"),
            "the session title is being read out as if it said something: {rendered}"
        );
        assert!(
            rendered.contains("ship the ladder"),
            "the title broke the message in two: {rendered}"
        );
    }

    #[test]
    fn chunks_that_join_into_a_tag_are_still_escaped() {
        let events = log(&[
            chunk("agent_message_chunk", "<scr"),
            chunk("agent_message_chunk", "ipt>alert(1)</script>"),
        ]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            !rendered.contains("<script>"),
            "joining the chunks let markup through: {rendered}"
        );
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
        let events = log(&[tool_call("call_1", "git commit")]);

        let rendered = chat_page(&manifest(RuntimeStatus::Live), RuntimeStatus::Live, &events);

        assert!(
            rendered.contains("<small>git commit · pending</small>"),
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
            rendered.contains("<blockquote class=\"dim\">Agent memory was reset"),
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

    /// The transcript is read for what the agent said; the user's own words
    /// back, its thoughts, notices, asides and tool calls are around it rather
    /// than in it, so they sit back and let the message carry the eye.
    #[test]
    fn every_line_but_the_agents_message_reads_dimmed() {
        let events = log(&[
            outbound_prompt("ship it"),
            chunk("agent_thought_chunk", "the ladder first"),
            tool_call("call_1", "git commit"),
            tool_result("call_1", "```console\nnothing to commit\n```"),
            json!({"sessionUpdate": "plan", "entries": []}),
            json!({"corcode": "reset_notice", "text": "Agent memory was reset."}),
            chunk("agent_message_chunk", "on it"),
        ]);

        let rendered = event_log(CHAT_ID, &events);

        for dimmed in [
            "<p class=\"dim\"><b>you:</b> ship it</p>",
            "<p class=\"dim\">the ladder first</p>",
            "<p class=\"dim\"><small>git commit · completed</small></p>",
            "<pre class=\"dim\">nothing to commit</pre>",
            "<p class=\"dim\"><small>plan</small></p>",
            "<blockquote class=\"dim\">Agent memory was reset.</blockquote>",
        ] {
            assert!(
                rendered.contains(dimmed),
                "{dimmed} does not read dimmed: {rendered}"
            );
        }
        assert!(
            rendered.contains("<p>on it</p>"),
            "the agent's own message reads dimmed too: {rendered}"
        );
    }

    /// The class the dimmed lines carry only dims them if the one stylesheet
    /// says by how much (ADR-0008 §3).
    #[test]
    fn what_dimming_is_stands_in_the_stylesheet() {
        assert!(
            crate::ui::CSS.contains(".dim{opacity:0.6;}"),
            "nothing in the stylesheet dims a dimmed line: {}",
            crate::ui::CSS
        );
    }

    /// A tool prints lines that can run wide; the one stylesheet is what keeps
    /// them inside their own box rather than the page's (ADR-0008 §3).
    #[test]
    fn what_holds_a_wide_result_in_its_box_stands_in_the_stylesheet() {
        assert!(
            crate::ui::CSS.contains("pre{overflow-x:auto;}"),
            "nothing in the stylesheet holds a wide result: {}",
            crate::ui::CSS
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

    /// Parked is the state the gate exists for: the workspace is still on
    /// disk holding work nothing has pushed (ADR-0002 rule 1).
    #[test]
    fn a_chat_that_still_holds_a_workspace_offers_to_archive_itself() {
        for status in [RuntimeStatus::Live, RuntimeStatus::Parked] {
            let manifest = manifest(status);

            let rendered = chat_page(&manifest, status, &[]);

            assert!(
                rendered.contains(&format!(
                    "hx-post=\"{}\"",
                    chat_archive_path(&manifest.chat_id)
                )),
                "a {} chat cannot be archived from its own page: {rendered}",
                status_word(status)
            );
            assert!(rendered.contains("Archive</button>"));
        }
    }

    #[test]
    fn an_archived_chat_is_offered_no_archive_button() {
        let rendered = chat_page(
            &manifest(RuntimeStatus::Archived),
            RuntimeStatus::Archived,
            &[],
        );

        assert!(
            !rendered.contains("Archive</button>"),
            "an archived chat was offered the gate again: {rendered}"
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
    fn a_sent_prompt_clears_the_box_it_was_typed_in() {
        let rendered = chat_page(&manifest(RuntimeStatus::Live), RuntimeStatus::Live, &[]);

        assert!(
            rendered.contains("hx-on::after-request=\"if(event.detail.successful)this.reset()\""),
            "the prompt box does not clear on a successful send: {rendered}"
        );
    }

    #[test]
    fn the_log_asks_for_itself_again_while_the_page_is_open() {
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
            )) && fragment.contains("hx-trigger=\"every 30s\""),
            "the log does not ask for itself again: {fragment}"
        );
        assert!(fragment.contains("<p>on it</p>"));
    }

    /// What the page is sent whole: everything settled, once, with an empty
    /// region at the end that polls on from where the settled log stops. The
    /// slow trigger on the section is what heals the two of them apart.
    #[test]
    fn the_full_log_is_settled_history_with_an_empty_tail_polling_after_it() {
        let events = log(&[
            outbound_prompt("ship it"),
            chunk("agent_message_chunk", "on it"),
        ]);

        let rendered = event_log(CHAT_ID, &events);

        assert_eq!(
            rendered,
            format!(
                "<section id=\"log\" hx-get=\"{}\" hx-trigger=\"every 30s\" \
                 hx-swap=\"outerHTML\"><div id=\"log-history\">\
                 <p class=\"dim\"><b>you:</b> ship it</p><p>on it</p></div>{}</section>",
                chat_events_path(CHAT_ID),
                hot_log(CHAT_ID, &events, events.len())
            )
        );
    }

    /// What a live chat asks for between resyncs is only what it has not seen
    /// yet, so a long transcript is not re-sent every couple of seconds.
    #[test]
    fn the_hot_log_is_the_tail_from_an_index_on_and_polls_for_the_next() {
        let events = log(&[
            outbound_prompt("ship it"),
            chunk("agent_message_chunk", "on it"),
        ]);

        let fragment = hot_log(CHAT_ID, &events, 1);

        assert!(
            fragment.contains("<p>on it</p>") && !fragment.contains("ship it"),
            "the hot region is not the tail alone: {fragment}"
        );
        assert!(
            fragment.contains("id=\"log-hot\"")
                && fragment.contains(&format!("hx-get=\"{}?from=1\"", chat_events_path(CHAT_ID)))
                && fragment.contains("hx-trigger=\"every 2s\""),
            "the hot region does not poll on for what comes next: {fragment}"
        );
    }

    /// Where `from` counts from: the events on disk, not the blocks they read
    /// as. A run split across two chunks is one block and two events, so a
    /// cursor of two is the second half of the sentence and nothing else.
    #[test]
    fn the_hot_log_counts_from_in_events_rather_than_in_blocks() {
        let events = log(&[
            outbound_prompt("ship it"),
            chunk("agent_message_chunk", "on "),
            chunk("agent_message_chunk", "it"),
        ]);

        let fragment = hot_log(CHAT_ID, &events, 2);

        assert!(
            fragment.contains("<p>it</p>"),
            "the cursor counted blocks rather than events: {fragment}"
        );
    }

    /// A page that has already read the whole log asks from past its end, and
    /// gets an empty region that keeps polling from there.
    #[test]
    fn a_hot_log_asked_from_past_the_end_is_empty_rather_than_a_panic() {
        let events = log(&[chunk("agent_message_chunk", "on it")]);

        assert_eq!(
            hot_log(CHAT_ID, &events, 9),
            format!(
                "<div id=\"log-hot\" hx-get=\"{}?from=9\" hx-trigger=\"every 2s\" \
                 hx-swap=\"outerHTML\"></div>",
                chat_events_path(CHAT_ID)
            )
        );
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
