# Hosted Proxy MVP

## Purpose

The hosted proxy lets one shared GithubClaw service receive GitHub App webhooks and forward each installation's traffic to that user's public tunnel URL.

For MVP, the only hosted state is:

- `installation_id -> tunnel_url`
- `installation_id -> update_secret_hash`
- `installation_id -> claimed_owner_id`
- claim-proof replay protection metadata

Everything else stays out of scope.

## Trust Model

The hosted proxy trusts exactly two facts:

1. A trusted GitHub App install/setup callback can prove which `installation_id` is being claimed and who owns it.
2. Possession of the current `update_secret` is enough to rotate the tunnel URL for that installation later.

The proxy does **not** trust:

- a bare `installation_id`
- control of a tunnel URL by itself
- a GitHub PAT or arbitrary user token sent to `POST /register`
- long-lived browser sessions, dashboards, or local operator access

### Initial Claim

After the GitHub App install/setup step, the hosted service mints a short-lived single-use `claim_proof` bound to:

- `installation_id`
- `owner_id`
- `expires_at`
- `nonce`

The proof is opaque to clients and signed by the hosted service.

### Later Updates

After a successful first claim, the proxy returns a long random `update_secret`.

- It is scoped to one `installation_id`.
- The server stores only a hash of the secret.
- Every successful update rotates the secret and invalidates the previous one immediately.

This keeps the core small: no accounts, no OAuth dance on every update, no dashboard state.

## Endpoint

### `POST /register`

Claims or updates the tunnel URL for one GitHub App installation.

Exactly one auth field must be present:

- `claim_proof` for the first claim
- `update_secret` for later updates

### Request Schema

```json
{
  "installation_id": 123456,
  "tunnel_url": "https://abc123.trycloudflare.com",
  "claim_proof": "optional-first-claim-proof",
  "update_secret": "optional-update-secret"
}
```

Rules:

- `installation_id` is required and must be a positive integer.
- `tunnel_url` is required.
- `claim_proof` and `update_secret` are mutually exclusive.
- Requests with both or neither must be rejected.

### Tunnel URL Rules

MVP validation is intentionally conservative:

- HTTPS only
- origin form only: `https://host[:port]`
- no path, query, fragment, or embedded credentials
- reject localhost, loopback, and private-network targets
- normalize host casing before persistence

## First-Claim Flow

Request:

```json
{
  "installation_id": 123456,
  "tunnel_url": "https://abc123.trycloudflare.com",
  "claim_proof": "cp_..."
}
```

Server behavior:

1. Verify the proof signature.
2. Reject if expired.
3. Reject if the proof nonce was already consumed.
4. Reject if the proof's `installation_id` does not match the body.
5. Reject if the proof's `owner_id` does not match the owner already stored for that installation.
6. Persist the normalized tunnel URL, claimed owner, and hashed `update_secret`.
7. Mark the proof nonce as consumed.
8. Return a fresh `update_secret`.

Success response:

```json
{
  "installation_id": 123456,
  "tunnel_url": "https://abc123.trycloudflare.com",
  "update_secret": "us_...",
  "rotated": false
}
```

Status:

- `201 Created` for the first successful claim

## Update Flow

Request:

```json
{
  "installation_id": 123456,
  "tunnel_url": "https://new-abc123.trycloudflare.com",
  "update_secret": "us_current_..."
}
```

Server behavior:

1. Look up the installation.
2. Verify the presented `update_secret` against the stored hash.
3. Normalize and validate the new tunnel URL.
4. Persist the new tunnel URL.
5. Mint a replacement `update_secret`.
6. Replace the stored secret hash atomically.

Success response:

```json
{
  "installation_id": 123456,
  "tunnel_url": "https://new-abc123.trycloudflare.com",
  "update_secret": "us_next_...",
  "rotated": true
}
```

Status:

- `200 OK` for a successful update

## Error Contract

Error body:

```json
{
  "error": {
    "code": "invalid_or_expired_claim_proof",
    "message": "claim_proof is invalid or expired"
  }
}
```

Status and codes:

| Status | Code | Meaning |
|--------|------|---------|
| `400 Bad Request` | `invalid_request` | Missing required fields, both auth modes present, or malformed JSON |
| `400 Bad Request` | `unsafe_tunnel_url` | URL is not an allowed public HTTPS origin |
| `403 Forbidden` | `invalid_or_expired_claim_proof` | Proof signature invalid or proof expired |
| `409 Conflict` | `claim_proof_already_used` | Proof nonce was already consumed |
| `403 Forbidden` | `installation_mismatch` | Body `installation_id` does not match the proof |
| `403 Forbidden` | `ownership_mismatch` | Claim proof owner does not match the owner already stored for that installation |
| `404 Not Found` | `registration_not_found` | Update requested before the installation was ever claimed |
| `403 Forbidden` | `invalid_update_secret` | Update secret is wrong, stale, or already rotated out |
| `409 Conflict` | `already_claimed` | Installation already has a registration and the client must use `update_secret` instead of a new first-claim proof |
| `500 Internal Server Error` | `persistence_error` | Durable state could not be written |

## Expiry And Single-Use Rules

- `claim_proof` must be short-lived. MVP target: 10 minutes or less.
- `claim_proof` is single-use. A consumed nonce must never succeed again.
- `update_secret` remains valid until the next successful update.
- Once a new `update_secret` is issued, the previous secret must fail immediately.
- Retrying the same update request after a successful rotation must fail with `invalid_update_secret` unless the caller uses the newly returned secret.

## Ownership Mismatch Handling

Ownership mismatch is only enforced at the claim boundary.

- The proof carries the GitHub owner identity established during the trusted install/setup step.
- The first successful claim stores that owner identity with the installation.
- Any later attempt to re-claim the same `installation_id` with a proof for a different owner must fail with `403 ownership_mismatch`.

MVP intentionally does **not** solve ownership transfer recovery. If installation ownership changes, recovery is an operator workflow or a later feature.

## Explicit Non-Goals

The hosted proxy MVP does **not** include:

- user accounts
- dashboard auth
- sending GitHub PATs or arbitrary user tokens to the proxy
- per-request GitHub ownership verification for update calls
- multi-user role management
- audit dashboards or self-service recovery flows
- broad SSRF exceptions for private or local targets

This contract is deliberately small so the core remains a simple forwarding service with one narrow trust boundary.
