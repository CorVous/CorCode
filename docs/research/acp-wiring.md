# Research: ACP wiring — Rust client to Claude Code in-container

Ticket: [CorVous/CorCode#3](https://github.com/CorVous/CorCode/issues/3) (part of #1)

## Question

How does the Rust core speak ACP (Agent Client Protocol) to a Claude Code agent
running inside a Docker container?

## 1. The `agent-client-protocol` Rust crate

- **Maturity**: production-adjacent. It's the reference implementation that
  "powers the integration with external agents in the Zed editor" — i.e. it is
  the same crate Zed itself ships with, not a toy SDK. Current version is
  `2.0.0` (Apache-2.0, MSRV 1.88.0), with 77 published versions and 3.4M+
  all-time downloads on crates.io.
- **Client-side support**: full. The crate implements *both* sides of the
  protocol. To build a client (our Rust core, not an agent), you implement the
  `Client` trait; the `Agent` trait is for the other side. A runnable client
  example ships in the `rust-sdk` repo as a starting point.
- **Ecosystem crates** (all under `agentclientprotocol/rust-sdk`):
  - `agent-client-protocol` — roles, connection builders, handlers, protocol types (the one to depend on).
  - `agent-client-protocol-tokio` — Tokio helpers for spawning/connecting to agent subprocesses over stdin/stdout. This is almost certainly what we want for the container-spawn case.
  - `agent-client-protocol-http` — HTTP/SSE and WebSocket transports (see §3; not the stable stdio path).
  - `agent-client-protocol-schema` — wire types only, versioned explicitly (e.g. `agent_client_protocol_schema::v1::SessionId`).
- **Versioning**: current stable **protocol** version is `1`; wire
  compatibility is negotiated via `protocolVersion` during `initialize`.
  Protocol types are exposed through explicit version modules, so the crate
  can support multiple protocol versions side by side without breaking
  callers pinned to v1.

## 2. The Claude Code ACP adapter (`@zed-industries/claude-code-acp`, now `claude-agent-acp`)

- Node package that wraps the official **Claude Agent SDK** and translates it
  into ACP JSON-RPC. Zed bundles it; it's also usable standalone by any ACP
  client (including a Rust core talking to it over stdio).
- No stability badge in the README (no "stable/beta/experimental" marker) —
  treat it as actively-developed-but-unversioned-for-stability. Notably its
  own docs say "ACP 1.2 has no standard subagent tool kind or nested-message
  relationship," i.e. the adapter is tracking a moving protocol target.
- **`session/new`**: generates a fresh session UUID, checks auth (looks for a
  `.claude.json` backup), and funnels into a shared internal `createSession`.
  Returns session ID + available models + available modes.
- **`session/load`** (`loadSession`) — **does a full transcript replay**. It
  reads the session's JSONL file line by line, filters to `user`/`assistant`
  entries (skipping sidechains, mismatched session IDs, and `summary`
  entries), converts them to ACP `sessionUpdate` notifications, and streams
  them to the client. This is the method to call if the Rust core's UI needs
  to redisplay history after a reconnect.
- **`resumeSession`** (currently `unstable_`) — restores only the SDK's
  internal conversation state, **no client-side replay**. Cheaper, but the
  client won't see history unless it already has it cached.
- **`forkSession`** (`unstable_`) — like resume, but mints a *new* session ID
  so the conversation branches instead of continuing in place.
- **Persistence**: JSONL, one entry per line, incrementally appended, at
  `$CLAUDE_CONFIG_DIR/projects/<encoded-cwd>/<sessionId>.jsonl` (default
  `~/.claude`; cwd's path separators become dashes). This is exactly the kind
  of on-disk state that survives a container restart, *provided the directory
  is on a volume mounted outside the container's writable layer*.
- **Session discovery**: `unstable_listSessions` scans all project dirs and
  returns sessionId/cwd/title/mtime, paginated — useful for a "here are your
  prior sessions" reconnect UI without needing our own index.

## 3. Transport across the container boundary

ACP's spec (agentclientprotocol.com/protocol/transports) only standardizes
two transports: **stdio** (the primary, required-where-possible one) and
**Streamable HTTP**, which is still a draft proposal. Everything else —
TCP, Unix sockets, WebSocket — is explicitly a "custom transport": allowed,
since the protocol is transport-agnostic, but not interoperable by spec; the
implementer must preserve JSON-RPC framing (newline-delimited, one message
per line, no embedded newlines, UTF-8, nothing non-protocol on stdout).

Two realistic options for crossing the container boundary:

- **`docker exec -i` (or `docker run -i`) as the "agent command."** This is
  the pattern the community has actually used for this exact scenario: a Zed
  discussion ([zed-industries/zed#54913](https://github.com/zed-industries/zed/discussions/54913))
  shows someone configuring Zed's `agent_servers` with
  `command: "docker", args: ["exec", "-i", "<container>", "claude-agent-acp"]`.
  Docker's own `docker agent serve acp` / `docker agent exec` tooling follows
  the identical shape. Simplest option: no extra moving parts, process
  lifecycle (agent dies when the pipe closes) falls out for free from
  `docker exec` semantics, and it maps directly onto
  `agent-client-protocol-tokio`'s "spawn a subprocess, wire up its
  stdin/stdout" helpers — the subprocess is just `docker` instead of the
  agent binary directly. Caveat: never allocate a TTY (`-t`) and make sure
  the container's entrypoint doesn't print startup banners to stdout, or it
  corrupts the JSON-RPC stream. The one open problem reported in that
  discussion: Zed's own client-side code has some hardcoded local-binary
  discovery that got in the way; a hand-rolled Rust client wouldn't inherit
  that specific bug, but it's a signal that "put docker in the exec path" is
  not yet a first-class, polished flow anywhere.
- **Unix-socket (or TCP) shim inside the container, bridged with `socat`.**
  Have the ACP agent process listen on (or be wrapped to expose) a Unix
  socket inside the container; either bind-mount that socket out to the host
  or run `socat` inside the container as `UNIX-LISTEN:/path,fork` and use
  `docker exec` or a mounted socket path to reach it from the host, or go
  further and expose it as TCP with `socat tcp-listen:PORT,fork,reuseaddr
  unix-connect:/path`. Makes sense when the agent needs to be long-running
  and shared across multiple client connections/reconnects, or when the Rust
  core can't shell out to `docker` directly (e.g. it only has network access
  to the container, not the Docker socket). Costs: socket lifecycle
  management, `fork`-related races on startup (need a short retry/backoff on
  first connect), and it's a fully custom, non-interoperable transport per
  the spec — we'd own the framing contract end-to-end.
- **Ruled out for now**: hijacking the raw Docker Engine API `attach`
  endpoint (`POST /containers/<id>/attach?stream=1&stdin=1&stdout=1`)
  directly. It works but multiplexes stdout/stderr with Docker's own raw-
  stream framing headers, which would corrupt naive JSON-RPC line parsing
  unless we de-multiplex first — extra complexity for no real benefit over
  `docker exec -i`.

**Recommendation for transport**: start with `docker exec -i` +
`agent-client-protocol-tokio`'s subprocess helpers. It's the pattern already
proven (if informally) by the Zed community for this exact "ACP agent inside
a container" scenario, requires no extra shim process, and gives us process
lifecycle for free. Move to a socket shim only if/when we need long-lived,
multi-client, or host-without-docker-socket-access scenarios.

## 4. What `acpx` does for crash-reconnect/session persistence — worth stealing

[`acpx`](https://github.com/openclaw/acpx) is a headless CLI client for ACP
(one session-oriented command surface across Claude/Codex/Gemini/etc., built
specifically to replace PTY-scraping orchestration). Its crash-recovery
design is directly applicable:

- **Recovery sequence on a dead process**: if the saved session's PID is dead
  when the next prompt comes in, acpx (1) respawns the agent process, (2)
  tries `session/resume` if the agent advertises it, else falls back to
  `session/load`, and (3) if reconnecting fails outright, transparently falls
  back to `session/new` rather than erroring out. This graceful-degradation
  ladder (resume → load → new) is the single most useful thing to steal.
- **Invalid/not-found sessions**: if the adapter reports the session ID as
  invalid, acpx just creates a fresh session and updates its own saved
  record — it doesn't treat that as a fatal error.
- **Graceful shutdown**: Ctrl+C sends `session/cancel` before any force-kill,
  giving the agent a chance to persist state cleanly.
- **Session metadata store**: `~/.acpx/sessions/` holds session records
  (scoped per repo, supports named parallel sessions like `-s backend`
  /`-s frontend`), with lightweight turn-history previews (role, timestamp,
  text preview) appended after each successful prompt — a cheap way to build
  a reconnect/history UI without re-parsing the agent's own JSONL transcript.
- **Prompt queueing**: prompts submitted while a session is already running
  are queued and drained in order by the owning process, with an idle TTL
  (default 300s, overridable) governing when a "queue owner" gives up
  ownership. Useful if the Rust core will have multiple callers/tabs racing
  to talk to the same underlying session.
- **Known rough edge**: there's an open upstream issue where the Claude
  adapter process spawns and immediately reports `status: dead` with no
  captured stderr — a reminder that this adapter's process-lifecycle
  reporting isn't fully solid yet, so our own health-check/retry logic
  shouldn't assume clean exit signaling.

## Sources

- [Agent Client Protocol — Rust SDK docs](https://agentclientprotocol.com/libraries/rust)
- [agentclientprotocol/rust-sdk (GitHub)](https://github.com/agentclientprotocol/rust-sdk)
- [agent-client-protocol crate (docs.rs)](https://docs.rs/agent-client-protocol)
- [Agent Client Protocol — Transports](https://agentclientprotocol.com/protocol/transports)
- [zed-industries/claude-agent-acp (GitHub)](https://github.com/zed-industries/claude-agent-acp)
- [@zed-industries/claude-code-acp (npm)](https://www.npmjs.com/package/@zed-industries/claude-code-acp)
- [Session Lifecycle Management — claude-code-acp (DeepWiki)](https://deepwiki.com/zed-industries/claude-code-acp/4.3-session-lifecycle-management)
- [Claude Code: Now in Beta in Zed (Zed blog)](https://zed.dev/blog/claude-code-via-acp)
- [ACPs Hosted as Docker Containers — zed-industries/zed Discussion #54913](https://github.com/zed-industries/zed/discussions/54913)
- [Docker Docs — ACP (Agent Client Protocol) integration](https://docs.docker.com/ai/docker-agent/features/acp/)
- [acpx — headless CLI client for ACP (GitHub)](https://github.com/openclaw/acpx)
- [acpx (npm)](https://www.npmjs.com/package/acpx)
- [acpx.sh — official docs](https://acpx.sh/)
- [openclaw/openclaw#29979 — claude-agent-acp persistent session dies](https://github.com/openclaw/openclaw/issues/29979)

## Recommendation

1. **Rust side**: depend directly on the `agent-client-protocol` crate
   (v2.x) plus `agent-client-protocol-tokio` for subprocess wiring, and
   implement the `Client` trait. It's the same code Zed ships in production,
   so we inherit its protocol-conformance rather than reimplementing framing.
2. **Transport**: cross the container boundary with `docker exec -i
   <container> claude-agent-acp` as the literal subprocess command handed to
   `agent-client-protocol-tokio`'s spawn helper. Do not allocate a TTY; keep
   the container entrypoint silent on stdout. Revisit a Unix-socket/`socat`
   shim only if we need a long-lived agent shared across multiple client
   connections or a host that can't invoke `docker exec` directly.
3. **Session lifecycle**: use `session/new` to start, `session/load` when the
   Rust core needs to redisplay a prior conversation (it replays full
   history via `sessionUpdate` notifications), and `resumeSession` when it
   only needs the SDK's internal state restored cheaply (no client replay).
   Persist nothing extra on our side for the transcript itself — the
   adapter's JSONL files under `$CLAUDE_CONFIG_DIR/projects/<encoded-cwd>/`
   already are that record, provided that directory lives on a volume that
   survives container restarts.
4. **Crash/reconnect**: steal acpx's degrade-gracefully ladder — on
   reconnect, try `session/resume`, fall back to `session/load`, fall back
   to `session/new` if both fail — and adopt its pattern of a small
   session-metadata store (session ID, container ID/name, cwd, last-seen
   timestamp) so the Rust core doesn't have to re-derive "which sessions
   exist" by re-scanning adapter-internal JSONL on every restart. Do not
   assume clean process-exit signaling from the adapter; poll/health-check
   rather than trusting exit codes, per the known acpx/claude-agent-acp
   issue where the process reports `dead` with no diagnostic output.
