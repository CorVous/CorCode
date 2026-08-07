# ADR-0009: Image freshness — CI-built image, weekly SDK bump-PR loop

Date: 2026-08-05
Status: Accepted
Wayfinder: [decision #14](https://github.com/CorVous/CorCode/issues/14)
Amends: ADR-0004 (build pipeline moves off-box), ADR-0008 (dead-simple
premise scoped to the app)

## Context

ADR-0004 pins everything exact and prescribed hand-run on-box builds — "no
registry, no CI" — under an over-broad reading of the dead-simple premise.
The premise constrains the **app**, not the project's tooling: the repo
already runs a full GitHub Actions pipeline, and the owner corrected the
record ("I meant the app itself simple").

The freshness problem is keeping Claude Code current, and the adapter is
the wrong thing to watch: `@zed-industries/claude-agent-acp` has been quiet
since 2026-03-26 (0.23.1) and pins `@anthropic-ai/claude-agent-sdk`
**exactly** at 0.2.83 (March), while the SDK is at 0.3.222 with near-weekly
releases. Rebuilding on the latest adapter still bakes in a four-month-stale
agent. _(Corrected 2026-08-07: the quiet was a rename, not abandonment.
0.23.1 was the last release under the Zed name; the same series continued as
`@agentclientprotocol/claude-agent-acp` and is now at 0.66.0, declaring
`@anthropic-ai/claude-agent-sdk` 0.3.220. The image installs the living name;
the override still forces the SDK forward, at 0.3.223. Whether a maintained
adapter is now worth watching as well is an open question this correction
leaves open.)_

## Decision

- **Build in CI.** GitHub Actions builds `docker/workspace/` on any change
  to it (plus manual dispatch) and pushes to GHCR under the same immutable
  date tags (`ghcr.io/corvous/corcode-workspace:YYYY-MM-DD`). The
  Dockerfile remains the reproducible artifact; exact pins stand. The
  on-box build script is superseded before it was written.
- **The SDK is the freshness lever.** The image's `package.json` forces
  `@anthropic-ai/claude-agent-sdk` forward via an npm `overrides` entry,
  pinned exact — overriding the adapter's stale internal pin.
- **Weekly bump PR.** A scheduled workflow checks npm weekly; a newer SDK
  yields a PR bumping the override pin. Merging it triggers the image
  build. GitHub's PR notification is the entire alerting story — the
  deliberate human act is the merge.
- **Smoke test gates the tag.** The image build spawns the adapter and
  completes an ACP `initialize` handshake; failure means no tag is pushed,
  so an override the adapter can't handle never becomes deployable.
- **Lazy pull by the core.** Upgrade remains a config tag-flip (ADR-0004);
  when a spawn finds the configured tag absent locally, the core pulls it
  from GHCR, then spawns. The app stays version-dumb: no UI nudge, no
  GHCR polling, no freshness state.
- **Everything else bumps by hand.** Base image and toolchain pins change
  by editing the Dockerfile; the merge rides the same build path.

## Consequences

- A GHCR pull credential lives on the NAS, and image bits are now built on
  GitHub's runners instead of the box.
- First spawn after a tag flip pays a one-time pull delay.
- The override can outrun the adapter's tested SDK range; the handshake
  smoke test is the only automated guard, deeper breakage surfaces at
  runtime, and rollback is flipping the tag back.
- Freshness rests on one adapter staying compatible while effectively
  unmaintained; if the override starts failing the smoke test repeatedly,
  the adapter choice itself (ADR-0004) is the decision to reopen.
