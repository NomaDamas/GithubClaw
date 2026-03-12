# Hosted Proxy MVP

This document defines the MVP trust model, the hosted setup callback that mints initial `claim_proof` values, and the `POST /register` contract for hosted proxy mode.

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

The minimal proof-issuance flow is a GitHub App setup callback:

1. Configure the GitHub App setup URL to point at `GET /setup/github-app`.
2. After installation, GitHub redirects the installer to that callback with `installation_id`.
3. The server mints a fresh `claim_proof` for that `installation_id`.
4. The server persists only the proof hash, installation binding, expiry time, and whether the proof has been consumed.
5. The callback returns JSON with:
   - `installation_id`
   - `claim_proof`
   - `expires_at`
   - `register_path`

Example response:

```json
{
  "installation_id": 123456,
  "claim_proof": "cp_opaque_server_minted_value",
  "expires_at": "2026-03-12T10:00:00Z",
  "register_path": "/register"
}
```

MVP operator flow:

- The installer can copy the proof directly from the callback response.
- CLI work in `#4` can later automate this by opening the setup URL and consuming the returned JSON.
- No user account, dashboard session, or manual proxy database edit is required.

## Stored State

Per proof, the proxy stores only:

- hashed proof value
- bound `installation_id`
- expiry timestamp
- consumed timestamp for replay protection

Per claimed installation, the proxy stores:

- canonical `tunnel_url`
- current `update_secret` hash
- creation and last-update timestamps

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
- After one successful claim, replay of the same proof fails with `403 claim_proof_already_used`.

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
