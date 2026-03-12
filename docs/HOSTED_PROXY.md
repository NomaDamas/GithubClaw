# Hosted Proxy MVP

This document defines the minimal hosted setup callback and the exact `POST /register` contract for hosted proxy mode.

## Trust Model

- GitHub identifies the installation during the install/setup callback.
- The setup callback mints a short-lived, single-use `claim_proof` bound to one `installation_id`.
- `POST /register` consumes that proof to create the first tunnel mapping and returns an `update_secret`.
- Later updates require the current `update_secret`, and every successful update rotates it.

## Stored State

Per installation, the proxy stores:

- canonical `tunnel_url`
- current `update_secret`
- creation and last-update timestamps

Per issued `claim_proof`, the proxy stores:

- bound `installation_id`
- expiry timestamp
- whether the proof has already been consumed

That state is sufficient for expiry checks and replay rejection without adding accounts, sessions, or dashboard state.

## Hosted Setup Callback

### `GET /setup/install/callback`

Query parameters:

- `installation_id`: positive GitHub App installation id from the setup redirect

Success response:

```json
{
  "status": "ready_to_claim",
  "installation_id": 123456,
  "claim_proof": "cp_opaque_server_minted_value",
  "expires_at": "2026-03-12T10:15:00Z",
  "register_request": {
    "installation_id": 123456,
    "claim_proof": "cp_opaque_server_minted_value",
    "tunnel_url": "https://your-tunnel-host.example"
  }
}
```

Rules:

- every successful callback hit mints a fresh proof
- the proof expires 10 minutes after issuance
- the proof is scoped to exactly one `installation_id`
- the callback returns the exact `installation_id` + `claim_proof` pair that must be forwarded to `POST /register`
- the callback does not create a user account, browser session, or dashboard state

Handoff to `POST /register`:

1. GitHub redirects the installer to `GET /setup/install/callback?installation_id=...`
2. the server returns a one-time `claim_proof` plus the registration template
3. the client collects the desired `tunnel_url`
4. the client sends `installation_id`, `tunnel_url`, and `claim_proof` to `POST /register`
5. a successful `POST /register` response returns the first `update_secret`

## `POST /register`

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
- `tunnel_url` must be an HTTPS origin only
- path, query, fragment, and userinfo are rejected
- localhost, loopback, and private-network IP destinations are rejected
- both auth fields present: `400 invalid_request`
- neither auth field present: `400 invalid_request`

### Initial Claim

Use `claim_proof` only when the installation has no stored mapping yet.

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

Claim-proof rules:

- the proof is bound to one `installation_id`
- the proof expires exactly 10 minutes after issuance
- successful initial claim consumes the proof
- failed validation does not consume the proof
- replaying a used proof returns `403 claim_proof_already_used`

### Update

Use `update_secret` after the installation has already been claimed.

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

Update rules:

- `update_secret` is scoped to exactly one installation
- every successful update rotates the secret
- stale secrets return `403 invalid_update_secret`

## Error Codes

`400 Bad Request`:

- `invalid_request`
- `invalid_tunnel_url`
- `unsafe_tunnel_url`

`403 Forbidden`:

- `invalid_claim_proof`
- `expired_claim_proof`
- `claim_proof_already_used`
- `invalid_update_secret`
- `ownership_mismatch`

`409 Conflict`:

- `already_claimed`

`500 Internal Server Error`:

- `persistence_failure`
