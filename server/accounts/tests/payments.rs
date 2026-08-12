//! M4b — the Stripe webhook auto-activation brain, tested with signed synthetic
//! events (no real Stripe): payment → grant, refund → revoke, idempotent
//! re-delivery, and rejection of a bad signature.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use vance_accounts::{build_router, config::Config, init_state};

const VERIFIER: &str = "test-verifier-0123456789-abcdefghijklmnopqrstuvwxyz-XYZ";
const WEBHOOK_SECRET: &str = "whsec_test_secret";

fn challenge() -> String {
    B64.encode(Sha256::digest(VERIFIER.as_bytes()))
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
        stripe_secret: None,
        stripe_webhook_secret: Some(WEBHOOK_SECRET.to_string()),
    };
    (build_router(init_state(cfg).await.unwrap()), dir)
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let b = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&b).unwrap_or(serde_json::Value::Null)
}
async fn get(app: &Router, uri: &str, bearer: Option<&str>) -> axum::response::Response {
    let mut b = Request::builder().method("GET").uri(uri);
    if let Some(t) = bearer {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    app.clone().oneshot(b.body(Body::empty()).unwrap()).await.unwrap()
}
async fn post_json(app: &Router, uri: &str, bearer: Option<&str>, json: serde_json::Value) -> axum::response::Response {
    let mut b = Request::builder().method("POST").uri(uri).header("content-type", "application/json");
    if let Some(t) = bearer {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    app.clone().oneshot(b.body(Body::from(json.to_string())).unwrap()).await.unwrap()
}

/// POST a raw body with a Stripe-Signature header.
async fn post_webhook(app: &Router, raw: &str, sig: &str) -> axum::response::Response {
    let req = Request::builder()
        .method("POST")
        .uri("/webhooks/stripe")
        .header("content-type", "application/json")
        .header("stripe-signature", sig)
        .body(Body::from(raw.to_string()))
        .unwrap();
    app.clone().oneshot(req).await.unwrap()
}

fn stripe_sig(payload: &str) -> String {
    let ts = "1700000000";
    let mut mac = Hmac::<Sha256>::new_from_slice(WEBHOOK_SECRET.as_bytes()).unwrap();
    mac.update(format!("{ts}.{payload}").as_bytes());
    let hexsig: String = mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect();
    format!("t={ts},v1={hexsig}")
}

async fn sign_in(app: &Router) -> String {
    let uri = format!(
        "/auth/start?provider=dev&code_challenge={}&redirect_uri=http://127.0.0.1:9999/cb&device_id=dev-A&device_name=T&platform=test",
        challenge()
    );
    let loc = get(app, &uri, None).await.headers().get("location").unwrap().to_str().unwrap().to_string();
    let code = loc.split("code=").nth(1).unwrap().to_string();
    body_json(post_json(app, "/auth/token", None, serde_json::json!({ "code": code, "code_verifier": VERIFIER })).await)
        .await["access_token"].as_str().unwrap().to_string()
}

fn event(id: &str, kind: &str, user_id: &str, sku: &str) -> String {
    serde_json::json!({
        "id": id, "type": kind,
        "data": { "object": { "metadata": { "vance_user_id": user_id, "vance_sku": sku } } }
    })
    .to_string()
}

async fn owned_skus(app: &Router, access: &str) -> Vec<String> {
    body_json(get(app, "/v1/entitlements", Some(access)).await).await["entitlements"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["sku"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn webhook_grants_on_payment_and_revokes_on_refund() {
    let (app, _dir) = mk_app().await;
    let access = sign_in(&app).await;
    let user_id = body_json(get(&app, "/v1/me", Some(&access)).await).await["id"].as_str().unwrap().to_string();
    assert!(owned_skus(&app, &access).await.is_empty());

    // Payment succeeds → auto-grant.
    let paid = event("evt_paid_1", "payment_intent.succeeded", &user_id, "com.paid.fx");
    assert_eq!(post_webhook(&app, &paid, &stripe_sig(&paid)).await.status(), StatusCode::OK);
    assert_eq!(owned_skus(&app, &access).await, vec!["com.paid.fx".to_string()]);

    // Re-delivery of the SAME event is idempotent (no double-processing).
    let resp = post_webhook(&app, &paid, &stripe_sig(&paid)).await;
    assert_eq!(body_json(resp).await["duplicate"], true);
    assert_eq!(owned_skus(&app, &access).await.len(), 1);

    // Refund → revoke.
    let refunded = event("evt_refund_1", "charge.refunded", &user_id, "com.paid.fx");
    assert_eq!(post_webhook(&app, &refunded, &stripe_sig(&refunded)).await.status(), StatusCode::OK);
    assert!(owned_skus(&app, &access).await.is_empty());
}

#[tokio::test]
async fn webhook_rejects_bad_signature() {
    let (app, _dir) = mk_app().await;
    let paid = event("evt_x", "payment_intent.succeeded", "u1", "com.paid.fx");
    // Correct format, wrong secret → rejected.
    assert_eq!(
        post_webhook(&app, &paid, "t=1700000000,v1=deadbeef").await.status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn checkout_is_gated_when_stripe_unconfigured() {
    let (app, _dir) = mk_app().await; // stripe_secret is None here
    let access = sign_in(&app).await;
    let resp = post_json(&app, "/v1/checkout", Some(&access), serde_json::json!({ "sku": "com.paid.fx" })).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
