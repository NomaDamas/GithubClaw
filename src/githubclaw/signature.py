"""Webhook signature verification using HMAC SHA-256."""

import hashlib
import hmac


def verify_webhook_signature(payload_body: bytes, signature_header: str, secret: str) -> bool:
    """Verify a GitHub webhook payload against the X-Hub-Signature-256 header.

    Args:
        payload_body: Raw bytes of the request body.
        signature_header: Value of the X-Hub-Signature-256 header (e.g. "sha256=abc123...").
        secret: The webhook secret shared with GitHub.

    Returns:
        True if the signature is valid, False otherwise.
    """
    if not signature_header:
        return False

    expected = (
        "sha256=" + hmac.new(secret.encode("utf-8"), payload_body, hashlib.sha256).hexdigest()
    )

    return hmac.compare_digest(expected, signature_header)
