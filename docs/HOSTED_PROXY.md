# Hosted Proxy MVP

This document defines the MVP trust model and `POST /register` contract for hosted proxy mode.

The scope is intentionally small: one GitHub App installation claims one tunnel URL, then rotates that URL later without user accounts, PAT-based auth, or dashboard state.

## Trust Model

The hosted proxy forwards GitHub App webhooks to a user-managed tunnel. The main trust boundary is preventing one installation from claiming or overwriting another installation's forwarding target.

For MVP, GithubClaw keeps that boundary narrow:

- Initial claim uses a short-lived one-time `claim_proof`.
- `claim_proof` is minted by a trusted hosted setup step and bound to exactly one `installation_id`.
- Successful initial claim returns an installation-scoped `update_secret`.
- Later updates require the current `update_secret`.
- Every successful update rotates `update_secret`.
- The proxy never asks the client to send a GitHub PAT or arbitrary long-lived GitHub token.

### Trusted components

- GitHub identifies the installation during the install/setup callback.
- The hosted setup step mints `claim_proof` for exactly one `installation_id`.
- The hosted proxy persists the tunnel mapping and credential replay state per installation.

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
- current `update_secret` verifier
- creation and last-update timestamps
- replay state sufficient to reject reused claim proofs

### Persistence format and location

The MVP persists hosted-proxy registration state to:

```text
~/.githubclaw/hosted_proxy/installations.json
```

The file is a JSON snapshot keyed by `installation_id`. Each entry contains:

- normalized `tunnel_url`
- `update_secret_hash` as a SHA-256 verifier, not the raw secret
- `created_at` and `updated_at` timestamps in RFC 3339 / `chrono` JSON format
- `used_claim_proof_hashes` as SHA-256 digests of successfully consumed claim proofs

The server writes the snapshot atomically with a temp file + rename so replay protection and secret rotation survive process restarts without partially consuming credentials.

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
  - rejected: localhost, loopback, and private-network IP destinations
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

### `403 Forbidden`

Possible `error.code` values:

- `invalid_claim_proof`
- `expired_claim_proof`
- `claim_proof_already_used`
- `invalid_update_secret`
- `ownership_mismatch`

### `409 Conflict`

Possible `error.code` values:

- `already_claimed`

### `500 Internal Server Error`

Possible `error.code` values:

- `persistence_failure`
- `secret_rotation_failure`

Server failures must not consume or rotate credentials unless the write succeeded.
