# Deploying CorCode on TrueNAS

CorCode runs as a Docker Compose stack the box brings up and keeps up
(ADR-0001). One service, one dataset, plain HTTP over the tailnet (ADR-0003).

The core image is built **on the box, from a checkout**: only the workspace
image is published to GHCR today (ADR-0009). That is why the install below
clones the repository rather than pasting YAML into the TrueNAS UI — there is
no source tree on the NAS to build from otherwise, and `build:` needs one.
Publishing the core image the same way the workspace image is published is
the obvious follow-up; the moment it exists, this becomes a paste-and-go
Custom App with `image:` in place of `build:`.

## 1. The dataset

Create one dataset for the whole deployment, e.g. `tank/corcode`, mounted at
`/mnt/tank/corcode`. Leave it empty: the core lays down `chats/` on boot and
`workspaces/` when the first chat is cut (ADR-0006). Do not create those two
by hand — a root that is missing entirely is the one thing the core refuses
to paper over, and that refusal is what tells you the dataset never mounted.

The dataset holds the durable record: manifests, event logs, agent memory,
and the working tree of every open chat. Back it up like anything else you
would miss.

Create a second dataset for the checkout, e.g. `tank/apps/corcode`. Keeping
the source out of the data dataset means a backup restore never drags a stale
checkout with it.

## 2. The checkout

SSH to the NAS, then clone the pinned revision you mean to run:

```
cd /mnt/tank/apps
git clone --branch main https://github.com/CorVous/CorCode.git corcode
cd corcode
git checkout <the tag or commit you are deploying>
```

Deploying by commit rather than by branch tip is what makes a rollback a
`git checkout` of the previous one.

## 3. The password hash

The single account's password is stored only as an argon2 hash (ADR-0003).
From the checkout:

```
docker compose --file deploy/compose.yaml run --rm --no-deps corcode hash-password
```

It reads the password from stdin — no echo at a terminal, no shell history,
no process listing — and prints the hash to stdout. Paste that line into
`CORCODE_PASSWORD_HASH`. (Building the image first, in step 5, makes this
run instant; otherwise it builds on the spot.)

## 4. The GHCR pull credential

The workspace image lives in GHCR (ADR-0009). The core pulls it lazily, the
first time a spawn finds the configured tag absent, so it needs a read
credential of its own: `CORCODE_REGISTRY_USER` is your GitHub username and
`CORCODE_REGISTRY_TOKEN` a classic personal access token carrying
`read:packages` and nothing else. This is separate from
`CORCODE_GITHUB_TOKEN`, which clones private repositories and pushes at
archive (ADR-0005).

## 5. The app

Edit [`compose.yaml`](compose.yaml) in the checkout:

- the host path on the left of `:/data`, and `CORCODE_HOST_DATA_DIR`, both to
  your dataset from step 1. **These two must agree.** Agent containers are
  siblings, not children: the core asks the host's daemon for them, and the
  daemon resolves their binds against the host filesystem, not against the
  core's own mount. Point `CORCODE_HOST_DATA_DIR` at `/data` and every agent
  gets an empty workspace.
- `CORCODE_USERNAME` and `CORCODE_PASSWORD_HASH` from step 3.
- `CORCODE_REPOS`, the comma-separated `owner/name` list the new-chat form
  offers, first entry the default.
- the credentials from step 4, and any of the optional tokens you want.
  Delete the lines you do not want rather than leaving a placeholder in them.

Every `REPLACE_WITH_...` value in the committed file is a placeholder; no
credential is ever committed to this repository. Your edited copy holds
secrets — leave it in the checkout, off any branch you push.

Then bring it up:

```
docker compose --file deploy/compose.yaml up --detach --build
```

Two ways to have TrueNAS own the stack afterwards: install a **Custom App**
pointed at this compose file on disk, or leave `restart: unless-stopped` to
bring the service back with the daemon. The former puts it in the Apps UI;
the latter is one less thing to configure.

`build:` names `context: ..`, the repository root — the Dockerfile is there,
not in `deploy/`. The build stage pins its own Rust, and `.dockerignore`
keeps `rust-toolchain.toml`, `target/` and the git history out of the
context, so the image never downloads a second toolchain or ships a
gigabyte of build output.

The compose file also binds `/var/run/docker.sock`. That is root on the NAS
in all but name, and it is the price of the sibling-container plane
(ADR-0001). Nothing about the bind is mitigated by the port: `8080` is
published on every interface the host has. What actually stands between it
and the world is where the host sits — reachable over the tailnet, not from
the internet — and the app's own session gate, which every path but
`/health` and `/login` goes through (ADR-0003). If the box is exposed
anywhere wider, fix that first.

The core's own container runs as root, which is how it reads that socket:
the socket is root-owned, and the group that owns it is a host number no
image can be built to match. Nothing is gained by dropping privilege in
front of a socket that hands out root anyway. The hardening that matters is
on the agent containers, which are the ones running work nobody reviewed
(ADR-0001). The dataset it writes is root-owned for the same reason; the
core is the only thing that reads it.

The core will not start without that socket: it fails at boot rather than
serve a console whose every button would fail. A container that exits
straight after `up` usually means the bind is missing or the daemon is not
running — `docker compose --file deploy/compose.yaml logs` says which.

## 6. First boot

1. Visit `http://<nas>:8080/` — you land on `/login`.
2. Sign in with the username and password from step 3. A wrong password
   backs off; a right one sets a signed session cookie (ADR-0003).
3. Cut a chat from the new-chat form. The first spawn pays a one-time pull
   of the workspace image; later ones do not.
4. Watch the status line: `pool n/m · parked k · img <tag> · sweep ok`.
   Expand it for the per-chat idle times (ADR-0008).

If `chats/` did not appear in the dataset, the app is running against the
wrong path — check the volume line before anything else.

## Upgrading and rolling back

**The core**: `git checkout` the revision you want in the checkout, then
`docker compose --file deploy/compose.yaml up --detach --build` again.
Rolling back is the same two commands with the previous revision.

**The workspace image**: a tag flip (ADR-0004). Edit
`CORCODE_WORKSPACE_IMAGE` to the new
`ghcr.io/corvous/corcode-workspace:YYYY-MM-DD` and restart the app. The next
spawn pulls it. Rollback is the same edit with the old date; the tags are
immutable, so the image you had is still exactly the image you get.

New tags come from the weekly SDK bump loop (ADR-0009): a scheduled workflow
opens a PR bumping the pinned `@anthropic-ai/claude-agent-sdk`, merging it
builds and pushes a new dated tag, and the merge is the deliberate human act.
That loop needs **Settings → Actions → General → Allow GitHub Actions to
create and approve pull requests** enabled on the repository, or
`sdk-bump.yml` fails at the point it opens its PR.
