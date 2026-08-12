//! Entitlements + install pipeline (spec §6). Owned-sku listing, the signed
//! entitlement token (verified offline on the client), artifact serve behind a
//! short-lived signed URL with an entitlement re-check + the 2-device cap
//! (auto-drop-oldest), and dev-only publish/revoke so the whole flow is testable
//! before payments (M4).

use crate::auth::AuthUser;
use crate::crypto::{self, DownloadClaims, EntitlementClaims, ENTITLEMENT_GRACE_SECS};
use crate::error::{AppError, AppResult};
use crate::state::{AppState, ServerEvent};
use crate::util::{new_id, now};
use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

const DEVICE_CAP: usize = 2;
const DOWNLOAD_TTL_SECS: i64 = 300;

// ---------------------------------------------------------------------------
// GET /v1/entitlements
// ---------------------------------------------------------------------------

pub async fn list(State(st): State<AppState>, user: AuthUser) -> AppResult<Json<Value>> {
    let rows = sqlx::query(
        "SELECT e.sku, e.source, e.state, e.granted_at, e.expires_at, a.display_name, a.dest
         FROM entitlements e
         LEFT JOIN plugin_artifacts a ON a.sku = e.sku
         WHERE e.user_id=? AND e.state='active'
         ORDER BY e.granted_at DESC",
    )
    .bind(&user.claims.sub)
    .fetch_all(&st.pool)
    .await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "sku": r.get::<String,_>("sku"),
                "source": r.get::<String,_>("source"),
                "state": r.get::<String,_>("state"),
                "granted_at": r.get::<i64,_>("granted_at"),
                "expires_at": r.get::<Option<i64>,_>("expires_at"),
                "display_name": r.get::<Option<String>,_>("display_name"),
                "dest": r.get::<Option<String>,_>("dest"),
            })
        })
        .collect();
    Ok(Json(json!({ "entitlements": items })))
}

// ---------------------------------------------------------------------------
// GET /v1/entitlement-token — signed, offline-verifiable list of owned skus.
// ---------------------------------------------------------------------------

pub async fn token(State(st): State<AppState>, user: AuthUser) -> AppResult<Json<Value>> {
    let skus = active_skus(&st, &user.claims.sub).await?;
    let ent_ver = user_ent_ver(&st, &user.claims.sub).await?;
    let claims = EntitlementClaims {
        sub: user.claims.sub.clone(),
        did: user.claims.did.clone(),
        skus,
        iat: now(),
        exp: now() + ENTITLEMENT_GRACE_SECS,
        ent_ver,
    };
    let token = crypto::sign_entitlement(&st.signing, &claims);
    Ok(Json(json!({ "token": token, "ent_ver": ent_ver })))
}

// ---------------------------------------------------------------------------
// POST /v1/artifacts/:sku — entitlement + device-cap check, then a signed URL.
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
pub struct ArtifactParams {
    #[allow(dead_code)]
    platform: Option<String>,
    /// The device's X25519 public key (base64url) — required to seal an
    /// encrypted (paid) effect's content key to this machine (spec §7).
    device_pubkey: Option<String>,
}

pub async fn artifact(
    State(st): State<AppState>,
    user: AuthUser,
    Path(sku): Path<String>,
    body: Option<Json<ArtifactParams>>,
) -> AppResult<Json<Value>> {
    let device_pubkey = body.and_then(|Json(b)| b.device_pubkey);
    let row = sqlx::query(
        "SELECT version, platform, sha256, dest, requires_entitlement, encrypted, cek, plaintext_sha256
         FROM plugin_artifacts WHERE sku=?",
    )
    .bind(&sku)
    .fetch_optional(&st.pool)
    .await?
    .ok_or_else(|| AppError::NotFound("no artifact for that effect".into()))?;

    let requires: bool = row.get::<i64, _>("requires_entitlement") != 0;
    let encrypted: bool = row.get::<i64, _>("encrypted") != 0;
    let version: String = row.get("version");
    let platform: String = row.get("platform");
    let sha256: String = row.get("sha256");
    let dest: String = row.get("dest");

    if requires {
        if !is_entitled(&st, &user.claims.sub, &sku).await? {
            return Err(AppError::Forbidden("you don't own this effect".into()));
        }
        // Records this device + enforces the 2-device cap (auto-drop-oldest). For
        // encrypted effects this is the HARD boundary: a dropped device stops
        // getting a fresh sealed CEK, so its lease lapses after the grace window.
        record_install_and_enforce_cap(&st, &user, &sku, &version).await?;
    }

    let dl = DownloadClaims {
        sku: sku.clone(),
        platform,
        exp: now() + DOWNLOAD_TTL_SECS,
    };
    let token = crypto::sign_download(&st.signing, &dl);
    let mut resp = json!({
        "download_url": format!("{}/download/{}", st.config.public_url, token),
        "sha256": sha256,
        "dest": dest,
        "version": version,
        "encrypted": encrypted,
    });

    if encrypted {
        let device_pubkey = device_pubkey
            .ok_or_else(|| AppError::BadRequest("device_pubkey required for a paid effect".into()))?;
        let cek_b64 = row
            .get::<Option<String>, _>("cek")
            .ok_or_else(|| AppError::Internal("artifact missing cek".into()))?;
        let cek = crypto::unb64_32(&cek_b64).ok_or_else(|| AppError::Internal("bad cek".into()))?;
        let plaintext_sha256 = row
            .get::<Option<String>, _>("plaintext_sha256")
            .unwrap_or_default();
        let sealed = crypto::seal_cek(&device_pubkey, &cek)
            .ok_or_else(|| AppError::BadRequest("invalid device_pubkey".into()))?;
        let lease = crypto::sign_lease(
            &st.signing,
            &crypto::LeaseClaims {
                sku: sku.clone(),
                device_id: user.claims.did.clone(),
                exp: now() + ENTITLEMENT_GRACE_SECS,
                plaintext_sha256,
                buyer_id: user.claims.sub.clone(),
            },
        );
        let obj = resp.as_object_mut().unwrap();
        obj.insert("sealed_cek".into(), serde_json::to_value(sealed).unwrap());
        obj.insert("lease".into(), json!(lease));
    }
    Ok(Json(resp))
}

// ---------------------------------------------------------------------------
// GET /download/:token — the signed URL. No bearer; the token is the capability.
// ---------------------------------------------------------------------------

pub async fn download(State(st): State<AppState>, Path(token): Path<String>) -> AppResult<Response> {
    let claims = crypto::verify_download(&st.verifying, &token)
        .ok_or_else(|| AppError::Forbidden("invalid download token".into()))?;
    if claims.exp < now() {
        return Err(AppError::Forbidden("download link expired".into()));
    }
    let object_key: String = sqlx::query("SELECT object_key FROM plugin_artifacts WHERE sku=?")
        .bind(&claims.sku)
        .fetch_optional(&st.pool)
        .await?
        .map(|r| r.get::<String, _>("object_key"))
        .ok_or_else(|| AppError::NotFound("artifact".into()))?;
    let bytes = tokio::fs::read(&object_key)
        .await
        .map_err(|e| AppError::Internal(format!("read artifact: {e}")))?;
    Ok((
        [(header::CONTENT_TYPE, "application/wasm")],
        bytes,
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// Dev-only: publish an artifact + grant the caller entitlement; and revoke.
// These let the full M2 pipeline be tested before payments (M4).
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct PublishParams {
    sku: String,
    file_path: String,
    dest: String,
    display_name: Option<String>,
}

pub async fn dev_publish(
    State(st): State<AppState>,
    user: AuthUser,
    Json(p): Json<PublishParams>,
) -> AppResult<Json<Value>> {
    dev_only(&st)?;
    let plaintext = tokio::fs::read(&p.file_path)
        .await
        .map_err(|e| AppError::BadRequest(format!("can't read {}: {e}", p.file_path)))?;

    // Paid effects are encrypted at rest (spec §7): store the CIPHERTEXT; the pin
    // (sha256) is over the ciphertext; the CEK + plaintext hash are held to seal
    // per-device at activation.
    let plaintext_sha = crypto::sha256_b64(&plaintext);
    let (cek, ciphertext) = crypto::encrypt_effect(&plaintext);
    let ct_path = format!("{}.enc", p.file_path);
    tokio::fs::write(&ct_path, &ciphertext)
        .await
        .map_err(|e| AppError::Internal(format!("write ciphertext: {e}")))?;
    let pin = crypto::sha256_b64(&ciphertext);

    sqlx::query(
        "INSERT INTO plugin_artifacts
            (sku, version, platform, object_key, sha256, dest, requires_entitlement, display_name, encrypted, cek, plaintext_sha256)
         VALUES (?, '1', 'any', ?, ?, ?, 1, ?, 1, ?, ?)
         ON CONFLICT(sku) DO UPDATE SET
            object_key=excluded.object_key, sha256=excluded.sha256, dest=excluded.dest,
            requires_entitlement=1, display_name=excluded.display_name,
            encrypted=1, cek=excluded.cek, plaintext_sha256=excluded.plaintext_sha256",
    )
    .bind(&p.sku)
    .bind(&ct_path)
    .bind(&pin)
    .bind(&p.dest)
    .bind(&p.display_name)
    .bind(crypto::b64(&cek))
    .bind(&plaintext_sha)
    .execute(&st.pool)
    .await?;

    // Grant the caller the entitlement (idempotent; reactivates a prior revoke).
    sqlx::query(
        "INSERT INTO entitlements (id, user_id, sku, source, state, granted_at)
         VALUES (?,?,?,'dev_grant','active',?)
         ON CONFLICT(user_id, sku) DO UPDATE SET state='active', source='dev_grant', granted_at=excluded.granted_at",
    )
    .bind(new_id())
    .bind(&user.claims.sub)
    .bind(&p.sku)
    .bind(now())
    .execute(&st.pool)
    .await?;

    let ent_ver = bump_ent_ver(&st, &user.claims.sub).await?;
    st.emit(ServerEvent {
        user_id: user.claims.sub.clone(),
        session_id: None,
        kind: "entitlement.changed".into(),
        data: json!({ "sku": p.sku, "state": "active", "ent_ver": ent_ver }),
    });
    Ok(Json(json!({ "sku": p.sku, "sha256": pin, "encrypted": true, "ent_ver": ent_ver })))
}

#[derive(Deserialize)]
pub struct RevokeEntParams {
    sku: String,
}

pub async fn dev_revoke(
    State(st): State<AppState>,
    user: AuthUser,
    Json(p): Json<RevokeEntParams>,
) -> AppResult<Json<Value>> {
    dev_only(&st)?;
    sqlx::query("UPDATE entitlements SET state='revoked' WHERE user_id=? AND sku=?")
        .bind(&user.claims.sub)
        .bind(&p.sku)
        .execute(&st.pool)
        .await?;
    sqlx::query("UPDATE device_installs SET uninstalled_at=? WHERE user_id=? AND sku=? AND uninstalled_at IS NULL")
        .bind(now())
        .bind(&user.claims.sub)
        .bind(&p.sku)
        .execute(&st.pool)
        .await?;
    let ent_ver = bump_ent_ver(&st, &user.claims.sub).await?;
    st.emit(ServerEvent {
        user_id: user.claims.sub.clone(),
        session_id: None,
        kind: "entitlement.changed".into(),
        data: json!({ "sku": p.sku, "state": "revoked", "ent_ver": ent_ver }),
    });
    Ok(Json(json!({ "sku": p.sku, "state": "revoked", "ent_ver": ent_ver })))
}

/// Dev-only: the active device sessions holding an install of `sku` (for the
/// caller). Lets tests + debugging observe the 2-device cap.
pub async fn dev_installs(
    State(st): State<AppState>,
    user: AuthUser,
    Path(sku): Path<String>,
) -> AppResult<Json<Value>> {
    dev_only(&st)?;
    let rows = sqlx::query(
        "SELECT session_id FROM device_installs
         WHERE user_id=? AND sku=? AND uninstalled_at IS NULL
         ORDER BY installed_at ASC",
    )
    .bind(&user.claims.sub)
    .bind(&sku)
    .fetch_all(&st.pool)
    .await?;
    let active: Vec<String> = rows.iter().map(|r| r.get::<String, _>("session_id")).collect();
    Ok(Json(json!({ "active": active })))
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn dev_only(st: &AppState) -> AppResult<()> {
    if st.config.dev_provider {
        Ok(())
    } else {
        Err(AppError::Forbidden("dev endpoints are disabled".into()))
    }
}

async fn active_skus(st: &AppState, user_id: &str) -> AppResult<Vec<String>> {
    let rows = sqlx::query(
        "SELECT sku FROM entitlements
         WHERE user_id=? AND state='active' AND (expires_at IS NULL OR expires_at > ?)",
    )
    .bind(user_id)
    .bind(now())
    .fetch_all(&st.pool)
    .await?;
    Ok(rows.iter().map(|r| r.get::<String, _>("sku")).collect())
}

async fn is_entitled(st: &AppState, user_id: &str, sku: &str) -> AppResult<bool> {
    let found = sqlx::query(
        "SELECT 1 FROM entitlements
         WHERE user_id=? AND sku=? AND state='active' AND (expires_at IS NULL OR expires_at > ?)",
    )
    .bind(user_id)
    .bind(sku)
    .bind(now())
    .fetch_optional(&st.pool)
    .await?;
    Ok(found.is_some())
}

async fn user_ent_ver(st: &AppState, user_id: &str) -> AppResult<i64> {
    Ok(sqlx::query("SELECT ent_ver FROM users WHERE id=?")
        .bind(user_id)
        .fetch_optional(&st.pool)
        .await?
        .map(|r| r.get::<i64, _>("ent_ver"))
        .unwrap_or(0))
}

async fn bump_ent_ver(st: &AppState, user_id: &str) -> AppResult<i64> {
    sqlx::query("UPDATE users SET ent_ver=ent_ver+1, updated_at=? WHERE id=?")
        .bind(now())
        .bind(user_id)
        .execute(&st.pool)
        .await?;
    user_ent_ver(st, user_id).await
}

/// Record this device's install of `sku`, then enforce the 2-device cap by
/// auto-dropping the oldest OTHER active device and pushing it an uninstall.
async fn record_install_and_enforce_cap(
    st: &AppState,
    user: &AuthUser,
    sku: &str,
    version: &str,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO device_installs (id, user_id, session_id, sku, installed_version, installed_at, last_seen_at)
         VALUES (?,?,?,?,?,?,?)
         ON CONFLICT(session_id, sku) DO UPDATE SET
            installed_version=excluded.installed_version, last_seen_at=excluded.last_seen_at, uninstalled_at=NULL",
    )
    .bind(new_id())
    .bind(&user.claims.sub)
    .bind(&user.claims.sid)
    .bind(sku)
    .bind(version)
    .bind(now())
    .bind(now())
    .execute(&st.pool)
    .await?;

    // Oldest-first list of active installs for this (user, sku).
    let active = sqlx::query(
        "SELECT session_id FROM device_installs
         WHERE user_id=? AND sku=? AND uninstalled_at IS NULL
         ORDER BY installed_at ASC",
    )
    .bind(&user.claims.sub)
    .bind(sku)
    .fetch_all(&st.pool)
    .await?;
    let sessions: Vec<String> = active.iter().map(|r| r.get::<String, _>("session_id")).collect();

    if sessions.len() > DEVICE_CAP {
        // Drop the oldest ones that aren't this device, until within the cap.
        let mut to_drop: Vec<String> = sessions
            .iter()
            .filter(|s| *s != &user.claims.sid)
            .cloned()
            .collect();
        let excess = sessions.len() - DEVICE_CAP;
        to_drop.truncate(excess);
        for sid in to_drop {
            sqlx::query(
                "UPDATE device_installs SET uninstalled_at=? WHERE session_id=? AND sku=? AND uninstalled_at IS NULL",
            )
            .bind(now())
            .bind(&sid)
            .bind(sku)
            .execute(&st.pool)
            .await?;
            st.emit(ServerEvent {
                user_id: user.claims.sub.clone(),
                session_id: Some(sid),
                kind: "install.revoked".into(),
                data: json!({ "sku": sku, "reason": "device_cap" }),
            });
        }
    }
    Ok(())
}
