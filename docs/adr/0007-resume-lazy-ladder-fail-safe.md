# ADR-0007: Resume flow — lazy trigger, reconnect ladder, fail-safe defaults

Date: 2026-08-05
Status: Accepted
Wayfinder: [decision #9](https://github.com/CorVous/CorCode/issues/9)

## Context

Chats outlive containers (ADR-0002) and the UI renders any chat from
`events.jsonl` without one (ADR-0006), so "reading a chat" and "resuming a
session" are separable acts. The ACP research (issue #3) gives two memory
restoration paths — `session/resume` (cheap, no replay; named
`methods.agent.session.resume` in ACP SDK 1.3.0, see #56) and
`session/load` (full transcript replay) — plus acpx's precedent of chaining
them. Open questions: when resume fires, how agent memory is restored, and
what happens when the remote or on-disk state has drifted underneath the
manifest.

## Decision

1. **Resume fires on first prompt, never on open.** Opening a chat is a pure
   read of `events.jsonl`. The resume machinery runs only when the user
   prompts a chat with no live ACP connection; the UI shows a waking state
   meanwhile.
2. **One flow, three entries.** Live container → just prompt. Parked → spawn
   a container, remount workspace + `claude/`. Archived → fresh workspace
   dir, clone, check out `branch` at `last_pushed_commit`, inject ADR-0002's
   reset notice.
3. **Reconnect ladder** once a container is up: `session/resume` →
   `session/load` → `session/new` plus an injected memory-reset notice.
   While a `session/load` replay is in flight the connection is
   **display-silent**: replayed `sessionUpdate`s rebuild agent memory only
   and are never appended to `events.jsonl`.
4. **Remote drift on revival**: `last_pushed_commit` unreachable → check out
   the branch tip and say so in the reset notice (the remote is the truth);
   branch deleted → hard error, chat stays readable, retry affordance — a
   deleted branch is never silently recreated.
5. **Everything else fails safe.** Any other broken invariant (missing
   workspace dir for a parked chat, unreadable manifest, …) → loud generic
   error state with retry; the core touches nothing and never auto-repairs.
   A "missing" workspace can mean an unmounted dataset — auto-healing would
   compound the damage.

## Amendment (2026-08-09): drift is also the tip moving on

Rule 4 read only the case where `last_pushed_commit` is gone. The commoner
drift leaves it right where it was and moves the branch past it — an external
push, or another chat's archive — and the revival is silently correct: the
workspace comes back exactly where the chat left it. What is no longer true is
that this chat can push there, so the archive that follows is refused and
rescued onto a branch of its own (issue #50, ruled 2026-08-07).

A revival that lands behind the tip therefore says so, in ADR-0006's
`drift_notice`, beside the reset notice. Still no repair and still no force:
the remote is the truth, and the chat is only told what it is working under.

## Consequences

- First prompt into a parked/archived chat pays spawn/clone latency;
  accepted, and the waking-state visuals belong to the UI ticket (#10).
- Core restarts need no reattach pass: dead exec pipes are rediscovered
  lazily when the next prompt runs the ladder. Mid-turn agent death is the
  same case — the turn errors, the next prompt reconnects.
- The `session/new` rung means agent memory can silently start thin; the
  injected notice (and the intact `events.jsonl` for the human) is the whole
  mitigation for the MVP.
- No auto-repair means some failures end in manual intervention — accepted
  in exchange for never destroying evidence.
