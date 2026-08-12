//! Runtime configuration, all from env with local-dev defaults so `cargo run`
//! works with nothing set. See `.env.example` for the full list.

use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct ProviderCreds {
    pub client_id: String,
    pub client_secret: String,
}

impl ProviderCreds {
    /// A provider is "configured" only when both id and secret are present.
    fn from_env(prefix: &str) -> Option<ProviderCreds> {
        let id = std::env::var(format!("VANCE_{prefix}_CLIENT_ID")).ok()?;
        let secret = std::env::var(format!("VANCE_{prefix}_CLIENT_SECRET")).ok()?;
        if id.trim().is_empty() || secret.trim().is_empty() {
            return None;
        }
        Some(ProviderCreds {
            client_id: id,
            client_secret: secret,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub addr: SocketAddr,
    pub db_path: PathBuf,
    pub signing_key_path: PathBuf,
    pub public_url: String,
    pub dev_provider: bool,
    pub github: Option<ProviderCreds>,
    pub google: Option<ProviderCreds>,
    pub apple: Option<ProviderCreds>,
    pub meta: Option<ProviderCreds>,
    /// M4 — Stripe (test or live) secret key + webhook signing secret. When
    /// unset, the payment endpoints report "not configured" rather than failing
    /// obscurely (the go-live gate: your Stripe account + keys).
    pub stripe_secret: Option<String>,
    pub stripe_webhook_secret: Option<String>,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

impl Config {
    pub fn from_env() -> Config {
        let addr = env_or("VANCE_ACCOUNTS_ADDR", "127.0.0.1:8787")
            .parse()
            .unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], 8787)));
        Config {
            addr,
            db_path: PathBuf::from(env_or("VANCE_ACCOUNTS_DB", "vance-accounts.db")),
            signing_key_path: PathBuf::from(env_or(
                "VANCE_ACCOUNTS_SIGNING_KEY",
                "signing_key.bin",
            )),
            public_url: env_or("VANCE_ACCOUNTS_PUBLIC_URL", "http://127.0.0.1:8787"),
            dev_provider: env_or("VANCE_ACCOUNTS_DEV_PROVIDER", "true") != "false",
            github: ProviderCreds::from_env("GITHUB"),
            google: ProviderCreds::from_env("GOOGLE"),
            apple: ProviderCreds::from_env("APPLE"),
            meta: ProviderCreds::from_env("META"),
            stripe_secret: std::env::var("VANCE_STRIPE_SECRET_KEY")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            stripe_webhook_secret: std::env::var("VANCE_STRIPE_WEBHOOK_SECRET")
                .ok()
                .filter(|s| !s.trim().is_empty()),
        }
    }
}
