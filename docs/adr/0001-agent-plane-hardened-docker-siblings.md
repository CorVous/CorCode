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

## Consequences

- Simplest possible MVP: one runtime, no new infra, the socket the core
  already holds.
- Accepted risk: no userns-remap means a kernel-level container escape from an
  agent is host root. Threat model for the MVP is the owner's own agents on
  the owner's repos; hardening flags are the only mitigation.
- The escape hatch is an ops migration, not a redesign: stand up a VM
  (ADR-worthy when it happens), repoint the endpoint. The Podman/LXC shape is
  explicitly dropped — most friction, middle isolation, experimental base.
