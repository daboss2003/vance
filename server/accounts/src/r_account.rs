//! Account routes: identity summary, set/change password (spec §4.2), and the
//! step-up "sudo" challenge that gates sensitive actions (spec §4.4).

use crate::auth::AuthUser;
use crate::crypto;
use crate::error::{AppError, AppResult};
use crate::state::AppState;
use crate::util::{new_id, now, random_token};
use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;

const STEPUP_TTL_SECS: i64 = 300;
const MIN_PASSWORD_LEN: usize = 8;

pub async fn me(
    State(st): State<AppState>,
    user: AuthUser,
) -> AppResult<Json<serde_json::Value>> {
    let row = sqlx::query("SELECT email, email_verified, password_hash FROM users WHERE id=?")
        .bind(&user.claims.sub)
        .fetch_optional(&st.pool)
        .await?
        .ok_or_else(|| AppError::NotFound("user".into()))?;
    Ok(Json(json!({
        "id": user.claims.sub,
        "email": row.get::<Option<String>, _>("email"),
        "email_verified": row.get::<i64, _>("email_verified") != 0,
        "password_set": row.get::<Option<String>, _>("password_hash").is_some(),
    })))
}

#[derive(Deserialize)]
pub struct SetPasswordParams {
    password: String,
    /// Required only when CHANGING an existing password (step-up, spec §4.4).
    stepup_token: Option<String>,
}

pub async fn set_password(
    State(st): State<AppState>,
    user: AuthUser,
    Json(p): Json<SetPasswordParams>,
) -> AppResult<Json<serde_json::Value>> {
    if p.password.chars().count() < MIN_PASSWORD_LEN {
        return Err(AppError::BadRequest(format!(
            "password must be at least {MIN_PASSWORD_LEN} characters"
        )));
    }
    let existing: Option<String> = sqlx::query("SELECT password_hash FROM users WHERE id=?")
        .bind(&user.claims.sub)
        .fetch_optional(&st.pool)
        .await?
        .and_then(|r| r.get::<Option<String>, _>("password_hash"));

    // Changing an existing password requires a fresh step-up; the first set
    // only needs a signed-in session.
    if existing.is_some() {
        let token = p
            .stepup_token
            .as_deref()
            .ok_or_else(|| AppError::Forbidden("step-up required to change password".into()))?;
        consume_stepup(&st, &user.claims.sub, &user.claims.sid, "change_password", None, token)
            .await?;
    }

    let hash = crypto::hash_password(&p.password)
        .map_err(|e| AppError::Internal(format!("hash: {e}")))?;
    sqlx::query("UPDATE users SET password_hash=?, password_set_at=?, updated_at=? WHERE id=?")
        .bind(&hash)
        .bind(now())
        .bind(now())
        .bind(&user.claims.sub)
        .execute(&st.pool)
        .await?;
    Ok(Json(json!({ "password_set": true })))
}

#[derive(Deserialize)]
pub struct StepUpParams {
    action: String,
    target_ref: Option<String>,
    password: String,
}

pub async fn stepup(
    State(st): State<AppState>,
    user: AuthUser,
    Json(p): Json<StepUpParams>,
) -> AppResult<Json<serde_json::Value>> {
    if !matches!(
        p.action.as_str(),
        "remove_device" | "change_password" | "purchase"
    ) {
        return Err(AppError::BadRequest("unknown step-up action".into()));
    }
    let hash: Option<String> = sqlx::query("SELECT password_hash FROM users WHERE id=?")
        .bind(&user.claims.sub)
        .fetch_optional(&st.pool)
        .await?
        .and_then(|r| r.get::<Option<String>, _>("password_hash"));
    let hash = hash.ok_or_else(|| {
        AppError::Forbidden("set an account password before this action".into())
    })?;
    if !crypto::verify_password(&p.password, &hash) {
        return Err(AppError::Unauthorized("incorrect password".into()));
    }

    let token = random_token(24);
    sqlx::query(
        "INSERT INTO step_up_challenges
            (id, user_id, session_id, action, target_ref, token_hash, created_at, expires_at)
         VALUES (?,?,?,?,?,?,?,?)",
    )
    .bind(new_id())
    .bind(&user.claims.sub)
    .bind(&user.claims.sid)
    .bind(&p.action)
    .bind(&p.target_ref)
    .bind(crypto::hash_token(&token))
    .bind(now())
    .bind(now() + STEPUP_TTL_SECS)
    .execute(&st.pool)
    .await?;
    Ok(Json(json!({ "stepup_token": token, "expires_in": STEPUP_TTL_SECS })))
}

/// Validate + single-use-consume a step-up token scoped to (user, session,
/// action, target). Used by password-change and device-revoke.
pub async fn consume_stepup(
    st: &AppState,
    user_id: &str,
    session_id: &str,
    action: &str,
    target_ref: Option<&str>,
    token: &str,
) -> AppResult<()> {
    let hash = crypto::hash_token(token);
    let row = sqlx::query(
        "SELECT id, target_ref, expires_at, consumed_at
         FROM step_up_challenges
         WHERE token_hash=? AND user_id=? AND session_id=? AND action=?",
    )
    .bind(&hash)
    .bind(user_id)
    .bind(session_id)
    .bind(action)
    .fetch_optional(&st.pool)
    .await?
    .ok_or_else(|| AppError::Forbidden("invalid step-up token".into()))?;

    if row.get::<Option<i64>, _>("consumed_at").is_some() {
        return Err(AppError::Forbidden("step-up token already used".into()));
    }
    if row.get::<i64, _>("expires_at") < now() {
        return Err(AppError::Forbidden("step-up token expired".into()));
    }
    let stored_target: Option<String> = row.get("target_ref");
    if stored_target.as_deref() != target_ref {
        return Err(AppError::Forbidden("step-up token target mismatch".into()));
    }
    sqlx::query("UPDATE step_up_challenges SET consumed_at=? WHERE id=?")
        .bind(now())
        .bind(row.get::<String, _>("id"))
        .execute(&st.pool)
        .await?;
    Ok(())
}
