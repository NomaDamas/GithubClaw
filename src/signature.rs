//! Webhook signature verification using HMAC SHA-256.
//!
//! Translates the Python `signature.py` module to Rust.
//! Verifies GitHub webhook payloads against the `X-Hub-Signature-256` header.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Verify a GitHub webhook payload against the `X-Hub-Signature-256` header.
///
/// # Arguments
///
/// * `payload_body` - Raw bytes of the request body.
/// * `signature_header` - Value of the `X-Hub-Signature-256` header (e.g. `"sha256=abc123..."`).
/// * `secret` - The webhook secret shared with GitHub.
///
/// # Returns
///
/// `true` if the signature is valid, `false` otherwise.
pub fn verify_webhook_signature(payload_body: &[u8], signature_header: &str, secret: &str) -> bool {
    if signature_header.is_empty() {
        return false;
    }

    let hex_signature = match signature_header.strip_prefix("sha256=") {
        Some(hex) => hex,
        None => return false,
    };

    let signature_bytes = match hex::decode(hex_signature) {
        Ok(bytes) => bytes,
        Err(_) => return false,
    };

    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC can take key of any size");
    mac.update(payload_body);

    // `verify_slice` performs constant-time comparison.
    mac.verify_slice(&signature_bytes).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hmac::Mac;

    /// Helper: compute a valid sha256=<hex> signature for the given payload and secret.
    fn compute_signature(payload: &[u8], secret: &str) -> String {
        let mut mac =
            HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC can take key of any size");
        mac.update(payload);
        let result = mac.finalize();
        format!("sha256={}", hex::encode(result.into_bytes()))
    }

    #[test]
    fn valid_signature_returns_true() {
        let payload = b"hello world";
        let secret = "my-secret";
        let sig = compute_signature(payload, secret);
        assert!(verify_webhook_signature(payload, &sig, secret));
    }

    #[test]
    fn invalid_signature_returns_false() {
        let payload = b"hello world";
        let secret = "my-secret";
        let bad_sig = "sha256=0000000000000000000000000000000000000000000000000000000000000000";
        assert!(!verify_webhook_signature(payload, bad_sig, secret));
    }

    #[test]
    fn empty_signature_header_returns_false() {
        assert!(!verify_webhook_signature(b"payload", "", "secret"));
    }

    #[test]
    fn missing_sha256_prefix_returns_false() {
        let payload = b"hello world";
        let secret = "my-secret";
        let sig = compute_signature(payload, secret);
        // Strip the "sha256=" prefix so only the hex remains.
        let hex_only = sig.strip_prefix("sha256=").unwrap();
        assert!(!verify_webhook_signature(payload, hex_only, secret));
    }

    #[test]
    fn invalid_hex_in_signature_returns_false() {
        assert!(!verify_webhook_signature(
            b"payload",
            "sha256=not-valid-hex!!!",
            "secret"
        ));
    }

    #[test]
    fn different_payload_produces_different_signature() {
        let secret = "shared-secret";
        let sig_a = compute_signature(b"payload-a", secret);
        let sig_b = compute_signature(b"payload-b", secret);
        assert_ne!(sig_a, sig_b);
        assert!(!verify_webhook_signature(b"payload-a", &sig_b, secret));
    }

    #[test]
    fn different_secret_produces_different_signature() {
        let payload = b"same payload";
        let sig_a = compute_signature(payload, "secret-a");
        let sig_b = compute_signature(payload, "secret-b");
        assert_ne!(sig_a, sig_b);
        assert!(!verify_webhook_signature(payload, &sig_a, "secret-b"));
    }
}
