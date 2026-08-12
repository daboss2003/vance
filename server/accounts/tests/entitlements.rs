//! M2 end-to-end: dev-publish → entitlement token/list → artifact signed URL →
//! download → 2-device cap (auto-drop-oldest) → revoke removes access.

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
    };
    let state = init_state(cfg).await.unwrap();
    (build_router(state), dir)
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

/// Sign in a device; returns (access, session_id).
async fn sign_in(app: &Router, device_id: &str) -> (String, String) {
    let uri = format!(
        "/auth/start?provider=dev&code_challenge={}&redirect_uri=http://127.0.0.1:9999/cb&device_id={}&device_name=Test&platform=test",
        challenge(),
        device_id
    );
    let resp = get(app, &uri, None).await;
    let loc = resp.headers().get("location").unwrap().to_str().unwrap().to_string();
    let code = loc.split("code=").nth(1).unwrap().to_string();
    let body = body_json(post(app, "/auth/token", None, serde_json::json!({ "code": code, "code_verifier": VERIFIER })).await).await;
    let access = body["access_token"].as_str().unwrap().to_string();
    let sid = sid_of(&access);
    (access, sid)
}

/// Read the `sid` claim straight out of the (unverified) access token payload.
fn sid_of(access: &str) -> String {
    let payload = access.split('.').nth(1).unwrap();
    let json: serde_json::Value = serde_json::from_slice(&B64.decode(payload).unwrap()).unwrap();
    json["sid"].as_str().unwrap().to_string()
}

fn token_skus(entitlement_token: &str) -> Vec<String> {
    let payload = entitlement_token.split('.').nth(1).unwrap();
    let json: serde_json::Value = serde_json::from_slice(&B64.decode(payload).unwrap()).unwrap();
    json["skus"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect()
}

async fn publish(app: &Router, bearer: &str, sku: &str, file: &std::path::Path) -> serde_json::Value {
    body_json(
        post(
            app,
            "/v1/dev/publish",
            Some(bearer),
            serde_json::json!({ "sku": sku, "file_path": file.to_str().unwrap(), "dest": "glow.wasm", "display_name": "Glow" }),
        )
        .await,
    )
    .await
}

#[tokio::test]
async fn full_entitlement_and_install_flow() {
    let (app, dir) = mk_app().await;
    let (access, _sid) = sign_in(&app, "device-A").await;

    // A fake effect artifact on disk.
    let wasm = dir.path().join("glow.wasm");
    let bytes = b"\x00asm\x01\x00\x00\x00-fake-effect-bytes".to_vec();
    std::fs::write(&wasm, &bytes).unwrap();

    let pub_resp = publish(&app, &access, "com.test.glow", &wasm).await;
    let published_sha = pub_resp["sha256"].as_str().unwrap().to_string();

    // Entitlement token + list now include the sku.
    let tok = body_json(get(&app, "/v1/entitlement-token", Some(&access)).await).await;
    assert!(token_skus(tok["token"].as_str().unwrap()).contains(&"com.test.glow".to_string()));
    let ents = body_json(get(&app, "/v1/entitlements", Some(&access)).await).await;
    assert_eq!(ents["entitlements"].as_array().unwrap().len(), 1);

    // Artifact endpoint issues a signed URL with the matching pin.
    let art = body_json(post(&app, "/v1/artifacts/com.test.glow", Some(&access), serde_json::json!({})).await).await;
    assert_eq!(art["sha256"], published_sha);
    assert_eq!(art["dest"], "glow.wasm");
    let url = art["download_url"].as_str().unwrap();
    let path = format!("/download/{}", url.split("/download/").nth(1).unwrap());

    // The signed URL downloads the exact bytes (no bearer needed).
    let dl = get(&app, &path, None).await;
    assert_eq!(dl.status(), StatusCode::OK);
    assert_eq!(body_bytes(dl).await, bytes);

    // Revoke removes it from the token and forbids the artifact.
    let rev = post(&app, "/v1/dev/revoke", Some(&access), serde_json::json!({ "sku": "com.test.glow" })).await;
    assert_eq!(rev.status(), StatusCode::OK);
    let tok = body_json(get(&app, "/v1/entitlement-token", Some(&access)).await).await;
    assert!(token_skus(tok["token"].as_str().unwrap()).is_empty());
    let art = post(&app, "/v1/artifacts/com.test.glow", Some(&access), serde_json::json!({})).await;
    assert_eq!(art.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn device_cap_auto_drops_oldest() {
    let (app, dir) = mk_app().await;
    let (access_a, sid_a) = sign_in(&app, "device-A").await;
    let (access_b, _sid_b) = sign_in(&app, "device-B").await;
    let (access_c, sid_c) = sign_in(&app, "device-C").await;

    let wasm = dir.path().join("fx.wasm");
    std::fs::write(&wasm, b"\x00asm\x01\x00\x00\x00fx").unwrap();
    publish(&app, &access_a, "com.test.fx", &wasm).await; // entitles the (shared) user

    // Install on A, then B → both active (cap = 2).
    post(&app, "/v1/artifacts/com.test.fx", Some(&access_a), serde_json::json!({})).await;
    post(&app, "/v1/artifacts/com.test.fx", Some(&access_b), serde_json::json!({})).await;
    let active = body_json(get(&app, "/v1/dev/installs/com.test.fx", Some(&access_a)).await).await;
    assert_eq!(active["active"].as_array().unwrap().len(), 2);

    // Install on C → cap exceeded, oldest (A) auto-dropped.
    post(&app, "/v1/artifacts/com.test.fx", Some(&access_c), serde_json::json!({})).await;
    let active = body_json(get(&app, "/v1/dev/installs/com.test.fx", Some(&access_c)).await).await;
    let ids: Vec<String> = active["active"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
    assert_eq!(ids.len(), 2, "cap holds at 2");
    assert!(!ids.contains(&sid_a), "oldest device A was dropped");
    assert!(ids.contains(&sid_c), "newest device C is active");
}
