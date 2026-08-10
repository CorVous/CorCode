# ADR-0001: Agent plane runs as hardened Docker siblings, behind a swappable spawn interface

Date: 2026-08-05
Status: Accepted
Wayfinder: [decision #12](https://github.com/CorVous/CorCode/issues/12), research [#2](https://github.com/CorVous/CorCode/issues/2), [#11](https://github.com/CorVous/CorCode/issues/11), [#13](https://github.com/CorVous/CorCode/issues/13)

## Context

The core runs as a TrueNAS SCALE Custom App (Docker Compose) and must spawn one
isolated workspace container per chat. Four runtime shapes were researched for
that agent plane (`docs/research/agent-plane-runtime.md`, `docs/research/vm-agent-plane.md`):
hardened Docker siblings on the native socket, rootless Podman in an LXC
Instance, Podman on the host OS, and plain Docker inside a KVM VM. Constraints
that shaped the call: TrueNAS's managed Docker has no userns-remap; LXC
Instances remain experimental until TrueNAS 26; host-OS installs don't survive
upgrades; true microVMs (Firecracker/Kata) have no supported path on TrueNAS;
KVM VMs are mature but cost an always-on guest OS, a cert lifecycle, and
reserved RAM.

## Decision

For the MVP, agent containers spawn as **siblings on TrueNAS's native Docker
daemon**, hardened per container: `cap_drop: ALL` (nothing added back),
`no-new-privileges`, read-only root filesystem with tmpfs scratch and a
writable workspace mount, non-root user, memory/CPU limits, and an internal
network with no route to the core's management surface or the docker socket.
Agent containers never mount the docker socket.

The core's container-spawn logic lives behind a small trait so the Docker
endpoint is swappable — the same `bollard`/Docker-API calls can later target a
slim KVM guest's daemon over TCP+mTLS without a rewrite.

## Amendment (2026-08-07): the agent network is a plain bridge

"An internal network with no route to the core's management surface or the
docker socket" was implemented as `internal: true`, which also cuts the agents
off from api.anthropic.com and github.com: the first prompt on the NAS came
back `Unable to connect to API` and no turn could ever have run
([#43](https://github.com/CorVous/CorCode/issues/43)). The agents' network is
now an ordinary bridge — plain outbound, still not the core's own network, and
still no docker socket. A deployment holding the old internal network has it
replaced at the next spawn, unless containers are still on it, in which case
the spawn refuses and says to stop them.

Accepted risk: outbound includes the LAN, so a compromised agent can reach the
NAS's other services. That is the same MVP bet as the missing userns-remap —
the owner's own agents on the owner's repos — and the tightening is again ops,
not redesign: block RFC1918 destinations out of the agent bridge, or put the
agents behind an allowlist egress proxy, when the threat model outgrows the
MVP.

## Amendment (2026-08-07): the container is the permission boundary

The baked managed settings shipped no `permissions.defaultMode`, so every
agent ran in Claude Code's interactive default: it asked before acting, the
ACP client declined on its behalf, and the hardening above was paid for twice
([#49](https://github.com/CorVous/CorCode/issues/49)). The flags in this ADR
are the boundary; a second gate inside them is the same over-reading that
produced `internal: true`. `/etc/claude-code/managed-settings.json` now sets
`permissions.defaultMode` to **`auto`** — safe calls approved by the model's
own classifier, risky ones still classified.

Managed scope, because it is ours. The SDK's trust filter drops an escalating
default (`auto`, `acceptEdits`, `bypassPermissions`) from exactly one place:
repo-committed `project` settings, `.claude/settings.json` in the clone.
User, local, flag and managed scopes are all honored. The image bakes the
managed file, so the mode travels with the image the hardening flags belong to
and a cloned repository cannot set it, unset it, or escalate past it.

Three properties of headless auto mode the settings file has to respect:

- **No `permissions.ask` rules.** An ask reaches a client that auto-declines,
  and the turn dead-ends where a classifier would have carried it.
- **No reliance on broad allow rules.** Auto mode drops them; a `Bash(*)`
  allowlist would read as permission granted and grant nothing.
- **Denies are sticky for the run, and repeated classifier blocks abort it.**
  A headless turn that keeps reaching for the same blocked call does not
  recover by retrying — it ends. Work the agent must do belongs inside the
  container's own writable surface, not behind a call the classifier stops.

**Known blind spot: nothing watches the mode a session actually opened in.**
Adapter 0.66.0 clamps silently — a model whose session reports no auto support
gets `default`, logged to the adapter's stderr and nowhere else. The one
observable is `modes.currentModeId` in the `session/new` result, and the core
keeps only `sessionId` from that response (`src/acp/mod.rs`), so no test can
reach it without the core surfacing it first. The image-side check proves the
settings resolve to `auto`; that the session honored it is unproven, and a
whole deployment could quietly fall back to asking. Surfacing the session's
mode — logged at open, or asserted in the docker-gated vertical — is the fix,
and it is a core change this image increment did not make.

**Closed ([#58](https://github.com/CorVous/CorCode/issues/58)).** The core
reads `modes.currentModeId` from the `session/new` and `session/load` answers,
logs it at open, and writes a `mode_notice` into the chat's own log (ADR-0006)
when it is not the mode the baked managed settings ask for — which is the one
place the core reads that mode from, so the file the image ships stays the
only place it is written down. An adapter that names no mode is a case, not a
fault: nothing is known, so nothing is claimed. The docker-gated vertical
reads the mode a real session opened in back off that log line and asserts it
is the managed one, so an adapter that renames the field fails the gate rather
than passing it by saying nothing.

## Consequences

- Simplest possible MVP: one runtime, no new infra, the socket the core
  already holds.
- Accepted risk: no userns-remap means a kernel-level container escape from an
  agent is host root. Threat model for the MVP is the owner's own agents on
  the owner's repos; hardening flags are the only mitigation.
- The escape hatch is an ops migration, not a redesign: stand up a VM
  (ADR-worthy when it happens), repoint the endpoint. The Podman/LXC shape is
  explicitly dropped — most friction, middle isolation, experimental base.
