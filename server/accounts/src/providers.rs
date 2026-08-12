//! OAuth providers. The backend is the OAuth *client* for all of them (spec
//! §4.1) — the desktop app never holds a provider secret. `dev` is handled
//! inline in the auth routes (no network); the real providers below do a
//! standard server-side code exchange and are active only when their
//! credentials are configured.

use crate::config::{Config, ProviderCreds};
use crate::error::{AppError, AppResult};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Dev,
    Google,
    Github,
    Apple,
    Meta,
}

impl Provider {
    pub fn parse(s: &str) -> Option<Provider> {
        Some(match s {
            "dev" => Provider::Dev,
            "google" => Provider::Google,
            "github" => Provider::Github,
            "apple" => Provider::Apple,
            "meta" => Provider::Meta,
            _ => return None,
        })
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Provider::Dev => "dev",
            Provider::Google => "google",
            Provider::Github => "github",
            Provider::Apple => "apple",
            Provider::Meta => "meta",
        }
    }

    fn creds<'a>(&self, cfg: &'a Config) -> Option<&'a ProviderCreds> {
        match self {
            Provider::Dev => None,
            Provider::Google => cfg.google.as_ref(),
            Provider::Github => cfg.github.as_ref(),
            Provider::Apple => cfg.apple.as_ref(),
            Provider::Meta => cfg.meta.as_ref(),
        }
    }

    /// Is this provider usable in the current configuration?
    pub fn is_configured(&self, cfg: &Config) -> bool {
        match self {
            Provider::Dev => cfg.dev_provider,
            _ => self.creds(cfg).is_some(),
        }
    }

    /// The backend's own callback URL that the provider redirects back to.
    fn callback(&self, cfg: &Config) -> String {
        format!("{}/auth/callback?provider={}", cfg.public_url, self.as_str())
    }
}

pub struct ProviderIdentity {
    pub subject: String,
    pub email: Option<String>,
}

/// Build the provider's authorize URL to redirect the user's browser to.
pub fn authorize_url(provider: Provider, cfg: &Config, state: &str) -> AppResult<String> {
    let creds = provider
        .creds(cfg)
        .ok_or_else(|| AppError::BadRequest(format!("{} is not configured", provider.as_str())))?;
    let callback = provider.callback(cfg);
    let redirect = urlencoding::encode(&callback);
    let cid = urlencoding::encode(&creds.client_id);
    let st = urlencoding::encode(state);
    Ok(match provider {
        Provider::Github => format!(
            "https://github.com/login/oauth/authorize?client_id={cid}&redirect_uri={redirect}&scope=read:user%20user:email&state={st}"
        ),
        Provider::Google => format!(
            "https://accounts.google.com/o/oauth2/v2/auth?client_id={cid}&redirect_uri={redirect}&response_type=code&scope=openid%20email&state={st}"
        ),
        Provider::Meta => format!(
            "https://www.facebook.com/v19.0/dialog/oauth?client_id={cid}&redirect_uri={redirect}&scope=email&state={st}"
        ),
        Provider::Apple => format!(
            "https://appleid.apple.com/auth/authorize?client_id={cid}&redirect_uri={redirect}&response_type=code&response_mode=query&scope=email&state={st}"
        ),
        Provider::Dev => return Err(AppError::BadRequest("dev provider has no authorize url".into())),
    })
}

/// Exchange the provider's `code` for a verified identity `(subject, email)`.
pub async fn exchange(provider: Provider, cfg: &Config, code: &str) -> AppResult<ProviderIdentity> {
    let creds = provider
        .creds(cfg)
        .ok_or_else(|| AppError::BadRequest(format!("{} is not configured", provider.as_str())))?;
    let redirect = provider.callback(cfg);
    match provider {
        Provider::Github => github_exchange(creds, code, &redirect).await,
        Provider::Google => google_exchange(creds, code, &redirect).await,
        // Meta and Apple credentials plumb through, but their exchange is wired
        // in M1e (Apple's client secret is itself a signed JWT). Fail loudly
        // rather than silently — never fake a login.
        Provider::Meta | Provider::Apple => Err(AppError::Internal(format!(
            "{} sign-in exchange is not wired yet (M1e)",
            provider.as_str()
        ))),
        Provider::Dev => Err(AppError::BadRequest("dev is handled inline".into())),
    }
}

async fn github_exchange(
    creds: &ProviderCreds,
    code: &str,
    redirect: &str,
) -> AppResult<ProviderIdentity> {
    let client = reqwest::Client::new();
    let token: serde_json::Value = client
        .post("https://github.com/login/oauth/access_token")
        .header("Accept", "application/json")
        .form(&[
            ("client_id", creds.client_id.as_str()),
            ("client_secret", creds.client_secret.as_str()),
            ("code", code),
            ("redirect_uri", redirect),
        ])
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("github token: {e}")))?
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("github token json: {e}")))?;
    let access = token
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::BadRequest("github returned no access_token".into()))?;

    let user: serde_json::Value = client
        .get("https://api.github.com/user")
        .header("Authorization", format!("Bearer {access}"))
        .header("User-Agent", "vance-accounts")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("github user: {e}")))?
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("github user json: {e}")))?;
    let subject = user
        .get("id")
        .map(|v| v.to_string())
        .ok_or_else(|| AppError::BadRequest("github user had no id".into()))?;
    let email = user
        .get("email")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Ok(ProviderIdentity { subject, email })
}

async fn google_exchange(
    creds: &ProviderCreds,
    code: &str,
    redirect: &str,
) -> AppResult<ProviderIdentity> {
    let client = reqwest::Client::new();
    let token: serde_json::Value = client
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("code", code),
            ("client_id", creds.client_id.as_str()),
            ("client_secret", creds.client_secret.as_str()),
            ("redirect_uri", redirect),
            ("grant_type", "authorization_code"),
        ])
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("google token: {e}")))?
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("google token json: {e}")))?;
    let id_token = token
        .get("id_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::BadRequest("google returned no id_token".into()))?;
    // The id_token came directly from Google's token endpoint over TLS, so we
    // read its claims. Verifying the JWT signature against Google's JWKS is a
    // hardening follow-up (defense-in-depth), noted here deliberately.
    let payload = id_token
        .split('.')
        .nth(1)
        .ok_or_else(|| AppError::BadRequest("malformed id_token".into()))?;
    let claims: serde_json::Value = serde_json::from_slice(
        &B64.decode(payload)
            .map_err(|_| AppError::BadRequest("id_token base64".into()))?,
    )
    .map_err(|_| AppError::BadRequest("id_token json".into()))?;
    let subject = claims
        .get("sub")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::BadRequest("id_token had no sub".into()))?
        .to_string();
    let email = claims
        .get("email")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Ok(ProviderIdentity { subject, email })
}
