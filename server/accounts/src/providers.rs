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
        Provider::Meta => meta_exchange(creds, code, &redirect).await,
        Provider::Apple => apple_exchange(creds, code, &redirect).await,
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
    verify_id_token(id_token, &creds.client_id, GOOGLE_JWKS, GOOGLE_ISS).await
}

const GOOGLE_JWKS: &str = "https://www.googleapis.com/oauth2/v3/certs";
const GOOGLE_ISS: &[&str] = &["https://accounts.google.com", "accounts.google.com"];
const APPLE_JWKS: &str = "https://appleid.apple.com/auth/keys";
const APPLE_ISS: &[&str] = &["https://appleid.apple.com"];

/// Verify an OIDC `id_token` against the provider's JWKS (RS256 signature) and
/// its `iss`/`aud`/`exp` claims — defense-in-depth beyond the TLS token
/// exchange. Shared by Google + Apple.
async fn verify_id_token(
    id_token: &str,
    aud: &str,
    jwks_url: &str,
    iss_allow: &[&str],
) -> AppResult<ProviderIdentity> {
    let parts: Vec<&str> = id_token.split('.').collect();
    if parts.len() != 3 {
        return Err(AppError::BadRequest("malformed id_token".into()));
    }
    let header: serde_json::Value = serde_json::from_slice(
        &B64.decode(parts[0]).map_err(|_| AppError::BadRequest("id_token header".into()))?,
    )
    .map_err(|_| AppError::BadRequest("id_token header json".into()))?;
    let kid = header["kid"]
        .as_str()
        .ok_or_else(|| AppError::BadRequest("id_token has no kid".into()))?;

    let jwks: serde_json::Value = reqwest::Client::new()
        .get(jwks_url)
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("jwks fetch: {e}")))?
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("jwks json: {e}")))?;
    let key = jwks["keys"]
        .as_array()
        .and_then(|ks| ks.iter().find(|k| k["kid"].as_str() == Some(kid)))
        .ok_or_else(|| AppError::BadRequest("id_token signing key not in JWKS".into()))?;
    let n = key["n"].as_str().ok_or_else(|| AppError::Internal("jwk n".into()))?;
    let e = key["e"].as_str().ok_or_else(|| AppError::Internal("jwk e".into()))?;
    verify_rs256(n, e, id_token, aud, iss_allow)
}

/// Pure RS256 verification: check the signature with the JWK `(n, e)` and the
/// `iss`/`aud`/`exp` claims. Returns the verified `(sub, email)`.
fn verify_rs256(
    n_b64: &str,
    e_b64: &str,
    id_token: &str,
    aud: &str,
    iss_allow: &[&str],
) -> AppResult<ProviderIdentity> {
    use rsa::{BigUint, Pkcs1v15Sign, RsaPublicKey};
    use sha2::{Digest, Sha256};

    let parts: Vec<&str> = id_token.split('.').collect();
    if parts.len() != 3 {
        return Err(AppError::BadRequest("malformed id_token".into()));
    }
    let n = BigUint::from_bytes_be(
        &B64.decode(n_b64).map_err(|_| AppError::Internal("jwk n b64".into()))?,
    );
    let e = BigUint::from_bytes_be(
        &B64.decode(e_b64).map_err(|_| AppError::Internal("jwk e b64".into()))?,
    );
    let pubkey =
        RsaPublicKey::new(n, e).map_err(|_| AppError::Internal("jwk not a valid RSA key".into()))?;

    let signing_input = format!("{}.{}", parts[0], parts[1]);
    let hashed = Sha256::digest(signing_input.as_bytes());
    let sig = B64
        .decode(parts[2])
        .map_err(|_| AppError::BadRequest("id_token signature b64".into()))?;
    pubkey
        .verify(Pkcs1v15Sign::new::<Sha256>(), hashed.as_slice(), &sig)
        .map_err(|_| AppError::BadRequest("id_token signature is invalid".into()))?;

    let claims: serde_json::Value = serde_json::from_slice(
        &B64.decode(parts[1]).map_err(|_| AppError::BadRequest("id_token payload".into()))?,
    )
    .map_err(|_| AppError::BadRequest("id_token payload json".into()))?;
    let iss = claims["iss"].as_str().unwrap_or("");
    if !iss_allow.contains(&iss) {
        return Err(AppError::BadRequest("id_token issuer mismatch".into()));
    }
    let aud_ok = match &claims["aud"] {
        serde_json::Value::String(s) => s == aud,
        serde_json::Value::Array(a) => a.iter().any(|v| v.as_str() == Some(aud)),
        _ => false,
    };
    if !aud_ok {
        return Err(AppError::BadRequest("id_token audience mismatch".into()));
    }
    if claims["exp"].as_i64().unwrap_or(0) < crate::util::now() {
        return Err(AppError::BadRequest("id_token expired".into()));
    }
    let subject = claims["sub"]
        .as_str()
        .ok_or_else(|| AppError::BadRequest("id_token had no sub".into()))?
        .to_string();
    let email = claims["email"].as_str().map(str::to_string);
    Ok(ProviderIdentity { subject, email })
}

async fn meta_exchange(
    creds: &ProviderCreds,
    code: &str,
    redirect: &str,
) -> AppResult<ProviderIdentity> {
    let client = reqwest::Client::new();
    let token: serde_json::Value = client
        .get("https://graph.facebook.com/v19.0/oauth/access_token")
        .query(&[
            ("client_id", creds.client_id.as_str()),
            ("redirect_uri", redirect),
            ("client_secret", creds.client_secret.as_str()),
            ("code", code),
        ])
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("meta token: {e}")))?
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("meta token json: {e}")))?;
    let access = token
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::BadRequest("meta returned no access_token".into()))?;

    let me: serde_json::Value = client
        .get("https://graph.facebook.com/me")
        .query(&[("fields", "id,email"), ("access_token", access)])
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("meta me: {e}")))?
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("meta me json: {e}")))?;
    let subject = me
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::BadRequest("meta profile had no id".into()))?
        .to_string();
    let email = me.get("email").and_then(|v| v.as_str()).map(str::to_string);
    Ok(ProviderIdentity { subject, email })
}

async fn apple_exchange(
    creds: &ProviderCreds,
    code: &str,
    redirect: &str,
) -> AppResult<ProviderIdentity> {
    // Apple's "client secret" is itself an ES256-signed JWT the operator
    // generates from their .p8 key (valid ≤6 months) — supplied whole via
    // VANCE_APPLE_CLIENT_SECRET. Auto-minting it from the .p8 is a later
    // enhancement; see the README.
    let client = reqwest::Client::new();
    let token: serde_json::Value = client
        .post("https://appleid.apple.com/auth/token")
        .form(&[
            ("client_id", creds.client_id.as_str()),
            ("client_secret", creds.client_secret.as_str()),
            ("code", code),
            ("grant_type", "authorization_code"),
            ("redirect_uri", redirect),
        ])
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("apple token: {e}")))?
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("apple token json: {e}")))?;
    let id_token = token
        .get("id_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::BadRequest("apple returned no id_token".into()))?;
    verify_id_token(id_token, &creds.client_id, APPLE_JWKS, APPLE_ISS).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rs256_verifies_and_rejects_tamper_and_wrong_aud() {
        use rsa::traits::PublicKeyParts;
        use rsa::{Pkcs1v15Sign, RsaPrivateKey};
        use sha2::{Digest, Sha256};

        let mut rng = rand::rngs::OsRng;
        let priv_key = RsaPrivateKey::new(&mut rng, 2048).unwrap();
        let pub_key = priv_key.to_public_key();
        let n_b64 = B64.encode(pub_key.n().to_bytes_be());
        let e_b64 = B64.encode(pub_key.e().to_bytes_be());

        let header = B64.encode(br#"{"alg":"RS256","kid":"test"}"#);
        let payload = B64.encode(
            serde_json::to_vec(&serde_json::json!({
                "sub": "u1", "email": "a@b.com", "aud": "client-123",
                "iss": "https://accounts.google.com", "exp": crate::util::now() + 600
            }))
            .unwrap(),
        );
        let signing_input = format!("{header}.{payload}");
        let hashed = Sha256::digest(signing_input.as_bytes());
        let sig = priv_key
            .sign(Pkcs1v15Sign::new::<Sha256>(), hashed.as_slice())
            .unwrap();
        let token = format!("{signing_input}.{}", B64.encode(&sig));

        let id = verify_rs256(&n_b64, &e_b64, &token, "client-123", GOOGLE_ISS).unwrap();
        assert_eq!(id.subject, "u1");
        assert_eq!(id.email.as_deref(), Some("a@b.com"));

        // Wrong audience → rejected.
        assert!(verify_rs256(&n_b64, &e_b64, &token, "someone-else", GOOGLE_ISS).is_err());
        // Tampered signing input → signature fails.
        let bad = format!("{header}.{payload}x.{}", B64.encode(&sig));
        assert!(verify_rs256(&n_b64, &e_b64, &bad, "client-123", GOOGLE_ISS).is_err());
    }
}
