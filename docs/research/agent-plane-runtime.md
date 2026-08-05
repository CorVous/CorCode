# Agent-plane runtime options on TrueNAS (Docker siblings vs Podman variants)

Research for GitHub issue [CorVous/CorCode#11](https://github.com/CorVous/CorCode/issues/11)
(part of #1). Builds on prior findings in
[`docs/research/truenas-hosting.md`](https://github.com/CorVous/CorCode/blob/research/truenas-hosting/docs/research/truenas-hosting.md)
(issue #2), which covered running the core itself as a TrueNAS Custom App and
spawning sibling containers on the native Docker socket. This brief goes one
level deeper: **given the core runs as a Docker Custom App, where should the
dynamically-spawned per-chat agent containers actually run?**

This is a decision brief, not a decision. Trade-offs are presented neutrally;
the choice is made in the follow-up ticket
([CorVous/CorCode#12](https://github.com/CorVous/CorCode/issues/12)) with the
user.

## Question

Compare four shapes for the agent plane (the per-chat, dynamically
spawned/torn-down worker containers), each evaluated on: isolation/escape
blast radius, upgrade & reboot survival, ops burden, how spawn/teardown and a
small (1–2 container) warm pool work over the runtime's API, and image
build/distribution.

- (a) Hardened sibling containers on TrueNAS's native Docker socket
- (b) Rootless Podman inside a TrueNAS Instance (LXC via Incus, SCALE 25.04+)
- (c) Podman installed directly on the TrueNAS host OS
- (d) Everything in one Instance: core + rootless Podman both in a single LXC

## Background: TrueNAS SCALE Instances (LXC/Incus) maturity

Fangtooth (25.04) introduced "Instances" — LXC system containers and
QEMU/KVM VMs, both managed through Incus — as a companion to the Docker-based
Apps system covered in issue #2. This is new territory relative to that prior
research and matters for every Podman-in-LXC option below:

- Instances launched in 25.04 explicitly labeled **experimental, "intended
  for community testing only,"** with docs warning that "functionality could
  change significantly between releases, and containers might not upgrade
  reliably."
  [25.04 Fangtooth release blog](https://www.truenas.com/blog/truenas-fangtooth-25-04-release/),
  [25.04 Containers tutorial](https://www.truenas.com/docs/scale/25.04/scaletutorials/containers/)
- As of the current stable line, **25.10 "Goldeye," Instances/LXC remain
  experimental** — the 25.10 beta announcement still describes "the
  experimental lightweight Linux Containers (LXC)... available under the
  Instances tab."
  [TrueNAS 25.10 Goldeye beta announcement](https://www.truenas.com/blog/truenas-goldeye-25-10-beta/)
- The experimental label is dropped in **TrueNAS 26** (in development as of
  this writing, Aug 2026): "LXC containers, introduced as an experimental
  feature in earlier TrueNAS releases, are fully supported in TrueNAS 26, and
  no configuration migration is required for containers created in prior
  releases."
  [TrueNAS 26 coverage, XDA Developers](https://www.xda-developers.com/truenas-isnt-trying-proxmox-getting-close-home-lab-users/)

Net effect: any option built on Instances today is built on an experimental
subsystem on the current stable release, with a stated (and so far honored)
promise of forward migration once it graduates in 26.

## (a) Hardened sibling containers on the Docker socket

Same mechanism as the core-spawning-siblings pattern from issue #2, applied
to per-chat agent containers instead: the core (already holding
`/var/run/docker.sock`) creates/destroys agent containers as ordinary
Docker siblings, hardened per-container.

**Hardening levers, confirmed available in plain `docker run`/Compose:**
`cap_drop: [ALL]` with only needed caps added back, `security_opt:
[no-new-privileges:true]`, `read_only: true` root filesystem with an
explicit `tmpfs` for scratch space, non-root `user:`, and per-container
resource limits (`mem_limit`, `cpus`/`cpu_shares`). Recommended baseline:
drop all capabilities, never add `SYS_ADMIN` back (it's a near-total
capability grant, almost never actually needed), and put agent containers on
an internal/isolated network rather than the default bridge so they can't
reach each other or the core's management surface.
[cap_add/cap_drop guide](https://oneuptime.com/blog/post/2026-02-08-how-to-use-docker-compose-capadd-and-capdrop/view),
[Compose Tip #29: capabilities and security options](https://lours.me/posts/compose-tip-029-container-capabilities/),
[hardening untrusted/trusted containers together](https://medium.com/@SecurityArchitect/docker-security-settings-for-running-untrusted-trusted-containers-at-the-same-time-88c4ca012726)

**userns-remap verified unavailable on TrueNAS SCALE.** This was the one
open item this ticket specifically had to check. Findings: TrueNAS's
middleware owns and generates `/etc/docker/daemon.json` and the Apps
lifecycle around it; there is no UI/API surface for `--userns-remap` today.
The only comparable escape hatch — a forum feature request asking for free-
form custom `daemon.json` content (registry mirrors/proxies) — was marked
"[Implemented 25.10]" for that narrow use case, not for user-namespace
remapping generally, and no evidence turned up of `userns-remap` support
being added or requested with traction. No official TrueNAS documentation
mentions `userns-remap` at all. Practical conclusion: **on TrueNAS's managed
Docker daemon, container root is real (unmapped) host UID 0 unless the
container itself runs as a non-root `user:`; there is no daemon-level
second line of defense the way `userns-remap` would provide on vanilla
Docker.** All isolation has to come from capability dropping, non-root user,
seccomp/AppArmor defaults, and read-only/no-new-privileges — the same tools
as any Docker sibling-container setup, just without the belt of user-ns
remapping alongside the suspenders.
[TrueNAS custom daemon.json feature request thread](https://forums.truenas.com/t/add-ability-to-have-custom-docker-json/26046),
[registry-mirrors daemon.json request, implemented 25.10](https://forums.truenas.com/t/implemented-25-10-allow-specify-registry-mirrors-or-proxies-in-etc-docker-daemon-json/13068),
[Docker userns-remap docs, for what the feature would otherwise provide](https://docs.docker.com/engine/security/userns-remap/)

**Isolation/escape blast radius:** shares a kernel and a Docker daemon with
the core and with TrueNAS's own Apps. A container escape (kernel exploit, or
a capability misconfiguration letting a process reach the daemon) lands
directly on the TrueNAS host — the same "socket access is host-root-
equivalent" caveat from issue #2 applies to whatever process ends up able to
reach `docker.sock`, and per-chat agent containers should never be the ones
holding that socket themselves. Without userns-remap, this is the
highest-blast-radius-on-compromise option of the four, mitigated only by
capability/seccomp hardening.

**Upgrade & reboot survival:** identical story to the core service from
issue #2 — this is the most-proven path since it's the same native Docker
backend TrueNAS has supported since 24.10 "Electric Eel," now on its second
major release (25.10) with no backend change signaled. Agent containers
themselves are ephemeral/reconciled by the core, so survival really only
matters for the Docker daemon and the core, both already covered.

**Ops burden:** lowest of the four — no second container runtime to
install, patch, or reconcile with TrueNAS updates; no cross-boundary API
transport to secure; the core already has this exact socket wired up for its
own lifecycle per issue #2.

**Warm pool / spawn-teardown over the API:** straightforward — `bollard`
(the Rust Docker client referenced in issue #2) against the same
`docker.sock` the core already uses. Pre-create 1–2 containers with the
hardened flags above, `start` immediately before use to reduce time-to-first-
token, `stop`/`remove` on chat end. No additional network hop — same
daemon, same host.

**Image build/distribution:** identical to issue #2's findings — `docker
build` directly on the TrueNAS shell (or build elsewhere, `docker save` /
SMB copy / `docker load`), reference locally without a registry, keep the
Dockerfile/build context on a data-pool dataset.

## (b) Rootless Podman inside a TrueNAS Instance (LXC via Incus, SCALE 25.04+)

The core (Docker app) stays where it is; per-chat agent containers run
inside Podman, itself running rootless inside an unprivileged LXC Instance.

**Isolation/escape blast radius:** two nested boundaries instead of one —
(1) the LXC Instance is, by default, an **unprivileged container**: "root
user inside the container is mapped to an unprivileged UID range on the
host... even if an attacker gains root inside the container, they have no
privileges on the host system." (2) Podman itself runs **rootless** inside
that Instance, adding a second user-namespace layer on top. In principle
this is defense in depth beyond option (a)'s single Docker daemon.  In
practice, opinion is split on whether nesting rootless-inside-unprivileged
actually buys much: one Proxmox forum position is that it's "doing the
isolation work twice... root in the container doesn't equal root on the
host" already, so the inner rootless layer is marginal; the counter-view is
that it still meaningfully caps a compromised agent container from reaching
even the LXC Instance's root, only the mapped range. Running Podman rootful
inside the Instance would give up that second layer entirely — rootless is
what makes this option's isolation story different from (a).
[Security of Docker/Podman in LXC discussion](https://discuss.linuxcontainers.org/t/security-of-docker-podman-in-lxc/18248),
[Podman in rootless mode on LXC — Proxmox forum](https://forum.proxmox.com/threads/podman-in-rootless-mode-on-lxc-container.141790/),
[TrueNAS unprivileged-by-default confirmation](https://forums.truenas.com/t/linux-jails-containers-vms-with-incus/23599)

**How the Docker-app core reaches the Podman REST API across the boundary:**
this is the crux of option (b), and it's a real integration cost, not a
detail. Podman's own docs say the REST/Libpod API is Docker-v1.40-compatible
(so `bollard`-style clients mostly work against it) but is designed to be
reached either via a local unix socket or via SSH tunnel — TCP exposure is
explicitly discouraged without mutual TLS ("strongly recommend against"
exposing over TCP without mTLS; "even localhost binding is risky"), and the
official Python bindings (`podman-py`) don't implement TCP at all as of this
writing. [podman-system-service docs](https://docs.podman.io/en/latest/markdown/podman-system-service.1.html),
[podman-py README](https://github.com/containers/podman-py/blob/main/README.md)
Two realistic transports for a Docker-container-to-LXC-Instance hop:
  - **TCP + mTLS across the TrueNAS bridge network.** Both the core's
    Docker app and the LXC Instance can sit on the same `truenasbr0` bridge
    (or a custom bridge) and reach each other by IP — this is the
    documented, TrueNAS-native way containers/VMs/Instances talk to each
    other and to the host.
    [Accessing NAS from VMs and Containers](https://www.truenas.com/docs/scale/25.04/scaletutorials/network/containernasbridge/),
    [Setting Up a Network Bridge](https://www.truenas.com/docs/scale/network/interfaces/settingupbridge/)
    Requires standing up `podman system service --tls-cert ... tcp://...`
    inside the Instance and a matching client cert on the core side — real
    but bounded ops work, and the one path Podman's own docs actually
    endorse for non-local access.
  - **Shared unix socket via a bind-mounted host path.** In principle the
    Podman socket could live under a directory that's both an Incus
    "filesystem device" mount into the Instance and a Docker host-path mount
    into the core's container — AF_UNIX sockets are reachable across mount
    namespaces via a shared inode the same way `docker.sock` sibling-mounting
    works today. This was not found documented anywhere for this exact
    Podman-in-Instance shape (unlike TCP+mTLS, it's inferred from how
    bind-mounted sockets behave generally, not verified against a TrueNAS
    Instances writeup) and has a UID-shift wrinkle: because the Instance is
    unprivileged, the socket file's owner UID as seen from outside the
    Instance is a shifted, unmapped host UID, so the core's container would
    need matching UID mapping or a permissive socket mode (e.g. `0666`) —
    itself a hardening trade-off. Treat as unconfirmed/needs a spike, not as
    load-bearing for the decision.

**Nesting/rootless constraints inside LXC:** confirmed non-trivial setup,
consistently reported across Proxmox/Incus/LXC community sources (TrueNAS
Instances uses the same Incus/LXC stack, so these transfer directly):
  - The Instance needs `security.nesting=true` set — not exposed in the
    TrueNAS Instances **web UI** as of this writing; has to be applied via
    the `incus` CLI from the TrueNAS shell or the middleware API directly
    (community reports also layer on `security.syscalls.intercept.mknod` /
    `.setxattr`, and some use `raw.lxc` passthrough).
    [Setup Podman on LXC gist](https://gist.github.com/GiovanniGrieco/b5a1ec548b993c8bc71c24f4b069d83a),
    [What does security.nesting=true? — LXD forum](https://discuss.linuxcontainers.org/t/what-does-security-nesting-true/7156)
  - Rootless Podman additionally needs `/etc/subuid`/`/etc/subgid` entries
    for the in-Instance user, and `/dev/net/tun` access if using
    slirp4netns/pasta for rootless networking.
  - Nesting an unprivileged rootless layer inside an already-unprivileged
    LXC Instance is a **known source of friction, not a solved recipe**:
    common failure modes reported include `newuidmap` UID/GID-range errors,
    boot-ID cache mismatches after Instance reboot requiring manual cleanup
    under `/tmp`, and (TrueNAS/ZFS-specific) Podman's overlay storage
    needing `acltype=posixacl` set on the backing dataset.
    [Podman in rootless mode on LXC — Proxmox forum](https://forum.proxmox.com/threads/podman-in-rootless-mode-on-lxc-container.141790/),
    [Rootless Docker inside unprivileged LXC — Proxmox forum](https://forum.proxmox.com/threads/rootless-docker-inside-unprivileged-lxc-container.91146/)
  - Separately, physical network device passthrough is reported to
    malfunction once `security.nesting=true` is set — worth checking if the
    Instance's networking plan involves anything beyond the standard bridge.
    [Incus issue #1774](https://github.com/lxc/incus/issues/1774)

**Storage for workspaces:** TrueNAS Instances support mounting host
datasets into a container as either a newly-created dataset or an existing
one via the Instances "disk"/"Filesystem Devices" UI, and separately track
zvol-backed volumes per Instance under Instances → Configuration → Manage
Volumes. This is a parallel, Instances-specific storage system from the
Docker-app "Host Path"/ixVolume mechanism covered in issue #2 — chat
workspace data would need its own convention here (e.g. a
`tank/apps/corcode/agent-workspaces` dataset mounted into the Instance),
distinct from wherever the core's own data lives under the Docker app.
[TrueNAS Containers tutorial](https://www.truenas.com/docs/scale/25.04/scaletutorials/containers/),
[Can LXC containers mount a ZFS dataset from TrueNAS host? — forum](https://forums.truenas.com/t/can-lxc-containers-mount-a-zfs-dataset-from-truenas-host/67173)

**Upgrade & reboot survival:** Instances are TrueNAS-managed (they show up
in the UI, have their own zvol storage, survive reboot) but sit on the
**experimental** subsystem flagged above — 25.10 docs still warn
functionality "could change significantly between releases" until the
TrueNAS 26 graduation. This is a materially different risk profile than
option (a)'s Docker path, which has two stable releases of track record.

**Ops burden:** highest of the four options that don't touch the host OS
directly — a second container engine (Podman) to keep patched inside the
Instance, `security.nesting`/subuid configuration to apply and preserve, and
a cross-boundary API transport (TCP+mTLS, most likely) to build and secure,
on top of an experimental TrueNAS subsystem.

**Warm pool / spawn-teardown over the API:** works the same shape as option
(a) once the transport is solved — Podman's Docker-compatible API layer
means `bollard` (or `podman-py` if reimplemented in whatever language calls
it) can `create`/`start`/`stop`/`remove` containers the same way. The actual
lifecycle mechanics (pre-warm 1–2, start on demand, reap on chat end) don't
differ from (a); only the transport to reach the API differs.

**Image build/distribution:** Podman's `podman build` (or `buildah`) can
build images rootless inside the Instance directly — no separate build host
needed, mirroring option (a)'s on-box `docker build`. Images built elsewhere
can move via `podman save`/`podman load`, or `skopeo copy`, analogous to the
`docker save`/`docker load` path from issue #2.

## (c) Podman installed directly on the TrueNAS host OS

Bypasses both the Docker Custom App and Instances entirely: install Podman
straight onto TrueNAS's own Debian-based host OS via developer mode.

**Developer-mode/immutable-rootfs reality:** TrueNAS SCALE ships an
unsupported developer mode (`install-dev-tools` in recent releases) that
restores `apt`, compilers, etc. It comes with an explicit support
consequence: **"iX will automatically delete any support requests you
generate"** once developer mode is enabled — it is not meant for deployed
systems. TrueNAS's boot environments are effectively an immutable/replaced
root filesystem across updates: "running stuff on the Linux that's inside
SCALE is totally unsupported, potentially fatal to stability."
[Persistent Debian jail gist, covering developer-mode context](https://gist.github.com/Jip-Hop/4704ba4aa87c99f342b2846ed7885a5d),
[TrueNAS community: what deployment modes are supported](https://www.truenas.com/community/threads/what-deployment-modes-are-supported.112487/)

**Concrete evidence on surviving TrueNAS upgrades — this was the ticket's
explicit ask, and the evidence is consistently negative:**
  - Community consensus, stated directly: "plain Docker support is not
    really a goal for TrueNAS SCALE — there are ways to get it to work, but
    none are easily supported."
  - The clearest documented failure mode: shell/post-init scripts survive
    an upgrade, but **compiled kernel modules do not** — `make`/`install`
    tooling isn't present post-upgrade unless developer mode is re-enabled
    each time. Podman itself doesn't need a kernel module, but this
    illustrates how thoroughly the upgrade process replaces the OS layer
    that a directly-installed Podman would depend on (its binary, its
    systemd units, `/etc/subuid`/`subgid`, its container storage under
    `/var/lib/containers`).
  - The TrueNAS community's own answer to "I need Podman/Docker without
    touching the host" was **not** "install it and it survives" — it was
    **Jailmaker**, a systemd-nspawn-based jail specifically built to install
    software like "docker-compose, portainer, podman, etc." *without
    modifying the host OS*, precisely because direct host installs don't
    reliably survive. Jailmaker itself is no longer maintained by its
    original author (last release v2.1.1, tested against 24.10) and predates
    the native Docker-app/Instances era covered elsewhere in this brief —
    included here only as corroborating evidence that direct-host-install
    was never a supported survival path, not as a live option in its own
    right.
    [Jailmaker gist](https://gist.github.com/Jip-Hop/4704ba4aa87c99f342b2846ed7885a5d),
    [Jailmaker repo](https://github.com/Jip-Hop/jailmaker),
    [Best way to run vanilla Docker? — TrueNAS forum](https://www.truenas.com/community/threads/best-way-to-run-vanilla-docker.108146/)

No source found describing a directly-host-installed Podman reliably
surviving a TrueNAS version upgrade (as opposed to a point-release patch);
every relevant thread either reports breakage or routes around the problem
via a jail/sandbox instead.

**Isolation/escape blast radius:** Podman itself can still run rootless
here, giving the same per-container isolation properties as option (b)'s
inner layer — but there's no LXC/Instance boundary around it at all, so a
kernel-level escape (or Podman daemon compromise) lands directly on the bare
TrueNAS host, with no unprivileged-container layer in between. This is
comparable to or worse than option (a) for blast radius, while carrying
strictly worse upgrade properties.

**Ops burden:** highest of all four in the specific sense that upgrade
survival isn't just "extra work," it's **not currently achievable through a
supported path** — every TrueNAS upgrade is a re-verify-and-possibly-
reinstall event for anything installed this way, plus the standing
support-ticket forfeiture from developer mode being enabled.

**Warm pool / spawn-teardown over the API:** functionally identical to
option (b)'s Podman API mechanics once installed, but reachable over
`localhost` (or a unix socket bind-mounted into the core's Docker container)
rather than needing a cross-Instance network hop — this is the one
genuine advantage over (b): no LXC boundary to tunnel across.

**Image build/distribution:** same as (b) — `podman build`/`buildah`
locally, `podman save`/`load` or `skopeo copy` to move images — but built
directly on host storage rather than Instance-scoped storage, so ordinary
data-pool dataset conventions from issue #2 apply without an extra storage
layer.

## (d) Everything in one Instance: core + rootless Podman, TrueNAS Docker untouched

Move the Rust core itself into the same LXC Instance as Podman, so both core
and agent runtime share one unprivileged container; the TrueNAS Docker/Apps
system is left alone entirely (not used for this workload at all).

**Isolation/escape blast radius:** collapses option (b)'s two-hop
core-to-Podman boundary into one process space — the core and Podman (and,
transitively, agent containers) now share a single unprivileged-LXC blast
radius instead of the core sitting in a separately-isolated Docker app. A
compromise of the core process has direct local access to the Podman socket
without crossing any network/API boundary, which is a smaller *lateral*
distance for an attacker who already got into the core, but the whole
package still benefits from the same unprivileged-Instance-to-host boundary
that protects option (b)'s Podman layer. Net: better boundary against the
TrueNAS host than option (a); worse boundary between "core" and "agent
runtime" than option (b), since those two are no longer separated at all.

**Upgrade & reboot survival:** inherits option (b)'s experimental-Instances
risk profile for the *whole system* now, including the core itself — under
option (b) only the agent plane was exposed to Instances' experimental
status; under option (d) the core's own survival is tied to it too. This is
the largest scope-of-exposure to Instances' current experimental label of
any option.

**Ops burden:** actually **lower** than (b) in one specific way — no
cross-boundary API transport to build/secure, since core and Podman are
colocated (same unix socket, no TLS/SSH tunnel needed). But it fully drops
the Docker-app benefits documented in issue #2: no Apps-middleware-tracked
install/upgrade/rollback lifecycle for the core, no Apps UI visibility, and
the core's own persistent data now needs the Instances storage/dataset
convention from option (b) rather than the Docker-app Host Path convention
issue #2 already worked out. Effectively trades "one integration boundary
to build" (the API transport) for "one lifecycle/tooling ecosystem to give
up" (Apps).

**Warm pool / spawn-teardown over the API:** simplest of the four
mechanically — core talks to Podman over a local unix socket inside the
same Instance, no network hop, same `bollard`-style client pattern as (a)
and (b) otherwise.

**Image build/distribution:** same as (b)/(c) — `podman build`/`buildah`
inside the Instance, `save`/`load`/`skopeo` to move images in or out.

## Comparison matrix

| | (a) Docker siblings | (b) Podman in Instance | (c) Podman on host OS | (d) Core+Podman in one Instance |
|---|---|---|---|---|
| Isolation boundary vs. TrueNAS host | Single Docker daemon; no userns-remap; capability/seccomp hardening only | Unprivileged LXC + rootless Podman (two nested layers) | Rootless Podman only; no container boundary around it | Unprivileged LXC + rootless Podman, but core shares that same boundary |
| Core-to-agent-runtime boundary | N/A (same process/daemon as core's own socket access) | Separate: network hop (TCP+mTLS) or unverified shared-socket trick | Separate but local (localhost/bind-mounted socket) | None — colocated, same unix socket |
| Subsystem maturity (as of 25.10) | Docker apps: stable since 24.10, 2 major releases | Instances: experimental since 25.04, still experimental in 25.10 | N/A (bare host OS) — but installs don't survive upgrades regardless | Instances: experimental since 25.04, still experimental in 25.10 |
| Upgrade survival evidence | Strong (same as core, per issue #2) | Depends on Instances graduating (due in 26); no breakage reported yet but subsystem young | Weak/negative — no confirmed case of surviving a major upgrade; community routes around it via jails | Same as (b), extended to cover the core too |
| Reboot survival | Strong, with restart policy (per issue #2) | Instance-level restart policy exists; Podman-inside needs its own systemd unit/socket-activation | Needs manual systemd unit; developer-mode re-setup risk after upgrade | Same as (b) |
| Ops burden | Lowest — one runtime, already wired up | High — second runtime + nesting/subuid config + cross-boundary transport | Highest in upgrade-survival terms — re-verify/reinstall each upgrade, support forfeited | Medium — second runtime + nesting/subuid, but no cross-boundary transport; loses Apps lifecycle for core |
| Warm-pool/spawn API | `bollard` on existing `docker.sock` | Docker-compatible Podman API; needs TCP+mTLS (or unconfirmed shared-socket) transport | Docker-compatible Podman API over localhost/local socket | Docker-compatible Podman API over local socket, no transport work |
| Image build/dist | `docker build` on box; `save`/`load` | `podman build`/`buildah` in Instance; `save`/`load`/`skopeo` | Same as (b), on bare host storage | Same as (b), inside the shared Instance |
| TrueNAS Apps UI visibility | Core tracked; agents deliberately not | Core tracked (Docker app); agent runtime/Instance separately visible in Instances UI | Neither tracked (host-level, unsupported) | Nothing tracked — core loses Apps UI/lifecycle entirely |

## Choose this if…

- **(a) Hardened Docker siblings** — choose this if minimizing new moving
  parts and staying entirely on the two-release-proven Docker-apps backend
  matters more than getting a second isolation layer around agent
  containers; accept that there's no userns-remap safety net and lean
  fully on capability dropping/read-only/no-new-privileges/internal
  networking instead.
- **(b) Rootless Podman in a TrueNAS Instance** — choose this if the
  extra unprivileged-LXC + rootless-Podman isolation layer around agent
  containers is worth taking on an experimental TrueNAS subsystem, a
  nesting/subuid setup with known rough edges, and a cross-boundary API
  transport to design and secure, while keeping the core on the
  proven, Apps-tracked Docker path.
- **(c) Podman on the TrueNAS host OS** — choose this only if something
  about the deployment specifically requires being outside any
  container/Instance boundary (e.g. needing capabilities Podman can't get
  inside LXC); the upgrade-survival evidence available today argues against
  it as a durable default, and the developer-mode support forfeiture is a
  standing cost, not a one-time one.
- **(d) Core + Podman together in one Instance** — choose this if avoiding
  a cross-boundary API transport (by colocating core and Podman) is worth
  giving up the Docker Apps lifecycle/UI for the core entirely and betting
  the whole system, not just the agent plane, on Instances graduating out
  of experimental status.

## Sources

- [TrueNAS 25.04 Fangtooth release blog](https://www.truenas.com/blog/truenas-fangtooth-25-04-release/)
- [TrueNAS 25.04 Containers tutorial](https://www.truenas.com/docs/scale/25.04/scaletutorials/containers/)
- [TrueNAS 25.10 Goldeye beta announcement](https://www.truenas.com/blog/truenas-goldeye-25-10-beta/)
- [TrueNAS 26 coverage — XDA Developers](https://www.xda-developers.com/truenas-isnt-trying-proxmox-getting-close-home-lab-users/)
- [Docker Compose cap_add/cap_drop guide](https://oneuptime.com/blog/post/2026-02-08-how-to-use-docker-compose-capadd-and-capdrop/view)
- [Compose Tip #29: container capabilities and security options](https://lours.me/posts/compose-tip-029-container-capabilities/)
- [Hardening Docker: settings for untrusted/trusted containers together](https://medium.com/@SecurityArchitect/docker-security-settings-for-running-untrusted-trusted-containers-at-the-same-time-88c4ca012726)
- [Docker userns-remap documentation](https://docs.docker.com/engine/security/userns-remap/)
- [TrueNAS forum: add ability to have custom docker.json](https://forums.truenas.com/t/add-ability-to-have-custom-docker-json/26046)
- [TrueNAS forum: registry-mirrors/proxies in daemon.json — implemented 25.10](https://forums.truenas.com/t/implemented-25-10-allow-specify-registry-mirrors-or-proxies-in-etc-docker-daemon-json/13068)
- [podman-system-service documentation](https://docs.podman.io/en/latest/markdown/podman-system-service.1.html)
- [podman-py README](https://github.com/containers/podman-py/blob/main/README.md)
- [TrueNAS: Accessing NAS from VMs and Containers](https://www.truenas.com/docs/scale/25.04/scaletutorials/network/containernasbridge/)
- [TrueNAS: Setting Up a Network Bridge](https://www.truenas.com/docs/scale/network/interfaces/settingupbridge/)
- [Security of Docker/Podman in LXC — Linux Containers forum](https://discuss.linuxcontainers.org/t/security-of-docker-podman-in-lxc/18248)
- [Podman in rootless mode on LXC — Proxmox forum](https://forum.proxmox.com/threads/podman-in-rootless-mode-on-lxc-container.141790/)
- [Rootless Docker inside unprivileged LXC — Proxmox forum](https://forum.proxmox.com/threads/rootless-docker-inside-unprivileged-lxc-container.91146/)
- [Setup Podman on LXC — gist](https://gist.github.com/GiovanniGrieco/b5a1ec548b993c8bc71c24f4b069d83a)
- [What does security.nesting=true? — LXD forum](https://discuss.linuxcontainers.org/t/what-does-security-nesting-true/7156)
- [Incus issue #1774: network device passthrough with security.nesting=true](https://github.com/lxc/incus/issues/1774)
- [TrueNAS forum: Can LXC containers mount a ZFS dataset from TrueNAS host?](https://forums.truenas.com/t/can-lxc-containers-mount-a-zfs-dataset-from-truenas-host/67173)
- [TrueNAS forum: Linux Jails (containers/vms) with Incus](https://forums.truenas.com/t/linux-jails-containers-vms-with-incus/23599)
- [Persistent Debian jail (Jailmaker) gist, developer-mode context](https://gist.github.com/Jip-Hop/4704ba4aa87c99f342b2846ed7885a5d)
- [Jailmaker repository](https://github.com/Jip-Hop/jailmaker)
- [TrueNAS forum: what deployment modes are supported?](https://www.truenas.com/community/threads/what-deployment-modes-are-supported.112487/)
- [TrueNAS forum: best way to run vanilla Docker?](https://www.truenas.com/community/threads/best-way-to-run-vanilla-docker.108146/)
