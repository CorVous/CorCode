//! The chat view: `events.jsonl` rendered as an event log (ADR-0006).
//!
//! The log is read rather than replayed: a run of chunks is one message, a
//! tool call is one line however many updates it took, and bookkeeping is left
//! out. One rendering serves both the page and the fragment htmx polls, so a
//! chat reads the same on load as it does while it streams.

use std::fmt::Write as _;
use std::sync::OnceLock;

use serde_json::Value;
use syntect::html::{ClassStyle, ClassedHTMLGenerator};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

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

/// How much code is read for what it says before it is simply shown. Reading
/// costs time on every poll a chat is open for, and a block this long is a
/// dump rather than something anyone reads on a screen.
const MOST_CODE_READ: usize = 100 * 1024;

/// How a code fence is spelled. An adapter wraps a tool's output in one for a
/// markdown reader, and an agent fences the code it writes; the transcript is
/// not markdown, so a fence is acted on rather than read out as itself. A
/// longer run of backticks is not read as a fence of its own.
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
        Block::Turn(said) | Block::Run(Voice::User, said) => said_html(said, &USER),
        Block::Run(Voice::Agent, said) => said_html(said, &AGENT),
        Block::Run(Voice::Thought, said) => said_html(said, &THOUGHT),
        Block::Notice(said) => said_html(said, &NOTICE),
        Block::Aside(said) => dimmed("p", &format!("<small>{}</small>", text(said))),
        Block::Tool(call) => format!(
            "{}{}",
            dimmed("p", &format!("<small>{}</small>", tool_line(call))),
            call.result.as_deref().map(printed_html).unwrap_or_default(),
        ),
    }
}

/// How a voice reads on the page: the element its prose sits in, whether it
/// stands back from the agent's message, and what names it as it opens.
struct Reading {
    tag: &'static str,
    set_back: bool,
    label: &'static str,
}

const AGENT: Reading = Reading {
    tag: "p",
    set_back: false,
    label: "",
};

const USER: Reading = Reading {
    tag: "p",
    set_back: true,
    label: "<b>you:</b> ",
};

const THOUGHT: Reading = Reading {
    tag: "p",
    set_back: true,
    label: "",
};

const NOTICE: Reading = Reading {
    tag: "blockquote",
    set_back: true,
    label: "",
};

impl Reading {
    /// One passage of prose in the element this voice reads in.
    fn paragraph(&self, said: &str) -> String {
        if self.set_back {
            dimmed(self.tag, said)
        } else {
            format!("<{tag}>{said}</{tag}>", tag = self.tag)
        }
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
    dimmed("pre", &colorize(&text(printed).to_string()))
}

/// Tool output with the tokens an operator scans for named for the stylesheet:
/// links, paths, counts and the marks of a change (ADR-0008 §3).
///
/// The text arrives escaped, so every escaped character is stepped over whole
/// — a span opened inside one would put the raw character back on the page.
/// Each token is claimed where it starts and the scan resumes after it, so
/// nothing inside a claimed token is read again as a token of its own.
fn colorize(escaped: &str) -> String {
    let mut colored = String::with_capacity(escaped.len());
    let mut rest = escaped;
    let mut previous = None;
    while !rest.is_empty() {
        let (taken, class) = piece(rest, previous);
        match class {
            Some(class) => write!(colored, "<span class=\"tok-{class}\">{taken}</span>")
                .expect("a String cannot fail to be written to"),
            None => colored.push_str(taken),
        }
        rest = &rest[taken.len()..];
        previous = taken.chars().next_back();
    }
    colored
}

/// The next piece of the text, and the token class it reads as if it is a
/// token at all. An escaped character is a piece of its own, ahead of every
/// token, which is what keeps a span from opening inside one.
fn piece(rest: &str, previous: Option<char>) -> (&str, Option<&'static str>) {
    if let Some(entity) = entity(rest) {
        return (entity, None);
    }
    if let Some((token, class)) = token(rest, previous) {
        return (token, Some(class));
    }
    let plain = rest.chars().next().expect("the rest is not empty");
    (&rest[..plain.len_utf8()], None)
}

/// The escaped character starting here, whole, if one starts here at all.
fn entity(rest: &str) -> Option<&str> {
    let inside = rest.strip_prefix('&')?;
    let end = inside.find(';')?;
    let named = &inside[..end];
    let escaped = !named.is_empty()
        && named
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '#');
    escaped.then(|| &rest[..end + 2])
}

/// The token starting here, highest priority first: what a link is made of
/// would otherwise read as a path, and what a path is made of as a count.
fn token(rest: &str, previous: Option<char>) -> Option<(&str, &'static str)> {
    link(rest)
        .or_else(|| path(rest))
        .or_else(|| count(rest, previous))
        .or_else(|| change(rest, previous))
        .or_else(|| mark(rest))
}

/// A link, to wherever the run of it stops. Nothing in escaped text ends a
/// URL but a space, since the markup characters are no longer in it.
fn link(rest: &str) -> Option<(&str, &'static str)> {
    if !rest.starts_with("https://") && !rest.starts_with("http://") {
        return None;
    }
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some((&rest[..end], "url"))
}

/// A path: a run with a directory in it, ending in a name with a suffix, and
/// the line number some tools hang off it.
fn path(rest: &str) -> Option<(&str, &'static str)> {
    let run = &rest[..rest
        .find(|character| !is_path(character))
        .unwrap_or(rest.len())];
    let (_, name) = run.rsplit_once('/')?;
    let (stem, suffix) = name.rsplit_once('.')?;
    if stem.is_empty() || suffix.is_empty() || !suffix.chars().all(char::is_alphanumeric) {
        return None;
    }
    Some((
        &rest[..run.len() + numbered_line(&rest[run.len()..])],
        "path",
    ))
}

/// How much of what follows a path is the line number hung off it.
fn numbered_line(rest: &str) -> usize {
    let Some(digits) = rest.strip_prefix(':') else {
        return 0;
    };
    let end = digits
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(digits.len());
    if end == 0 { 0 } else { end + 1 }
}

/// What a path is spelled with. A path is claimed from the first character of
/// the run it is in, so no boundary of its own is needed: any later start
/// inside that run ends at the same name and would read the same.
fn is_path(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '/' | '.' | '_' | '-')
}

/// A count, where one begins and to the last digit of it. The two ends are
/// not symmetric: a digit glued to the end of a word is part of the word
/// (`v2`, `sha1`), while a unit glued to the end of a count is not part of
/// the count, so `200ms` colours its `200`. The separators a number is
/// written with belong to it; the punctuation that follows one does not.
fn count(rest: &str, previous: Option<char>) -> Option<(&str, &'static str)> {
    if previous.is_some_and(char::is_alphanumeric)
        || !rest.starts_with(|first: char| first.is_ascii_digit())
    {
        return None;
    }
    let run = &rest[..rest
        .find(|character| !matches!(character, '0'..='9' | '.' | ','))
        .unwrap_or(rest.len())];
    let last = run
        .rfind(|character: char| character.is_ascii_digit())
        .expect("the run starts with a digit");
    Some((&run[..=last], "num"))
}

/// The mark a diff hangs off a line or a stat: a run of one sign, standing on
/// its own. A mark heads a line, or is a stat of more than one sign after a
/// space; either way the signs run out into whitespace, which is what tells a
/// mark from the `--lib` of a command line.
fn change(rest: &str, previous: Option<char>) -> Option<(&str, &'static str)> {
    let sign = rest
        .chars()
        .next()
        .filter(|first| matches!(first, '+' | '-'))?;
    let run = &rest[..rest.len() - rest.trim_start_matches(sign).len()];
    let signs = &rest[..rest.len() - rest.trim_start_matches(['+', '-']).len()];
    let runs_out = rest[signs.len()..]
        .chars()
        .next()
        .is_none_or(char::is_whitespace);
    let heads_a_line = matches!(previous, None | Some('\n'));
    let stands_apart = previous
        .is_some_and(|before| before.is_whitespace() || matches!(before, '+' | '-'))
        && run.len() > 1;
    (runs_out && (heads_a_line || stands_apart))
        .then_some((run, if sign == '+' { "add" } else { "del" }))
}

/// The glyph a tool signs a result with.
fn mark(rest: &str) -> Option<(&str, &'static str)> {
    let glyph = rest.chars().next()?;
    let class = match glyph {
        '✓' => "ok",
        '✗' => "err",
        _ => return None,
    };
    Some((&rest[..glyph.len_utf8()], class))
}

/// Said words as HTML: prose escaped with the line breaks the speaker meant,
/// and code the speaker fenced off kept as code in a block of its own beside
/// it — never inside it, which a paragraph would not hold (ADR-0008).
///
/// Whoever spoke is named as the message opens, on the first words of it;
/// a message of nothing but code still opens with who is speaking.
fn said_html(said: &str, reading: &Reading) -> String {
    let mut passages = passages(said);
    if !reading.label.is_empty() && !matches!(passages.first(), Some(Passage::Prose(_))) {
        passages.insert(0, Passage::Prose(Vec::new()));
    }
    let mut label = reading.label;
    let mut html = String::new();
    for passage in &passages {
        match passage {
            Passage::Prose(lines) => {
                html.push_str(&reading.paragraph(&format!("{label}{}", prose_html(lines))));
                label = "";
            }
            Passage::Code(fenced) => html.push_str(&fenced.html(reading.set_back)),
        }
    }
    html
}

/// A message read as the run of prose and code it is, in the order it was
/// said, and never empty: a message that says nothing is one empty passage.
///
/// The whole message is read at once rather than chunk by chunk, so a fence
/// the adapter split across two chunks is still a fence. A fence left open is
/// read to the end of the message: a turn is shown while it streams, so the
/// closing fence has usually not arrived yet.
fn passages(said: &str) -> Vec<Passage<'_>> {
    let mut passages = Vec::new();
    let mut prose: Vec<&str> = Vec::new();
    let mut fenced: Option<Fenced<'_>> = None;
    for line in said.split('\n') {
        if let Some(mut open) = fenced.take() {
            if line.trim_end() == FENCE {
                passages.push(Passage::Code(open));
            } else {
                open.lines.push(line);
                fenced = Some(open);
            }
            continue;
        }
        if let Some(said_of_it) = line.strip_prefix(FENCE) {
            push_prose(&mut passages, &mut prose);
            fenced = Some(Fenced {
                language: said_of_it.split_whitespace().next().unwrap_or_default(),
                lines: Vec::new(),
            });
            continue;
        }
        prose.push(line);
    }
    match fenced {
        Some(open) => passages.push(Passage::Code(open)),
        None if passages.is_empty() => passages.push(Passage::Prose(prose)),
        None => push_prose(&mut passages, &mut prose),
    }
    passages
}

/// One stretch of a message, read as what it is.
enum Passage<'a> {
    Prose(Vec<&'a str>),
    Code(Fenced<'a>),
}

/// Prose beside code stands as a passage only when it says something: the
/// blank line under a fence is the fence's punctuation, not a paragraph.
fn push_prose<'a>(passages: &mut Vec<Passage<'a>>, prose: &mut Vec<&'a str>) {
    let says_something = prose.iter().any(|line| !line.is_empty());
    let prose = std::mem::take(prose);
    if says_something {
        passages.push(Passage::Prose(prose));
    }
}

/// A run of lines the speaker fenced off as code, and the one word of what
/// they said of it that names what it is written in: the rest is said to a
/// markdown reader, and naming it here would put words of the agent's
/// choosing into the page's own classes.
struct Fenced<'a> {
    language: &'a str,
    lines: Vec<&'a str>,
}

impl Fenced<'_> {
    /// The block as HTML: read for what the code says where the language is
    /// one we know, escaped either way, and named for whoever colours it. The
    /// element holds the line breaks, so nothing is turned into markup here.
    fn html(&self, set_back: bool) -> String {
        let named = if self.language.is_empty() {
            String::new()
        } else {
            format!(" class=\"lang-{}\"", text(self.language))
        };
        let stands_back = if set_back { " dim" } else { "" };
        let code = self.lines.join("\n");
        let read = read_as(&code, self.language).unwrap_or_else(|| text(&code).to_string());
        format!("<pre class=\"code{stands_back}\"><code{named}>{read}</code></pre>")
    }
}

/// Code with each word of it named for what it is doing, or nothing at all if
/// no syntax here is written in that language, the block is longer than one
/// worth reading, or the reader gives up on it — the caller shows the code
/// plainly for any of the three. The reader escapes the code as it goes, so
/// what comes back is already safe to put on the page.
///
/// The classes carry a prefix of their own: a syntax names scopes as broadly
/// as `source` and `text`, and the page has classes of its own to keep clear
/// of (ADR-0008 §3).
fn read_as(code: &str, language: &str) -> Option<String> {
    if code.len() > MOST_CODE_READ {
        return None;
    }
    let syntaxes = syntaxes();
    let syntax = syntaxes.find_syntax_by_token(language)?;
    let mut reader = ClassedHTMLGenerator::new_with_class_style(
        syntax,
        syntaxes,
        ClassStyle::SpacedPrefixed { prefix: "hl-" },
    );
    for line in LinesWithEndings::from(code) {
        reader
            .parse_html_for_line_which_includes_newline(line)
            .ok()?;
    }
    Some(reader.finalize())
}

/// Every syntax the highlighter knows, unpacked once: the set is a dump of a
/// few megabytes, and a chat re-reads its whole log on every poll.
fn syntaxes() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// Lines of prose as HTML, keeping the line breaks the speaker meant.
fn prose_html(prose: &[&str]) -> String {
    prose
        .iter()
        .map(|line| text(line).to_string())
        .collect::<Vec<_>>()
        .join("<br>")
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

    /// Whether the page opens a block of code while prose is still open, which
    /// no browser will read as the nesting it looks like.
    fn nests_a_block_inside_prose(rendered: &str) -> bool {
        let mut open = false;
        for tag in rendered.match_indices('<').map(|(at, _)| &rendered[at..]) {
            if open && tag.starts_with("<pre") {
                return true;
            }
            if tag.starts_with("<p>") || tag.starts_with("<p ") || tag.starts_with("<blockquote") {
                open = true;
            } else if tag.starts_with("</p>") || tag.starts_with("</blockquote>") {
                open = false;
            }
        }
        false
    }

    /// What sits inside the one block of code on the page.
    fn code_body(rendered: &str) -> &str {
        let after = &rendered[position(rendered, "<code") + "<code".len()..];
        let inside = &after[position(after, ">") + 1..];
        &inside[..position(inside, "</code>")]
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

    /// Code the agent writes is read as code: kept as it was written, in its
    /// own block, and at the brightness of the message it belongs to.
    #[test]
    fn a_fenced_block_in_a_message_reads_as_code() {
        let events = log(&[chunk(
            "agent_message_chunk",
            "look:\nat this:\n```ladder\nlet x = 1;\nlet y = 2;\n```\nand done",
        )]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains(
                "<pre class=\"code\"><code class=\"lang-ladder\">let x = 1;\nlet y = 2;</code></pre>"
            ),
            "the fenced block did not read as code: {rendered}"
        );
        assert!(
            rendered.contains("look:<br>at this:") && rendered.contains("and done"),
            "the prose around the block did not read as prose: {rendered}"
        );
        assert!(
            !rendered.contains('`'),
            "the fence itself reached the page: {rendered}"
        );
    }

    #[test]
    fn a_fence_that_names_no_language_names_none() {
        let events = log(&[chunk("agent_message_chunk", "```\nplain\n```")]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains("<pre class=\"code\"><code>plain</code></pre>"),
            "a block of no language named one anyway: {rendered}"
        );
    }

    #[test]
    fn a_message_with_no_fence_in_it_reads_exactly_as_it_did() {
        let events = log(&[chunk("agent_message_chunk", "one\ntwo")]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains("<p>one<br>two</p>"),
            "prose stopped reading as prose: {rendered}"
        );
        assert!(
            !rendered.contains("<p></p>"),
            "a message with no code in it grew a block of nothing: {rendered}"
        );
    }

    #[test]
    fn a_fence_that_is_never_closed_reads_as_code_to_the_end() {
        let events = log(&[chunk("agent_message_chunk", "here:\n```ladder\nlet x = 1;")]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains(
                "<pre class=\"code\"><code class=\"lang-ladder\">let x = 1;</code></pre>"
            ),
            "an unclosed fence lost the code under it: {rendered}"
        );
    }

    /// The adapter splits a message mid-word, so a fence arrives in pieces;
    /// what is read for a fence is the message, not the piece.
    #[test]
    fn a_fence_that_arrives_in_pieces_is_still_a_fence() {
        let events = log(&[
            chunk("agent_message_chunk", "```lad"),
            chunk("agent_message_chunk", "der\nlet x = 1;\n```"),
        ]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains("<code class=\"lang-ladder\">let x = 1;</code>"),
            "a fence split across chunks was not read as one: {rendered}"
        );
    }

    #[test]
    fn a_run_of_backticks_inside_a_line_is_not_a_fence() {
        let events = log(&[chunk("agent_message_chunk", "see ```rust here")]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains("<p>see ```rust here</p>"),
            "backticks mid-line opened a block: {rendered}"
        );
    }

    #[test]
    fn an_empty_fenced_block_reads_as_an_empty_block() {
        let events = log(&[chunk("agent_message_chunk", "```\n```")]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains("<pre class=\"code\"><code></code></pre>"),
            "an empty block did not survive as one: {rendered}"
        );
        assert!(
            !rendered.contains("<p></p>"),
            "the blank either side of the fence became a paragraph: {rendered}"
        );
    }

    #[test]
    fn code_in_a_message_cannot_smuggle_markup_into_the_log() {
        let events = log(&[chunk(
            "agent_message_chunk",
            "```\n<script>alert(1)</script>\n```",
        )]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            !rendered.contains("<script>"),
            "code let markup through: {rendered}"
        );
        assert!(
            rendered.contains("&lt;script&gt;"),
            "the code itself did not survive escaping: {rendered}"
        );
    }

    /// A paragraph will not hold a block of code: a browser answers one by
    /// closing the paragraph early, and the rest of the message falls out of
    /// the shape the log was written in.
    #[test]
    fn code_in_a_message_stands_beside_the_prose_rather_than_inside_it() {
        let events = log(&[chunk(
            "agent_message_chunk",
            "look:\n```ladder\nlet x = 1;\n```\nand done",
        )]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains(concat!(
                "<p>look:</p>",
                "<pre class=\"code\"><code class=\"lang-ladder\">let x = 1;</code></pre>",
                "<p>and done</p>",
            )),
            "the code did not stand beside the prose: {rendered}"
        );
        assert!(
            !nests_a_block_inside_prose(&rendered),
            "a block opened inside a paragraph: {rendered}"
        );
    }

    #[test]
    fn code_in_a_users_turn_stands_back_with_the_rest_of_the_turn() {
        let events = log(&[outbound_prompt("try:\n```\nmake test\n```\nthen ship")]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains(concat!(
                "<p class=\"dim\"><b>you:</b> try:</p>",
                "<pre class=\"code dim\"><code>make test</code></pre>",
                "<p class=\"dim\">then ship</p>",
            )),
            "the user's code did not stand back with the turn: {rendered}"
        );
    }

    /// A turn of nothing but code is still the user's turn, and still says so.
    #[test]
    fn a_user_who_says_only_code_is_still_named() {
        let events = log(&[outbound_prompt("```\nmake test\n```")]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains(concat!(
                "<p class=\"dim\"><b>you:</b> </p>",
                "<pre class=\"code dim\"><code>make test</code></pre>",
            )),
            "a turn of pure code lost whose turn it was: {rendered}"
        );
    }

    #[test]
    fn code_in_a_thought_stands_back_beside_it_rather_than_inside_it() {
        let events = log(&[chunk("agent_thought_chunk", "maybe:\n```\nmake test\n```")]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains(concat!(
                "<p class=\"dim\">maybe:</p>",
                "<pre class=\"code dim\"><code>make test</code></pre>",
            )),
            "the thought's code did not stand back beside it: {rendered}"
        );
        assert!(
            !nests_a_block_inside_prose(&rendered),
            "a block opened inside prose: {rendered}"
        );
    }

    #[test]
    fn code_in_a_notice_stands_beside_the_quote_rather_than_inside_it() {
        let events = log(&[json!({
            "corcode": "reset_notice",
            "text": "run this again:\n```\nmake test\n```",
        })]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains(concat!(
                "<blockquote class=\"dim\">run this again:</blockquote>",
                "<pre class=\"code dim\"><code>make test</code></pre>",
            )),
            "the notice's code did not stand beside the quote: {rendered}"
        );
        assert!(
            !nests_a_block_inside_prose(&rendered),
            "a block opened inside prose: {rendered}"
        );
    }

    /// A fence may say more about the code than what it is written in; what
    /// names the block is the first word, and only that.
    #[test]
    fn a_fence_names_only_its_first_word_as_the_language() {
        let events = log(&[chunk(
            "agent_message_chunk",
            "```ladder ignore dim\nlet x = 1;\n```",
        )]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains("<code class=\"lang-ladder\">let x = 1;</code>"),
            "the rest of the fence's line rode in on the class: {rendered}"
        );
        assert!(
            !rendered.contains("ignore"),
            "a word the fence said reached the page: {rendered}"
        );
    }

    #[test]
    fn a_fence_cannot_name_a_language_that_breaks_out_of_its_attribute() {
        let events = log(&[chunk(
            "agent_message_chunk",
            "```x\" onmouseover=\"alert(1)\nlet x = 1;\n```",
        )]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains("<code class=\"lang-x&quot;\">"),
            "the language was not named as the text it is: {rendered}"
        );
        assert!(
            !rendered.contains("onmouseover"),
            "a language named an attribute of its own: {rendered}"
        );
    }

    /// Code the agent writes is read for what each word of it is doing, and
    /// the classes say so; the colours are the stylesheet's (ADR-0008 §3).
    #[test]
    fn code_in_a_language_that_is_known_is_read_for_what_it_says() {
        let events = log(&[chunk(
            "agent_message_chunk",
            "```rust\n// note\nfn main() {\n    let v = Vec::new();\n    let x = 42;\n    say(\"hi\");\n}\n```",
        )]);

        let rendered = event_log(CHAT_ID, &events);

        for named in [
            "hl-comment",
            "hl-storage",
            "hl-entity",
            "hl-keyword",
            "hl-constant",
            "hl-string",
            "hl-support",
        ] {
            assert!(
                rendered.contains(named),
                "the code was not read for its {named}: {rendered}"
            );
        }
        assert!(
            rendered.contains("<pre class=\"code\"><code class=\"lang-rust\">"),
            "the block the colours sit in changed: {rendered}"
        );
        assert!(
            code_body(&rendered).contains('\n'),
            "the lines of the code ran together into one: {rendered}"
        );
    }

    #[test]
    fn code_in_a_thought_is_read_for_what_it_says_and_still_stands_back() {
        let events = log(&[chunk("agent_thought_chunk", "```rust\nlet x = 42;\n```")]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains("<pre class=\"code dim\"><code class=\"lang-rust\">"),
            "the thought's code did not stand back: {rendered}"
        );
        assert!(
            rendered.contains("hl-constant"),
            "a voice that stands back had its code left unread: {rendered}"
        );
    }

    /// A diff the agent pastes is marked the way a diff a tool printed is
    /// (ADR-0008 §3), so the same colours mean the same thing either way.
    #[test]
    fn a_diff_the_agent_writes_is_read_for_what_it_changes() {
        let events = log(&[chunk(
            "agent_message_chunk",
            "```diff\n--- a/x.rs\n+++ b/x.rs\n-old line\n+new line\n```",
        )]);

        let rendered = event_log(CHAT_ID, &events);

        for named in ["hl-deleted", "hl-inserted"] {
            assert!(
                rendered.contains(named),
                "the diff was not read for its {named}: {rendered}"
            );
        }
    }

    /// Reading a block costs time on every poll, forever; past a size no one
    /// reads on a screen anyway, the code is simply shown.
    #[test]
    fn code_too_long_to_read_is_shown_rather_than_read() {
        let long = "let x = 42;\n".repeat(10_000);
        let events = log(&[chunk("agent_message_chunk", &format!("```rust\n{long}```"))]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains("<code class=\"lang-rust\">let x = 42;"),
            "a long block lost the code under it: {rendered}"
        );
        assert!(
            !rendered.contains("hl-"),
            "a block too long to read was read anyway: {rendered}"
        );
    }

    /// What a sentence is about is what the speaker marked, not what a
    /// tokenizer guesses: a count, a date and a pair of signs are words.
    #[test]
    fn numbers_and_signs_in_a_sentence_stay_words() {
        let events = log(&[chunk(
            "agent_message_chunk",
            "I changed 3 things, landed 2026-08-06, +12 -4",
        )]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains("<p>I changed 3 things, landed 2026-08-06, +12 -4</p>"),
            "a sentence did not come through as a sentence: {rendered}"
        );
        assert!(
            !rendered.contains("tok-"),
            "a sentence was read for tokens: {rendered}"
        );
    }

    #[test]
    fn what_the_speaker_put_in_backticks_reads_as_code() {
        let events = log(&[chunk(
            "agent_message_chunk",
            "look at `src/ui/mod.rs` and `run` it",
        )]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains(concat!(
                "<p>look at <code class=\"tok-path\">src/ui/mod.rs</code>",
                " and <code class=\"tok-path\">run</code> it</p>",
            )),
            "backticks did not read as code: {rendered}"
        );
    }

    #[test]
    fn code_in_a_sentence_cannot_smuggle_markup_into_the_log() {
        let events = log(&[chunk("agent_message_chunk", "run `<b>&x</b>` now")]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains("<code class=\"tok-path\">&lt;b&gt;&amp;x&lt;/b&gt;</code>"),
            "code in a sentence reached the page as markup: {rendered}"
        );
    }

    #[test]
    fn a_backtick_with_no_partner_is_just_a_backtick() {
        let events = log(&[chunk("agent_message_chunk", "a `b and c")]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains("<p>a `b and c</p>"),
            "a lone backtick opened code: {rendered}"
        );
    }

    #[test]
    fn a_pair_of_backticks_with_nothing_between_them_is_two_backticks() {
        let events = log(&[chunk("agent_message_chunk", "see `` here")]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains("<p>see `` here</p>"),
            "an empty pair of backticks opened code: {rendered}"
        );
    }

    #[test]
    fn a_link_in_a_sentence_is_marked_as_one() {
        let events = log(&[chunk("agent_message_chunk", "see https://corvous.dev now")]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains("<p>see <span class=\"tok-url\">https://corvous.dev</span> now</p>"),
            "a link in a sentence went unmarked: {rendered}"
        );
    }

    /// Every voice's words are read the same way, not only the agent's.
    #[test]
    fn every_voice_reads_its_words_the_same_way() {
        let events = log(&[
            outbound_prompt("read `src/ui/mod.rs`"),
            chunk("agent_thought_chunk", "maybe https://corvous.dev first"),
            json!({"corcode": "reset_notice", "text": "run `make test` again"}),
        ]);

        let rendered = event_log(CHAT_ID, &events);

        for read in [
            "<p class=\"dim\"><b>you:</b> read <code class=\"tok-path\">src/ui/mod.rs</code></p>",
            concat!(
                "<p class=\"dim\">maybe <span class=\"tok-url\">https://corvous.dev</span>",
                " first</p>",
            ),
            concat!(
                "<blockquote class=\"dim\">run <code class=\"tok-path\">make test</code>",
                " again</blockquote>",
            ),
        ] {
            assert!(
                rendered.contains(read),
                "a voice went unread, wanted {read}: {rendered}"
            );
        }
    }

    #[test]
    fn a_sentence_with_nothing_marked_in_it_reads_exactly_as_it_did() {
        let events = log(&[chunk(
            "agent_message_chunk",
            "shipping the ladder now, boss\nand it's one-screen-deep",
        )]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered
                .contains("<p>shipping the ladder now, boss<br>and it&#39;s one-screen-deep</p>"),
            "prose stopped reading as the prose it is: {rendered}"
        );
        for marked in ["tok-", "<code"] {
            assert!(
                !rendered.contains(marked),
                "a plain sentence grew a {marked} of its own: {rendered}"
            );
        }
    }

    /// Code in a fence is the highlighter's to read; the inline reading of a
    /// sentence never reaches inside a block.
    #[test]
    fn code_in_a_block_is_read_by_the_highlighter_and_not_as_a_sentence() {
        let events = log(&[chunk(
            "agent_message_chunk",
            "here:\n```rust\nlet x = `42`;\n```",
        )]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            code_body(&rendered).contains("hl-"),
            "the code was not read as code: {rendered}"
        );
        for marked in ["tok-", "<code"] {
            assert!(
                !code_body(&rendered).contains(marked),
                "the code was read as a sentence, finding {marked}: {rendered}"
            );
        }
    }

    #[test]
    fn what_a_tool_printed_is_still_read_for_every_token() {
        let events = log(&[
            tool_call("call_1", "git diff --stat"),
            tool_result("call_1", "src/ui/mod.rs | 3 +++\n- gone"),
        ]);

        let rendered = event_log(CHAT_ID, &events);

        for marked in [
            "<span class=\"tok-path\">src/ui/mod.rs</span>",
            "<span class=\"tok-num\">3</span>",
            "<span class=\"tok-add\">+++</span>",
            "<span class=\"tok-del\">-</span>",
        ] {
            assert!(
                rendered.contains(marked),
                "tool output lost its {marked}: {rendered}"
            );
        }
    }

    #[test]
    fn code_in_a_language_nothing_here_knows_reads_as_plain_code() {
        let events = log(&[chunk(
            "agent_message_chunk",
            "```nosuchlang\nlet x = 1;\n```",
        )]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains("<code class=\"lang-nosuchlang\">let x = 1;</code>"),
            "a language nothing knows lost the code under it: {rendered}"
        );
        assert!(
            !rendered.contains("hl-"),
            "code was coloured by a reading nothing here has: {rendered}"
        );
    }

    #[test]
    fn code_the_fence_names_no_language_for_reads_as_plain_code() {
        let events = log(&[chunk("agent_message_chunk", "```\nlet x = 1;\n```")]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains("<code>let x = 1;</code>"),
            "an unnamed block lost the code under it: {rendered}"
        );
        assert!(
            !rendered.contains("hl-"),
            "code was coloured in a language nobody named: {rendered}"
        );
    }

    #[test]
    fn coloured_code_cannot_smuggle_markup_into_the_log() {
        let events = log(&[chunk(
            "agent_message_chunk",
            "```rust\nsay(\"<script>alert(1)</script>\");\n```",
        )]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            !rendered.contains("<script>"),
            "coloured code let markup through: {rendered}"
        );
        assert!(
            rendered.contains("&lt;script&gt;"),
            "the code itself did not survive escaping once: {rendered}"
        );
    }

    #[test]
    fn a_blank_line_in_prose_still_reads_as_the_break_it_was() {
        let events = log(&[chunk("agent_message_chunk", "one\n\ntwo")]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains("<p>one<br><br>two</p>"),
            "a blank line broke the paragraph in two: {rendered}"
        );
    }

    #[test]
    fn a_message_of_several_blocks_keeps_every_one_of_them() {
        let events = log(&[chunk(
            "agent_message_chunk",
            "a\n```\nx\n```\nb\n```\ny\n```\nc",
        )]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains(concat!(
                "<p>a</p>",
                "<pre class=\"code\"><code>x</code></pre>",
                "<p>b</p>",
                "<pre class=\"code\"><code>y</code></pre>",
                "<p>c</p>",
            )),
            "a message of several blocks lost some of them: {rendered}"
        );
    }

    #[test]
    fn a_fence_closed_over_windows_line_endings_still_closes() {
        let events = log(&[chunk(
            "agent_message_chunk",
            "here:\r\n```ladder\r\nlet x = 1;\r\n```\r\n",
        )]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains("<pre class=\"code\"><code class=\"lang-ladder\">let x = 1;"),
            "the block did not read as code: {rendered}"
        );
        assert!(
            !rendered.contains('`'),
            "the closing fence was read as code instead of a close: {rendered}"
        );
    }

    /// Only a line that is a fence and nothing else closes a block, so code
    /// that starts with backticks of its own stays code.
    #[test]
    fn a_line_that_only_starts_with_a_fence_does_not_close_the_block() {
        let events = log(&[chunk("agent_message_chunk", "```\nx\n```end\ny\n```")]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains("<code>x\n```end\ny</code>"),
            "a line that merely starts with a fence closed the block: {rendered}"
        );
    }

    #[test]
    fn an_indented_fence_is_not_a_fence() {
        let events = log(&[chunk("agent_message_chunk", "  ```rust\nlet x = 1;")]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains("<p>  ```rust<br>let x = 1;</p>"),
            "an indented fence opened a block: {rendered}"
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

        let ending = colorize("47 ++++----");

        assert!(
            ending
                .contains("<span class=\"tok-add\">++++</span><span class=\"tok-del\">----</span>"),
            "a stat at the end of a line is not marked as a change: {ending}"
        );
    }

    /// A flag is spelled like a diff mark and means nothing of the sort, and
    /// tool output is mostly command lines.
    #[test]
    fn a_flag_is_not_a_change() {
        for flagged in [
            "cargo test --lib",
            "diff --git a/x b/x",
            "docker compose --no-cache",
        ] {
            let coloured = colorize(flagged);

            assert!(
                !coloured.contains("tok-del"),
                "a flag was read as what a diff takes away: {coloured}"
            );
        }
    }

    #[test]
    fn a_hyphen_in_a_word_or_between_words_is_not_a_change() {
        let coloured = colorize("a - b and site-packages and test-1 and i--");

        assert!(
            !coloured.contains("tok-del"),
            "a hyphen was read as what a diff takes away: {coloured}"
        );
    }

    #[test]
    fn a_digit_glued_to_a_word_is_not_a_count() {
        assert_eq!(colorize("abc123 v2"), "abc123 v2");
    }

    /// Colouring is for what a tool printed. What the agent said is prose,
    /// and a path in prose is part of the sentence (ADR-0008 §3).
    #[test]
    fn what_the_agent_said_is_left_uncoloured() {
        let events = log(&[chunk("agent_message_chunk", "see src/a.rs 47")]);

        let rendered = event_log(CHAT_ID, &events);

        assert!(
            rendered.contains("<p>see src/a.rs 47</p>"),
            "the agent's message did not come through as prose: {rendered}"
        );
        assert!(
            !rendered.contains("tok-"),
            "prose was coloured like tool output: {rendered}"
        );
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
        for dark in [
            "--tok-num:#f5a742;",
            "--tok-path:#61afef;",
            "--tok-url:#56b6c2;",
            "--tok-add:#56d364;",
            "--tok-del:#f47067;",
        ] {
            assert!(
                position(crate::ui::CSS, "@media(prefers-color-scheme:dark)")
                    < position(crate::ui::CSS, dark),
                "{dark} is not behind the scheme it is for: {}",
                crate::ui::CSS
            );
        }
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
