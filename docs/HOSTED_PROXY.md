# Hosted Proxy MVP

This document defines the MVP trust model and the initial hosted setup handoff for claiming one tunnel URL per GitHub App installation.

The goal stays narrow: the hosted proxy identifies a GitHub App installation during setup, mints one short-lived single-use `claim_proof`, and the CLI uses that proof once against `POST /register`.

## Trust Model

- GitHub identifies the installation during the trusted install/setup callback.
- The hosted proxy mints a `claim_proof` bound to exactly one `installation_id`.
- `claim_proof` expires 10 minutes after issuance.
- `claim_proof` is consumed only after a successful initial claim write.
- Successful initial claim returns an installation-scoped `update_secret`.
- Later updates use the current `update_secret`, not `claim_proof`.

Untrusted by default:

- request-body `installation_id` by itself
- public knowledge of the tunnel URL
- replayed or stale `claim_proof`
- replayed or stale `update_secret`

## Stored State

Per installation, the proxy stores:

- canonical `tunnel_url`
- current `update_secret`
- creation and last-update timestamps

Per proof, the proxy stores only the state needed for expiry and replay prevention:

- proof digest
- bound `installation_id`
- issued-at timestamp
- expiry timestamp
- whether the proof has already been used

## Trusted Setup Callback

The hosted setup entry point is:

```text
GET /setup/install?installation_id=123456&setup_action=install
```

Behavior:

- validates `installation_id`
- mints a new one-time `claim_proof`
- returns the proof plus expiry metadata
- includes the exact `POST /register` payload shape for CLI handoff

Delivery path:

- Browser setup flow: render a minimal HTML handoff page showing the one-time JSON payload the user needs next.
- CLI or scripted setup flow: request the same endpoint with `Accept: application/json` and read `claim_proof` from the JSON response.

Example JSON response:

```json
{
  "status": "claim_proof_issued",
  "installation_id": 123456,
  "claim_proof": "cp_opaque_random_value",
  "expires_at": "2026-03-12T12:34:56Z",
  "register_path": "/register",
  "cli_handoff": {
    "method": "POST",
    "path": "/register",
    "body": {
      "installation_id": 123456,
      "tunnel_url": "https://YOUR-TUNNEL.example",
      "claim_proof": "cp_opaque_random_value"
    }
  }
}
```

## `POST /register`

`POST /register` handles both initial claim and later tunnel rotation.

Exactly one of `claim_proof` or `update_secret` must be present.

Common request shape:

```json
{
  "installation_id": 123456,
  "tunnel_url": "https://abc123.trycloudflare.com",
  "claim_proof": "optional-initial-proof",
  "update_secret": "optional-current-update-secret"
}
```

Validation rules:

- `installation_id` must be positive
- `tunnel_url` must be an HTTPS origin with no path, query, fragment, or userinfo
- localhost, loopback, private-network, and IP-literal targets are rejected
- both auth fields present: `400 invalid_request`
- neither auth field present: `400 invalid_request`

### Initial Claim

Use `claim_proof` only when the installation has no stored mapping yet.

Security rules:

- the proof must exist
- the proof must still be within its 10-minute TTL
- the proof must be bound to the same `installation_id`
- the proof must not have been used before
- successful claim consumes the proof
- malformed or unsafe requests do not consume the proof

Success response:

```json
{
  "status": "claimed",
  "installation_id": 123456,
  "tunnel_url": "https://abc123.trycloudflare.com",
  "update_secret": "us_new_secret_value",
  "rotated": false
}
```

### Update

Use `update_secret` after the first claim.

Rules:

- `update_secret` is scoped to exactly one `installation_id`
- every successful write rotates `update_secret`
- failed update attempts do not rotate the secret

Success response:

```json
{
  "status": "updated",
  "installation_id": 123456,
  "tunnel_url": "https://next456.trycloudflare.com",
  "update_secret": "us_replacement_secret_value",
  "rotated": true
}
```

## Error Codes

`400 Bad Request`

- `invalid_request`
- `invalid_tunnel_url`
- `unsafe_tunnel_url`

`403 Forbidden`

- `invalid_claim_proof`
- `expired_claim_proof`
- `claim_proof_already_used`
- `invalid_update_secret`
- `ownership_mismatch`

`409 Conflict`

- `already_claimed`

`500 Internal Server Error`

- `persistence_failure`
