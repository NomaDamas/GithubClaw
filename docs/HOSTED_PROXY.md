# Hosted Proxy MVP

This document defines the MVP trust model and the exact `POST /register` contract for hosted proxy mode.

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

### Trusted components

- GitHub identifies the installation during the install/setup callback.
- The hosted setup step mints `claim_proof` for exactly one `installation_id`.
- The hosted proxy persists the tunnel mapping and current update credential state per installation.

### Untrusted inputs

- `installation_id` in the request body by itself
- public knowledge of the tunnel URL
- replayed old `claim_proof` values
- replayed or stale `update_secret` values

### Required security properties

- Knowing `installation_id` alone is never enough to claim or update.
- One installation cannot overwrite another installation's tunnel URL.
- Successful initial claim consumes the proof.
- Successful update invalidates the previous `update_secret`.
- The proxy accepts only conservative tunnel origins: HTTPS origin only, with no path, query, fragment, or userinfo.

## Stored State

Per `installation_id`, the proxy stores:

- canonical `tunnel_url`
- current `update_secret` or a verifier derived from it
- creation and last-update timestamps
- replay state sufficient to reject reused claim proofs

The MVP does not require user accounts, dashboards, or multi-user ownership state.

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

Validation rules:

- both auth fields present: `400 Bad Request`
- neither auth field present: `400 Bad Request`
- invalid or unsafe tunnel URL: `400 Bad Request`
- server normalizes `tunnel_url` before storing or returning it

### Initial claim

Use this when the installation has no stored mapping yet.

```json
{
  "installation_id": 123456,
  "tunnel_url": "https://abc123.trycloudflare.com",
  "claim_proof": "cp_opaque_server_minted_value"
}
```

`claim_proof` requirements:

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

Use this after the installation has already been claimed.

```json
{
  "installation_id": 123456,
  "tunnel_url": "https://next456.trycloudflare.com",
  "update_secret": "us_current_secret_value"
}
```

`update_secret` requirements:

- scoped to exactly one `installation_id`
- returned only by a successful `POST /register` response
- treated as a write credential
- rotated on every successful update, even if the normalized `tunnel_url` does not change

Rotation behavior:

- Successful update invalidates the previous `update_secret`.
- Failed update attempts do not rotate the secret.
- Clients must persist the replacement secret from every successful response before making another update.

## Success Responses

### `201 Created` for initial claim

```json
{
  "status": "claimed",
  "installation_id": 123456,
  "tunnel_url": "https://abc123.trycloudflare.com",
  "update_secret": "us_new_secret_value",
  "rotated": false
}
```

### `200 OK` for update

```json
{
  "status": "updated",
  "installation_id": 123456,
  "tunnel_url": "https://next456.trycloudflare.com",
  "update_secret": "us_replacement_secret_value",
  "rotated": true
}
```

Response rules:

- `tunnel_url` is the normalized stored value, not necessarily the raw input string.
- `update_secret` is always the credential required for the next update.
- The server returns a fresh `update_secret` on every successful write.

## Error Responses

All error responses use this shape:

```json
{
  "error": {
    "code": "machine_readable_code",
    "message": "human readable explanation"
  }
}
```

### `400 Bad Request`

Possible `error.code` values:

- `invalid_request`
- `invalid_tunnel_url`
- `unsafe_tunnel_url`

This status covers malformed request shape, missing required fields, invalid `installation_id`, both-or-neither auth fields, and conservative tunnel URL rejection.

### `403 Forbidden`

Possible `error.code` values:

- `invalid_claim_proof`
- `expired_claim_proof`
- `claim_proof_already_used`
- `invalid_update_secret`
- `ownership_mismatch`

`ownership_mismatch` means the supplied credential is bound to a different `installation_id` than the one in the request.

### `409 Conflict`

Possible `error.code` values:

- `already_claimed`

Use this when the caller presents `claim_proof` for an installation that has already been claimed. After the first claim, the caller must use the current `update_secret`.

### `500 Internal Server Error`

Possible `error.code` values:

- `persistence_failure`
- `secret_rotation_failure`

Server failures must not consume or rotate credentials unless the write succeeded.

## Expiry, Replay, and Determinism

- `claim_proof` expires exactly 10 minutes after issuance.
- `claim_proof` is one-time: one successful claim permanently consumes it.
- `update_secret` has no fixed time expiry in MVP, but becomes invalid immediately after the next successful write for that installation.
- Replaying an older `update_secret` after rotation must return `403 invalid_update_secret`.
- Replaying an already-used `claim_proof` must return `403 claim_proof_already_used`.

Deterministic handler rules:

- first successful claim returns `201`
- successful later updates return `200`
- failed validation does not rotate or consume credentials
- wrong credential for the current state returns `409 already_claimed` or `403 invalid_update_secret`, depending on what was supplied

## Hosted Setup Flow

The MVP hosted setup step can use a minimal callback endpoint:

```http
GET /hosted/setup?installation_id=123456
```

Success response:

```json
{
  "status": "claim_proof_issued",
  "installation_id": 123456,
  "claim_proof": "cp_server_minted_value",
  "expires_at": "2026-03-12T12:00:00Z",
  "register_url": "/register"
}
```

Setup flow rules:

- GitHub identifies the installation before this step runs
- the hosted proxy mints and stores `claim_proof` with its bound `installation_id`
- persisted proof state includes expiry and single-use replay status
- clients can immediately call `POST /register` using the returned proof
- failed `POST /register` validation does not consume the proof

## Security Assumptions

- The hosted setup flow is trusted to mint `claim_proof` only after GitHub has identified the correct installation.
- The server uses cryptographically strong randomness for `update_secret`.
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
- broader platform features beyond secure registration and tunnel rotation

Keeping the contract this small matches `.githubclaw/VALUE.md`: Rust core, GitHub-first workflow, and no unnecessary hosted-platform surface area.
