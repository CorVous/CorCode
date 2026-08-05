# VM outer layer for the agent plane (Docker inside a TrueNAS VM)

Research for GitHub issue [CorVous/CorCode#13](https://github.com/CorVous/CorCode/issues/13)
(part of #1). Extends the four-option brief in
[`docs/research/agent-plane-runtime.md`](https://github.com/CorVous/CorCode/blob/research/agent-plane-runtime/docs/research/agent-plane-runtime.md)
(issue #11), which itself built on
[`docs/research/truenas-hosting.md`](https://github.com/CorVous/CorCode/blob/research/truenas-hosting/docs/research/truenas-hosting.md)
(issue #2). This brief adds a fifth shape, **option (e)**: the agent plane
lives entirely inside a TrueNAS-hosted VM running a plain Docker daemon; the
core (still a TrueNAS Docker Custom App) drives that daemon remotely over the
network instead of sharing a socket or an LXC boundary.

This is a decision brief, not a decision. Trade-offs are presented neutrally;
the choice is made in [CorVous/CorCode#12](https://github.com/CorVous/CorCode/issues/12)
with the user.

## Question

- (e) Core (Docker Custom App) drives a plain Docker daemon running inside a
  TrueNAS-hosted VM, reached over the network (TCP+mTLS), instead of a Docker
  socket (a) or a Podman-in-LXC boundary (b)/(d).

Evaluated on the same axes as the prior brief, plus VM-specific questions the
ticket called out explicitly: Instances-VM maturity on 25.10, guest RAM/disk
floor, the remote-Docker-API transport, VM lifecycle, and workspace storage
into the VM.

## Headline finding: VMs are not on the experimental subsystem the ticket assumed

The ticket frames this as "Docker inside an **Instances** VM," carrying over
option (b)'s framing where Instances (Incus-based LXC/VM) is the experimental
subsystem flagged in the prior brief. **That framing turned out to be stale
by 25.10.** The actual history:

- TrueNAS 25.04.0/25.04.1 ("Fangtooth") initially routed *both* LXC and VM
  creation through Incus under a single "Instances" surface — this is what
  the 25.04 blog and containers tutorial describe, and what the prior brief's
  "Instances" background section (still accurate for LXC) was written
  against.
  [25.04 Fangtooth release blog](https://www.truenas.com/blog/truenas-fangtooth-25-04-release/)
- That specific choice — moving VM management onto Incus — drew heavy
  community pushback in 25.04.0/25.04.1: broken/frozen VM migrations ("QEMU
  HARDDISK... Not Found" on boot), containers/VMs disappearing after upgrade,
  more complex network configuration, and an Instances tab that could break
  entirely if the system dataset pool moved. One forum summary of the
  sentiment: shipping "an admittedly incomplete and unstable implementation
  due to the 6-month release schedule while removing the previous stable
  management system" did not build confidence.
  [Fangtooth BETA: big changes for VMs — forum thread](https://forums.truenas.com/t/fangtooth-beta-25-04-big-changes-for-vms-dont-get-intentionality-of-experimental-language/34564),
  [Update from 25.04.1 to 25.04.2.1 Incus migration workaround](https://forums.truenas.com/t/update-from-25-04-1-to-25-04-2-1-incus-migration-work-around/50379)
- iX reversed course in **25.04.2**: reintroduced "classic," libvirt-based VM
  management under a separate **Virtual Machines** screen, and confined Incus
  to LXC only going forward. New VM creation through the Instances/Incus path
  ended at that point.
  [TrueNAS 25.04.2: Fangtooth re-enables "Virtualization"](https://www.truenas.com/blog/truenas-fangtooth-25-04-2/),
  [TrueNAS Virtualization Plans for 25.04.2 — forum](https://forums.truenas.com/t/truenas-virtualization-plans-for-25-04-2/46236)
- **As of 25.10 "Goldeye," the split is permanent and is the current shipping
  architecture:** "TrueNAS 25.10 includes separate tabs for its two different
  Virtualization solutions. The experimental lightweight Linux Containers
  (LXC) are available under the Instances tab, with full KVM-powered Virtual
  Machines (VM) available under the Virtualization tab." VMs in 25.10 gained
  Secure Boot, multi-format disk import/export (QCOW2, RAW, VDI, VHDX, VMDK),
  and Enterprise HA failover — features framed under the "TrueNAS Data
  Hypervisor" (KVM + ZFS + HA) branding. HA failover is Enterprise-gated, but
  "the same virtualization technology is available in the Community Edition
  for all use cases." **No experimental label appears anywhere in the 25.10
  docs or release notes for the Virtualization/VM path** — only LXC Instances
  retain it ("intended for community testing only").
  [TrueNAS 25.10 Goldeye beta announcement](https://www.truenas.com/blog/truenas-goldeye-25-10-beta/),
  [TrueNAS 25.10 Goldeye Highlights](https://www.truenas.com/blog/truenas-goldeye-25-10/),
  [25.10 Version Notes](https://www.truenas.com/docs/scale/25.10/gettingstarted/versionnotes/)
- The one lingering scar from the 25.04.0/25.04.1 episode: VMs that were
  *created* during that narrow window via the old Instances/Containers screen
  "do not autostart in 25.10 or later," specifically to prevent zvol
  conflicts with VMs created on the new Virtual Machines screen. This is a
  migration-hygiene issue for pre-existing VMs, not a property of VMs created
  fresh on 25.10's Virtual Machines screen, which is what option (e) would
  use.
  [25.10 Version Notes](https://www.truenas.com/docs/scale/25.10/gettingstarted/versionnotes/)

**Practical conclusion for this ticket:** option (e)'s VM does not sit on the
experimental subsystem at all. It sits on the *reverted-to-classic*,
libvirt/KVM "Virtualization" path — the same VM technology TrueNAS has
shipped in some form since long before the Docker-apps era, now re-branded
and re-invested in as the "Data Hypervisor," with the only 25.04-era churn
being the (since-corrected) detour through Incus. This is a materially better
maturity story than the Instances/LXC path options (b) and (d) in the prior
brief depend on, and better than this ticket itself assumed going in.

## How slim a Docker-host guest can be

- **Alpine.** Base idle memory is commonly reported under ~50 MB before any
  workload runs, and Alpine installs are usable with as little as 128 MB of
  RAM in some hypervisor contexts, which is why it's a common choice for
  small utility VMs. Base image size (~5 MB) is roughly a quarter of Debian
  Slim's (~22 MB) — the gap carries through to a running guest because Alpine
  skips systemd and uses musl/BusyBox instead of a full glibc userland.
  [Best Linux Distros for Low-Memory Servers](https://factually.co/product-reviews/electronics-tech/best-linux-distros-low-memory-servers-containers-alpine-tinycore-busybox-eee0f9),
  [Comparing Debian vs Alpine for containers — TurnKey](https://www.turnkeylinux.org/blog/alpine-vs-debian)
- **dockerd itself** adds an anecdotally-reported ~50–150 MB RSS on top of
  the base OS idle figure (no single authoritative benchmark found; treat as
  a planning range, not a verified number).
- **Purpose-built container-host distros** (Flatcar Container Linux, Fedora
  CoreOS) are the other realistic option: both ship a container runtime
  (Flatcar: Docker/containerd out of the box; Fedora CoreOS: Moby/Podman) as
  a first-class component, use atomic/image-based updates, and are
  specifically positioned to minimize attack surface and image size. Flatcar's
  raw disk image is small out of the box (~4.4 GiB observed on a real
  deployment, EFI + two 1G USR partitions for A/B updates + ~2.1G root),
  expanding as needed; bare-metal install guidance asks for at least 8 GB
  disk / 2 GB RAM to boot. The trade-off against Alpine/Debian: provisioning
  goes through Ignition (declarative first-boot config), not a familiar
  interactive install or a TrueNAS-native cloud-init flow, which is a real
  setup-cost line item, not just a footnote.
  [Flatcar FAQ](https://www.flatcar.org/faq),
  [Flatcar disk layout on Linode — issue thread](https://github.com/flatcar/Flatcar/issues/1875),
  [I Installed Flatcar Linux on Proxmox — Virtualization Howto](https://www.virtualizationhowto.com/2026/04/i-installed-flatcar-linux-on-proxmox-and-its-not-like-a-normal-linux-vm/)
- **Debian**, as the highest-compatibility, best-documented option (and the
  same base OS family TrueNAS SCALE's own host runs), idles noticeably higher
  than Alpine due to systemd + glibc, but is the safest choice if anything
  in the agent-container images or tooling assumes glibc.

**Planning numbers, not measured on real TrueNAS hardware for this project:**
guest OS + dockerd realistically needs a **several-hundred-MB floor**
(Alpine) to **~1 GB** (Debian) before any agent container runs; add whatever
1–3 warm-pool agent containers actually need (workload-dependent, not
estimated here) on top. Disk: Alpine guest root well under 1 GB; Flatcar/CoreOS
in the low single-digit GB; Debian minimal in the 1–2 GB range — in all cases
plan for additional space for pulled/built agent images inside the guest's own
Docker storage, which is separate from whatever workspace storage volume is
attached (see below).

**virtio performance considerations:** TrueNAS's own VM wizard recommends
VirtIO over AHCI disks for performance when the guest OS supports VirtIO
drivers (all realistic Linux guest candidates here do), noting AHCI exists
mainly for Windows compatibility. Real-world numbers are workload-dependent —
one independent benchmark of a virtualized TrueNAS-on-Proxmox setup saw
~280 MB/s write / ~560 MB/s read, comparable to a physical NVMe virtual disk
— but community threads also report VMs feeling sluggish on RAIDZ1-backed
zvols despite VirtIO, so vdev topology under the zvol matters as much as the
disk-type choice itself.
[TrueNAS Virtual Machines tutorial (25.10)](https://www.truenas.com/docs/scale/25.10/scaletutorials/virtualmachines/),
[Benchmarking a TrueNAS SCALE VM in Proxmox](https://syntacticsugar.nl/benchmarking-a-truenas-scale-vm-in-proxmox),
[Sluggish VM? — TrueNAS forum](https://www.truenas.com/community/threads/sluggish-vm.102709/)

## Docker Engine remote API over TCP + mTLS

This is the one genuinely new integration surface option (e) introduces
relative to (a): the core has to reach a Docker daemon it does not share a
kernel or a bind-mounted socket with.

- **Daemon-side setup is a first-class, documented Docker feature** (unlike
  Podman's TCP path in the prior brief, which Podman's own docs "strongly
  recommend against" without mTLS and don't fully support in their official
  Python bindings). Configure via `daemon.json`:
  ```json
  {
    "hosts": ["unix:///var/run/docker.sock", "tcp://0.0.0.0:2376"],
    "tls": true,
    "tlsverify": true,
    "tlscacert": "/etc/docker/certs/ca.pem",
    "tlscert": "/etc/docker/certs/server-cert.pem",
    "tlskey": "/etc/docker/certs/server-key.pem"
  }
  ```
  or the equivalent `dockerd` flags. **`tlsverify` (not just `tls`) is the
  setting that actually authenticates clients** — `tls` alone only encrypts
  the channel. Port 2376 is the TLS convention; 2375 (plaintext) should never
  be exposed. If the guest's Docker service is systemd-managed with its own
  `-H` flags in the unit file, those need to be reconciled with `daemon.json`
  (override the unit's `ExecStart` to avoid a host-flag conflict).
  [Protect the Docker daemon socket — Docker docs](https://docs.docker.com/engine/security/protect-access/),
  [How to Secure Docker's TCP Socket With TLS](https://www.howtogeek.com/devops/how-to-secure-dockers-tcp-socket-with-tls/)
- **Cert generation/rotation practice:** run your own small CA, protect the
  CA private key closely (anyone holding it can mint a client cert with full
  daemon access), issue shorter-lived server/client certs off it, and rotate
  on a schedule rather than letting certs run to expiry. Community tooling
  examples use CA validity around 900 days with 365-day leaf certs as a
  starting point — pick a shorter leaf lifetime and automate reissuance if
  rotation without downtime matters.
  [Docker Remote API with client verification — gist](https://gist.github.com/kekru/974e40bb1cd4b947a53cca5ba4b0bbe5)
- **Network path:** the core's Docker container and the VM both need to sit
  on a bridge they can reach each other over — the same `truenasbr0`/custom
  bridge pattern the prior brief already established for the Podman-in-Instance
  case applies unchanged here.
  [Accessing NAS from VMs and Containers](https://www.truenas.com/docs/scale/25.04/scaletutorials/network/containernasbridge/)
- **`bollard` (the Rust client already in use per issue #2/#11) has native,
  first-class TLS support** — this is a meaningfully better story than
  option (b)'s Podman-over-TCP situation, which had no confirmed official
  client-library support:
  ```rust
  use bollard::{API_DEFAULT_VERSION, Docker};
  use std::path::Path;

  let docker = Docker::connect_with_ssl(
      "tcp://vm-host:2376/",
      Path::new("/certs/key.pem"),
      Path::new("/certs/cert.pem"),
      Path::new("/certs/ca.pem"),
      120,
      API_DEFAULT_VERSION,
  ).unwrap();
  ```
  Requires building `bollard` with its `ssl` (or `aws-lc-rs`) Cargo feature
  enabled — omitting it makes `connect_with_ssl` panic at runtime rather than
  fail to compile, worth a note-to-self for whoever wires this up. Bollard
  also documents Podman support (auto socket discovery) as a first-class
  citizen, but that's not relevant to this plain-Docker-daemon option.
  [bollard GitHub](https://github.com/fussybeaver/bollard),
  [Docker struct docs — docs.rs](https://docs.rs/bollard/latest/bollard/struct.Docker.html)

Net: the transport is real work (stand up a CA, issue/rotate certs, open a
port on the bridge, get the `daemon.json` right) but it is the **best-trodden
path of any cross-boundary option in this whole brief** — plain Docker's
TLS story is mainstream and `bollard` supports it natively, versus Podman's
TCP path being explicitly discouraged by its own maintainers with unclear
client-library support.

## VM lifecycle: autostart, boot ordering, reboot/upgrade

- **Autostart:** each VM has a single "Start on Boot" checkbox in the
  creation/edit wizard — a per-VM boolean, nothing more granular.
  [TrueNAS Virtual Machines tutorial (25.10)](https://www.truenas.com/docs/scale/25.10/scaletutorials/virtualmachines/)
- **No native ordering between VMs and Docker apps.** Multiple forum threads
  confirm TrueNAS SCALE has no GUI or middleware feature to sequence VM
  startup relative to apps, or apps relative to each other: "You can't. You
  could create a feature request." The standard community workaround is to
  disable autostart on the dependent side and use **Init/Shutdown Scripts**
  (System → Advanced → Tasks) to poll (e.g. ping, or a `midclt` call) until
  the VM is reachable before starting the app.
  [Ordering of VM auto startup — forum](https://forums.truenas.com/t/ordering-of-vm-auto-startup/8872),
  [Delay VM startup — forum](https://forums.truenas.com/t/delay-vm-startup/49951),
  [VM start order on Scale — forum](https://www.truenas.com/community/threads/vm-start-order-on-scale.107735/)
  For this project's shape, the practical implication is the same either
  way: **the core has to retry/backoff against the VM's Docker TCP endpoint
  on its own startup**, the same defensive posture it would want regardless
  of whatever ordering TrueNAS does or doesn't guarantee. 25.10 separately
  extended the Docker *app* service timeout to 960 seconds specifically to
  tolerate slow storage init, which is at least evidence iX is aware startup
  races exist and gives generous grace periods elsewhere in the same boot
  sequence.
  [25.10 Version Notes](https://www.truenas.com/docs/scale/25.10/gettingstarted/versionnotes/)
- **Reboot/upgrade behavior:** no breakage evidence found for VMs created
  natively on the 25.10 Virtual Machines screen (see the maturity section
  above for the one caveat, which only affects VMs created during the
  25.04.0/25.04.1 Incus-VM window). Docker itself inside the guest needs
  ordinary `systemctl enable docker` — boring and well-understood, no
  TrueNAS-specific wrinkle since it's entirely inside a guest OS TrueNAS
  doesn't manage.

## Workspace storage into the VM

The ticket specifically flags this because the host/core may want to inspect
the same per-chat git workspace the VM's agent containers are writing to —
that concurrent-access requirement rules out anything VM-exclusive.

- **virtiofs/9p: not natively supported.** TrueNAS's 25.10 VM creation UI
  offers no virtiofs or 9p host-directory-passthrough option — only zvol-backed
  disks (new, existing, or imported from QCOW2/RAW/VDI/VHDX/VMDK). An open
  community feature request for host-path mounts via virtiofs exists and was
  specifically refocused in August 2025 toward libvirt's own virtiofs support
  after the Incus-VM reversal, but it isn't shipped. A community project,
  [`truenas-vm-virtiofs`](https://github.com/dragosstoenica/truenas-vm-virtiofs),
  patches TrueNAS's middleware so libvirt spawns `virtiofsd` per share/VM,
  claiming ~20x the small-file throughput of NFS, with a "fail-safe" design
  (VMs boot normally without the share if the patch or binary is missing) and
  a drift guard for detecting when a TrueNAS upgrade has clobbered the patch.
  This is real, but it is an unofficial system-file patch — the same category
  of risk this project's own prior research flagged for developer-mode/host
  installs (option (c) in the prior brief): survives only in the current boot
  environment, needs re-verification after every TrueNAS upgrade, no iX
  support if it breaks something.
  [Create host path mounts (virtiofs) in VMs UI — feature request](https://forums.truenas.com/t/create-host-path-mounts-e-g-virtioifs-in-vms-ui/39834),
  [Mounting host dataset directly to VM aka VirtioFS — forum](https://forums.truenas.com/t/mounting-host-dataset-directly-to-vm-aka-virtiofs/66878)
- **NFS from the NAS itself: the supported path today.** TrueNAS's own
  "Accessing NAS from VMs and Containers" tutorial is the sanctioned route —
  put the VM's NIC on a bridge (`truenasbr0` or a custom bridge) so it gets
  its own IP, then mount the NFS export from that IP like any other NFS
  client; loopback/localhost from inside the VM does not reach the host,
  since the VM is a genuinely separate machine from the NAS's point of view.
  A real security caveat surfaced in community threads: NFS "Authorized
  Hosts and IP addresses" restrictions are IP-based, and any other device on
  the LAN that spoofs the VM's IP/MAC can also mount the share; binding the
  NFS service to a specific interface is the mitigation described in the
  thread that hit this. This gives the host/core and the VM's agent
  containers genuinely concurrent access to the same workspace dataset —
  the property this ticket cares about — at the cost of NFS's latency/small-file
  overhead relative to virtiofs.
  [Accessing NAS from VMs and Containers](https://www.truenas.com/docs/scale/25.04/scaletutorials/network/containernasbridge/),
  [NFS Share to VM Guest but Block LAN Access — forum](https://www.truenas.com/community/threads/nfs-share-to-vm-guest-but-block-lan-access.97393/)
- **zvol disks:** fine, and the recommended choice, for the guest's own root
  disk and Docker image storage — but a zvol attached as a VM disk is
  exclusive to that VM while it's running (an ordinary block device, not a
  shared filesystem), so it doesn't satisfy the "host/core may also want to
  inspect it" requirement for per-chat workspaces on its own. It would need
  to be paired with NFS (or the virtiofs patch) re-exporting from inside the
  guest, which just re-introduces the same trade-off one layer down.

**Bottom line for workspace storage:** NFS-over-bridge is the only supported,
concurrently-accessible option today; virtiofs would be a better fit
(near-local performance, no second network protocol) but currently means
taking on an unofficial, upgrade-fragile system patch to get it.

## True microVM tech (Firecracker, cloud-hypervisor, Kata) on TrueNAS

Not realistically usable without leaving TrueNAS's supported surface:

- TrueNAS's own virtualization prerequisites (KVM, Intel VT-x/AMD-V) are
  present, and the host kernel is recent enough (6.12 as of 25.04+) — the
  hardware/kernel floor for Kata+Firecracker or Kata+cloud-hypervisor exists
  in principle.
- But neither Incus (LXC/VM management) nor the reverted libvirt/KVM
  "Virtualization" stack has any integration point for swapping in Kata as a
  container runtime, and no documentation or community report surfaced of
  anyone running Kata/Firecracker/cloud-hypervisor natively on TrueNAS SCALE.
  TrueNAS's sanctioned isolation primitives remain exactly three: Docker
  containers (Apps), LXC (Instances, experimental), and full KVM VMs
  (Virtualization).
- The only theoretically available path is **nesting**: run the plain-Docker
  guest VM this ticket proposes, enable nested virtualization (CPU model
  `host` passthrough) inside it, and run Kata+Firecracker/cloud-hypervisor as
  a second virtualization layer inside that guest. This adds a second nested
  hypervisor boundary on top of option (e) itself, is unevidenced for TrueNAS
  specifically, and trades away most of the RAM/disk-slimness argument for
  running a VM in the first place.
  [Kata Containers vs Firecracker vs gVisor — Northflank](https://northflank.com/blog/kata-containers-vs-firecracker-vs-gvisor),
  [Kata Containers project](https://katacontainers.io/)

**Verdict:** an Instances/Virtualization QEMU-KVM VM (i.e., option (e) as
proposed) is the only realistic microVM-adjacent shape available on TrueNAS
without host OS modification. True Firecracker/Kata isolation is out of
reach on TrueNAS today short of nesting a second hypervisor inside the VM
this ticket already proposes — not a credible near-term plan.

## Updated comparison matrix

Extends the matrix from [`docs/research/agent-plane-runtime.md`](https://github.com/CorVous/CorCode/blob/research/agent-plane-runtime/docs/research/agent-plane-runtime.md)
with option (e).

| | (a) Docker siblings | (b) Podman in Instance | (d) Core+Podman in one Instance | (e) Docker in a TrueNAS VM |
|---|---|---|---|---|
| Isolation boundary vs. TrueNAS host | Single Docker daemon; no userns-remap; capability/seccomp hardening only | Unprivileged LXC + rootless Podman (two nested layers) | Unprivileged LXC + rootless Podman, but core shares that same boundary | Full hardware-virtualized VM (separate kernel) — strongest host boundary of any option; plain (non-rootless) Docker inside it |
| Core-to-agent-runtime boundary | N/A (same process/daemon as core's own socket access) | Separate: network hop (TCP+mTLS) or unverified shared-socket trick | None — colocated, same unix socket | Separate: network hop, TCP+mTLS — but over Docker's own first-class, well-documented TLS support rather than Podman's discouraged/unclear-client-support TCP path |
| Subsystem maturity (as of 25.10) | Docker apps: stable since 24.10, 2 major releases | Instances/LXC: experimental since 25.04, still experimental in 25.10 | Instances/LXC: experimental since 25.04, still experimental in 25.10 | Virtualization/KVM: **not experimental** — reverted to classic libvirt in 25.04.2 after a brief, since-corrected Incus detour; no experimental label in 25.10 docs |
| Upgrade survival evidence | Strong (same as core, per issue #2) | Depends on Instances graduating (due in 26); no breakage reported yet but subsystem young | Same as (b), extended to cover the core too | Strong for VMs created on the current Virtual Machines screen; only pre-25.04.2 Incus-created VMs hit a (since-fixed-forward) autostart wrinkle |
| Reboot survival | Strong, with restart policy (per issue #2) | Instance-level restart policy exists; Podman-inside needs its own systemd unit/socket-activation | Same as (b) | Per-VM "Start on Boot" checkbox; Docker-in-guest needs ordinary `systemctl enable`; no native VM-vs-app ordering, core must retry/backoff on connect |
| Ops burden | Lowest — one runtime, already wired up | High — second runtime + nesting/subuid config + cross-boundary transport | Medium — second runtime + nesting/subuid, but no cross-boundary transport; loses Apps lifecycle for core | High — a second full guest OS to patch/maintain, a CA/cert lifecycle to run, a permanently-reserved RAM/disk budget for the guest |
| Warm-pool/spawn API | `bollard` on existing `docker.sock` | Docker-compatible Podman API; needs TCP+mTLS (or unconfirmed shared-socket) transport | Docker-compatible Podman API over local socket, no transport work | `bollard` over TCP+mTLS to a real dockerd — most "vanilla Docker" of the cross-boundary options; native TLS client support confirmed |
| Image build/dist | `docker build` on box; `save`/`load` | `podman build`/`buildah` in Instance; `save`/`load`/`skopeo` | Same as (b), inside the shared Instance | `docker build`/`save`/`load` inside the guest — identical mechanics to (a), just on the VM's own disk |
| Workspace storage for per-chat workspaces | Host-path dataset, direct bind mount (per issue #2) | Instances "disk"/Filesystem Devices UI or zvol volumes — separate convention from Docker Host Path | Same as (b) | No native virtiofs/9p; supported path is NFS-over-bridge (concurrent host+VM access, network overhead); virtiofs possible only via an unofficial, upgrade-fragile system patch |
| TrueNAS Apps UI visibility | Core tracked; agents deliberately not | Core tracked (Docker app); agent runtime/Instance separately visible in Instances UI | Nothing tracked — core loses Apps UI/lifecycle entirely | Core tracked (Docker app, unchanged); VM itself visible in Virtualization UI (start/stop/stats); Docker daemon and agent containers *inside* the VM invisible to TrueNAS entirely |

(Option (c), Podman directly on the TrueNAS host OS, is omitted here as in
the original matrix's weakest-upgrade-survival case — see the prior brief for
its full row.)

## Choose this if…

- **(e) Docker inside a TrueNAS VM** — choose this if the strongest
  available host-isolation boundary (a real VM, not a shared kernel or a
  nested-namespace LXC) is worth taking on a second full guest OS to patch
  and operate, a TLS certificate authority to run and rotate, a permanently
  reserved chunk of RAM/disk for the guest, and — until virtiofs support
  either ships officially or the community patch is accepted as tolerable —
  routing per-chat workspace storage through NFS instead of a direct mount.
  In exchange, this option sidesteps every experimental-subsystem risk the
  other three carry (Instances/LXC's "still experimental in 25.10" status
  doesn't apply to it at all), and its cross-boundary transport (Docker's
  own TLS, natively supported by `bollard`) is better-trodden than Podman's
  TCP path in options (b)/(d).

## Sources

- [TrueNAS 25.04 Fangtooth release blog](https://www.truenas.com/blog/truenas-fangtooth-25-04-release/)
- [Fangtooth BETA (25.04): big changes for VMs — forum thread](https://forums.truenas.com/t/fangtooth-beta-25-04-big-changes-for-vms-dont-get-intentionality-of-experimental-language/34564)
- [Update from 25.04.1 to 25.04.2.1 Incus migration work-around — forum](https://forums.truenas.com/t/update-from-25-04-1-to-25-04-2-1-incus-migration-work-around/50379)
- [TrueNAS 25.04.2: Fangtooth re-enables "Virtualization"](https://www.truenas.com/blog/truenas-fangtooth-25-04-2/)
- [TrueNAS Virtualization Plans for 25.04.2 — forum](https://forums.truenas.com/t/truenas-virtualization-plans-for-25-04-2/46236)
- [TrueNAS 25.10 "Goldeye" BETA announcement](https://www.truenas.com/blog/truenas-goldeye-25-10-beta/)
- [TrueNAS 25.10 "Goldeye" Highlights](https://www.truenas.com/blog/truenas-goldeye-25-10/)
- [25.10 (Goldeye) Version Notes](https://www.truenas.com/docs/scale/25.10/gettingstarted/versionnotes/)
- [TrueNAS Virtual Machines tutorial (25.10)](https://www.truenas.com/docs/scale/25.10/scaletutorials/virtualmachines/)
- [Best Linux Distros for Low-Memory Servers and Containers](https://factually.co/product-reviews/electronics-tech/best-linux-distros-low-memory-servers-containers-alpine-tinycore-busybox-eee0f9)
- [Comparing Debian vs Alpine for container & Docker apps — TurnKey](https://www.turnkeylinux.org/blog/alpine-vs-debian)
- [Flatcar Container Linux FAQ](https://www.flatcar.org/faq)
- [Flatcar imported image disk layout — GitHub issue](https://github.com/flatcar/Flatcar/issues/1875)
- [I Installed Flatcar Linux on Proxmox — Virtualization Howto](https://www.virtualizationhowto.com/2026/04/i-installed-flatcar-linux-on-proxmox-and-its-not-like-a-normal-linux-vm/)
- [Benchmarking a TrueNAS SCALE VM in Proxmox](https://syntacticsugar.nl/benchmarking-a-truenas-scale-vm-in-proxmox)
- [Sluggish VM? — TrueNAS forum](https://www.truenas.com/community/threads/sluggish-vm.102709/)
- [Protect the Docker daemon socket — Docker docs](https://docs.docker.com/engine/security/protect-access/)
- [How to Secure Docker's TCP Socket With TLS](https://www.howtogeek.com/devops/how-to-secure-dockers-tcp-socket-with-tls/)
- [Docker Remote API with client verification via daemon.json — gist](https://gist.github.com/kekru/974e40bb1cd4b947a53cca5ba4b0bbe5)
- [TrueNAS: Accessing NAS from VMs and Containers](https://www.truenas.com/docs/scale/25.04/scaletutorials/network/containernasbridge/)
- [bollard GitHub repository](https://github.com/fussybeaver/bollard)
- [bollard Docker struct docs — docs.rs](https://docs.rs/bollard/latest/bollard/struct.Docker.html)
- [Ordering of VM auto startup — TrueNAS forum](https://forums.truenas.com/t/ordering-of-vm-auto-startup/8872)
- [Delay VM startup — TrueNAS forum](https://forums.truenas.com/t/delay-vm-startup/49951)
- [VM start order on Scale — TrueNAS forum](https://www.truenas.com/community/threads/vm-start-order-on-scale.107735/)
- [Create host path mounts (e.g., virtiofs) in VMs UI — feature request](https://forums.truenas.com/t/create-host-path-mounts-e-g-virtioifs-in-vms-ui/39834)
- [Mounting host dataset directly to VM aka VirtioFS — TrueNAS forum](https://forums.truenas.com/t/mounting-host-dataset-directly-to-vm-aka-virtiofs/66878)
- [truenas-vm-virtiofs — community middleware patch project](https://github.com/dragosstoenica/truenas-vm-virtiofs)
- [NFS Share to VM Guest but Block LAN Access — TrueNAS forum](https://www.truenas.com/community/threads/nfs-share-to-vm-guest-but-block-lan-access.97393/)
- [Kata Containers vs Firecracker vs gVisor — Northflank](https://northflank.com/blog/kata-containers-vs-firecracker-vs-gvisor)
- [Kata Containers project](https://katacontainers.io/)
