# ADR-0003: App auth is a single config-held account with a signed session cookie

Date: 2026-08-05
Status: Accepted, amended 2026-08-06 (operational secrets are settable at
runtime; the login account is not)
Wayfinder: [decision #4](https://github.com/CorVous/CorCode/issues/4)

## Context

Tailscale is the network boundary, but the app carries its own auth against two
threats: (a) a lost/unlocked device holding a live tailnet session, and (c)
accidental exposure beyond the tailnet (Funnel, port forward, ACL mistake).
Other tailnet users are not a threat the app defends against. Auth must never
assume Tailscale is present. Candidates weighed: static passphrase, passkeys,
self-hosted OIDC, Tailscale identity headers — identity headers die with
threat (c), OIDC is ceremony for one user, passkeys add WebAuthn machinery and
require HTTPS the deployment doesn't have.

## Decision

- **Single principal**: one account, stored as `username` + argon2 password
  hash in app config (env vars at deploy). No user table, registration, or
  roles. Username/password shape (not a bare passphrase) so a second account
  is a clean later migration.
- **Login form → stateless HMAC-signed session cookie**, `HttpOnly`,
  `SameSite=Strict`, no `Secure` flag (app serves plain HTTP over the
  tailnet). Signing key auto-generated at first boot, persisted on the NAS
  dataset.
- **Every request is gated** — pages, API, SSE/WebSocket — except the login
  route and an unauthenticated health check. Reads (transcripts, repos, live
  sessions) are as sensitive as writes.
- **30-day sliding expiry**: each authenticated request refreshes the window.
- **Revocation = key rotation**: a "log out everywhere" action rotates the
  signing key, doubling as the lost-phone kill switch. No per-device sessions.
- **Operational secrets are not the account** _(amended 2026-08-06)_: the
  GitHub token and the Anthropic API key the core runs chats on are read from
  `secrets/<name>` files on the dataset when present, over the env vars that
  bootstrapped them, so either can be rotated from the running app. Each file
  is the owner's alone (0600 in a 0700 directory). The login account —
  `username` and the argon2 hash — stays env-only: who may log in changes at
  deploy, never at runtime.
- **Hardening**: constant-time credential comparison, Origin-header check on
  mutating requests (htmx sends none cross-origin without CORS consent), and
  an in-memory rate limit with backoff on the login endpoint.

## Consequences

- No dependency on the (undecided) persistence schema: no session store,
  sessions survive restarts.
- Revocation is all-or-nothing — acceptable at one-or-two users.
- Plain HTTP means the password travels clear if the app is ever exposed as
  raw HTTP; accepted, since the real path is WireGuard-encrypted.
- The deferred passkey upgrade forces HTTPS first (WebAuthn requires a secure
  context).
- Multi-user is out of the MVP; promoting the config account to a users table
  changes nothing in the cookie/session design.
