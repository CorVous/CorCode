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

## Consequences

- Chat branches carry only agent-authored semantic commits; mechanical
  commits exist solely on checkpoint branches at archive time.
- Checkpoint branches accumulate on GitHub (one per dirty archive) until
  manually deleted — no auto-cleanup in the MVP.
- Durability between pushes rests on the NAS dataset for parked sessions —
  the dataset is already trusted with transcripts.
- The web UI's new-chat dialog needs repo, base branch, optional branch name,
  and a work-directly-on-branch toggle (UI ticket).
