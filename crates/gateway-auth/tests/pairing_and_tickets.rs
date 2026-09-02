//! End-to-end checks of the pairing and ticket rules, against real SQLite.

use std::sync::Arc;
use std::time::Duration;

use gateway_auth::{AuthConfig, AuthService, MachineIdentity, PairingRequest};
use gateway_core::ErrorKind;
use gateway_core::ids::DeviceId;
use gateway_core::ports::{Clock, SystemClock};
use gateway_store::Database;

async fn service(config: AuthConfig) -> AuthService {
    let db = Database::connect_in_memory().await.unwrap();
    AuthService::new(
        MachineIdentity::ephemeral(),
        db.devices(),
        db.pairing_codes(),
        db.tickets(),
        Arc::new(SystemClock) as Arc<dyn Clock>,
        config,
    )
}

fn pairing_request(offer: &gateway_auth::PairingOffer) -> PairingRequest {
    PairingRequest {
        pairing_code: offer.pairing_code.clone(),
        nonce: offer.nonce.clone(),
        device_name: "iPhone".to_owned(),
        platform: "ios".to_owned(),
        // 32 zero bytes, base64.
        public_key: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_owned(),
    }
}

#[tokio::test]
async fn a_paired_device_can_open_exactly_one_socket_per_ticket() {
    let auth = service(AuthConfig::default()).await;
    let offer = auth.begin_pairing(None).await.unwrap();
    let device = auth
        .complete_pairing(pairing_request(&offer))
        .await
        .unwrap();

    let ticket = auth.issue_ticket(&device.id).await.unwrap();
    assert_eq!(auth.redeem_ticket(&ticket.id).await.unwrap().id, device.id);

    let error = auth.redeem_ticket(&ticket.id).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Expired);
}

#[tokio::test]
async fn a_pairing_code_cannot_be_reused_or_guessed() {
    let auth = service(AuthConfig::default()).await;
    let offer = auth.begin_pairing(None).await.unwrap();

    let wrong_nonce = PairingRequest {
        nonce: "not-the-nonce".to_owned(),
        ..pairing_request(&offer)
    };
    assert_eq!(
        auth.complete_pairing(wrong_nonce).await.unwrap_err().kind(),
        ErrorKind::Unauthenticated
    );

    // The failed attempt consumed the code: a shoulder-surfed code is worth
    // one attempt, not unlimited guesses at the nonce.
    assert_eq!(
        auth.complete_pairing(pairing_request(&offer))
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::Expired
    );
}

#[tokio::test]
async fn an_expired_pairing_code_is_refused() {
    let auth = service(AuthConfig {
        pairing_ttl: Duration::from_millis(1),
        ..AuthConfig::default()
    })
    .await;
    let offer = auth.begin_pairing(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(
        auth.complete_pairing(pairing_request(&offer))
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::Expired
    );
}

#[tokio::test]
async fn an_expired_ticket_is_refused() {
    let auth = service(AuthConfig {
        ticket_ttl: Duration::from_millis(1),
        ..AuthConfig::default()
    })
    .await;
    let offer = auth.begin_pairing(None).await.unwrap();
    let device = auth
        .complete_pairing(pairing_request(&offer))
        .await
        .unwrap();
    let ticket = auth.issue_ticket(&device.id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(
        auth.redeem_ticket(&ticket.id).await.unwrap_err().kind(),
        ErrorKind::Expired
    );
}

#[tokio::test]
async fn a_revoked_device_gets_no_new_tickets_and_cannot_use_old_ones() {
    let auth = service(AuthConfig::default()).await;
    let offer = auth.begin_pairing(None).await.unwrap();
    let device = auth
        .complete_pairing(pairing_request(&offer))
        .await
        .unwrap();
    let issued_before_revocation = auth.issue_ticket(&device.id).await.unwrap();

    auth.revoke_device(&device.id).await.unwrap();

    assert_eq!(
        auth.issue_ticket(&device.id).await.unwrap_err().kind(),
        ErrorKind::Forbidden
    );
    assert_eq!(
        auth.redeem_ticket(&issued_before_revocation.id)
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::Forbidden
    );
}

#[tokio::test]
async fn revoking_an_unknown_device_is_an_error_not_a_silent_success() {
    let auth = service(AuthConfig::default()).await;
    assert_eq!(
        auth.revoke_device(&DeviceId::new("device_nope"))
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::NotFound
    );
}

#[tokio::test]
async fn a_ticket_from_another_machine_is_refused() {
    let db = Database::connect_in_memory().await.unwrap();
    let clock = Arc::new(SystemClock) as Arc<dyn Clock>;
    let first = AuthService::new(
        MachineIdentity::ephemeral(),
        db.devices(),
        db.pairing_codes(),
        db.tickets(),
        Arc::clone(&clock),
        AuthConfig::default(),
    );
    // A second gateway identity sharing the same database stands in for a
    // ticket that was minted elsewhere and replayed here.
    let second = AuthService::new(
        MachineIdentity::ephemeral(),
        db.devices(),
        db.pairing_codes(),
        db.tickets(),
        clock,
        AuthConfig::default(),
    );

    let offer = first.begin_pairing(None).await.unwrap();
    let device = first
        .complete_pairing(pairing_request(&offer))
        .await
        .unwrap();
    let ticket = first.issue_ticket(&device.id).await.unwrap();

    assert_eq!(
        second.redeem_ticket(&ticket.id).await.unwrap_err().kind(),
        ErrorKind::Unauthenticated
    );
}

#[tokio::test]
async fn a_ticket_request_must_be_signed_by_the_paired_device() {
    use base64::Engine;
    use ed25519_dalek::{Signer, SigningKey};

    let auth = service(AuthConfig::default()).await;
    let offer = auth.begin_pairing(None).await.unwrap();

    let key = SigningKey::generate(&mut rand::rngs::OsRng);
    let engine = base64::engine::general_purpose::STANDARD;
    let device = auth
        .complete_pairing(PairingRequest {
            public_key: engine.encode(key.verifying_key().as_bytes()),
            ..pairing_request(&offer)
        })
        .await
        .unwrap();

    let issued_at = chrono::Utc::now();
    let challenge = auth.ticket_challenge(&device.id, issued_at);
    let signature = engine.encode(key.sign(challenge.as_bytes()).to_bytes());

    let ticket = auth
        .issue_ticket_for_signed_request(&device.id, issued_at, &signature)
        .await
        .unwrap();
    assert_eq!(auth.redeem_ticket(&ticket.id).await.unwrap().id, device.id);

    // A signature over someone else's challenge does not work.
    let forged = engine.encode(key.sign(b"ws-ticket:other").to_bytes());
    assert_eq!(
        auth.issue_ticket_for_signed_request(&device.id, issued_at, &forged)
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::Unauthenticated
    );

    // Neither does a valid signature from last week.
    let stale = chrono::Utc::now() - chrono::Duration::hours(1);
    let stale_signature = engine.encode(
        key.sign(auth.ticket_challenge(&device.id, stale).as_bytes())
            .to_bytes(),
    );
    assert_eq!(
        auth.issue_ticket_for_signed_request(&device.id, stale, &stale_signature)
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::Unauthenticated
    );
}

#[tokio::test]
async fn the_qr_payload_matches_the_documented_shape() {
    let auth = service(AuthConfig::default()).await;
    let offer = auth
        .begin_pairing(Some("https://relay.example.com".to_owned()))
        .await
        .unwrap();
    let payload: serde_json::Value = serde_json::from_str(&offer.to_qr_payload().unwrap()).unwrap();
    assert!(
        payload["machine_id"]
            .as_str()
            .unwrap()
            .starts_with("machine_")
    );
    assert_eq!(payload["pairing_code"].as_str().unwrap().len(), 6);
    assert!(!payload["nonce"].as_str().unwrap().is_empty());
    assert_eq!(payload["endpoint"], "https://relay.example.com");
}
