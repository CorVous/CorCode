# ADR-0006: Per-chat persistence — dual log, two-tree layout, streaming writes

Date: 2026-08-05
Status: Accepted
Wayfinder: [decision #8](https://github.com/CorVous/CorCode/issues/8)

## Context

Each chat must survive container teardown (ADR-0002) with two consumers of
its history: the agent, whose memory is replayed by `session/load`, and the
human, whose UI must render any chat — live, parked, or archived — on demand.

The ACP wiring research (issue #3) established the mechanics: the Claude Code
ACP adapter appends its own JSONL transcript at
`$CLAUDE_CONFIG_DIR/projects/<encoded-cwd>/<sessionId>.jsonl`, and
`session/load` replays that file. The format is adapter-internal and
unversioned; the only supported reader is a running agent process. ADR-0004
already requires `CLAUDE_CONFIG_DIR` to point at a writable mount (read-only
rootfs).

Rejected shapes: relying on `session/load` as the UI's render path (reading a
parked chat would evict a container from the pool; reading an archived chat
would force the full revival flow — a read mutating lifecycle state), and
parsing the adapter's JSONL in the core (a second parser for an internal
format, re-implementing the adapter's own replay filter, re-validated on
every image bump).

## Decision

**Dual log.** The adapter's JSONL — persisted on the NAS via the mounted
`claude/` dir — is the agent's memory and the sole input to
resume/`session/load`. Separately, the core appends every event it already
handles to a per-chat `events.jsonl`: the user's `session/prompt` requests
(outbound, easy to forget) and every incoming `sessionUpdate`, one
`{"ts": ..., "event": <ACP payload>}` line each. The UI renders **only** from
`events.jsonl` — reading a chat never touches a container, and the display
format is ACP-shaped (spec'd, versioned), which the UI must render live
anyway. One renderer, one format.

**Two-tree layout** on the manually-created dataset (ADR-0002's hosting
findings), lifetime boundaries as directory boundaries:

```
corcode/
  chats/<chat-id>/          # durable, survives archive
    manifest.json
    events.jsonl            # core's ACP event log (display record)
    claude/                 # mounted as CLAUDE_CONFIG_DIR (agent memory)
  workspaces/<chat-id>/     # exists iff session open; deleted on archive
```

ADR-0002's orphan sweep diffs `workspaces/` against open sessions; archive
teardown deletes one tree containing nothing durable.

**Fixed mount point.** The workspace mounts at the same container path
(`/workspace`) for every chat, forever — the adapter's transcript path
encodes the cwd, so a moved mount point breaks `session/load` for every
existing chat. Spec-level invariant, not a manifest field.

**Manifest schema** (`manifest.json`):

```json
{
  "schema": 1,
  "chat_id": "<ulid>",
  "title": "auto or user-set",
  "state": "open | archived",
  "repo": "CorVous/CorCode",
  "branch": "chat/2026-08-05-persistence",
  "base_branch": "main",
  "last_pushed_commit": "<sha>",
  "acp_session_id": "<adapter session uuid>",
  "created_at": "...",
  "last_active_at": "..."
}
```

Only non-derivable state is stored: no `parked` (open-with-container vs
parked is reconstructable from `docker ps`), no image tag (resume always uses
the active tag from core config), no cwd (constant). `last_pushed_commit` is
written by the close/archive push gate and is what archive revival re-clones
at (ADR-0002).

**Streaming writes, nothing on teardown.** `events.jsonl`: append + flush per
line; a core crash loses at most the line in flight. The adapter's JSONL is
already incremental onto the NAS mount. `manifest.json`: atomic
temp-file-and-rename on state transitions; `last_active_at` (ADR-0002's LRU
parking order) updated once per completed turn, not per event. Per-line fsync
against chat-turn event rates on a NAS dataset is a non-issue.

## Amendment (2026-08-05): on-disk event shapes

`events.jsonl` lines are `{"ts": ..., "event": <payload>}`. The payload is
the ACP message verbatim, not a shape of our own. Writer and renderer bind to
this list; anything else is a payload the renderer names but does not
interpret.

Outbound `session/prompt` params, one line per turn the user takes:

```json
{"sessionId": "<uuid>", "prompt": [{"type": "text", "text": "..."}]}
```

`prompt` is an array of content blocks; only `type: "text"` blocks carry
words. There is no `sessionUpdate` key on an outbound prompt — that is what
distinguishes it from everything else in the file.

Inbound `session/update` notification params, one line per update:

```json
{"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "..."}}
```

The same shape carries `user_message_chunk` and `agent_thought_chunk`; the
text lives in the content block, never at the top level. Tool calls carry
their own fields instead:

```json
{"sessionUpdate": "tool_call", "toolCallId": "...", "title": "...",
 "kind": "execute", "status": "pending"}
```

`tool_call_update` repeats `toolCallId` with whatever changed. Other
`sessionUpdate` kinds (`plan`, `available_commands_update`, ...) are written
through unchanged.

A `session/update` whose params carry their fields directly, with no `update`
member, is written through whole rather than as the member that is not there:
a shape the build does not know is still a shape, and a `null` line is not.

Core-injected notices are the one payload that is not ACP. They carry a
`corcode` key naming the kind, a `text` the renderer says out loud, and no
`sessionUpdate`, so no reader can mistake one for an agent message:

```json
{"corcode": "reset_notice", "text": "..."}
{"corcode": "permission_declined", "text": "..."}
{"corcode": "refusal", "text": "..."}
{"corcode": "wake_failure", "text": "..."}
{"corcode": "push_failure", "text": "..."}
{"corcode": "mode_notice", "text": "..."}
{"corcode": "connection_lost", "text": "..."}
```

`reset_notice` says where the agent's memory was cut. `permission_declined`
says the agent asked the operator for something and this client answered no
on their behalf, which is the only answer it has: the core declares no client
capabilities, and an unanswered request blocks the agent's whole turn.
`refusal` says a prompt never went out because the chat was busy or already
being woken. `wake_failure` says a prompt never went out because the chat
could not be brought back at all, and names what stopped it (ADR-0007).
`push_failure` says the archive gate got nothing onto the remote, so
the chat is still open with its container up and can be archived again
(ADR-0002 rule 3). `mode_notice` says the session opened in some other
permission mode than the one the image's managed settings ask for, which is
otherwise invisible: an agent clamped into asking has every ask declined and
reads as a mute one (ADR-0001). `connection_lost` says a turn ended in a
failure rather than an answer, what became of the connection it went over, and
that the prompt can be put again: without it the log stops at the prompt and
the page shows a chat that is still thinking (issue #65). It is the one kind
whatever ended the turn. A channel some process the agent ran wrote over is
told apart in the `text` rather than in the kind — it names the corruption and
says to archive and revive a chat it keeps happening to — because it is the
same news about the same chat, and the renderer says every kind out loud the
same way. All of them belong in
the log because the log
is the whole of what the chat page renders (ADR-0008); a status code the
browser swallows tells the operator nothing.

ADR-0008 renders every core line as a block quote.

## Amendment (2026-08-05): `checkpoint_branch`, and why the schema stays 1

The archive gate puts a dirty working tree on a branch of its own (ADR-0005),
and the manifest is the only record of which one:

```json
{"checkpoint_branch": "chat/2026-08-05-persistence-chkpt-20260805T142033"}
```

It is optional and read with a default: a chat archived before this field
existed simply has no such branch, which is exactly what a missing key means.
Nothing has to be migrated and no reader has to know two schemas, so `schema`
stays `1`. A field that *changed* the meaning of what is already on disk would
not get this treatment — the version is for readers that would otherwise
misread a file, not for every addition.

The compatibility runs one way only. Manifests are read with
`deny_unknown_fields`, so a binary from before this field that meets a
manifest carrying it fails the read — and because the console scans the whole
of `chats/`, that failure takes the console with it rather than one chat
(ADR-0007 rule 5: no skipping). Rolling a deployment back past this change
means rolling the dataset back with it.

The stamp is `yyyymmddTHHMMSSsss` in UTC — milliseconds, so that branches
minted back to back cannot collide. Seconds and not minutes: a push the
remote refuses is retried straight away, and two archives in one minute must
not name the same branch — the second push would be refused as a
non-fast-forward and the retry would fail for a reason that has nothing to do
with what went wrong the first time.

## Amendment (2026-08-08): the render path is two regions and a resync

The UI still renders solely from `events.jsonl`, and still by polling. What
changed is what a poll carries. A whole render is settled history —
`#log-history`, every event there is — with an empty `#log-hot` after it that
asks for `events?from=<count>` every 2s. The tail is rendered from
`events[from..]`, so an open page is sent the turn in flight and not the
transcript behind it.

The cursor is fixed at render time and nothing moves it, so the tail re-sends
the whole of a long turn on every poll. The section asks for itself again
every 30s, and that whole render is what settles the turn into history and
starts the tail over from the new end.

The resync is also what heals a read the cursor cuts across. The log is read
rather than replayed: a run of chunks in one voice is one message however many
events it arrived as, and a tool call is one line however many updates it took.
That reading is bounded by whatever slice it is given, so a run or a call that
began before `from` starts again as its own block in the tail — a split run
reads as two messages — until the next whole render puts it back together.
Slow region, correct; fast region, current.

A poll that names no cursor, or one this build cannot read — garbled, empty,
or given twice — is answered with the whole render rather than an error. A
section whose cursor has gone wrong therefore heals into a resync.

## Amendment (2026-08-09): `rescue_branch` and `drift_notice`

```json
{"corcode": "rescue_branch", "text": "..."}
{"corcode": "drift_notice", "text": "..."}
```

`rescue_branch` says the remote would not take the chat's own branch —
protected, or moved on under the chat — so the archive pushed the same work
once more under a `chat/<name>-rescue-<timestamp>` name and finished (issue
#50, ruled 2026-08-07). It is not a `push_failure`: the archive is done and
the chat is closed. The line exists because the log is the only place the
operator can read where their work went. Nothing was forced, so what the
remote holds on the chat's branch still stands.

The rescue fires on any push git refuses, not only on a refusal that means
"this branch is not yours": a push that never reached the remote at all —
credentials, DNS, a host that is down — is refused the same way, so it too
gets one attempt under the rescue name before the gate gives up. That costs a
second doomed push and reads no worse in the log, and telling the two apart
means parsing git's prose, which is a worse thing to depend on.

`drift_notice` says a revival came back on a commit the remote has since built
on (ADR-0007 rule 4): the chat lost nothing, but its next archive is the push a
remote can refuse, and the rescue should not be the first anyone hears of it.
It sits beside the `reset_notice` rather than inside it — that one says what
the revival cost, this one says what the remote has done since.

## Consequences

- ADR-0002's "transcript flushed" step in park/close is a no-op — no buffered
  state exists; teardown is container-kill (plus workspace delete on
  archive). The crash path and the teardown path are the same path.
- Reading any chat is a pure file read with no lifecycle side effects.
- The transcript is stored twice (adapter JSONL + events.jsonl); accepted —
  the copies serve different readers and neither derives from the other.
- Changing the `/workspace` mount constant is a breaking migration for every
  existing chat's resumability.
- Resume (issue #9) starts from `manifest.acp_session_id` + the `claude/`
  dir; the chat list is a scan of `chats/*/manifest.json`.
