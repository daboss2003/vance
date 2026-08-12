//! Device (session) management — the "Manage Devices" screen (spec §5). List
//! where you're signed in; sign a machine out remotely (step-up gated), which
//! pushes `session.revoked` so that device self-uninstalls its paid effects.

use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::r_account::consume_stepup;
use crate::state::{AppState, ServerEvent};
use crate::util::now;
use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;

pub async fn list(
    State(st): State<AppState>,
    user: AuthUser,
) -> AppResult<Json<serde_json::Value>> {
    let rows = sqlx::query(
        "SELECT id, device_name, platform, app_version, created_at, last_seen_at
         FROM sessions
         WHERE user_id=? AND revoked_at IS NULL
         ORDER BY last_seen_at DESC",
    )
    .bind(&user.claims.sub)
    .fetch_all(&st.pool)
    .await?;

    let devices: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let id: String = r.get("id");
            json!({
                "id": id,
                "device_name": r.get::<String, _>("device_name"),
                "platform": r.get::<String, _>("platform"),
                "app_version": r.get::<Option<String>, _>("app_version"),
                "created_at": r.get::<i64, _>("created_at"),
                "last_seen_at": r.get::<i64, _>("last_seen_at"),
                "this_device": id == user.claims.sid,
            })
        })
        .collect();
    Ok(Json(json!({ "devices": devices })))
}

#[derive(Deserialize)]
pub struct RevokeParams {
    stepup_token: String,
}

pub async fn revoke(
    State(st): State<AppState>,
    user: AuthUser,
    Path(target): Path<String>,
    Json(p): Json<RevokeParams>,
) -> AppResult<axum::http::StatusCode> {
    // Step-up scoped to exactly this device id (spec §4.4).
    consume_stepup(
        &st,
        &user.claims.sub,
        &user.claims.sid,
        "remove_device",
        Some(&target),
        &p.stepup_token,
    )
    .await?;

    // Must belong to the caller.
    let owns = sqlx::query("SELECT 1 FROM sessions WHERE id=? AND user_id=?")
        .bind(&target)
        .bind(&user.claims.sub)
        .fetch_optional(&st.pool)
        .await?
        .is_some();
    if !owns {
        return Err(AppError::NotFound("device".into()));
    }

    sqlx::query(
        "UPDATE sessions SET revoked_at=?, revoked_reason='remote_signout'
         WHERE id=? AND revoked_at IS NULL",
    )
    .bind(now())
    .bind(&target)
    .execute(&st.pool)
    .await?;

    // Push so the removed device learns immediately (tier 1, spec §5).
    st.emit(ServerEvent {
        user_id: user.claims.sub.clone(),
        session_id: Some(target),
        kind: "session.revoked".into(),
        data: json!({ "reason": "remote_signout" }),
    });
    Ok(axum::http::StatusCode::NO_CONTENT)
}
