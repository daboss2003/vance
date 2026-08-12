//! Crypto primitives kept in one place: Ed25519 access-token signing (spec
//! §4.3), Argon2id password hashing (§4.2), refresh-token hashing, and PKCE
//! S256 verification.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

// ---------------------------------------------------------------------------
// Access tokens — compact EdDSA-signed JWTs the client verifies OFFLINE with
// the public key. 15-minute TTL; a revoked session is caught by a server-side
// check on each protected call (spec §5, tier 3).
// ---------------------------------------------------------------------------

pub const ACCESS_TTL_SECS: i64 = 15 * 60;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Claims {
    pub sub: String, // user id
    pub sid: String, // session id
    pub did: String, // device id
    pub ent_ver: i64,
    pub iat: i64,
    pub exp: i64,
}

/// Load the signing key from `path`, or generate + persist one (dev
/// convenience — in prod mount a real secret at this path).
pub fn load_or_create_signing_key(path: &Path) -> std::io::Result<SigningKey> {
    match std::fs::read(path) {
        Ok(bytes) if bytes.len() == 32 => {
            let arr: [u8; 32] = bytes.try_into().expect("checked len == 32");
            Ok(SigningKey::from_bytes(&arr))
        }
        _ => {
            let mut csprng = rand::rngs::OsRng;
            let key = SigningKey::generate(&mut csprng);
            std::fs::write(path, key.to_bytes())?;
            Ok(key)
        }
    }
}

/// Generic EdDSA compact token: base64url(header).base64url(payload).base64url(sig)
/// with a fixed EdDSA/JWT header. Used for access, entitlement, and download tokens.
pub fn sign_payload(key: &SigningKey, payload_json: &[u8]) -> String {
    let header = B64.encode(br#"{"alg":"EdDSA","typ":"JWT"}"#);
    let payload = B64.encode(payload_json);
    let signing_input = format!("{header}.{payload}");
    let sig = key.sign(signing_input.as_bytes());
    format!("{signing_input}.{}", B64.encode(sig.to_bytes()))
}

/// Verify signature + structure; returns the raw payload bytes. Does NOT check
/// any `exp` — the caller does, so tests stay deterministic.
pub fn verify_payload(vk: &VerifyingKey, token: &str) -> Option<Vec<u8>> {
    let mut parts = token.split('.');
    let header = parts.next()?;
    let payload = parts.next()?;
    let sig = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let signing_input = format!("{header}.{payload}");
    let sig_bytes = B64.decode(sig).ok()?;
    let signature = Signature::from_slice(&sig_bytes).ok()?;
    vk.verify(signing_input.as_bytes(), &signature).ok()?;
    B64.decode(payload).ok()
}

pub fn sign_access(key: &SigningKey, claims: &Claims) -> String {
    sign_payload(key, &serde_json::to_vec(claims).expect("claims serialize"))
}

pub fn verify_access(vk: &VerifyingKey, token: &str) -> Option<Claims> {
    serde_json::from_slice(&verify_payload(vk, token)?).ok()
}

// --- Entitlement token (spec §6) — signed list of owned skus, cached on the
// --- client and verified OFFLINE. `exp` is the 3-day offline grace window.

pub const ENTITLEMENT_GRACE_SECS: i64 = 3 * 24 * 3600;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EntitlementClaims {
    pub sub: String,
    pub did: String,
    pub skus: Vec<String>,
    pub iat: i64,
    pub exp: i64,
    pub ent_ver: i64,
}

pub fn sign_entitlement(key: &SigningKey, claims: &EntitlementClaims) -> String {
    sign_payload(key, &serde_json::to_vec(claims).expect("entitlement serialize"))
}

// --- Download token — a short-lived signed URL's proof (spec §6): the artifact
// --- endpoint mints it only after an entitlement re-check.

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DownloadClaims {
    pub sku: String,
    pub platform: String,
    pub exp: i64,
}

pub fn sign_download(key: &SigningKey, claims: &DownloadClaims) -> String {
    sign_payload(key, &serde_json::to_vec(claims).expect("download serialize"))
}

pub fn verify_download(vk: &VerifyingKey, token: &str) -> Option<DownloadClaims> {
    serde_json::from_slice(&verify_payload(vk, token)?).ok()
}

pub fn public_key_b64(vk: &VerifyingKey) -> String {
    B64.encode(vk.to_bytes())
}

// ---------------------------------------------------------------------------
// Refresh tokens — opaque 256-bit; we store only sha256(token).
// ---------------------------------------------------------------------------

pub fn sha256_b64(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    B64.encode(h.finalize())
}

pub fn hash_token(token: &str) -> String {
    sha256_b64(token.as_bytes())
}

// ---------------------------------------------------------------------------
// Passwords — Argon2id (spec §4.2).
// ---------------------------------------------------------------------------

pub fn hash_password(pw: &str) -> Result<String, String> {
    use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
    use argon2::Argon2;
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(pw.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

pub fn verify_password(pw: &str, phc: &str) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    use argon2::Argon2;
    match PasswordHash::new(phc) {
        Ok(parsed) => Argon2::default()
            .verify_password(pw.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// PKCE S256 — challenge == base64url(sha256(verifier)).
// ---------------------------------------------------------------------------

pub fn verify_pkce_s256(verifier: &str, challenge: &str) -> bool {
    let mut h = Sha256::new();
    h.update(verifier.as_bytes());
    B64.encode(h.finalize()) == challenge
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_token_round_trips_and_rejects_tamper() {
        let mut csprng = rand::rngs::OsRng;
        let key = SigningKey::generate(&mut csprng);
        let vk = key.verifying_key();
        let claims = Claims {
            sub: "u1".into(),
            sid: "s1".into(),
            did: "d1".into(),
            ent_ver: 3,
            iat: 100,
            exp: 200,
        };
        let tok = sign_access(&key, &claims);
        let got = verify_access(&vk, &tok).expect("valid");
        assert_eq!(got.sub, "u1");
        assert_eq!(got.ent_ver, 3);

        // Flip a character in the payload → signature no longer verifies.
        let mut chars: Vec<char> = tok.chars().collect();
        let mid = chars.len() / 2;
        chars[mid] = if chars[mid] == 'A' { 'B' } else { 'A' };
        let tampered: String = chars.into_iter().collect();
        assert!(verify_access(&vk, &tampered).is_none());
    }

    #[test]
    fn password_hash_verifies() {
        let phc = hash_password("hunter2-correct-horse").unwrap();
        assert!(verify_password("hunter2-correct-horse", &phc));
        assert!(!verify_password("wrong", &phc));
    }

    #[test]
    fn pkce_s256_matches_reference() {
        // RFC 7636 appendix B reference vector.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert!(verify_pkce_s256(verifier, challenge));
        assert!(!verify_pkce_s256("not-the-verifier", challenge));
    }
}
