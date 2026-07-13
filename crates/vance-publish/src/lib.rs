//! `vance-publish` — direct publishing to YouTube / Vimeo / TikTok.
//!
//! Publishing to these platforms is externally gated: each requires the
//! user's OWN registered OAuth application (client id/secret) — Vance
//! cannot embed platform API keys (against every platform's ToS, and
//! per-app quota'd). This crate builds the fully-verifiable, offline
//! pieces: the OAuth 2.0 Authorization-Code + PKCE flow (authorize URL,
//! PKCE challenge, token-exchange form), the per-platform endpoints +
//! scopes, and the upload-request construction (URLs / headers / JSON).
//! The app layer performs the actual HTTP with the user's credentials.

#![warn(missing_docs)]

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A publishing target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Platform {
    /// YouTube (Google) — Data API v3 resumable upload.
    YouTube,
    /// Vimeo.
    Vimeo,
    /// TikTok (content posting API).
    TikTok,
}

impl Platform {
    /// OAuth authorization endpoint.
    pub fn auth_endpoint(&self) -> &'static str {
        match self {
            Platform::YouTube => "https://accounts.google.com/o/oauth2/v2/auth",
            Platform::Vimeo => "https://api.vimeo.com/oauth/authorize",
            Platform::TikTok => "https://www.tiktok.com/v2/auth/authorize/",
        }
    }
    /// OAuth token endpoint.
    pub fn token_endpoint(&self) -> &'static str {
        match self {
            Platform::YouTube => "https://oauth2.googleapis.com/token",
            Platform::Vimeo => "https://api.vimeo.com/oauth/access_token",
            Platform::TikTok => "https://open.tiktokapis.com/v2/oauth/token/",
        }
    }
    /// Space-separated OAuth scopes needed to upload.
    pub fn scopes(&self) -> &'static str {
        match self {
            Platform::YouTube => "https://www.googleapis.com/auth/youtube.upload",
            Platform::Vimeo => "upload",
            Platform::TikTok => "video.upload",
        }
    }
    /// The OAuth `client_id` parameter NAME (TikTok calls it `client_key`).
    pub fn client_id_param(&self) -> &'static str {
        match self {
            Platform::TikTok => "client_key",
            _ => "client_id",
        }
    }
    /// Human label.
    pub fn label(&self) -> &'static str {
        match self {
            Platform::YouTube => "YouTube",
            Platform::Vimeo => "Vimeo",
            Platform::TikTok => "TikTok",
        }
    }
}

/// The user's OAuth application credentials + redirect.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OAuthConfig {
    /// OAuth client id (client_key for TikTok).
    pub client_id: String,
    /// OAuth client secret (public/native clients may omit it).
    #[serde(default)]
    pub client_secret: Option<String>,
    /// Registered redirect URI, e.g. `http://127.0.0.1:8723/callback`.
    pub redirect_uri: String,
}

/// A PKCE verifier + its S256 challenge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pkce {
    /// High-entropy verifier (kept private; sent at token exchange).
    pub verifier: String,
    /// `BASE64URL(SHA256(verifier))` — sent at authorize time.
    pub challenge: String,
}

/// Build a PKCE pair from caller-supplied random bytes (≥ 32 recommended).
/// The crate stays pure/deterministic; the app provides OS randomness.
pub fn pkce_from_bytes(random: &[u8]) -> Pkce {
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let verifier = b64.encode(random);
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = b64.encode(digest);
    Pkce { verifier, challenge }
}

/// Percent-encode for an `application/x-www-form-urlencoded` value / query
/// component (RFC 3986 unreserved kept literal).
pub fn form_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Build the browser authorization URL for the Authorization-Code + PKCE
/// flow. `state` is an opaque CSRF token the caller verifies on callback.
pub fn authorize_url(platform: Platform, cfg: &OAuthConfig, challenge: &str, state: &str) -> String {
    let mut params = vec![
        ("response_type", "code".to_string()),
        (platform.client_id_param(), cfg.client_id.clone()),
        ("redirect_uri", cfg.redirect_uri.clone()),
        ("scope", platform.scopes().to_string()),
        ("state", state.to_string()),
        ("code_challenge", challenge.to_string()),
        ("code_challenge_method", "S256".to_string()),
    ];
    // Google needs offline access + a consent prompt to return a refresh token.
    if platform == Platform::YouTube {
        params.push(("access_type", "offline".to_string()));
        params.push(("prompt", "consent".to_string()));
    }
    let query = params
        .iter()
        .map(|(k, v)| format!("{}={}", k, form_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{}?{}", platform.auth_endpoint(), query)
}

/// Build the `application/x-www-form-urlencoded` body for the token
/// exchange (authorization code → access token).
pub fn token_exchange_form(
    platform: Platform,
    cfg: &OAuthConfig,
    code: &str,
    verifier: &str,
) -> String {
    let mut params = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code.to_string()),
        ("redirect_uri", cfg.redirect_uri.clone()),
        (platform.client_id_param(), cfg.client_id.clone()),
        ("code_verifier", verifier.to_string()),
    ];
    if let Some(secret) = &cfg.client_secret {
        params.push(("client_secret", secret.clone()));
    }
    params
        .iter()
        .map(|(k, v)| format!("{}={}", k, form_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// The token response fields we consume.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct TokenResponse {
    /// Bearer access token.
    pub access_token: String,
    /// Refresh token (present when the platform grants offline access).
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Lifetime in seconds.
    #[serde(default)]
    pub expires_in: Option<i64>,
    /// Token type (usually "Bearer").
    #[serde(default)]
    pub token_type: Option<String>,
}

/// Visibility for a published video.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Privacy {
    /// Publicly listed.
    Public,
    /// Unlisted (link-only).
    Unlisted,
    /// Private.
    Private,
}

impl Privacy {
    /// YouTube `status.privacyStatus` string.
    pub fn youtube(&self) -> &'static str {
        match self {
            Privacy::Public => "public",
            Privacy::Unlisted => "unlisted",
            Privacy::Private => "private",
        }
    }
}

/// User-facing metadata for the upload.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UploadMeta {
    /// Title.
    pub title: String,
    /// Description.
    #[serde(default)]
    pub description: String,
    /// Tags / keywords.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Visibility.
    pub privacy: Privacy,
}

/// A prepared HTTP request the app layer executes (with the bearer token).
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedRequest {
    /// HTTP method.
    pub method: &'static str,
    /// Full URL.
    pub url: String,
    /// Extra headers (besides Authorization, which the app adds).
    pub headers: Vec<(String, String)>,
    /// Request body (may be empty).
    pub body: Vec<u8>,
}

/// Build the YouTube resumable-upload INITIATION request (Data API v3). The
/// app POSTs this with the bearer token; YouTube returns a `Location`
/// header (the session URI) that the app then `PUT`s the video bytes to.
pub fn youtube_resumable_init(meta: &UploadMeta) -> PreparedRequest {
    let body = serde_json::json!({
        "snippet": {
            "title": meta.title,
            "description": meta.description,
            "tags": meta.tags,
        },
        "status": { "privacyStatus": meta.privacy.youtube() },
    });
    let bytes = serde_json::to_vec(&body).unwrap_or_default();
    PreparedRequest {
        method: "POST",
        url: "https://www.googleapis.com/upload/youtube/v3/videos?uploadType=resumable&part=snippet,status".into(),
        headers: vec![
            ("Content-Type".into(), "application/json; charset=UTF-8".into()),
            ("X-Upload-Content-Type".into(), "video/*".into()),
        ],
        body: bytes,
    }
}

/// Errors.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PublishError {
    /// JSON (de)serialization error.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_is_deterministic_and_urlsafe() {
        let p = pkce_from_bytes(&[0u8; 32]);
        // Verifier is base64url of 32 zero bytes; challenge = SHA256 of that.
        assert!(!p.verifier.contains('+') && !p.verifier.contains('/') && !p.verifier.contains('='));
        assert!(!p.challenge.contains('+') && !p.challenge.contains('/') && !p.challenge.contains('='));
        // Same input → same output (pure).
        assert_eq!(p, pkce_from_bytes(&[0u8; 32]));
        // Different input → different verifier.
        assert_ne!(p.verifier, pkce_from_bytes(&[1u8; 32]).verifier);
    }

    #[test]
    fn authorize_url_has_pkce_and_platform_params() {
        let cfg = OAuthConfig {
            client_id: "cid".into(),
            client_secret: None,
            redirect_uri: "http://127.0.0.1:8723/callback".into(),
        };
        let url = authorize_url(Platform::YouTube, &cfg, "CHAL", "STATE");
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        assert!(url.contains("code_challenge=CHAL"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("client_id=cid"));
        assert!(url.contains("access_type=offline"), "google offline access");
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A8723%2Fcallback"));
        // TikTok uses client_key, not client_id.
        let t = authorize_url(Platform::TikTok, &cfg, "C", "S");
        assert!(t.contains("client_key=cid") && !t.contains("client_id="));
    }

    #[test]
    fn token_form_includes_verifier_and_secret() {
        let cfg = OAuthConfig {
            client_id: "cid".into(),
            client_secret: Some("sec ret".into()),
            redirect_uri: "http://x/cb".into(),
        };
        let form = token_exchange_form(Platform::YouTube, &cfg, "the code", "verif");
        assert!(form.contains("grant_type=authorization_code"));
        assert!(form.contains("code=the%20code"));
        assert!(form.contains("code_verifier=verif"));
        assert!(form.contains("client_secret=sec%20ret"));
    }

    #[test]
    fn youtube_init_body_has_snippet_and_privacy() {
        let meta = UploadMeta {
            title: "My Clip".into(),
            description: "hi".into(),
            tags: vec!["a".into(), "b".into()],
            privacy: Privacy::Unlisted,
        };
        let req = youtube_resumable_init(&meta);
        assert_eq!(req.method, "POST");
        assert!(req.url.contains("uploadType=resumable"));
        let v: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(v["snippet"]["title"], "My Clip");
        assert_eq!(v["status"]["privacyStatus"], "unlisted");
        assert_eq!(v["snippet"]["tags"][1], "b");
    }
}
