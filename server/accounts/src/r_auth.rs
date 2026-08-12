//! Auth routes: the loopback-PKCE handshake to our backend-as-OAuth-client
//! (spec §4.1) and token issue/rotation.
//!
//! Flow: client → GET /auth/start (PKCE challenge + device info) → provider →
//! GET /auth/callback → 302 back to the client's 127.0.0.1 loopback with a
//! one-time login code → POST /auth/token (code + verifier) → {access, refresh}.
//! The `dev` provider collapses start+callback locally so the whole thing runs
//! with no external service.

use crate::crypto::{self, Claims, ACCESS_TTL_SECS};
use crate::error::{AppError, AppResult};
use crate::providers::{self, Provider, ProviderIdentity};
use crate::state::AppState;
use crate::util::{new_id, now, random_token};
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Redirect};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;

/// Refresh tokens rotate on every use; the immediately-previous token is still
/// accepted for this many seconds so a crash/race between "server rotated" and
/// "client persisted" doesn't nuke the session (spec §4.3 trap).
const REFRESH_OVERLAP_SECS: i64 = 30;
const PENDING_TTL_SECS: i64 = 600;

// ---------------------------------------------------------------------------

pub async fn pubkey(State(st): State<AppState>) -> impl IntoResponse {
    Json(json!({ "alg": "EdDSA", "public_key": crypto::public_key_b64(&st.verifying) }))
}

pub async fn healthz() -> impl IntoResponse {
    Json(json!({ "ok": true }))
}

// ---------------------------------------------------------------------------
// /auth/start
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct StartParams {
    provider: String,
    code_challenge: String,
    redirect_uri: String,
    device_id: String,
    device_name: String,
    platform: String,
    app_version: Option<String>,
    /// dev provider only: pick a stable dev user (defaults to one shared user).
    login_hint: Option<String>,
}

pub async fn start(
    State(st): State<AppState>,
    Query(p): Query<StartParams>,
) -> AppResult<Redirect> {
    let provider = Provider::parse(&p.provider)
        .ok_or_else(|| AppError::BadRequest(format!("unknown provider '{}'", p.provider)))?;
    if !provider.is_configured(&st.config) {
        return Err(AppError::BadRequest(format!(
            "{} sign-in is not configured on this server",
            provider.as_str()
        )));
    }
    // Only ever redirect back to a loopback address (open-redirect / SSRF guard).
    if !is_loopback_redirect(&p.redirect_uri) {
        return Err(AppError::BadRequest(
            "redirect_uri must be http://127.0.0.1:<port>/… (loopback only)".into(),
        ));
    }

    let state_tok = random_token(24);
    sqlx::query(
        "INSERT INTO pending_auth
            (id, state, code_challenge, redirect_uri, provider, device_id, device_name, platform, app_version, created_at, expires_at)
         VALUES (?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(new_id())
    .bind(&state_tok)
    .bind(&p.code_challenge)
    .bind(&p.redirect_uri)
    .bind(provider.as_str())
    .bind(&p.device_id)
    .bind(&p.device_name)
    .bind(&p.platform)
    .bind(&p.app_version)
    .bind(now())
    .bind(now() + PENDING_TTL_SECS)
    .execute(&st.pool)
    .await?;

    if provider == Provider::Dev {
        // No external round-trip: mint the identity locally and jump straight
        // back to the loopback with a login code.
        let subject = p.login_hint.clone().unwrap_or_else(|| "dev-default".into());
        let email = p
            .login_hint
            .clone()
            .or_else(|| Some("dev@vance.local".into()));
        let identity = ProviderIdentity { subject, email };
        let url = complete_login(&st, &state_tok, provider, identity, true).await?;
        return Ok(Redirect::to(&url));
    }

    let url = providers::authorize_url(provider, &st.config, &state_tok)?;
    Ok(Redirect::to(&url))
}

// ---------------------------------------------------------------------------
// /auth/callback  (real providers redirect here)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CallbackParams {
    provider: String,
    code: Option<String>,
    state: String,
    error: Option<String>,
}

pub async fn callback(
    State(st): State<AppState>,
    Query(p): Query<CallbackParams>,
) -> AppResult<Redirect> {
    if let Some(err) = p.error {
        return Err(AppError::BadRequest(format!("provider returned error: {err}")));
    }
    let provider = Provider::parse(&p.provider)
        .ok_or_else(|| AppError::BadRequest("unknown provider".into()))?;
    let code = p
        .code
        .ok_or_else(|| AppError::BadRequest("callback missing code".into()))?;

    // The pending row proves this callback matches a start we issued.
    let pending = fetch_pending_by_state(&st, &p.state).await?;
    if pending.provider != provider.as_str() {
        return Err(AppError::BadRequest("provider mismatch".into()));
    }

    let identity = providers::exchange(provider, &st.config, &code).await?;
    let email_verified = identity.email.is_some();
    let user_id = upsert_user(&st, provider, &identity, email_verified).await?;
    let session_id = upsert_session(&st, &user_id, &pending).await?;
    let login_code = random_token(24);
    sqlx::query("UPDATE pending_auth SET login_code=?, user_id=?, session_id=? WHERE state=?")
        .bind(&login_code)
        .bind(&user_id)
        .bind(&session_id)
        .bind(&p.state)
        .execute(&st.pool)
        .await?;
    Ok(Redirect::to(&append_code(&pending.redirect_uri, &login_code)))
}

// ---------------------------------------------------------------------------
// /auth/token
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct TokenParams {
    code: String,
    code_verifier: String,
}

pub async fn token(
    State(st): State<AppState>,
    Json(p): Json<TokenParams>,
) -> AppResult<Json<serde_json::Value>> {
    let row = sqlx::query(
        "SELECT code_challenge, user_id, session_id, device_id, expires_at, consumed_at
         FROM pending_auth WHERE login_code = ?",
    )
    .bind(&p.code)
    .fetch_optional(&st.pool)
    .await?
    .ok_or_else(|| AppError::BadRequest("invalid or expired login code".into()))?;

    if row.get::<Option<i64>, _>("consumed_at").is_some() {
        return Err(AppError::BadRequest("login code already used".into()));
    }
    if row.get::<i64, _>("expires_at") < now() {
        return Err(AppError::BadRequest("login code expired".into()));
    }
    let challenge: String = row.get("code_challenge");
    if !crypto::verify_pkce_s256(&p.code_verifier, &challenge) {
        return Err(AppError::BadRequest("PKCE verification failed".into()));
    }
    let user_id: String = row
        .get::<Option<String>, _>("user_id")
        .ok_or_else(|| AppError::BadRequest("login not completed".into()))?;
    let session_id: String = row
        .get::<Option<String>, _>("session_id")
        .ok_or_else(|| AppError::BadRequest("login not completed".into()))?;
    let device_id: String = row.get("device_id");

    sqlx::query("UPDATE pending_auth SET consumed_at=? WHERE login_code=?")
        .bind(now())
        .bind(&p.code)
        .execute(&st.pool)
        .await?;

    // First refresh for this session (rotate from the null current hash).
    let pair = issue_pair(&st, &session_id, &user_id, &device_id, None).await?;
    let (email, password_set) = user_summary(&st, &user_id).await?;
    Ok(Json(json!({
        "access_token": pair.access,
        "refresh_token": pair.refresh,
        "token_type": "Bearer",
        "expires_in": ACCESS_TTL_SECS,
        "user": { "id": user_id, "email": email, "password_set": password_set },
    })))
}

// ---------------------------------------------------------------------------
// /auth/refresh  (rotation + overlap + reuse detection — spec §4.3)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct RefreshParams {
    refresh_token: String,
}

pub async fn refresh(
    State(st): State<AppState>,
    Json(p): Json<RefreshParams>,
) -> AppResult<Json<serde_json::Value>> {
    let h = crypto::hash_token(&p.refresh_token);
    let row = sqlx::query(
        "SELECT id, user_id, device_id, refresh_hash, prev_refresh_hash, prev_rotated_at, revoked_at
         FROM sessions WHERE refresh_hash = ? OR prev_refresh_hash = ? LIMIT 1",
    )
    .bind(&h)
    .bind(&h)
    .fetch_optional(&st.pool)
    .await?
    .ok_or_else(|| AppError::Unauthorized("unknown refresh token".into()))?;

    let session_id: String = row.get("id");
    let user_id: String = row.get("user_id");
    let device_id: String = row.get("device_id");
    let current: Option<String> = row.get("refresh_hash");
    let prev: Option<String> = row.get("prev_refresh_hash");
    let prev_at: Option<i64> = row.get("prev_rotated_at");
    if row.get::<Option<i64>, _>("revoked_at").is_some() {
        return Err(AppError::Unauthorized("session revoked".into()));
    }

    let is_current = current.as_deref() == Some(h.as_str());
    let is_prev = prev.as_deref() == Some(h.as_str());
    if is_prev && !is_current {
        // Presenting the previous token: fine within the overlap window, else
        // it's a replay of a rotated-away token → revoke the whole session.
        let within = prev_at.map_or(false, |t| now() - t <= REFRESH_OVERLAP_SECS);
        if !within {
            sqlx::query("UPDATE sessions SET revoked_at=?, revoked_reason='refresh_reuse' WHERE id=?")
                .bind(now())
                .bind(&session_id)
                .execute(&st.pool)
                .await?;
            st.emit(crate::state::ServerEvent {
                user_id: user_id.clone(),
                session_id: Some(session_id.clone()),
                kind: "session.revoked".into(),
                data: json!({ "reason": "refresh_reuse" }),
            });
            return Err(AppError::Unauthorized("refresh token reuse detected".into()));
        }
    }

    let pair = issue_pair(&st, &session_id, &user_id, &device_id, current).await?;
    Ok(Json(json!({
        "access_token": pair.access,
        "refresh_token": pair.refresh,
        "token_type": "Bearer",
        "expires_in": ACCESS_TTL_SECS,
    })))
}

// ---------------------------------------------------------------------------
// /auth/logout — revoke just this session (the caller's device).
// ---------------------------------------------------------------------------

pub async fn logout(
    State(st): State<AppState>,
    Json(p): Json<RefreshParams>,
) -> AppResult<axum::http::StatusCode> {
    let h = crypto::hash_token(&p.refresh_token);
    sqlx::query(
        "UPDATE sessions SET revoked_at=?, revoked_reason='logout'
         WHERE (refresh_hash=? OR prev_refresh_hash=?) AND revoked_at IS NULL",
    )
    .bind(now())
    .bind(&h)
    .bind(&h)
    .execute(&st.pool)
    .await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

struct Pending {
    redirect_uri: String,
    provider: String,
    device_id: String,
    device_name: String,
    platform: String,
    app_version: Option<String>,
}

async fn fetch_pending_by_state(st: &AppState, state: &str) -> AppResult<Pending> {
    let row = sqlx::query(
        "SELECT redirect_uri, provider, device_id, device_name, platform, app_version, expires_at, consumed_at
         FROM pending_auth WHERE state = ?",
    )
    .bind(state)
    .fetch_optional(&st.pool)
    .await?
    .ok_or_else(|| AppError::BadRequest("unknown auth state".into()))?;
    if row.get::<Option<i64>, _>("consumed_at").is_some() {
        return Err(AppError::BadRequest("auth state already used".into()));
    }
    if row.get::<i64, _>("expires_at") < now() {
        return Err(AppError::BadRequest("auth state expired".into()));
    }
    Ok(Pending {
        redirect_uri: row.get("redirect_uri"),
        provider: row.get("provider"),
        device_id: row.get("device_id"),
        device_name: row.get("device_name"),
        platform: row.get("platform"),
        app_version: row.get("app_version"),
    })
}

/// dev-provider path: upsert user+session and stamp the login code onto the
/// pending row, returning the loopback redirect URL to send the browser to.
async fn complete_login(
    st: &AppState,
    state: &str,
    provider: Provider,
    identity: ProviderIdentity,
    email_verified: bool,
) -> AppResult<String> {
    let pending = fetch_pending_by_state(st, state).await?;
    let user_id = upsert_user(st, provider, &identity, email_verified).await?;
    let session_id = upsert_session(st, &user_id, &pending).await?;
    let login_code = random_token(24);
    sqlx::query("UPDATE pending_auth SET login_code=?, user_id=?, session_id=? WHERE state=?")
        .bind(&login_code)
        .bind(&user_id)
        .bind(&session_id)
        .bind(state)
        .execute(&st.pool)
        .await?;
    Ok(append_code(&pending.redirect_uri, &login_code))
}

async fn upsert_user(
    st: &AppState,
    provider: Provider,
    identity: &ProviderIdentity,
    email_verified: bool,
) -> AppResult<String> {
    if let Some(row) =
        sqlx::query("SELECT user_id FROM oauth_identities WHERE provider=? AND provider_subject=?")
            .bind(provider.as_str())
            .bind(&identity.subject)
            .fetch_optional(&st.pool)
            .await?
    {
        return Ok(row.get::<String, _>("user_id"));
    }
    let uid = new_id();
    sqlx::query(
        "INSERT INTO users (id, email, email_verified, ent_ver, created_at, updated_at)
         VALUES (?,?,?,0,?,?)",
    )
    .bind(&uid)
    .bind(&identity.email)
    .bind(email_verified as i64)
    .bind(now())
    .bind(now())
    .execute(&st.pool)
    .await?;
    sqlx::query(
        "INSERT INTO oauth_identities (id, user_id, provider, provider_subject, email_at_provider, linked_at)
         VALUES (?,?,?,?,?,?)",
    )
    .bind(new_id())
    .bind(&uid)
    .bind(provider.as_str())
    .bind(&identity.subject)
    .bind(&identity.email)
    .bind(now())
    .execute(&st.pool)
    .await?;
    Ok(uid)
}

/// Reuse the (user, device) session row if it exists (a machine is one stable
/// device — spec §5), else create it. Clears any prior revocation on re-auth.
async fn upsert_session(st: &AppState, user_id: &str, pending: &Pending) -> AppResult<String> {
    let row = sqlx::query(
        "INSERT INTO sessions
            (id, user_id, device_id, device_name, platform, app_version, created_at, last_seen_at)
         VALUES (?,?,?,?,?,?,?,?)
         ON CONFLICT(user_id, device_id) DO UPDATE SET
            device_name=excluded.device_name,
            platform=excluded.platform,
            app_version=excluded.app_version,
            last_seen_at=excluded.last_seen_at,
            revoked_at=NULL,
            revoked_reason=NULL
         RETURNING id",
    )
    .bind(new_id())
    .bind(user_id)
    .bind(&pending.device_id)
    .bind(&pending.device_name)
    .bind(&pending.platform)
    .bind(&pending.app_version)
    .bind(now())
    .bind(now())
    .fetch_one(&st.pool)
    .await?;
    Ok(row.get::<String, _>("id"))
}

struct TokenPair {
    access: String,
    refresh: String,
}

/// Mint an access token + rotate in a fresh refresh token. `prev_hash` is the
/// session's current refresh hash to demote to `prev_refresh_hash` (None on the
/// very first issue).
async fn issue_pair(
    st: &AppState,
    session_id: &str,
    user_id: &str,
    device_id: &str,
    prev_hash: Option<String>,
) -> AppResult<TokenPair> {
    let ent_ver: i64 = sqlx::query("SELECT ent_ver FROM users WHERE id=?")
        .bind(user_id)
        .fetch_optional(&st.pool)
        .await?
        .map(|r| r.get::<i64, _>("ent_ver"))
        .unwrap_or(0);

    let claims = Claims {
        sub: user_id.to_string(),
        sid: session_id.to_string(),
        did: device_id.to_string(),
        ent_ver,
        iat: now(),
        exp: now() + ACCESS_TTL_SECS,
    };
    let access = crypto::sign_access(&st.signing, &claims);

    let refresh = random_token(32);
    let new_hash = crypto::hash_token(&refresh);
    sqlx::query(
        "UPDATE sessions SET
            prev_refresh_hash=?, prev_rotated_at=?, refresh_hash=?,
            refresh_generation=refresh_generation+1, last_seen_at=?
         WHERE id=?",
    )
    .bind(&prev_hash)
    .bind(now())
    .bind(&new_hash)
    .bind(now())
    .bind(session_id)
    .execute(&st.pool)
    .await?;

    Ok(TokenPair { access, refresh })
}

async fn user_summary(st: &AppState, user_id: &str) -> AppResult<(Option<String>, bool)> {
    let row = sqlx::query("SELECT email, password_hash FROM users WHERE id=?")
        .bind(user_id)
        .fetch_optional(&st.pool)
        .await?
        .ok_or_else(|| AppError::NotFound("user".into()))?;
    let email: Option<String> = row.get("email");
    let password_set = row.get::<Option<String>, _>("password_hash").is_some();
    Ok((email, password_set))
}

fn is_loopback_redirect(uri: &str) -> bool {
    uri.starts_with("http://127.0.0.1:") || uri.starts_with("http://localhost:")
}

fn append_code(redirect_uri: &str, code: &str) -> String {
    let sep = if redirect_uri.contains('?') { '&' } else { '?' };
    format!("{redirect_uri}{sep}code={code}")
}
