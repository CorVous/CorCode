# ADR-0005: Agent-authored commits, blocking stop-hook nudge, core-side archive gate

Date: 2026-08-05
Status: Accepted
Wayfinder: [decision #7](https://github.com/CorVous/CorCode/issues/7)
Amends: ADR-0002 (parking no longer forces a checkpoint)

## Context

Git is the source of truth via constant commits and pushes. ADR-0002 gated
teardown on a final push and originally demanded a host-triggerable WIP
checkpoint for parking. The owner wants the agent to author its commits —
semantic history, not mechanical snapshots — with machinery only where a
workspace is about to be destroyed.

## Decision

- **In-session commits are agent-authored.** CLAUDE.md instructions (in the
  image's baked config skeleton, ADR-0004) ask for meaningful commits pushed
  as work progresses. No host-side auto-commit daemon.
- **Stop-hook blocking nudge, one shot per turn**: when a turn ends with a
  dirty tree or unpushed commits, the hook rejects the stop with the details;
  the agent writes its own commit and pushes, then stops. The
  `stop_hook_active` guard lets the second stop through so a genuinely failed
  push is reported in chat instead of looping.
- **Parking forces nothing** (amends ADR-0002): container torn down, workspace
  dir retained — dir retention alone carries progress. Accepted trade-off:
  a parked session's uncommitted/unpushed work exists only on the NAS dataset
  until resume.
- **Close/archive keeps the hard gate, run by the core**: the core (workspace
  dataset mounted, GitHub token in hand) commits any dirty state and pushes —
  no agent involved, one code path for live and parked sessions. A dirty tree
  commits to a **fresh checkpoint branch** `<chat-branch>-chkpt-<UTC stamp>`
  (e.g. `-chkpt-20260805T2140`), leaving the chat branch at the agent's last
  semantic commit; a clean tree makes no branch. The manifest records the
  checkpoint branch for archive revival (resume ticket). Push failure blocks
  teardown (ADR-0002 rule 3 stands).
- **New chats branch by default**: the new-chat dialog picks repo + base
  branch and creates `chat/<date>-<short-slug>` off it (optional name
  override; upstream push on first commit), with an opt-out to work directly
  on the selected branch.
- **In-session push failure**: agent reports it in chat and continues; the
  next turn's nudge retries naturally. No core-side retry machinery.

## Amendment (2026-08-05): what "clean" means, and how the stamp is spelled

"A clean tree makes no branch" was written with a workspace standing on its
chat branch in mind. An agent that wanders — a detached rebase, a stray
`checkout -b` it never merged — leaves commits the chat branch does not carry
and the archive would delete unpushed. So the gate checkpoints a clean tree
too whenever HEAD is not on the chat branch, and the rule reads: everything
the chat branch does not already carry goes onto the checkpoint branch.

The stamp is to the second (`-chkpt-20260805T214033`), not the minute: a
refused push is retried immediately, and a retry within the same minute would
otherwise name a branch the remote already has.

## Amendment (2026-08-07): the nudge stands down where no push could land

The blocking nudge demanded a push of workspaces that hold no GitHub
credential, so every turn in one ended with the agent narrating an
unresolvable impasse ([#47](https://github.com/CorVous/CorCode/issues/47)).
The hook now asks first whether pushing is possible at all, and rules on what
"unsafe" means:

- **No `origin` remote** — the hook says so in one line and exits 0. Removing
  the remote disarms it; that is a deliberate act by whoever set the workspace
  up, not a hole an agent can dig.
- **An `http(s)` `origin` no credential answers for** — `git credential fill`
  with prompting disabled is the test, so a configured helper that has nothing
  for this host counts as nothing. One line, exit 0. Other transports carry
  their own keys and are left alone: the hook keeps its teeth.
- **A credential answers** — the nudge is unchanged, except that **untracked
  files no longer block**. Counting them was the "dirty tree" over-read
  ([#48](https://github.com/CorVous/CorCode/issues/48)): scratch output is not
  work git is missing, and demanding a commit of it taught agents to commit
  noise. Staged and modified tracked files still block, as do unpushed commits
  and a branch with no upstream.

Accepted risk: a workspace whose credential is merely broken looks the same as
one that never had one, and the hook stands down for both. Losing a turn's
nudge is cheaper than losing every turn to an impossible demand.

## Consequences

- Chat branches carry only agent-authored semantic commits; mechanical
  commits exist solely on checkpoint branches at archive time.
- Checkpoint branches accumulate on GitHub (one per dirty archive) until
  manually deleted — no auto-cleanup in the MVP.
- Durability between pushes rests on the NAS dataset for parked sessions —
  the dataset is already trusted with transcripts.
- The web UI's new-chat dialog needs repo, base branch, optional branch name,
  and a work-directly-on-branch toggle (UI ticket).

## Amendment (2026-08-08): per-chat startup script runs inside the agent's container

A chat may carry a startup script set on the new-chat form
([#14](https://github.com/CorVous/CorCode/issues/14)). It is executed
**inside that chat's own workspace container, as the agent (uid 1000), with
the workspace as cwd and the chat's spawn env visible** — the same sandbox the
agent itself runs in (ADR-0001). It is not run on the host and is handed no
credential the agent would not already hold, so it adds no privilege: whatever
the script can reach, the agent could reach anyway.

- **Every (re)spawn runs it.** Containers are ephemeral (ADR-0002), so the
  script runs on creation and again whenever a parked chat is spun back up,
  before the ACP session opens and after the workspace and env are ready.
- **Failure is non-blocking.** A non-zero exit or an exec that could not run
  never fails the spawn; the chat still opens. The exit code and combined
  output (truncated past 16 KiB) are written to the transcript as a core
  notice, the operator's only record of what setup did.
- **User env cannot shadow system vars.** Custom variables are added only
  where they name nothing the core already set (ADR-0001), and reserved names
  are refused at the form, so a script can never be pointed at a credential the
  operator typed.

## Amendment (2026-08-09): the stamp is to the millisecond, and never repeats

A second is not fine enough
([#68](https://github.com/CorVous/CorCode/issues/68)): two archives of one
chat can fall inside the same second, and the second push is refused as a
non-fast-forward on a branch the remote already has. The stamp now carries
milliseconds (`-chkpt-20260805T214033172`), and a core process never mints two
checkpoints from the same millisecond: a stamp no later than the last one is
taken to be the millisecond after it, so names rise even when the clock does
not. A collision is impossible within a process; across processes the
millisecond makes it improbable.
