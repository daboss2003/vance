//! Shared application state (axum `State`) + the server-sent event type used to
//! push `session.revoked` / `entitlement.changed` to connected devices (spec §5).

use crate::config::Config;
use crate::db::Db;
use ed25519_dalek::{SigningKey, VerifyingKey};
use std::sync::Arc;
use tokio::sync::broadcast;

/// An event pushed to SSE subscribers. `session_id = None` targets every device
/// of the user; `Some(sid)` targets one device (e.g. a remote sign-out).
#[derive(Clone, Debug)]
pub struct ServerEvent {
    pub user_id: String,
    pub session_id: Option<String>,
    pub kind: String,
    pub data: serde_json::Value,
}

#[derive(Clone)]
pub struct AppState {
    pub pool: Db,
    pub signing: Arc<SigningKey>,
    pub verifying: VerifyingKey,
    pub config: Arc<Config>,
    pub events: broadcast::Sender<ServerEvent>,
}

impl AppState {
    pub fn emit(&self, event: ServerEvent) {
        // Err just means no subscribers right now — the poll/heartbeat tier
        // (spec §5) is the backstop, so a dropped push is not fatal.
        let _ = self.events.send(event);
    }
}
