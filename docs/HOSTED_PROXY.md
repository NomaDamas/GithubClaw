# Hosted Proxy MVP

This document defines the MVP trust model and the exact hosted-proxy registration contract.

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

## Stored State

Per `installation_id`, the proxy stores:

- canonical `tunnel_url`
- current `update_secret` verifier
- creation and last-update timestamps

Per issued claim proof, the proxy stores:

- bound `installation_id`
- verifier for the opaque proof value
- issuance and expiry timestamps
- whether the proof has already been consumed

## Setup Flow

Configure the GitHub App setup URL to point at:

```text
GET /setup/claim-proof
```

GitHub appends the installation id in the callback query string. The MVP setup endpoint accepts:

```text
GET /setup/claim-proof?installation_id=123456
```

Success response:

```json
{
  "installation_id": 123456,
  "claim_proof": "cp_v1.opaque_proof_id.opaque_secret",
  "expires_at": "2026-03-12T10:15:00Z",
  "register_url": "/register"
}
```

Rules:

- `installation_id` must be a positive integer
- each request mints a fresh proof
- proof expiry is exactly 10 minutes after issuance
- the proof is bound to one installation and rejected for any other installation

## `POST /register`

`POST /register` handles both initial claim and later tunnel rotation.

Exactly one of `claim_proof` or `update_secret` must be present.

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
- `claim_proof`: opaque string for first claim only
- `update_secret`: opaque string for update only

Validation rules:

- both auth fields present: `400 Bad Request`
- neither auth field present: `400 Bad Request`
- invalid or unsafe tunnel URL: `400 Bad Request`
- server normalizes `tunnel_url` before storing or returning it

## Success Responses

Initial claim:

```json
{
  "status": "claimed",
  "installation_id": 123456,
  "tunnel_url": "https://abc123.trycloudflare.com",
  "update_secret": "us_new_secret_value",
  "rotated": false
}
```

Update:

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

Common codes:

- `invalid_request`
- `invalid_tunnel_url`
- `unsafe_tunnel_url`
- `invalid_claim_proof`
- `expired_claim_proof`
- `claim_proof_already_used`
- `invalid_update_secret`
- `ownership_mismatch`
- `already_claimed`

## CLI Consumption

Mint a proof:

```bash
curl "https://your-hosted-proxy.example.com/setup/claim-proof?installation_id=123456"
```

Use it immediately:

```bash
curl -X POST "https://your-hosted-proxy.example.com/register" \
  -H "Content-Type: application/json" \
  -d '{
    "installation_id": 123456,
    "tunnel_url": "https://abc123.trycloudflare.com",
    "claim_proof": "cp_v1.opaque_proof_id.opaque_secret"
  }'
```

Persist the returned `update_secret`. Every successful update returns the next secret and invalidates the previous one.
