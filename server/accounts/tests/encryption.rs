//! M4 crypto round-trip: publish a paid (encrypted) effect → activate with a
//! device X25519 key → download ciphertext → unseal the CEK → decrypt → recover
//! the exact plaintext, and verify the signed lease. This exercises the whole
//! decrypt-on-load path the client will run.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use vance_accounts::{build_router, config::Config, init_state};
use x25519_dalek::{PublicKey, StaticSecret};

const VERIFIER: &str = "test-verifier-0123456789-abcdefghijklmnopqrstuvwxyz-XYZ";

fn challenge() -> String {
    B64.encode(Sha256::digest(VERIFIER.as_bytes()))
}
fn sha_b64(b: &[u8]) -> String {
    B64.encode(Sha256::digest(b))
}

async fn mk_app() -> (Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        addr: "127.0.0.1:0".parse().unwrap(),
        db_path: dir.path().join("t.db"),
        signing_key_path: dir.path().join("key.bin"),
        public_url: "http://127.0.0.1:8787".into(),
        dev_provider: true,
        github: None,
        google: None,
        apple: None,
        meta: None,
    };
    (build_router(init_state(cfg).await.unwrap()), dir)
}
async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let b = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&b).unwrap_or(serde_json::Value::Null)
}
async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
    axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec()
}
async fn get(app: &Router, uri: &str, bearer: Option<&str>) -> axum::response::Response {
    let mut b = Request::builder().method("GET").uri(uri);
    if let Some(t) = bearer {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    app.clone().oneshot(b.body(Body::empty()).unwrap()).await.unwrap()
}
async fn post(app: &Router, uri: &str, bearer: Option<&str>, json: serde_json::Value) -> axum::response::Response {
    let mut b = Request::builder().method("POST").uri(uri).header("content-type", "application/json");
    if let Some(t) = bearer {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    app.clone().oneshot(b.body(Body::from(json.to_string())).unwrap()).await.unwrap()
}
async fn sign_in(app: &Router, device_id: &str) -> String {
    let uri = format!(
        "/auth/start?provider=dev&code_challenge={}&redirect_uri=http://127.0.0.1:9999/cb&device_id={}&device_name=T&platform=test",
        challenge(), device_id
    );
    let loc = get(app, &uri, None).await.headers().get("location").unwrap().to_str().unwrap().to_string();
    let code = loc.split("code=").nth(1).unwrap().to_string();
    body_json(post(app, "/auth/token", None, serde_json::json!({ "code": code, "code_verifier": VERIFIER })).await)
        .await["access_token"].as_str().unwrap().to_string()
}

/// AES-256-GCM decrypt of `nonce(12) || ciphertext`.
fn aead_open(key: &[u8; 32], blob: &[u8]) -> Vec<u8> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    cipher.decrypt(Nonce::from_slice(&blob[..12]), &blob[12..]).expect("decrypt")
}

#[tokio::test]
async fn paid_effect_encrypts_seals_and_decrypts() {
    let (app, dir) = mk_app().await;
    let wasm = dir.path().join("glow.wasm");
    let plaintext = b"\x00asm\x01\x00\x00\x00-secret-paid-effect".to_vec();
    std::fs::write(&wasm, &plaintext).unwrap();

    // Publish (paid → encrypted) + grant the dev user.
    let pubr = body_json(post(&app, "/v1/dev/publish", Some(&sign_in(&app, "seed").await), serde_json::json!({
        "sku": "com.paid.glow", "file_path": wasm.to_str().unwrap(), "dest": "glow.wasm", "display_name": "Glow"
    })).await).await;
    assert_eq!(pubr["encrypted"], true);

    let access = sign_in(&app, "device-A").await;

    // The device's static X25519 key.
    let dev_secret = StaticSecret::from([9u8; 32]);
    let dev_pub_b64 = B64.encode(PublicKey::from(&dev_secret).as_bytes());

    // Missing device key → rejected for a paid effect.
    let denied = post(&app, "/v1/artifacts/com.paid.glow", Some(&access), serde_json::json!({})).await;
    assert_eq!(denied.status(), StatusCode::BAD_REQUEST);

    // Activate with the device key.
    let art = body_json(post(&app, "/v1/artifacts/com.paid.glow", Some(&access), serde_json::json!({ "device_pubkey": dev_pub_b64 })).await).await;
    assert_eq!(art["encrypted"], true);
    let sealed = &art["sealed_cek"];
    let lease = art["lease"].as_str().unwrap();

    // Download the ciphertext; the pin must match.
    let path = format!("/download/{}", art["download_url"].as_str().unwrap().split("/download/").nth(1).unwrap());
    let ciphertext = body_bytes(get(&app, &path, None).await).await;
    assert_eq!(sha_b64(&ciphertext), art["sha256"].as_str().unwrap());

    // Unseal the CEK: ECDH(dev_secret, eph_pub) → sha256("vance-cek-seal-v1"||shared) → AES key.
    let eph_pub: [u8; 32] = B64.decode(sealed["eph_pub"].as_str().unwrap()).unwrap().try_into().unwrap();
    let shared = dev_secret.diffie_hellman(&PublicKey::from(eph_pub));
    let mut h = Sha256::new();
    h.update(b"vance-cek-seal-v1");
    h.update(shared.as_bytes());
    let seal_key: [u8; 32] = h.finalize().into();
    let sealed_blob = {
        let mut v = B64.decode(sealed["nonce"].as_str().unwrap()).unwrap();
        v.extend_from_slice(&B64.decode(sealed["ct"].as_str().unwrap()).unwrap());
        v
    };
    let cek: [u8; 32] = aead_open(&seal_key, &sealed_blob).try_into().unwrap();

    // Decrypt the effect → recover the EXACT plaintext.
    let recovered = aead_open(&cek, &ciphertext);
    assert_eq!(recovered, plaintext);

    // Lease claims: signed, bound to this device + sku + the plaintext hash.
    let claims: serde_json::Value =
        serde_json::from_slice(&B64.decode(lease.split('.').nth(1).unwrap()).unwrap()).unwrap();
    assert_eq!(claims["sku"], "com.paid.glow");
    assert_eq!(claims["plaintext_sha256"], sha_b64(&plaintext));
    assert!(claims["exp"].as_i64().unwrap() > 0);

    // And the lease signature verifies against the server's published key.
    let pk_b64 = body_json(get(&app, "/auth/pubkey", None).await).await["public_key"].as_str().unwrap().to_string();
    let vk = ed25519_dalek::VerifyingKey::from_bytes(&B64.decode(&pk_b64).unwrap().try_into().unwrap()).unwrap();
    let (h2, p2, s2) = {
        let mut it = lease.split('.');
        (it.next().unwrap(), it.next().unwrap(), it.next().unwrap())
    };
    let sig = ed25519_dalek::Signature::from_slice(&B64.decode(s2).unwrap()).unwrap();
    use ed25519_dalek::Verifier;
    assert!(vk.verify(format!("{h2}.{p2}").as_bytes(), &sig).is_ok());
}
