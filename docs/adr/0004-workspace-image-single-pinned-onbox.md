# ADR-0004: One pinned workspace image, built on-box, minimal baked home

Date: 2026-08-05
Status: Accepted, amended by ADR-0009 (build moved to CI/GHCR; SDK
freshness override)
Wayfinder: [decision #6](https://github.com/CorVous/CorCode/issues/6)

## Context

Every chat spawns a workspace container (ADR-0001: hardened sibling, read-only
rootfs, non-root user). The image must carry Claude Code behind the ACP
adapter — Zed's `@zed-industries/claude-agent-acp`, a Node package wrapping
Anthropic's Claude Agent SDK (renamed from `claude-code-acp`; 0.23.1 as of
2026-08-05, historically several releases/month, quiet since March).
_(Corrected 2026-08-07: the quiet was a third rename. The package lives on as
`@agentclientprotocol/claude-agent-acp`, 0.66.0, and the image installs
that.)_ TrueNAS
hosting research settled the mechanics: `docker build` works on-box, local
tags resolve without a registry when compose's pull policy never pulls, and
the Dockerfile — not the image store — is the artifact to preserve.

## Decision

- **One shared image** for all repos — no per-project variants, no
  image-selection mapping in the core.
- **Contents**: Node.js (adapter requirement), git, `gh`, everyday CLI tools
  (curl, jq, ripgrep, build essentials); language toolchains **Rust, Node,
  Python** — others added lazily at a rebuild.
- **Everything pinned exact** in the Dockerfile: adapter, Claude Code / Agent
  SDK, base image tag. Upgrades are deliberate bump-and-rebuild, never
  `latest`.
- **Build pipeline**: Dockerfile lives in this repo (`docker/workspace/`);
  MVP builds are a small script run by hand on the NAS shell — pull,
  `docker build`, stamp an **immutable date tag** (`corcode-workspace:
  YYYY-MM-DD`). The core reads the active tag from config: upgrade = build
  and flip config, rollback = flip back. No registry, no CI. _(Amended by
  ADR-0009: builds run in GitHub Actions and push to GHCR; the on-box
  script is superseded. Date tags, config-held active tag, and
  flip-to-upgrade/rollback all stand.)_
- **Baked home** (image's non-root user): git identity (owner name + noreply
  email) and a skeleton Claude Code config with sane defaults. No personal
  dotfiles. Secrets stay env-at-spawn; per-repo instructions stay in each
  repo's CLAUDE.md.

## Consequences

- The image is fat (three toolchains) but boring: every workspace is
  byte-identical, known-good software.
- Commit/push cadence enforcement (its own ticket) composes into the baked
  config skeleton rather than fighting it.
- Read-only rootfs means Claude Code's mutable state (`CLAUDE_CONFIG_DIR`,
  transcript JSONL) must point at a writable mount — location decided by the
  persistence ticket.
- Trailing the newest Claude Code by one deliberate rebuild is accepted.
