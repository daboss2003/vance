//! M3 end-to-end: dev-publish a FREE listing → public browse/detail → public
//! free download → owner-gated reviews.

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
    let mut h = Sha256::new();
    h.update(VERIFIER.as_bytes());
    B64.encode(h.finalize())
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
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
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
    let body = body_json(post(app, "/auth/token", None, serde_json::json!({ "code": code, "code_verifier": VERIFIER })).await).await;
    body["access_token"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn browse_install_and_gated_review() {
    let (app, dir) = mk_app().await;
    let wasm = dir.path().join("blur.wasm");
    let bytes = b"\x00asm\x01\x00\x00\x00free-blur".to_vec();
    std::fs::write(&wasm, &bytes).unwrap();

    // Publish a FREE listing (dev).
    let resp = post(
        &app,
        "/v1/dev/catalog-publish",
        None,
        serde_json::json!({
            "sku": "com.free.blur", "file_path": wasm.to_str().unwrap(), "dest": "blur.wasm",
            "publisher": "Vance", "display_name": "Soft Blur", "description": "A gentle blur.",
            "categories": ["Blur/Sharpen"], "tags": ["blur", "soft"], "capabilities": []
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Public browse finds it.
    let list = body_json(get(&app, "/v1/catalog?q=blur", None).await).await;
    let effects = list["effects"].as_array().unwrap();
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0]["sku"], "com.free.blur");
    assert_eq!(effects[0]["price_cents"], 0);

    // Public detail marks it installable.
    let detail = body_json(get(&app, "/v1/catalog/com.free.blur", None).await).await;
    assert_eq!(detail["installable"], true);
    assert_eq!(detail["categories"][0], "Blur/Sharpen");

    // Public free download (no auth) returns the exact bytes + bumps downloads.
    let art = body_json(post(&app, "/v1/catalog/com.free.blur/artifact", None, serde_json::json!({})).await).await;
    let path = format!("/download/{}", art["download_url"].as_str().unwrap().split("/download/").nth(1).unwrap());
    assert_eq!(body_bytes(get(&app, &path, None).await).await, bytes);
    let after = body_json(get(&app, "/v1/catalog/com.free.blur", None).await).await;
    assert_eq!(after["downloads"], 1);

    // Reviews are gated: a signed-in user who hasn't installed can't review.
    let access = sign_in(&app, "device-A").await;
    let denied = post(&app, "/v1/catalog/com.free.blur/reviews", Some(&access), serde_json::json!({ "stars": 5 })).await;
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);

    // Install as that user (bearer recorded) → review now allowed.
    post(&app, "/v1/catalog/com.free.blur/artifact", Some(&access), serde_json::json!({})).await;
    let ok = post(&app, "/v1/catalog/com.free.blur/reviews", Some(&access), serde_json::json!({ "stars": 5, "body": "love it" })).await;
    assert_eq!(ok.status(), StatusCode::OK);

    // The review shows in the aggregate + list.
    let revs = body_json(get(&app, "/v1/catalog/com.free.blur/reviews", None).await).await;
    assert_eq!(revs["count"], 1);
    assert_eq!(revs["rating"], 5.0);
    let list = body_json(get(&app, "/v1/catalog", None).await).await;
    assert_eq!(list["effects"][0]["ratings_count"], 1);
    assert_eq!(list["effects"][0]["rating"], 5.0);
}
