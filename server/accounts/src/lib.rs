//! `vance-accounts` library surface — the module tree, the router builder, and
//! state construction. The binary (`main.rs`) is a thin shim over this, and
//! integration tests drive `build_router` directly.

pub mod auth;
pub mod config;
pub mod crypto;
pub mod db;
pub mod error;
pub mod providers;
pub mod r_account;
pub mod r_auth;
pub mod r_devices;
pub mod r_events;
pub mod state;
pub mod util;

use axum::routing::{get, post};
use axum::Router;
use state::AppState;
use std::sync::Arc;
use tokio::sync::broadcast;

/// Build application state from config: open the DB (+migrate), load/create the
/// signing key, and set up the SSE broadcast channel.
pub async fn init_state(config: config::Config) -> Result<AppState, String> {
    let pool = db::init(&config.db_path)
        .await
        .map_err(|e| format!("database init: {e}"))?;
    let signing = crypto::load_or_create_signing_key(&config.signing_key_path)
        .map_err(|e| format!("signing key: {e}"))?;
    let verifying = signing.verifying_key();
    let (events, _keep) = broadcast::channel(256);
    Ok(AppState {
        pool,
        signing: Arc::new(signing),
        verifying,
        config: Arc::new(config),
        events,
    })
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(r_auth::healthz))
        .route("/auth/pubkey", get(r_auth::pubkey))
        .route("/auth/start", get(r_auth::start))
        .route("/auth/callback", get(r_auth::callback))
        .route("/auth/token", post(r_auth::token))
        .route("/auth/refresh", post(r_auth::refresh))
        .route("/auth/logout", post(r_auth::logout))
        .route("/v1/me", get(r_account::me))
        .route("/v1/password", post(r_account::set_password))
        .route("/v1/stepup", post(r_account::stepup))
        .route("/v1/devices", get(r_devices::list))
        .route("/v1/devices/:id/revoke", post(r_devices::revoke))
        .route("/v1/heartbeat", post(r_events::heartbeat))
        .route("/v1/events", get(r_events::events))
        .with_state(state)
}
