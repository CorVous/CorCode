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
   the record of the run rather than a summary of one.)_
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
   its own box instead of widening the page on a phone.)_
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
