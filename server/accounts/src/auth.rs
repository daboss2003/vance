//! `AuthUser` extractor: verifies the Bearer access token offline (Ed25519),
//! checks it hasn't expired, and confirms the session still exists and isn't
//! revoked — tier-3 of the revocation design (spec §5). Any protected handler
//! just takes `user: AuthUser`.

use crate::crypto::{self, Claims};
use crate::error::AppError;
use crate::state::AppState;
use crate::util::now;
use axum::async_trait;
use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use sqlx::Row;

pub struct AuthUser {
    pub claims: Claims,
}

#[async_trait]
impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| AppError::Unauthorized("missing Authorization header".into()))?;
        let token = header
            .strip_prefix("Bearer ")
            .ok_or_else(|| AppError::Unauthorized("expected Bearer token".into()))?;

        let claims = crypto::verify_access(&state.verifying, token)
            .ok_or_else(|| AppError::Unauthorized("invalid token".into()))?;
        if claims.exp < now() {
            return Err(AppError::Unauthorized("token expired".into()));
        }

        // The session must still be live. This is the real boundary a revoked
        // (but not-yet-expired) access token hits.
        let row = sqlx::query("SELECT revoked_at FROM sessions WHERE id = ?")
            .bind(&claims.sid)
            .fetch_optional(&state.pool)
            .await?;
        match row {
            None => return Err(AppError::Unauthorized("no such session".into())),
            Some(r) => {
                let revoked: Option<i64> = r.try_get("revoked_at").unwrap_or(None);
                if revoked.is_some() {
                    return Err(AppError::Unauthorized("session revoked".into()));
                }
            }
        }
        Ok(AuthUser { claims })
    }
}
