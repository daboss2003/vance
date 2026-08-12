//! The revocation-delivery tiers that aren't the 401 backstop (spec §5):
//! `POST /v1/heartbeat` (tier 2 — poll) and `GET /v1/events` (tier 1 — SSE push).

use crate::auth::AuthUser;
use crate::error::AppResult;
use crate::state::AppState;
use crate::util::now;
use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;
use std::convert::Infallible;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};

/// Cheap poll: refresh `last_seen_at`, hand back the current entitlement version
/// so the client can reconcile installs, and confirm the session is live (a
/// revoked one is already 401'd by the extractor before reaching here).
pub async fn heartbeat(
    State(st): State<AppState>,
    user: AuthUser,
) -> AppResult<Json<serde_json::Value>> {
    sqlx::query("UPDATE sessions SET last_seen_at=? WHERE id=?")
        .bind(now())
        .bind(&user.claims.sid)
        .execute(&st.pool)
        .await?;
    Ok(Json(json!({
        "revoked": false,
        "ent_ver": user.claims.ent_ver,
        "server_time": now(),
    })))
}

/// Long-lived SSE stream. Delivers events for this user, and events targeted at
/// a specific session only to that session (e.g. remote sign-out).
pub async fn events(State(st): State<AppState>, user: AuthUser) -> impl IntoResponse {
    let uid = user.claims.sub.clone();
    let sid = user.claims.sid.clone();
    let rx = st.events.subscribe();

    let stream = BroadcastStream::new(rx).filter_map(move |ev| {
        let ev = ev.ok()?;
        let for_me = ev.user_id == uid && ev.session_id.as_deref().map_or(true, |s| s == sid);
        if !for_me {
            return None;
        }
        let payload = json!({ "kind": ev.kind, "data": ev.data }).to_string();
        Some(Ok::<Event, Infallible>(
            Event::default().event(ev.kind).data(payload),
        ))
    });

    // Type hint so axum sees a Stream<Item = Result<Event, Infallible>>.
    let stream: std::pin::Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> =
        Box::pin(stream);
    Sse::new(stream).keep_alive(KeepAlive::default())
}
