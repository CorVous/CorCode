# TrueNAS SCALE hosting for the core and per-chat containers

Research for GitHub issue [CorVous/CorCode#2](https://github.com/CorVous/CorCode/issues/2) (part of #1).

## Question

How should the Rust core service and its dynamically-spawned per-chat containers
run directly on TrueNAS SCALE? Specifically: custom app (compose-based) vs plain
docker-socket access; whether the core can create/destroy sibling containers via
the socket; survival across TrueNAS upgrades/reboots; where images live and how
they get built on-box; and how to mount NAS datasets into containers for
artifact persistence (host path validation, apps-dataset conventions).

## Context / current TrueNAS SCALE state (as of Aug 2026)

- TrueNAS SCALE **24.10 "Electric Eel"** (Oct 2024) replaced the old
  Kubernetes/k3s app backend with a native Docker backend. This is the
  dividing line for almost everything below — guidance written for 23.10
  "Cobia" or earlier (containerd/k3s, no `/var/run/docker.sock`) no longer
  applies.
- The current stable line is **25.10 "Goldeye"** (25.10.3, released
  2026‑04‑14), built on the same Docker-based apps architecture introduced in
  24.10. TrueNAS 26 is in development but keeps the same Docker apps model.
  [25.10 version notes](https://www.truenas.com/docs/scale/25.10/gettingstarted/versionnotes/),
  [TrueNAS 26 version notes](https://www.truenas.com/docs/scale/26/gettingstarted/versionnotes/)
- Net effect: since 24.10, the TrueNAS host runs a real Docker daemon and
  exposes `/var/run/docker.sock`. Apps are plain Docker containers/compose
  stacks, not Kubernetes pods.

## Custom app (compose) vs plain docker-socket access

TrueNAS SCALE's "Apps" system supports installing a **Custom App**: go to
*Apps → Discover Apps → Custom App* (guided wizard) or *Install via YAML*
(paste a full `docker-compose.yml`). This is the fully-supported path — the
resulting containers are tracked by the Apps middleware, show up in the Apps
UI, and get the standard install/upgrade/start/stop/rollback lifecycle.
[Custom App screens reference](https://www.truenas.com/docs/scale/25.04/scaleuireference/apps/installcustomappscreens/),
[Installing Custom Apps](https://apps.truenas.com/managing-apps/installing-custom-apps/)

Nothing stops a custom app's container from **also** bind-mounting
`/var/run/docker.sock:/var/run/docker.sock` and talking to the host Docker
daemon directly (the classic "sibling container" pattern used by Portainer,
Dockge, etc.). This is exactly the shape our core service needs: it runs as
one custom-app container, and it spawns/destroys per-chat containers as
siblings on the same daemon via the Docker API/SDK.

Practical notes from real deployments doing this (Portainer-on-SCALE guides):

- Mount the socket as an **Additional App Storage → Host Path**:
  `/var/run/docker.sock` → `/var/run/docker.sock`.
  [How to Install Portainer on TrueNAS](https://oneuptime.com/blog/post/2026-03-20-portainer-truenas/view)
- The container's process needs a GID matching the host's `docker` group
  (reported as GID 999 on SCALE) to actually use the socket without running
  as root. [same source]
- Mounting the socket is **root-equivalent access to the host** (a container
  with socket access can launch privileged containers, mount the host
  filesystem, etc.). This is a real security boundary decision, not just
  plumbing — treat the core service like it has host root, and restrict
  what can reach it accordingly.

**Recommendation for this project:** deploy the core service as a Custom App
via Install-via-YAML (so it's visible/manageable in the Apps UI and gets
persistent-storage validation), with the docker socket bind-mounted in. Spawn
per-chat containers as plain sibling `docker run`/Docker-API containers, *not*
through the Apps middleware — the per-chat containers are ephemeral and
numerous, and the Apps system isn't designed for a service creating/deleting
many containers programmatically. Give spawned containers clear labels
(e.g. `com.corcode.managed=true`, `com.corcode.chat-id=...`) so the core can
reconcile/reap them on its own restart, since nothing else will track them.

## Spawning/destroying sibling containers via the socket

This is the standard, well-supported pattern (not docker-in-docker):

- The core process uses a Docker client library (e.g. `bollard` in Rust)
  against the mounted socket to create, start, stop, and remove per-chat
  containers as siblings of itself — same daemon, same host namespace for
  networking/volumes.
- Containers created this way are otherwise ordinary Docker containers: they
  need an explicit restart policy if they should survive a daemon/host
  restart, and their filesystem is wiped on removal unless data lives on a
  mounted volume/host path.
- They are **not** tracked by the TrueNAS Apps UI/middleware at all — from
  TrueNAS's point of view they're just containers on the shared Docker
  daemon. That's fine for ephemeral per-chat workers, but means the core
  service alone is responsible for cleanup/GC; TrueNAS won't do anything
  with them beyond what "the docker daemon is running" implies.
  [Guide: Building and Running Custom Docker Applications on TrueNAS SCALE](https://www.truenas.com/community/threads/guide-building-and-running-custom-docker-applications-on-truenas-scale.111846/)
  (note: this specific thread predates 24.10 and its docker-in-Portainer
  setup broke across the Cobia containerd switch — cited here only for the
  general sibling-container pattern, not as current instructions)

## Survival across TrueNAS upgrades and reboots

Two independent things need to survive: **the containers themselves** and
**their data**.

- **Docker daemon / container restart on boot:** since 24.10, Docker is a
  native TrueNAS service and starts automatically. Containers you create
  directly via the socket only come back after a reboot if you gave them an
  explicit restart policy (`--restart unless-stopped` or `always`); without
  one, a socket-created container that isn't otherwise managed simply stays
  stopped. Community testing confirms `unless-stopped`/`always` containers
  do come back up after a TrueNAS reboot.
  [TrueNAS Community: "custom docker containers are not persistent" thread](https://www.truenas.com/community/threads/custom-docker-containers-are-not-persistent.98569/)
  For our design: the **core** container should run under the Apps
  middleware with a restart policy (Custom Apps get this by default), so it
  reliably comes back after reboot/upgrade and can re-establish/reconcile
  whatever per-chat containers should exist. Per-chat containers themselves
  are cheap to recreate, so the core reconciling them on startup (rather than
  relying on Docker's own restart policy for each one) is the more robust
  design — recreate from persisted chat state rather than trust container
  survival.
- **TrueNAS version upgrades:** major version upgrades (e.g. 24.10 → 25.04,
  25.04 → 25.10) keep the same Docker-based apps architecture, so
  Docker-level state generally carries forward. The one repeatedly-flagged
  failure mode: **manually installed Docker** (from before 24.10, or
  installed outside the TrueNAS-managed path) conflicts with the
  TrueNAS-native Docker setup and can break Apps entirely after an upgrade —
  this is called out explicitly in both the 25.04 and TrueNAS 26 release
  notes. Rule: never install Docker yourself on the box; only use the
  TrueNAS-managed Docker daemon that Apps already provides.
  [25.04 version notes](https://www.truenas.com/docs/scale/25.04/gettingstarted/scalereleasenotes/),
  [TrueNAS 26 version notes](https://www.truenas.com/docs/scale/26/gettingstarted/versionnotes/)
- **Data survival:** container filesystems are not durable across
  recreation/upgrade. Anything that must persist (chat transcripts, core
  service state, per-chat working files) must live on a host-path-mounted
  dataset, never in the container's writable layer or in an auto-created
  ixVolume meant for "quick test deployments." (See dataset section below.)

## Where images live and how they're built on-box

- Since 24.10, the TrueNAS host has a real `docker` CLI/daemon, so
  `docker build -t myimage:tag .` works directly from the TrueNAS shell —
  confirmed by TrueNAS community members building images "on the truenas
  machine itself." An alternative for build-elsewhere workflows:
  `docker save image:tag > img.tar` on a build machine, copy it over
  (e.g. SMB), then `docker load` on the NAS — useful for avoiding a private
  registry.
- When referencing a locally-built image in the Custom App / Install-via-YAML
  wizard, no registry URL is needed — TrueNAS checks the local image cache
  by tag first. Make sure the compose file's pull policy is set to not
  force-pull (`pull_policy: never`/`if_not_present`), or TrueNAS will try to
  fetch the tag from Docker Hub and fail/overwrite the local image.
- Keep the Dockerfile/build context on a **data pool dataset**, not the boot
  device — the boot environment can be replaced across TrueNAS updates, and
  iXsystems considers ad hoc CLI/Docker-level modifications to the apps
  system "unsupported," so anything you rely on that lives outside a
  data-pool dataset should be treated as disposable.
- Internal Docker state (image layers, container metadata, catalog data) for
  TrueNAS-managed Docker lives in a hidden, TrueNAS-managed dataset:
  `ix-apps`, mounted at `/mnt/.ix-apps` on whichever pool is configured as
  the "Apps" pool. This is explicitly **not** meant to be touched directly —
  it doesn't inherit pool encryption, must not be nested inside an SMB/NFS
  share, and has no supported UI backup/restore as of 25.10.0. Your own
  locally-built images end up inside this managed Docker storage the same
  as any pulled image; there's no separate "your images" location to back up
  — back up the Dockerfile/source instead and rebuild if needed.
  [App Storage — TrueNAS Apps Market](https://apps.truenas.com/getting-started/app-storage/)

## Mounting NAS datasets into containers (host path validation, dataset conventions)

- Two storage mechanisms are offered when configuring app storage: **Host
  Path** (bind-mount an existing user-created dataset/directory) and
  **ixVolume** (TrueNAS auto-creates and manages a dataset under the hidden
  `ix-apps` tree). Both attach as Docker bind mounts under the hood.
  ixVolumes are convenient for throwaway/test deployments but are explicitly
  discouraged for real persistent data because they complicate backup;
  **host paths on manually-created datasets are the recommended pattern**
  for anything that must survive/be backed up/replicated.
  [App Storage — TrueNAS Apps Market](https://apps.truenas.com/getting-started/app-storage/)
- **Host path safety/validation checks:** TrueNAS refuses to deploy an app
  whose host path dataset overlaps with a path already exported via
  SMB/NFS (sharing the same dataset/path as a share is treated as
  insecure). Practical fix is namespacing paths so they don't literally
  collide, e.g. `/mnt/tank/media-shares/...` for the SMB share and
  `/mnt/tank/media-apps/...` for app storage, rather than nesting one under
  the other's exact path. Disabling the safety check is possible but not
  recommended.
  [Configuring Host Path Safety Checks](https://www.truenas.com/docs/scale/22.12/scaletutorials/apps/appadvancedsettings/configuring-host-path-safety-checks/)
- **Permissions:** unlike ixVolumes (auto-permissioned), host paths need
  explicit ACLs matching the container's run-as UID/GID (shown in the app's
  "Run As Context" in the wizard), via *Enable ACL*, or *Automatic
  Permissions* for the Postgres-style convention (only works on empty
  directories, must be set before first use). Getting this wrong is the most
  common cause of a custom app failing to start against a host-path mount.
  Separately, note that stable-train catalog apps commonly run as UID/GID
  `473:473` while community/enterprise-train apps commonly run as
  `568:568` ("apps:apps") — check whichever applies and match dataset ACLs
  to it, or to the core service's own configured user if it deviates.
- **Convention for this project:** create dedicated datasets ahead of time
  (e.g. `tank/apps/corcode/core-data`, `tank/apps/corcode/chats/<id>`) rather
  than relying on ixVolumes, mount them as host paths with ACLs matching the
  core/per-chat container's UID:GID, and keep them out of any SMB/NFS share
  tree. Per-chat containers can be given their own subdirectory under a
  parent "chats" dataset so ZFS snapshots/quotas can apply per chat if
  needed later.

## Recommendation

Run the Rust core as a TrueNAS **Custom App** (Install-via-YAML / Docker
Compose) so it's tracked by the Apps middleware, gets a restart policy, and
survives reboots/upgrades the standard way; bind-mount
`/var/run/docker.sock` into it (with the core's process GID matching the
host `docker` group) so it can create and destroy **sibling** per-chat
containers directly via the Docker API — do not route per-chat containers
through the Apps middleware itself, since that system isn't built for
programmatic high-churn container lifecycles, and do not use
docker-in-docker. Build/rebuild images with the native `docker build` on the
box (Dockerfile/build context kept on a data-pool dataset, not the boot
device) and reference them locally in the compose file with a
never/if-not-present pull policy; don't rely on any single "image store"
being backed up — the source of truth is the Dockerfile, not the image
cache inside the hidden `ix-apps` dataset. Persist all durable state (chat
data, core state) on manually-created datasets mounted as host paths with
ACLs matching the containers' run-as UID:GID, kept path-disjoint from any
SMB/NFS shares to pass TrueNAS's host-path safety checks, and have the core
reconcile per-chat containers from that persisted state on its own startup
rather than depending on Docker restart policies for each spawned
container. Never install Docker manually outside TrueNAS's managed path —
that's the one thing release notes repeatedly flag as breaking Apps across
upgrades.

This is safe to build against **today's stable line (25.10 "Goldeye")** and
should keep working across upgrades to TrueNAS 26, since both are built on
the same Docker-apps architecture introduced in 24.10 — the only real
compatibility risk is a future backend change of similar magnitude to the
24.10 Kubernetes→Docker migration, which is not currently signaled anywhere
in the 25.10/26 release notes.
