# Hosted Proxy MVP

This document defines the MVP trust model and the exact hosted registration handoff for proxy mode.

The scope is intentionally small: one GitHub App installation claims one tunnel URL, then rotates that URL later without introducing user accounts, dashboards, or PAT-based auth.

## Trust Model

The hosted proxy forwards GitHub App webhooks to a user-managed tunnel. The main trust boundary is preventing one installation from claiming or overwriting another installation's forwarding target.

For MVP, GithubClaw keeps that boundary narrow:

- Initial claim uses a short-lived one-time `claim_proof`.
- `claim_proof` is minted by a trusted hosted setup step after GitHub identifies the installation.
- `claim_proof` is bound to exactly one `installation_id`.
- Successful initial claim returns an installation-scoped `update_secret`.
- Later updates require the current `update_secret`.
- Every successful update rotates `update_secret`.
- The proxy never asks the client to send a GitHub PAT or arbitrary long-lived GitHub token.

## Hosted Setup Callback

The minimal hosted setup entry point is `GET /setup`.

Expected query parameters:

- `installation_id`: positive GitHub App installation id from the GitHub App install/setup callback
- `setup_action`: optional passthrough from GitHub (`install`, `request`, or similar)

Successful response:

```json
{
  "status": "ready",
  "installation_id": 123456,
  "setup_action": "install",
  "claim_proof": "cp_v1.123456.1710000000.nonce.signature",
  "expires_at": "2026-03-12T10:10:00Z",
  "register_path": "/register",
  "register_request": {
    "installation_id": 123456,
    "tunnel_url": "https://YOUR-TUNNEL-ORIGIN",
    "claim_proof": "cp_v1.123456.1710000000.nonce.signature"
  }
}
```

Handoff contract to the registration consumer:

- the setup flow returns the minted `claim_proof` directly to the consumer
- the consumer sends that same `claim_proof` to `POST /register` on the same hosted service origin
- the consumer supplies the real `tunnel_url` and the same `installation_id`
- the consumer must complete the first `POST /register` before `expires_at`
- after one successful claim, the consumer must discard `claim_proof` and persist the returned `update_secret`

This keeps the hosted side minimal: GitHub callback in, one-time proof out, then `POST /register` completes the ownership claim.

## `POST /register`

`POST /register` handles both initial claim and later tunnel rotation.

Exactly one of `claim_proof` or `update_secret` must be present.

### Common request shape

```json
{
  "installation_id": 123456,
  "tunnel_url": "https://abc123.trycloudflare.com",
  "claim_proof": "optional-initial-proof",
  "update_secret": "optional-current-update-secret"
}
```

Field rules:

- `installation_id`: positive GitHub App installation id
- `tunnel_url`: HTTPS origin only
  - allowed: `https://abc123.trycloudflare.com`
  - allowed: `https://example.com:8443`
  - rejected: `http://...`
  - rejected: URLs with path, query, fragment, or embedded credentials
  - rejected: localhost, loopback, and private-network destinations
- `claim_proof`: opaque string for first claim only
- `update_secret`: opaque string for update only

### Initial claim

- minted by the hosted setup flow after GitHub identifies the installation
- bound to exactly one `installation_id`
- expires 10 minutes after issuance
- single-use
- never accepted for later update requests

Single-use behavior:

- The server consumes `claim_proof` only after a successful claim write.
- If the proof is valid but the request fails because the payload is malformed or the tunnel URL is unsafe, the proof remains usable until expiry.
- After one successful claim, replay of the same proof must fail.

### Update

`update_secret` is:

- scoped to exactly one `installation_id`
- returned only by a successful initial `POST /register`
- treated as a write credential
- rotated on every successful update, even if the normalized `tunnel_url` does not change

## Deterministic Outcomes

- first successful claim returns `201 Created`
- successful later updates return `200 OK`
- `claim_proof_already_used` returns `403 Forbidden`
- expired proof returns `403 Forbidden`
- reused stale `update_secret` returns `403 Forbidden`
- a fresh proof for an already claimed installation returns `409 Conflict`

## Security Assumptions

- GitHub identifies the installation during the setup callback.
- The hosted setup flow mints `claim_proof` only after GitHub has identified the correct installation.
- The server uses cryptographically strong randomness for `claim_proof` nonces and `update_secret`.
- The server persists enough state to reject proof replay and stale-secret replay across restarts.
- The endpoint is an ownership gate for one installation mapping, not a full user identity system.

## Non-Goals

Out of scope for the MVP:

- user accounts
- hosted dashboard auth
- GitHub PAT submission to the proxy
- long-lived browser sessions
- multi-actor ownership workflows
- complex recovery UX for lost `update_secret`
