use chrono::{Duration, Utc};
use githubclaw::hosted_proxy::{
    mint_claim_proof, ClaimProofClaims, HostedProxyError, HostedProxyState, RegisterRequest,
    RegistrationStatus,
};
use tempfile::TempDir;
use uuid::Uuid;

fn valid_claim(installation_id: u64, secret: &str) -> String {
    mint_claim_proof(
        &ClaimProofClaims {
            installation_id,
            issued_at: Utc::now(),
            expires_at: Utc::now() + Duration::minutes(10),
            nonce: format!("nonce-{installation_id}-{}", Uuid::new_v4()),
        },
        secret,
    )
}

#[test]
fn register_rejects_invalid_and_unsafe_tunnel_urls_and_normalizes_valid_ones() {
    let tmp = TempDir::new().unwrap();
    let state =
        HostedProxyState::load(tmp.path().join("hosted-proxy.json"), "claim-secret").unwrap();

    for tunnel_url in ["http://example.com", "example.com"] {
        let err = state
            .register(RegisterRequest {
                installation_id: 1,
                tunnel_url: tunnel_url.to_string(),
                claim_proof: Some(valid_claim(1, "claim-secret")),
                update_secret: None,
            })
            .unwrap_err();
        assert_eq!(
            err,
            HostedProxyError::InvalidTunnelUrl,
            "{tunnel_url} should be invalid"
        );
    }

    for tunnel_url in [
        "https://example.com/path",
        "https://user@example.com",
        "https://localhost",
        "https://127.0.0.1",
    ] {
        let err = state
            .register(RegisterRequest {
                installation_id: 1,
                tunnel_url: tunnel_url.to_string(),
                claim_proof: Some(valid_claim(1, "claim-secret")),
                update_secret: None,
            })
            .unwrap_err();
        assert_eq!(
            err,
            HostedProxyError::UnsafeTunnelUrl,
            "{tunnel_url} should be unsafe"
        );
    }

    let response = state
        .register(RegisterRequest {
            installation_id: 1,
            tunnel_url: "HTTPS://Example.COM:443".to_string(),
            claim_proof: Some(valid_claim(1, "claim-secret")),
            update_secret: None,
        })
        .unwrap();

    assert_eq!(response.status, RegistrationStatus::Claimed);
    assert_eq!(response.tunnel_url, "https://example.com");
    assert!(!response.rotated);
}

#[test]
fn initial_claim_rejects_invalid_expired_and_replayed_claim_proofs() {
    let tmp = TempDir::new().unwrap();
    let state =
        HostedProxyState::load(tmp.path().join("hosted-proxy.json"), "claim-secret").unwrap();

    let err = state
        .register(RegisterRequest {
            installation_id: 7,
            tunnel_url: "https://alpha.trycloudflare.com".to_string(),
            claim_proof: Some("garbage".to_string()),
            update_secret: None,
        })
        .unwrap_err();
    assert_eq!(err, HostedProxyError::InvalidClaimProof);

    let expired = mint_claim_proof(
        &ClaimProofClaims {
            installation_id: 7,
            issued_at: Utc::now() - Duration::minutes(11),
            expires_at: Utc::now() - Duration::seconds(1),
            nonce: "expired".to_string(),
        },
        "claim-secret",
    );
    let err = state
        .register(RegisterRequest {
            installation_id: 7,
            tunnel_url: "https://alpha.trycloudflare.com".to_string(),
            claim_proof: Some(expired),
            update_secret: None,
        })
        .unwrap_err();
    assert_eq!(err, HostedProxyError::ExpiredClaimProof);

    let proof = valid_claim(7, "claim-secret");
    let claimed = state
        .register(RegisterRequest {
            installation_id: 7,
            tunnel_url: "https://alpha.trycloudflare.com".to_string(),
            claim_proof: Some(proof.clone()),
            update_secret: None,
        })
        .unwrap();
    assert_eq!(claimed.status, RegistrationStatus::Claimed);

    let err = state
        .register(RegisterRequest {
            installation_id: 7,
            tunnel_url: "https://beta.trycloudflare.com".to_string(),
            claim_proof: Some(proof),
            update_secret: None,
        })
        .unwrap_err();
    assert_eq!(err, HostedProxyError::ClaimProofAlreadyUsed);
}

#[test]
fn update_requires_current_secret_and_rotates_it_even_when_url_is_unchanged() {
    let tmp = TempDir::new().unwrap();
    let state =
        HostedProxyState::load(tmp.path().join("hosted-proxy.json"), "claim-secret").unwrap();

    let claimed = state
        .register(RegisterRequest {
            installation_id: 42,
            tunnel_url: "https://alpha.trycloudflare.com".to_string(),
            claim_proof: Some(valid_claim(42, "claim-secret")),
            update_secret: None,
        })
        .unwrap();

    let err = state
        .register(RegisterRequest {
            installation_id: 42,
            tunnel_url: "https://beta.trycloudflare.com".to_string(),
            claim_proof: None,
            update_secret: Some("wrong-secret".to_string()),
        })
        .unwrap_err();
    assert_eq!(err, HostedProxyError::InvalidUpdateSecret);

    let updated = state
        .register(RegisterRequest {
            installation_id: 42,
            tunnel_url: "https://ALPHA.trycloudflare.com:443".to_string(),
            claim_proof: None,
            update_secret: Some(claimed.update_secret.clone()),
        })
        .unwrap();
    assert_eq!(updated.status, RegistrationStatus::Updated);
    assert_eq!(updated.tunnel_url, "https://alpha.trycloudflare.com");
    assert!(updated.rotated);
    assert_ne!(updated.update_secret, claimed.update_secret);

    let err = state
        .register(RegisterRequest {
            installation_id: 42,
            tunnel_url: "https://gamma.trycloudflare.com".to_string(),
            claim_proof: None,
            update_secret: Some(claimed.update_secret),
        })
        .unwrap_err();
    assert_eq!(err, HostedProxyError::InvalidUpdateSecret);
}

#[test]
fn duplicate_claim_and_cross_installation_credentials_are_blocked() {
    let tmp = TempDir::new().unwrap();
    let state =
        HostedProxyState::load(tmp.path().join("hosted-proxy.json"), "claim-secret").unwrap();

    let install_one = state
        .register(RegisterRequest {
            installation_id: 1,
            tunnel_url: "https://one.trycloudflare.com".to_string(),
            claim_proof: Some(valid_claim(1, "claim-secret")),
            update_secret: None,
        })
        .unwrap();

    let err = state
        .register(RegisterRequest {
            installation_id: 1,
            tunnel_url: "https://other.trycloudflare.com".to_string(),
            claim_proof: Some(valid_claim(1, "claim-secret")),
            update_secret: None,
        })
        .unwrap_err();
    assert_eq!(err, HostedProxyError::AlreadyClaimed);

    let err = state
        .register(RegisterRequest {
            installation_id: 2,
            tunnel_url: "https://two.trycloudflare.com".to_string(),
            claim_proof: Some(valid_claim(1, "claim-secret")),
            update_secret: None,
        })
        .unwrap_err();
    assert_eq!(err, HostedProxyError::OwnershipMismatch);

    state
        .register(RegisterRequest {
            installation_id: 2,
            tunnel_url: "https://two.trycloudflare.com".to_string(),
            claim_proof: Some(valid_claim(2, "claim-secret")),
            update_secret: None,
        })
        .unwrap();

    let err = state
        .register(RegisterRequest {
            installation_id: 2,
            tunnel_url: "https://attacker.trycloudflare.com".to_string(),
            claim_proof: None,
            update_secret: Some(install_one.update_secret),
        })
        .unwrap_err();
    assert_eq!(err, HostedProxyError::OwnershipMismatch);
}

#[test]
fn hosted_proxy_state_persists_installations_and_replay_protection_across_reload() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("hosted-proxy.json");
    let proof = valid_claim(9, "claim-secret");

    let initial_secret = {
        let state = HostedProxyState::load(&path, "claim-secret").unwrap();
        let claimed = state
            .register(RegisterRequest {
                installation_id: 9,
                tunnel_url: "https://start.trycloudflare.com".to_string(),
                claim_proof: Some(proof.clone()),
                update_secret: None,
            })
            .unwrap();
        claimed.update_secret
    };

    let reloaded = HostedProxyState::load(&path, "claim-secret").unwrap();
    let registration = reloaded.get_registration(9).unwrap();
    assert_eq!(registration.tunnel_url, "https://start.trycloudflare.com");

    let err = reloaded
        .register(RegisterRequest {
            installation_id: 9,
            tunnel_url: "https://replay.trycloudflare.com".to_string(),
            claim_proof: Some(proof),
            update_secret: None,
        })
        .unwrap_err();
    assert_eq!(err, HostedProxyError::ClaimProofAlreadyUsed);

    let updated = reloaded
        .register(RegisterRequest {
            installation_id: 9,
            tunnel_url: "https://next.trycloudflare.com".to_string(),
            claim_proof: None,
            update_secret: Some(initial_secret.clone()),
        })
        .unwrap();
    assert_eq!(updated.tunnel_url, "https://next.trycloudflare.com");

    let err = HostedProxyState::load(&path, "claim-secret")
        .unwrap()
        .register(RegisterRequest {
            installation_id: 9,
            tunnel_url: "https://stale.trycloudflare.com".to_string(),
            claim_proof: None,
            update_secret: Some(initial_secret),
        })
        .unwrap_err();
    assert_eq!(err, HostedProxyError::InvalidUpdateSecret);
}
