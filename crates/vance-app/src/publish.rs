//! GOAT-11 — direct publishing to YouTube / Vimeo / TikTok.
//!
//! Publishing is externally gated: each platform requires the USER's own
//! registered OAuth app (client id/secret) — Vance can't embed platform
//! keys. The verifiable pieces (OAuth URL, PKCE, token/upload request
//! construction) live in `vance-publish`; this module performs the live
//! HTTP with `reqwest` + a local loopback redirect listener that captures
//! the OAuth callback code. Without the user's credentials it does
//! nothing but surface setup instructions.
//!
//! Concurrency: the long HTTP round-trips run WITHOUT the hub lock held —
//! commands lock only to read config / store the resulting token.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vance_publish::{
    authorize_url, pkce_from_bytes, token_exchange_form, youtube_resumable_init, OAuthConfig,
    Pkce, Platform, TokenResponse, UploadMeta,
};

/// Errors from the publish pipeline.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PublishError {
    /// No OAuth in progress / not authorized for this platform.
    #[error("{0}")]
    State(String),
    /// The loopback redirect / socket failed.
    #[error("redirect listener: {0}")]
    Io(#[from] std::io::Error),
    /// An HTTP call failed.
    #[error("http: {0}")]
    Http(String),
    /// The platform returned an error / unexpected response.
    #[error("{0}")]
    Api(String),
}

/// In-flight OAuth state (one at a time).
struct Pending {
    platform: Platform,
    cfg: OAuthConfig,
    pkce: Pkce,
    state: String,
    /// Filled by the redirect listener with `(code, state)`.
    slot: Arc<Mutex<Option<(String, String)>>>,
    stop: Arc<AtomicBool>,
}

/// The publish hub (Tauri-managed state).
#[derive(Default)]
pub struct PublishHub {
    pending: Option<Pending>,
    /// Access token per platform, once authorized.
    tokens: std::collections::HashMap<String, String>,
}

impl PublishHub {
    /// Begin OAuth: spawn a loopback listener for the redirect, open the
    /// consent page in the browser, and return the authorize URL (also
    /// shown to the user in case the auto-open is blocked).
    pub fn start_oauth(
        &mut self,
        platform: Platform,
        client_id: String,
        client_secret: Option<String>,
        redirect_uri: String,
    ) -> Result<String, PublishError> {
        // Cancel any previous flow.
        if let Some(p) = self.pending.take() {
            p.stop.store(true, Ordering::Release);
        }
        let cfg = OAuthConfig {
            client_id,
            client_secret: client_secret.filter(|s| !s.is_empty()),
            redirect_uri: redirect_uri.clone(),
        };
        // PKCE verifier + CSRF state from OS entropy (two v4 UUIDs).
        let mut rnd = [0u8; 32];
        rnd[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        rnd[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        let pkce = pkce_from_bytes(&rnd);
        let state = uuid::Uuid::new_v4().to_string();

        let port = redirect_port(&redirect_uri)
            .ok_or_else(|| PublishError::State("redirect_uri must be http://127.0.0.1:PORT/...".into()))?;
        let (slot, stop) = spawn_redirect_listener(port)?;

        let url = authorize_url(platform, &cfg, &pkce.challenge, &state);
        open_browser(&url);
        self.pending = Some(Pending {
            platform,
            cfg,
            pkce,
            state,
            slot,
            stop,
        });
        Ok(url)
    }

    /// Snapshot the in-flight redirect slot (for polling without the lock).
    pub fn pending_slot(&self) -> Option<(Arc<Mutex<Option<(String, String)>>>, String)> {
        self.pending
            .as_ref()
            .map(|p| (p.slot.clone(), p.state.clone()))
    }

    /// After the code is captured + exchanged, store the token.
    pub fn finish(&mut self, access_token: String) {
        if let Some(p) = self.pending.take() {
            self.tokens.insert(p.platform.label().into(), access_token);
            p.stop.store(true, Ordering::Release);
        }
    }

    /// Consume the exchange context (platform, cfg, verifier) for the
    /// current flow so the token request can run OFF the lock.
    pub fn exchange_ctx(&self) -> Option<(Platform, OAuthConfig, String)> {
        self.pending
            .as_ref()
            .map(|p| (p.platform, p.cfg.clone(), p.pkce.verifier.clone()))
    }

    /// The stored access token for a platform, if authorized.
    pub fn token(&self, platform: Platform) -> Option<String> {
        self.tokens.get(platform.label()).cloned()
    }

    /// Is this platform authorized?
    pub fn is_authorized(&self, platform: Platform) -> bool {
        self.tokens.contains_key(platform.label())
    }
}

/// Await the redirect code (polling the shared slot WITHOUT the hub lock),
/// verify state, exchange for a token via HTTPS, and hand the token back.
/// The caller stores it via [`PublishHub::finish`].
pub fn await_and_exchange(
    slot: &Mutex<Option<(String, String)>>,
    expected_state: &str,
    platform: Platform,
    cfg: &OAuthConfig,
    verifier: &str,
) -> Result<String, PublishError> {
    // Poll up to 3 minutes for the browser callback.
    let deadline = Instant::now() + Duration::from_secs(180);
    let (code, got_state) = loop {
        if let Some(pair) = slot.lock().ok().and_then(|mut s| s.take()) {
            break pair;
        }
        if Instant::now() >= deadline {
            return Err(PublishError::State("timed out waiting for authorization".into()));
        }
        std::thread::sleep(Duration::from_millis(150));
    };
    if got_state != expected_state {
        return Err(PublishError::State("OAuth state mismatch (possible CSRF)".into()));
    }
    let form = token_exchange_form(platform, cfg, &code, verifier);
    let client = http_client()?;
    let resp = client
        .post(platform.token_endpoint())
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form)
        .send()
        .map_err(|e| PublishError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        let s = resp.status();
        let body = resp.text().unwrap_or_default();
        return Err(PublishError::Api(format!("token exchange {s}: {}", truncate(&body))));
    }
    let token: TokenResponse = resp.json().map_err(|e| PublishError::Http(e.to_string()))?;
    if token.access_token.is_empty() {
        return Err(PublishError::Api("token response had no access_token".into()));
    }
    Ok(token.access_token)
}

/// Upload a rendered file to YouTube via the resumable protocol. Runs OFF
/// the hub lock. Returns the watch URL.
pub fn upload_youtube(
    access_token: &str,
    file_path: &str,
    meta: &UploadMeta,
) -> Result<String, PublishError> {
    let bytes = std::fs::read(file_path)?;
    let client = http_client()?;
    // 1. Initiate the resumable session.
    let init = youtube_resumable_init(meta);
    let mut rb = client
        .post(&init.url)
        .bearer_auth(access_token)
        .header("X-Upload-Content-Length", bytes.len().to_string());
    for (k, v) in &init.headers {
        rb = rb.header(k, v);
    }
    let init_resp = rb
        .body(init.body)
        .send()
        .map_err(|e| PublishError::Http(e.to_string()))?;
    if !init_resp.status().is_success() {
        let s = init_resp.status();
        let body = init_resp.text().unwrap_or_default();
        return Err(PublishError::Api(format!("youtube init {s}: {}", truncate(&body))));
    }
    let session = init_resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .ok_or_else(|| PublishError::Api("youtube init returned no upload session URI".into()))?;
    // 2. PUT the whole file to the session URI.
    let put = client
        .put(&session)
        .header("Content-Type", "video/*")
        .body(bytes)
        .send()
        .map_err(|e| PublishError::Http(e.to_string()))?;
    if !put.status().is_success() {
        let s = put.status();
        let body = put.text().unwrap_or_default();
        return Err(PublishError::Api(format!("youtube upload {s}: {}", truncate(&body))));
    }
    let v: serde_json::Value = put.json().map_err(|e| PublishError::Http(e.to_string()))?;
    let id = v["id"].as_str().unwrap_or_default();
    Ok(format!("https://youtu.be/{id}"))
}

fn http_client() -> Result<reqwest::blocking::Client, PublishError> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| PublishError::Http(e.to_string()))
}

fn truncate(s: &str) -> String {
    s.chars().take(500).collect()
}

/// Parse the port out of a `http://127.0.0.1:PORT/...` redirect URI.
fn redirect_port(uri: &str) -> Option<u16> {
    let after = uri.split("://").nth(1)?;
    let host = after.split('/').next()?;
    host.rsplit(':').next()?.parse().ok()
}

/// Bind a loopback listener that captures ONE OAuth redirect's `code` +
/// `state`. Returns the shared slot + a stop flag.
fn spawn_redirect_listener(
    port: u16,
) -> std::io::Result<(Arc<Mutex<Option<(String, String)>>>, Arc<AtomicBool>)> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    listener.set_nonblocking(true)?;
    let slot = Arc::new(Mutex::new(None));
    let stop = Arc::new(AtomicBool::new(false));
    let slot_t = slot.clone();
    let stop_t = stop.clone();
    std::thread::Builder::new()
        .name("vance.oauth-redirect".into())
        .spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(300);
            loop {
                if stop_t.load(Ordering::Acquire) || Instant::now() >= deadline {
                    break;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut buf = [0u8; 4096];
                        let n = stream.read(&mut buf).unwrap_or(0);
                        let req = String::from_utf8_lossy(&buf[..n]);
                        if let Some(pair) = parse_callback(&req) {
                            if let Ok(mut s) = slot_t.lock() {
                                *s = Some(pair);
                            }
                            let _ = stream.write_all(RESPONSE_OK.as_bytes());
                            break;
                        } else {
                            let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(80));
                    }
                    Err(_) => break,
                }
            }
        })?;
    Ok((slot, stop))
}

const RESPONSE_OK: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n<!doctype html><html><body style=\"font-family:sans-serif;background:#111;color:#ddd;text-align:center;padding-top:20vh\"><h2>Vance is authorized</h2><p>You can close this tab and return to Vance.</p></body></html>";

/// Extract `(code, state)` from an HTTP request's `GET /...?code=..&state=..`.
fn parse_callback(request: &str) -> Option<(String, String)> {
    let line = request.lines().next()?;
    // "GET /callback?code=..&state=.. HTTP/1.1"
    let target = line.split_whitespace().nth(1)?;
    let query = target.split_once('?')?.1;
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            let val = url_decode(v);
            match k {
                "code" => code = Some(val),
                "state" => state = Some(val),
                _ => {}
            }
        }
    }
    Some((code?, state?))
}

/// Minimal percent-decode for query values.
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Open a URL in the OS default browser (best-effort).
fn open_browser(url: &str) {
    let _ = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()
    } else if cfg!(target_os = "windows") {
        std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(url).spawn()
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_oauth_callback() {
        let req = "GET /callback?code=abc123&state=xyz&scope=x HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
        assert_eq!(parse_callback(req), Some(("abc123".into(), "xyz".into())));
        // Missing code → None.
        assert!(parse_callback("GET /callback?state=x HTTP/1.1\r\n").is_none());
    }

    #[test]
    fn url_decode_and_redirect_port() {
        assert_eq!(url_decode("a%20b+c"), "a b c");
        assert_eq!(redirect_port("http://127.0.0.1:8723/callback"), Some(8723));
        assert_eq!(redirect_port("nonsense"), None);
    }
}
