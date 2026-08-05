# CorCode — Context

Web app for running containerized coding agents on a TrueNAS box: Rust core,
htmx frontend, per-chat Docker workspace containers speaking ACP to Claude
Code. Decisions live in `docs/adr/`; this file holds the vocabulary.

## Glossary

- **Chat** — the durable unit of work: one conversation, one repo+branch,
  one persistent record under `chats/<chat-id>/`. Survives archive; the thing
  the UI lists. States: `open` or `archived` (ADR-0002, ADR-0006).
- **Session** — the ACP conversation inside a chat, identified by the
  adapter's session id (`acp_session_id`). Resumed via
  `resumeSession`/`session/load`. A session is not a container (ADR-0002).
- **Workspace** — the git clone a chat's agent works in: a dir under
  `workspaces/<chat-id>/` on the NAS, bind-mounted at the fixed container
  path `/workspace`. Exists iff the chat is open (ADR-0002, ADR-0006).
- **Workspace container** — the disposable hardened Docker sibling
  (ADR-0001) running the agent behind the ACP adapter, from the single
  pinned workspace image (ADR-0004).
- **Parked** — an open chat with no live container: workspace retained,
  container torn down. Runtime pool state, reconstructable from `docker ps`,
  never persisted (ADR-0002, ADR-0006).
- **Archived** — a chat explicitly closed: final commit push-gated,
  container destroyed, workspace deleted. Revivable from git + chat dir
  alone; revival resets the workspace and notifies the agent (ADR-0002).
- **Events log** — `events.jsonl` in the chat dir: the core's append-only
  record of prompts and `sessionUpdate`s in ACP shape. The UI's only render
  source (ADR-0006).
- **Manifest** — `manifest.json` in the chat dir: the chat's non-derivable
  state (schema in ADR-0006). The chat list is a scan of these.
- **Agent memory** — the adapter's internal JSONL under the chat dir's
  `claude/` mount (`CLAUDE_CONFIG_DIR`). Read only by the agent via
  `session/load`; the core never parses it (ADR-0006).
- **Reconnect ladder** — the ordered attempts to restore agent memory when a
  prompt hits a chat with no live ACP connection:
  `resumeSession` → `session/load` → `session/new` + reset notice. Replay
  during `session/load` is display-silent — it never feeds the events log
  (ADR-0007).
- **Reset notice** — the automated context line the core injects before the
  agent's first turn when its workspace or memory doesn't carry over:
  archive revival, branch-tip fallback, or a `session/new` memory reset
  (ADR-0002, ADR-0007).
- **Push gate** — the rule that no teardown destroys work git doesn't have:
  push failure blocks close/archive/checkpoint paths (ADR-0002, ADR-0005).
- **Checkpoint branch** — `<branch>-chkpt-<stamp>`, minted by the
  close/archive gate when dirty state can't land on the chat branch
  (ADR-0005).
