//! M5 — seller trust + anti-scam: paid listings require a verified seller,
//! new-seller price caps, and report → auto-freeze (which hides the listings).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use vance_accounts::{build_router, config::Config, init_state};

const VERIFIER: &str = "test-verifier-0123456789-abcdefghijklmnopqrstuvwxyz-XYZ";

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
        stripe_webhook_secret: None,
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
async fn post(app: &Router, uri: &str, bearer: Option<&str>, json: serde_json::Value) -> axum::response::Response {
    let mut b = Request::builder().method("POST").uri(uri).header("content-type", "application/json");
    if let Some(t) = bearer {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    app.clone().oneshot(b.body(Body::from(json.to_string())).unwrap()).await.unwrap()
}
/// Sign in a distinct dev user (via login_hint) so reports come from different people.
async fn sign_in_as(app: &Router, device_id: &str, hint: &str) -> String {
    let uri = format!(
        "/auth/start?provider=dev&login_hint={hint}&code_challenge={}&redirect_uri=http://127.0.0.1:9999/cb&device_id={device_id}&device_name=T&platform=test",
        challenge()
    );
    let loc = get(app, &uri, None).await.headers().get("location").unwrap().to_str().unwrap().to_string();
    let code = loc.split("code=").nth(1).unwrap().to_string();
    body_json(post(app, "/auth/token", None, serde_json::json!({ "code": code, "code_verifier": VERIFIER })).await)
        .await["access_token"].as_str().unwrap().to_string()
}

fn publish_body(sku: &str, path: &str, price: i64) -> serde_json::Value {
    serde_json::json!({
        "sku": sku, "file_path": path, "dest": "fx.wasm", "publisher": "Studio",
        "display_name": "Effect", "price_cents": price, "categories": ["Stylize"]
    })
}

#[tokio::test]
async fn paid_listing_requires_verified_and_caps_new_sellers() {
    let (app, dir) = mk_app().await;
    let wasm = dir.path().join("fx.wasm");
    std::fs::write(&wasm, b"\x00asm\x01\x00\x00\x00fx").unwrap();
    let path = wasm.to_str().unwrap();
    let seller = sign_in_as(&app, "seller", "studio").await;

    // Unverified → can't sell paid.
    assert_eq!(
        post(&app, "/v1/dev/catalog-publish", Some(&seller), publish_body("com.s.a", path, 1000)).await.status(),
        StatusCode::FORBIDDEN
    );

    // Verify (dev stand-in for Stripe KYC).
    post(&app, "/v1/dev/seller", Some(&seller), serde_json::json!({ "verified": true })).await;

    // Under the new-seller cap ($20) → ok; over it → rejected until trusted.
    assert_eq!(
        post(&app, "/v1/dev/catalog-publish", Some(&seller), publish_body("com.s.a", path, 1000)).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        post(&app, "/v1/dev/catalog-publish", Some(&seller), publish_body("com.s.big", path, 5000)).await.status(),
        StatusCode::BAD_REQUEST
    );
    post(&app, "/v1/dev/seller", Some(&seller), serde_json::json!({ "trusted": true })).await;
    assert_eq!(
        post(&app, "/v1/dev/catalog-publish", Some(&seller), publish_body("com.s.big", path, 5000)).await.status(),
        StatusCode::OK
    );

    // The paid effect is browsable, badged as a verified (now trusted) seller.
    let list = body_json(get(&app, "/v1/catalog", None).await).await;
    let card = list["effects"].as_array().unwrap().iter().find(|e| e["sku"] == "com.s.a").unwrap();
    assert_eq!(card["price_cents"], 1000);
    assert_eq!(card["verified_seller"], true);
    assert_eq!(card["new_seller"], false);
}

#[tokio::test]
async fn reports_auto_freeze_and_hide_the_seller() {
    let (app, dir) = mk_app().await;
    let wasm = dir.path().join("fx.wasm");
    std::fs::write(&wasm, b"\x00asm\x01\x00\x00\x00fx").unwrap();
    let seller = sign_in_as(&app, "seller", "studio").await;

    // A free listing (no verification needed).
    post(&app, "/v1/dev/catalog-publish", Some(&seller), publish_body("com.s.free", wasm.to_str().unwrap(), 0)).await;
    assert_eq!(body_json(get(&app, "/v1/catalog", None).await).await["effects"].as_array().unwrap().len(), 1);

    // Three distinct users report it → auto-freeze (0 downloads → rate = 1.0).
    for who in ["alice", "bob", "carol"] {
        let u = sign_in_as(&app, who, who).await;
        assert_eq!(
            post(&app, "/v1/catalog/com.s.free/report", Some(&u), serde_json::json!({ "reason": "broken" })).await.status(),
            StatusCode::OK
        );
    }

    // The seller is frozen: their listings vanish from browse.
    assert_eq!(body_json(get(&app, "/v1/catalog", None).await).await["effects"].as_array().unwrap().len(), 0);
    // And the seller sees they're frozen.
    assert_eq!(body_json(get(&app, "/v1/seller", Some(&seller)).await).await["frozen"], true);
}
