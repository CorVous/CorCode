# ADR-0008: Web UI — one-screen-deep console, near-unstyled HTML

Date: 2026-08-05
Status: Accepted, amended by ADR-0009 (dead-simple premise scoped to the
app, not project tooling)
Wayfinder: [decision #10](https://github.com/CorVous/CorCode/issues/10)

## Context

The MVP needs a mobile-first htmx UI covering four surfaces: chat list,
chat view, new-chat form, container status. A throwaway prototype
(branch `prototype/web-ui-screens`) put three structurally different
variants in front of the user — messenger-style Inbox, ops-log Console,
git-forward Workbench. The user's verdict set a standing premise for the
whole MVP, not just the UI: **dead simple everywhere** — "incredibly
simple, nearly un-styled to keep css simple", structure "one-screen-deep".

## Decision

1. **One screen deep.** A single main screen holds three stacked
   sections; the only navigation in the app is main screen ↔ chat view.
   - **Status line** at the top: `pool n/m · parked k · img <tag> ·
     sweep ok`, expanding in place (`<details>`) to the full container
     picture — warm-pool slots with per-chat idle times, parked count,
     pinned image tag (ADR-0004), orphan-sweep result. There is no
     separate machines page.
   - **New chat** as an inline collapsed form, not a page: repo input
     over a `<datalist>` of the configured repositories, the first of
     them its default value; base-branch select, slug input with a live
     `chat/<date>-<slug>` branch preview, and the direct-on-base opt-out
     (ADR-0005). _(Amended 2026-08-06: the repo select became a free
     input — any `owner/name` or `https://` clone URL is accepted, and
     `CORCODE_REPOS` only suggests.)_
   - **Chat list** grouped by state — live / parked / archived
     (ADR-0002) — each row linking to the chat view, with branch and
     last-push shown small.
2. **Chat view is an event log**, rendered straight from `events.jsonl`
   (ADR-0006): user prompts, agent text, tool calls and git
   commits/pushes as small inline lines, reset notices (ADR-0007) as
   block quotes in sequence. Prompt input sits at the bottom; parked and
   archived chats show a one-line hint of what the first prompt will do
   (re-spin / revive) per ADR-0007's lazy resume. _(Amended 2026-08-08:
   what a tool printed hangs under its line in full, in a `<pre>` and
   out of the code fence the adapter wraps it in, so the transcript is
   the record of the run rather than a summary of one.)_ _(Amended
   2026-08-09: a fence in a message opens a block of its own —
   `<pre class="code"><code class="lang-…">`, escaped, at the brightness
   of the message it sits in, since code the agent writes is what the
   turn is for and not chrome around it. The block stands beside the
   message's paragraphs rather than inside one, which no browser would
   read as written. Prose outside a fence is unchanged, and a message
   with no code in it is the one paragraph it always was. A fence is
   read on the assembled message, so one split
   across streamed chunks still counts, and a fence left open runs to
   the end of the message because a turn is shown while it streams.
   Only the first word after a fence names the language: the rest is
   said to a markdown reader, and taking it would let a message name
   the page's own classes. A run of more than three backticks is not
   read as a fence of its own.)_ _(Amended 2026-08-09: the code in a
   block is read by syntect — the bundled syntaxes, the pure-Rust
   fancy-regex engine so nothing links against oniguruma, and no themes,
   since the colours are the stylesheet's. It names each word of the code
   with classes prefixed `hl-`, clear of the page's own, and escapes the
   code as it reads it. The set of syntaxes is unpacked once for the
   process, because a chat re-reads its whole log on every poll. A
   language nothing here knows, a fence that names none, a block longer
   than 100 KiB — a dump rather than something read on a screen, and one
   that would be read again on every poll for as long as the chat is open
   — or a reader that gives up on the code all fall back to the plain
   escaped block.)_
3. **Near-zero CSS.** Semantic HTML on browser defaults; the styling
   budget is on the order of a dozen lines (viewport meta,
   `color-scheme: light dark` so default link and form-control colors
   adapt to dark mode, 16px form-control font size so iOS Safari
   doesn't auto-zoom on focus, padding, overflow guards). No CSS
   framework, no theming, no custom fonts. A later restyle is additive
   because the DOM stays semantic. _(Amended 2026-08-08: one rule joins
   the budget — a `.dim` class at `opacity:0.6` on every transcript line
   except the agent's own message, so the log is read for what the agent
   said.)_ _(Amended 2026-08-08: a second rule joins it —
   `pre{overflow-x:auto;}`, so a wide line of tool output scrolls inside
   its own box instead of widening the page on a phone.)_ _(Amended
   2026-08-09: the declaration cap is lifted from a dozen to two dozen
   for a token palette on tool output — the link, path, count, diff mark
   and pass/fail glyph each carry a `tok-` class. The colours are custom
   properties on `:root`, respelled once under
   `@media(prefers-color-scheme:dark)`, so a class never names a hex and
   each colour is written once per scheme. The spans sit inside the
   dimmed `<pre>`, so they read muted rather than bright: the palette
   marks what to scan for and does not take the eye off the agent's
   message. Prose is untouched — this is tool output only. A token is
   claimed once and never read into again, and the two ends of a count
   are deliberately not symmetric: a digit glued to the end of a word is
   part of the word (`v2`), while a unit glued to the end of a count is
   not part of the count (`200ms` colours its `200`). A run of `+` or
   `-` is a diff mark only where it runs out into whitespace, so
   `--no-cache` is a flag and not a deletion.)_ _(Amended 2026-08-09: the
   cap is lifted again, to three dozen, for the same palette spent on
   highlighted code — comment, string, constant, keyword and storage,
   support and entity, each a class the highlighter emits under its `hl-`
   prefix, and the two marks of a diff. Four colours join the palette
   (keyword, string, type, comment) and the rest are the ones tool output
   already spends — a diff the agent pastes is marked in the same green
   and red as a diff a tool printed, so the colours mean one thing on the
   page. Every one is still a custom property answered under the dark
   scheme.)_ _(Amended 2026-08-09: prose is read by its structure and the
   lexical reading is now tool output's alone, explicitly. OpenCode is the
   model: a message is coloured by what the speaker marked up, never by
   what a run of characters looks like, so a count, a date or a pair of
   diff signs in a sentence stays words. Two markings so far, both inline
   and both closing on the line they open on: a backtick run reads as
   `<code>` in the path colour — where a path in a message now lives —
   and a bare link is marked as tool output marks it, by the same
   reading, so the two agree on what a link is. Real bullet lists, as
   sibling blocks beside the paragraphs, are the next step.)_
4. **Reference artifact:** variant D on the `prototype/web-ui-screens`
   branch is the shape to imitate; variants A–C are rejected directions
   kept only as prototype history.

## Consequences

- The htmx surface stays tiny: server-rendered fragments for the three
  sections and the log, no client state beyond `<details>`.
- Visual polish is consciously deferred; nothing in the DOM blocks a
  future stylesheet, but adding one is a new decision.
- The dead-simple premise is recorded on the wayfinder map as standing
  guidance for every remaining MVP decision, UI or not. _(Amended by
  ADR-0009: the premise constrains the app — product surface and
  architecture — not the project's tooling; CI and build automation are
  fair game.)_
