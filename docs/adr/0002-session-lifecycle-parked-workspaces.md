# ADR-0002: Session lifecycle — disposable containers, parked workspaces, archive deletes

Date: 2026-08-05
Status: Accepted, amended by ADR-0005 (parking no longer forces a checkpoint)
Wayfinder: [decision #5](https://github.com/CorVous/CorCode/issues/5)

## Context

The MVP brief said "containers tear down fully on exit; keep 1–2 warm for
quick resume," which left "exit" and "warm" ambiguous. The owner's actual
constraints: sessions explicitly closed must close; sessions left open must
never be auto-closed or lose work; git is the source of truth; and workspace
folders must not accumulate on the NAS. Agents also produce state outside
git's reach (gitignored build artifacts, out-of-repo scratch) that naive
teardown would destroy.

## Decision

**A session is not a container.** The chat is the durable object (transcript +
manifest on the NAS, history in git); containers are disposable resources.

Each open session owns a **workspace directory on a NAS dataset**, bind-mounted
into its container. Invariant: **a workspace dir exists iff its session is
open.** Git remains the durable source of truth; the dataset dir is a working
cache that makes park/resume lossless and fast (no re-clone, gitignored
artifacts survive).

Lifecycle rules:

1. **Close/archive (explicit)**: final commit pushed (gate) → transcript
   flushed → container destroyed → workspace dir deleted. Archived chats hold
   zero workspace disk and remain revivable from git + transcript alone.
2. **Idle open sessions**: never auto-closed. The 2 most recently touched keep
   live containers (cap-only LRU, no TTL). Older ones are **parked**:
   transcript flushed, container torn down, workspace dir retained. Resume is
   remount-and-go. _(Amended by ADR-0005: no forced WIP checkpoint — dir
   retention alone carries progress.)_
3. **Push failure blocks teardown** in every path — the container stays up and
   the UI flags it. Work is never destroyed unless git has it.
4. **Orphan sweep**: the core reconciles dataset dirs against the session
   manifest; dirs with no open session are flagged and removed. "No piles"
   is an enforced invariant, not a hope.
5. **Reviving an archived chat**: the core injects an automated notice into
   the agent's context before the first user turn — workspace reset to a
   fresh clone of the repo/branch at the recorded commit; prior untracked/
   gitignored files are gone. Parked-session resume needs no notice (the
   workspace is intact).

## Amendment (2026-08-05): a configured cap, and a sweep that yields

Rule 2's "2" is the default of `CORCODE_WARM_POOL`, not a constant: a bigger
box can hold more warm chats without a new decision. The order is the
manifest's `last_active_at`, written once per completed turn (ADR-0006), and
the cap is enforced after each spawn and each turn.

A turn in flight outranks the cap. `last_active_at` is written when a turn
ends, so the chat that has been answering longest reads as the stalest chat
there is, and parking it would kill the agent mid-sentence. A chat holding its
connection keeps its container, and the pool runs over its cap for as long as
that turn does. The same lock refuses the archive gate: rule 3's commit and
teardown must never happen under an agent writing into the tree.

Rule 4 gets one exception. A workspace dir no open chat claims but whose
container is still up is a contradiction the sweep cannot resolve by
deleting: pulling the tree out from under a running agent destroys work that
git does not have, which rule 3 forbids. The sweep says so loudly and removes
nothing.

## Consequences

- Container count is bounded (2 lingering + active) regardless of how many
  chats stay open; NAS disk is bounded by open sessions only.
- _(Superseded by ADR-0005)_ ~~Parking depends on a host-triggerable WIP
  checkpoint~~ — the cadence design kept the host-run checkpoint only for
  close/archive; parked unpushed work lives on the NAS dataset until resume.
- Resume flow (issue #9) must implement the archive-revival reset notice and
  the parked/archived branch in its state machine.
- Long-forgotten open chats hold workspace disk until closed — accepted;
  they hold no container.
