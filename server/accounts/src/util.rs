//! Tiny shared helpers: monotonic-ish wall-clock seconds and id/token gen.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use rand::RngCore;
use std::time::{SystemTime, UNIX_EPOCH};

/// Unix time in whole seconds. All timestamps in the DB use this.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A fresh random id (UUIDv4, hyphenated).
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// `n` bytes of OS randomness, base64url (no pad). Used for refresh tokens,
/// step-up tokens, PKCE state, and one-time login codes.
pub fn random_token(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    B64.encode(bytes)
}
