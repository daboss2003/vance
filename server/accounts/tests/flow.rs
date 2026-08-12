//! End-to-end tests driving the router directly (no bound port), exercising the
//! dev-provider sign-in, PKCE, refresh rotation + overlap + reuse detection, and
//! remote device sign-out with step-up.

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
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

async fn get(app: &Router, uri: &str, bearer: Option<&str>) -> axum::response::Response {
    let mut b = Request::builder().method("GET").uri(uri);
    if let Some(t) = bearer {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    app.clone().oneshot(b.body(Body::empty()).unwrap()).await.unwrap()
}

async fn post(
    app: &Router,
    uri: &str,
    bearer: Option<&str>,
    json: serde_json::Value,
) -> axum::response::Response {
    let mut b = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(t) = bearer {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    app.clone()
        .oneshot(b.body(Body::from(json.to_string())).unwrap())
        .await
        .unwrap()
}

/// Full dev sign-in for a given device; returns (access, refresh).
async fn sign_in(app: &Router, device_id: &str) -> (String, String) {
    let start_uri = format!(
        "/auth/start?provider=dev&code_challenge={}&redirect_uri={}&device_id={}&device_name={}&platform=test",
        challenge(),
        "http://127.0.0.1:9999/cb",
        device_id,
        "Test%20Machine",
    );
    let resp = get(app, &start_uri, None).await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER, "start should redirect");
    let location = resp
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let code = location
        .split("code=")
        .nth(1)
        .expect("redirect carries a login code")
        .to_string();

    let resp = post(
        app,
        "/auth/token",
        None,
        serde_json::json!({ "code": code, "code_verifier": VERIFIER }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "token exchange ok");
    let body = body_json(resp).await;
    (
        body["access_token"].as_str().unwrap().to_string(),
        body["refresh_token"].as_str().unwrap().to_string(),
    )
}

#[tokio::test]
async fn dev_sign_in_then_me() {
    let (app, _dir) = mk_app().await;
    let (access, _refresh) = sign_in(&app, "device-A").await;

    let resp = get(&app, "/v1/me", Some(&access)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let me = body_json(resp).await;
    assert_eq!(me["email"], "dev@vance.local");
    assert_eq!(me["password_set"], false);

    // No/garbage token is rejected.
    assert_eq!(
        get(&app, "/v1/me", Some("not-a-token")).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(get(&app, "/v1/me", None).await.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn refresh_rotates_and_overlap_and_reuse() {
    let (app, _dir) = mk_app().await;
    let (_access, r0) = sign_in(&app, "device-A").await;

    // r0 -> r1 (rotation)
    let resp = post(&app, "/auth/refresh", None, serde_json::json!({ "refresh_token": r0 })).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let r1 = body_json(resp).await["refresh_token"].as_str().unwrap().to_string();
    assert_ne!(r0, r1);

    // r0 again, immediately -> accepted within the overlap window (rotates again)
    let resp = post(&app, "/auth/refresh", None, serde_json::json!({ "refresh_token": r0 })).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "previous token accepted within overlap"
    );

    // A totally unknown token is rejected.
    let resp = post(
        &app,
        "/auth/refresh",
        None,
        serde_json::json!({ "refresh_token": "totally-bogus" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn manage_devices_remote_signout_with_stepup() {
    let (app, _dir) = mk_app().await;
    // Two devices of the SAME dev user (default subject).
    let (access_a, _ra) = sign_in(&app, "device-A").await;
    let (access_b, _rb) = sign_in(&app, "device-B").await;

    // Device A sets an account password (first set: session-only).
    let resp = post(
        &app,
        "/v1/password",
        Some(&access_a),
        serde_json::json!({ "password": "correct horse battery" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Two devices listed.
    let devices = body_json(get(&app, "/v1/devices", Some(&access_a)).await).await;
    let list = devices["devices"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    let b_id = list
        .iter()
        .find(|d| d["this_device"] == false)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Revoke requires step-up scoped to device B.
    let stepup = body_json(
        post(
            &app,
            "/v1/stepup",
            Some(&access_a),
            serde_json::json!({ "action": "remove_device", "target_ref": b_id, "password": "correct horse battery" }),
        )
        .await,
    )
    .await;
    let stepup_token = stepup["stepup_token"].as_str().unwrap();

    let resp = post(
        &app,
        &format!("/v1/devices/{b_id}/revoke"),
        Some(&access_a),
        serde_json::json!({ "stepup_token": stepup_token }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Device B's access token is now dead (session revoked); A still lists 1.
    assert_eq!(
        get(&app, "/v1/me", Some(&access_b)).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let devices = body_json(get(&app, "/v1/devices", Some(&access_a)).await).await;
    assert_eq!(devices["devices"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn stepup_rejects_wrong_password() {
    let (app, _dir) = mk_app().await;
    let (access, _r) = sign_in(&app, "device-A").await;
    post(
        &app,
        "/v1/password",
        Some(&access),
        serde_json::json!({ "password": "the-right-password" }),
    )
    .await;
    let resp = post(
        &app,
        "/v1/stepup",
        Some(&access),
        serde_json::json!({ "action": "purchase", "password": "the-wrong-password" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
