# Hosted Proxy MVP

This document defines the hosted proxy registration boundary for the MVP setup flow.

## Trust Model

- GitHub identifies the installation during the App install/setup callback.
- The setup callback mints a short-lived `claim_proof` bound to one `installation_id`.
- `POST /register` consumes that proof on the first successful claim and returns an `update_secret`.
- Later tunnel updates require the current `update_secret`, and every successful update rotates it.

## Stored State

Per installation, the proxy persists:

- canonical `tunnel_url`
- current `update_secret`
- creation and update timestamps

Per minted proof, the proxy persists:

- bound `installation_id`
- issuance timestamp
- exact expiry timestamp
- whether the proof has already been consumed

That state is enough to reject expired proofs, stale update secrets, and proof replay across restarts.

## Hosted Setup Callback

The minimal hosted setup flow issues a claim proof at:

```text
GET /setup/callback?installation_id=123456
```

Success response:

```json
{
  "status": "proof_issued",
  "installation_id": 123456,
  "claim_proof": "cp_opaque_server_minted_value",
  "expires_at": "2026-03-12T12:34:56Z",
  "register_path": "/register"
}
```

Rules:

- `installation_id` must be a positive integer
- each successful callback mints a fresh proof
- `claim_proof` expires exactly 10 minutes after issuance
- proof replay is enforced in `POST /register`

## Handoff into `POST /register`

After the callback returns the proof, the client immediately claims its tunnel mapping:

```json
{
  "installation_id": 123456,
  "tunnel_url": "https://abc123.trycloudflare.com",
  "claim_proof": "cp_opaque_server_minted_value"
}
```

`POST /register` validates:

- exactly one of `claim_proof` or `update_secret` is present
- `tunnel_url` is an HTTPS origin only, with no path, query, fragment, or userinfo
- localhost, loopback, and private-network IP destinations are rejected

## Registration Responses

Initial claim success:

```json
{
  "status": "claimed",
  "installation_id": 123456,
  "tunnel_url": "https://abc123.trycloudflare.com",
  "update_secret": "us_new_secret_value",
  "rotated": false
}
```

Update success:

```json
{
  "status": "updated",
  "installation_id": 123456,
  "tunnel_url": "https://next456.trycloudflare.com",
  "update_secret": "us_replacement_secret_value",
  "rotated": true
}
```

## Failure Cases

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

## Deterministic Rules

- validation failures do not consume proofs or rotate secrets
- the first successful claim returns `201 Created`
- later successful updates return `200 OK`
- replaying an already-used proof returns `403 claim_proof_already_used`
- replaying a stale rotated secret returns `403 invalid_update_secret`
