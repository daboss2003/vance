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

use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use vance_publish::{
    authorize_url, pkce_from_bytes, token_exchange_form, youtube_resumable_init, OAuthConfig, Pkce,
    Platform, TokenResponse, UploadMeta,
};

/// How long we wait for the browser callback. The redirect listener and the
/// polling loop share this deadline so a late callback can't be captured
/// after we've already given up (which would strand a one-time code and show
/// a misleading "authorized" page).
const OAUTH_TIMEOUT_SECS: u64 = 180;

/// Resumable-upload chunk size. Must be a multiple of 256 KiB per the
/// YouTube resumable protocol (except the final chunk). 8 MiB keeps the
/// in-flight allocation tiny regardless of the export size.
const UPLOAD_CHUNK: u64 = 8 * 1024 * 1024;

/// Bounded retries for a single chunk before the whole upload fails.
const UPLOAD_MAX_RETRIES: u32 = 5;

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

/// Shared slot the redirect listener fills with the redirect's `(code, state)`.
type RedirectSlot = Arc<Mutex<Option<(String, String)>>>;

/// In-flight OAuth state (one at a time).
struct Pending {
    platform: Platform,
    cfg: OAuthConfig,
    pkce: Pkce,
    state: String,
    /// Filled by the redirect listener with `(code, state)`.
    slot: RedirectSlot,
    stop: Arc<AtomicBool>,
    /// The redirect-listener thread. Joined on teardown so its loopback
    /// socket is released BEFORE any subsequent flow rebinds the same port.
    handle: Option<JoinHandle<()>>,
}

impl Drop for Pending {
    /// Deterministically stop and join the redirect listener so the port is
    /// freed synchronously — otherwise a restart on the same port races the
    /// old (still-bound) socket and fails with `AddrInUse`, and on shutdown
    /// the thread would linger to its own deadline.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
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
        // Cancel any previous flow SYNCHRONOUSLY: dropping the old `Pending`
        // stops + joins its listener thread, guaranteeing the old loopback
        // socket is closed before we rebind the (fixed) redirect port below.
        drop(self.pending.take());
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

        let port = redirect_port(&redirect_uri).ok_or_else(|| {
            PublishError::State("redirect_uri must be http://127.0.0.1:PORT/...".into())
        })?;
        let (slot, stop, handle) = spawn_redirect_listener(port)?;

        let url = authorize_url(platform, &cfg, &pkce.challenge, &state);
        open_browser(&url);
        self.pending = Some(Pending {
            platform,
            cfg,
            pkce,
            state,
            slot,
            stop,
            handle: Some(handle),
        });
        Ok(url)
    }

    /// Snapshot the in-flight redirect slot (for polling without the lock).
    pub fn pending_slot(&self) -> Option<(RedirectSlot, String)> {
        self.pending
            .as_ref()
            .map(|p| (p.slot.clone(), p.state.clone()))
    }

    /// After the code is captured + exchanged, store the token. Dropping the
    /// `Pending` here stops + joins the (already-finished) listener thread.
    pub fn finish(&mut self, access_token: String) {
        if let Some(p) = self.pending.take() {
            self.tokens.insert(p.platform.label().into(), access_token);
            // `p` dropped here → stop + join the redirect listener.
        }
    }

    /// Abandon the in-flight flow (e.g. the poll timed out). Dropping the
    /// `Pending` stops + joins the listener so its port is freed immediately
    /// and no late callback can be captured after we've given up.
    pub fn cancel(&mut self) {
        drop(self.pending.take());
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
    // Poll for the browser callback, sharing the listener's deadline.
    let deadline = Instant::now() + Duration::from_secs(OAUTH_TIMEOUT_SECS);
    let (code, got_state) = loop {
        if let Some(pair) = slot.lock().ok().and_then(|mut s| s.take()) {
            break pair;
        }
        if Instant::now() >= deadline {
            return Err(PublishError::State(
                "timed out waiting for authorization".into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(150));
    };
    if got_state != expected_state {
        return Err(PublishError::State(
            "OAuth state mismatch (possible CSRF)".into(),
        ));
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
        return Err(PublishError::Api(format!(
            "token exchange {s}: {}",
            truncate(&body)
        )));
    }
    let token: TokenResponse = resp.json().map_err(|e| PublishError::Http(e.to_string()))?;
    if token.access_token.is_empty() {
        return Err(PublishError::Api(
            "token response had no access_token".into(),
        ));
    }
    Ok(token.access_token)
}

/// Upload a rendered file to YouTube via the resumable protocol. Runs OFF
/// the hub lock. Returns the watch URL.
///
/// The file is streamed in fixed-size chunks (never read whole into RAM, so
/// a multi-GB export uploads in a bounded footprint), each sent with a
/// `Content-Range`. A transient network failure retries the current chunk —
/// re-querying the server's committed offset first — instead of restarting
/// the whole transfer.
pub fn upload_youtube(
    access_token: &str,
    file_path: &str,
    meta: &UploadMeta,
) -> Result<String, PublishError> {
    let file_len = std::fs::metadata(file_path)?.len();
    if file_len == 0 {
        return Err(PublishError::Api("cannot upload an empty file".into()));
    }
    let client = http_client()?;
    // 1. Initiate the resumable session (declaring the total length up front,
    //    read cheaply from the file's metadata — not the bytes).
    let init = youtube_resumable_init(meta);
    let mut rb = client
        .post(&init.url)
        .bearer_auth(access_token)
        .header("X-Upload-Content-Length", file_len.to_string());
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
        return Err(PublishError::Api(format!(
            "youtube init {s}: {}",
            truncate(&body)
        )));
    }
    let session = init_resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .ok_or_else(|| PublishError::Api("youtube init returned no upload session URI".into()))?;

    // 2. Stream the file to the session URI in chunks.
    let mut file = std::fs::File::open(file_path)?;
    let mut offset: u64 = 0;
    let mut attempts: u32 = 0;
    loop {
        let this_len = UPLOAD_CHUNK.min(file_len - offset);
        let end = offset + this_len - 1;
        let mut chunk = vec![0u8; this_len as usize];
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut chunk)?;
        let range = format!("bytes {offset}-{end}/{file_len}");
        let resp = client
            .put(&session)
            .header("Content-Type", "video/*")
            .header("Content-Range", &range)
            .body(chunk)
            .send();
        match resp {
            Ok(r) => {
                let status = r.status().as_u16();
                if status == 200 || status == 201 {
                    // Final chunk committed — the body carries the video id.
                    let v: serde_json::Value =
                        r.json().map_err(|e| PublishError::Http(e.to_string()))?;
                    let id = v["id"].as_str().unwrap_or_default();
                    return Ok(format!("https://youtu.be/{id}"));
                } else if status == 308 {
                    // Resume Incomplete — advance to the server's committed end.
                    offset = committed_end(&r).map(|n| n + 1).unwrap_or(end + 1);
                    attempts = 0;
                } else {
                    let s = r.status();
                    let body = r.text().unwrap_or_default();
                    return Err(PublishError::Api(format!(
                        "youtube upload {s}: {}",
                        truncate(&body)
                    )));
                }
            }
            Err(e) => {
                // Transient network failure: retry the current position after
                // re-syncing with the server's committed offset.
                attempts += 1;
                if attempts > UPLOAD_MAX_RETRIES {
                    return Err(PublishError::Http(format!(
                        "youtube upload failed after {UPLOAD_MAX_RETRIES} retries: {e}"
                    )));
                }
                match query_committed_offset(&client, &session, file_len) {
                    // The whole file already landed but we lost the final
                    // response — probe for the resulting video id.
                    Some(n) if n >= file_len => {
                        if let Some(url) = fetch_uploaded_url(&client, access_token) {
                            return Ok(url);
                        }
                        offset = file_len.saturating_sub(1);
                    }
                    Some(n) => offset = n,
                    None => { /* keep `offset`, retry the same chunk */ }
                }
            }
        }
    }
}

/// Parse the committed byte end from a resumable response's `Range: bytes=0-N`.
fn committed_end(resp: &reqwest::blocking::Response) -> Option<u64> {
    parse_range_end(resp.headers().get("range")?.to_str().ok()?)
}

/// Parse the trailing byte index from a `bytes=0-N` (or `bytes 0-N`) range.
fn parse_range_end(range: &str) -> Option<u64> {
    range.rsplit('-').next()?.trim().parse().ok()
}

/// Query the resumable session for how many bytes it has committed. Sends a
/// zero-length `PUT` with `Content-Range: bytes * /total`; a 308 carries the
/// committed range, a 2xx means the upload already completed.
fn query_committed_offset(
    client: &reqwest::blocking::Client,
    session: &str,
    total: u64,
) -> Option<u64> {
    let r = client
        .put(session)
        .header("Content-Range", format!("bytes */{total}"))
        .body(Vec::<u8>::new())
        .send()
        .ok()?;
    let status = r.status().as_u16();
    if status == 308 {
        // Next byte to send = committed_end + 1. No Range header ⇒ nothing
        // committed yet ⇒ resume from 0.
        Some(committed_end(&r).map(|n| n + 1).unwrap_or(0))
    } else if r.status().is_success() {
        Some(total)
    } else {
        None
    }
}

/// Best-effort lookup of the most recently uploaded video's watch URL, used
/// only to recover the id when the final upload response was lost.
fn fetch_uploaded_url(client: &reqwest::blocking::Client, access_token: &str) -> Option<String> {
    let r = client
        .get("https://www.googleapis.com/youtube/v3/search?part=id&forMine=true&type=video&maxResults=1")
        .bearer_auth(access_token)
        .send()
        .ok()?;
    if !r.status().is_success() {
        return None;
    }
    let v: serde_json::Value = r.json().ok()?;
    let id = v["items"][0]["id"]["videoId"].as_str()?;
    Some(format!("https://youtu.be/{id}"))
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
/// `state`. Returns the shared slot, a stop flag, and the thread handle (so
/// the caller can join it and free the port deterministically).
fn spawn_redirect_listener(
    port: u16,
) -> std::io::Result<(RedirectSlot, Arc<AtomicBool>, JoinHandle<()>)> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    listener.set_nonblocking(true)?;
    let slot = Arc::new(Mutex::new(None));
    let stop = Arc::new(AtomicBool::new(false));
    let slot_t = slot.clone();
    let stop_t = stop.clone();
    let handle = std::thread::Builder::new()
        .name("vance.oauth-redirect".into())
        .spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(OAUTH_TIMEOUT_SECS);
            loop {
                if stop_t.load(Ordering::Acquire) || Instant::now() >= deadline {
                    break;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // Read the request fully (it may arrive across several
                        // TCP segments, and the accepted socket's blocking mode
                        // is platform-dependent), bounded so a silent peer
                        // can't wedge the single-threaded accept loop.
                        let req = read_http_request(&mut stream);
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
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(_) => break,
                }
            }
        })?;
    Ok((slot, stop, handle))
}

/// Read an HTTP request from an accepted redirect connection until the whole
/// request line is in hand (or headers end / a cap / a timeout is hit).
///
/// The single-`read` approach is unsound: `accept()` returns as soon as the
/// handshake completes — possibly BEFORE the browser's bytes arrive — and the
/// accepted socket inherits the listener's non-blocking flag on some
/// platforms (so one read can return `WouldBlock` → 0 bytes and silently drop
/// the code) yet is blocking on others (so a peer that sends nothing would
/// block forever). Normalising to a blocking socket with a bounded read
/// timeout and looping until the request line is complete handles both.
fn read_http_request(stream: &mut TcpStream) -> String {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(4)));
    let mut data: Vec<u8> = Vec::new();
    let mut buf = [0u8; 2048];
    let overall = Instant::now() + Duration::from_secs(4);
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break, // peer closed
            Ok(n) => {
                data.extend_from_slice(&buf[..n]);
                // The request target (with the query) lives on the first line,
                // so a single complete line is all `parse_callback` needs.
                if data.contains(&b'\n') {
                    break;
                }
                // Full header block, or an over-long line: stop reading.
                if data.windows(4).any(|w| w == b"\r\n\r\n") || data.len() >= 64 * 1024 {
                    break;
                }
            }
            // Timed out / would-block with a partial (or empty) request: give
            // up on this connection rather than blocking the accept loop.
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                break
            }
            Err(_) => break,
        }
        if Instant::now() >= overall {
            break;
        }
    }
    String::from_utf8_lossy(&data).into_owned()
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
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()
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
        // A trailing bare `%` (or `%X`) must not panic — it is emitted as-is.
        assert_eq!(url_decode("a%"), "a%");
        assert_eq!(url_decode("a%2"), "a%2");
        assert_eq!(url_decode("%41"), "A");
    }

    #[test]
    fn parses_resumable_range() {
        assert_eq!(parse_range_end("bytes=0-262143"), Some(262143));
        assert_eq!(parse_range_end("bytes 0-100"), Some(100));
        assert_eq!(parse_range_end("garbage"), None);
        assert_eq!(parse_range_end(""), None);
    }

    #[test]
    fn redirect_listener_assembles_segmented_request() {
        use std::net::TcpStream;
        // Pick a free port, then hand it to the listener.
        let probe = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let (slot, stop, handle) = spawn_redirect_listener(port).unwrap();

        // Send the request line split across two writes with a pause between,
        // exactly the segmentation that broke the old single-read path.
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.write_all(b"GET /callback?code=abc123").unwrap();
        c.flush().unwrap();
        std::thread::sleep(Duration::from_millis(40));
        c.write_all(b"&state=xyz HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        c.flush().unwrap();
        // Drain the server's response so it doesn't block on write.
        let mut resp = Vec::new();
        let _ = c.read_to_end(&mut resp);

        // The captured pair should appear promptly.
        let mut got = None;
        for _ in 0..200 {
            if let Some(pair) = slot.lock().unwrap().take() {
                got = Some(pair);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, Ordering::Release);
        let _ = handle.join();
        assert_eq!(got, Some(("abc123".to_string(), "xyz".to_string())));
    }
}
