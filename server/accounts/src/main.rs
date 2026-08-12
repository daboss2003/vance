//! `vance-accounts` — the Vance accounts service (spec docs/marketplace-spec.md
//! §4–§6, milestone M1). Thin shim over the library: build state, serve. Run
//! with nothing configured and the built-in `dev` provider gives an end-to-end
//! sign-in with no external services.

use vance_accounts::{build_router, config::Config, init_state};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,sqlx=warn".into()),
        )
        .init();

    let config = Config::from_env();
    let addr = config.addr;
    let state = init_state(config).await.expect("init state");
    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("bind {addr}: {e}"));
    tracing::info!("vance-accounts listening on http://{addr}");
    axum::serve(listener, app).await.expect("serve");
}
