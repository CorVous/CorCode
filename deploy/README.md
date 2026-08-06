# Deploying CorCode on TrueNAS

CorCode runs as a TrueNAS **Custom App** — a Docker Compose stack the box
brings up and keeps up (ADR-0001). One service, one dataset, plain HTTP over
the tailnet (ADR-0003).

## 1. The dataset

Create one dataset for the whole deployment, e.g. `tank/corcode`, mounted at
`/mnt/tank/corcode`. Leave it empty: the core lays down `chats/` on boot and
`workspaces/` when the first chat is cut (ADR-0006). Do not create those two
by hand — a root that is missing entirely is the one thing the core refuses
to paper over, and that refusal is what tells you the dataset never mounted.

The dataset holds the durable record: manifests, event logs, agent memory,
and the working tree of every open chat. Back it up like anything else you
would miss.

## 2. The password hash

The single account's password is stored only as an argon2 hash (ADR-0003).
Generate one without putting the password in your shell history:

```
cargo run --quiet -- hash-password
```

or, once the app is built, from the box:

```
docker compose --file deploy/compose.yaml run --rm --no-deps corcode hash-password
```

Both read the password from stdin and print the hash to stdout. Paste that
line into `CORCODE_PASSWORD_HASH` below.

## 3. The GHCR pull credential

The workspace image lives in GHCR (ADR-0009). The core pulls it lazily, the
first time a spawn finds the configured tag absent, so it needs a read
credential of its own: `CORCODE_REGISTRY_USER` is your GitHub username and
`CORCODE_REGISTRY_TOKEN` a classic personal access token carrying
`read:packages` and nothing else. This is separate from
`CORCODE_GITHUB_TOKEN`, which clones private repositories and pushes at
archive (ADR-0005).

## 4. The app

In the TrueNAS UI: **Apps → Discover Apps → Custom App → Install via YAML**,
then paste [`compose.yaml`](compose.yaml) and edit it:

- the host path on the left of `:/data`, and `CORCODE_HOST_DATA_DIR`, both to
  your dataset. **These two must agree.** Agent containers are siblings, not
  children: the core asks the host's daemon for them, and the daemon resolves
  their binds against the host filesystem, not against the core's own mount.
  Point `CORCODE_HOST_DATA_DIR` at `/data` and every agent gets an empty
  workspace.
- `CORCODE_USERNAME` and `CORCODE_PASSWORD_HASH` from step 2.
- `CORCODE_REPOS`, the comma-separated `owner/name` list the new-chat form
  offers, first entry the default.
- the credentials from step 3, and any of the optional tokens you want.
  Delete the lines you do not want rather than leaving a placeholder in
  them.

Every `REPLACE_WITH_...` value in the file is a placeholder; no credential is
ever committed to this repository.

The compose file also binds `/var/run/docker.sock`. That is root on the NAS
in all but name, and it is the price of the sibling-container plane
(ADR-0001): the mitigation is that the core is reachable only over the
tailnet and runs one hardened image it pins itself.

`build: .` builds the core image from this repository's root `Dockerfile` on
the box. There is no CI-built core image today — only the workspace image is
published (ADR-0009). Publishing the core the same way is the obvious
follow-up; until then, deploying a new core means rebuilding the app.

## 5. First boot

1. Visit `http://<nas>:8080/` — you land on `/login`.
2. Sign in with the username and password from step 2. A wrong password
   backs off; a right one sets a signed session cookie (ADR-0003).
3. Cut a chat from the new-chat form. The first spawn pays a one-time pull
   of the workspace image; later ones do not.
4. Watch the status line: `pool n/m · parked k · img <tag> · sweep ok`.
   Expand it for the per-chat idle times (ADR-0008).

If `chats/` did not appear in the dataset, the app is running against the
wrong path — check the volume line before anything else.

## Upgrading and rolling back the workspace image

Upgrade is a tag flip (ADR-0004): edit `CORCODE_WORKSPACE_IMAGE` to the new
`ghcr.io/corvous/corcode-workspace:YYYY-MM-DD` and restart the app. The next
spawn pulls it. Rollback is the same edit with the old date; the tags are
immutable, so the image you had is still exactly the image you get.

New tags come from the weekly SDK bump loop (ADR-0009): a scheduled workflow
opens a PR bumping the pinned `@anthropic-ai/claude-agent-sdk`, merging it
builds and pushes a new dated tag, and the merge is the deliberate human act.
That loop needs **Settings → Actions → General → Allow GitHub Actions to
create and approve pull requests** enabled on the repository, or
`sdk-bump.yml` fails at the point it opens its PR.
