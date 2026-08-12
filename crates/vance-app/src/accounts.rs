//! M1 — client half of the accounts system (spec §4–§5).
//!
//! Talks to the `vance-accounts` service via the same loopback-PKCE flow the
//! backend expects. Tokens live in the OS keychain (`keyring`), with an
//! app-managed AES-256-GCM file fallback when no keychain daemon is present
//! (headless/minimal Linux) — the gap flagged in `settings.rs`, now closed.
//!
//! Network calls are blocking `reqwest` and are invoked from async Tauri
//! commands via `spawn_blocking`. Refresh is single-flight behind a mutex, and
//! the new refresh token is persisted BEFORE the new access token is used
//! (spec §4.3 trap).

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine as _;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, State};

use crate::engine::{EngineHandle, EntitlementGate};
use crate::settings;

const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8787";
const KEYRING_SERVICE: &str = "com.vance.accounts";
const SIGN_IN_TIMEOUT_SECS: u64 = 180;
const ACCESS_MARGIN_SECS: i64 = 60;
const HEARTBEAT_SECS: u64 = 60;

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ===========================================================================
// Secret store — OS keychain, or an encrypted file fallback.
// ===========================================================================

enum Backend {
    Keyring,
    File(PathBuf),
}

pub struct SecretStore {
    backend: Backend,
}

impl SecretStore {
    fn new() -> SecretStore {
        if keyring_available() {
            SecretStore {
                backend: Backend::Keyring,
            }
        } else {
            let path = settings::config_dir()
                .unwrap_or_else(|_| std::env::temp_dir())
                .join("credentials.enc");
            tracing::warn!(
                "no OS keychain available; storing account tokens in an \
                 encrypted file ({}). This is best-effort at-rest protection.",
                path.display()
            );
            SecretStore {
                backend: Backend::File(path),
            }
        }
    }

    fn set(&self, key: &str, val: &str) -> Result<(), String> {
        match &self.backend {
            Backend::Keyring => keyring::Entry::new(KEYRING_SERVICE, key)
                .and_then(|e| e.set_password(val))
                .map_err(|e| e.to_string()),
            Backend::File(path) => {
                let mut map = load_file_map(path);
                map.insert(key.to_string(), val.to_string());
                save_file_map(path, &map)
            }
        }
    }

    fn get(&self, key: &str) -> Option<String> {
        match &self.backend {
            Backend::Keyring => keyring::Entry::new(KEYRING_SERVICE, key)
                .ok()
                .and_then(|e| e.get_password().ok()),
            Backend::File(path) => load_file_map(path).get(key).cloned(),
        }
    }

    fn delete(&self, key: &str) {
        match &self.backend {
            Backend::Keyring => {
                if let Ok(e) = keyring::Entry::new(KEYRING_SERVICE, key) {
                    let _ = e.delete_credential();
                }
            }
            Backend::File(path) => {
                let mut map = load_file_map(path);
                if map.remove(key).is_some() {
                    let _ = save_file_map(path, &map);
                }
            }
        }
    }
}

fn keyring_available() -> bool {
    let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, "__probe__") else {
        return false;
    };
    if entry.set_password("1").is_err() {
        return false;
    }
    let ok = entry.get_password().map(|v| v == "1").unwrap_or(false);
    let _ = entry.delete_credential();
    ok
}

// --- encrypted-file backend ------------------------------------------------

/// 32-byte key derived from a machine-stable secret, so the file is neither
/// plaintext nor portable to another machine. Best-effort (a same-user attacker
/// with the machine id can still derive it) — documented honestly in §15.4.
fn machine_key() -> [u8; 32] {
    let id = std::fs::read_to_string("/etc/machine-id")
        .ok()
        .or_else(|| std::fs::read_to_string("/var/lib/dbus/machine-id").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(persisted_random_id);
    let mut h = Sha256::new();
    h.update(b"vance-secretstore-v1");
    h.update(id.as_bytes());
    h.finalize().into()
}

/// Fallback machine secret when there's no /etc/machine-id: a random value
/// persisted (0600) next to the credentials file.
fn persisted_random_id() -> String {
    use rand::RngCore;
    let path = settings::config_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("device.key");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        if !existing.trim().is_empty() {
            return existing.trim().to_string();
        }
    }
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    let id = b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let _ = write_private(&path, id.as_bytes());
    id
}

fn write_private(path: &PathBuf, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn load_file_map(path: &PathBuf) -> HashMap<String, String> {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Key, Nonce};
    let Ok(blob) = std::fs::read(path) else {
        return HashMap::new();
    };
    if blob.len() < 12 {
        return HashMap::new();
    }
    let (nonce_bytes, ct) = blob.split_at(12);
    let key = machine_key();
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    match cipher.decrypt(Nonce::from_slice(nonce_bytes), ct) {
        Ok(pt) => serde_json::from_slice(&pt).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

fn save_file_map(path: &PathBuf, map: &HashMap<String, String>) -> Result<(), String> {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Key, Nonce};
    use rand::RngCore;
    let plaintext = serde_json::to_vec(map).map_err(|e| e.to_string())?;
    let key = machine_key();
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    let mut nonce_bytes = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), plaintext.as_slice())
        .map_err(|e| e.to_string())?;
    let mut blob = Vec::with_capacity(12 + ct.len());
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ct);
    write_private(path, &blob).map_err(|e| e.to_string())
}

// ===========================================================================
// Accounts client state
// ===========================================================================

pub struct AccountsState {
    store: SecretStore,
    base_url: Mutex<String>,
    device_id: String,
    device_name: String,
    platform_str: String,
    /// Serialises refresh so concurrent commands don't rotate in parallel.
    refresh_guard: Mutex<()>,
    watcher_started: AtomicBool,
    /// M2 — shared with the engine; enforces the load-time entitlement gate.
    gate: Arc<EntitlementGate>,
    /// Last entitlement version seen from the server, so the heartbeat can
    /// detect a change and prompt a reconcile. -1 = unknown.
    last_ent_ver: AtomicI64,
}

/// Construct the managed accounts state (called once at app setup).
pub fn new_state() -> Arc<AccountsState> {
    let store = SecretStore::new();
    let device_id = store.get("device_id").unwrap_or_else(|| {
        let id = uuid::Uuid::new_v4().to_string();
        let _ = store.set("device_id", &id);
        id
    });
    let base_url = settings::load()
        .accounts_base_url
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    Arc::new(AccountsState {
        store,
        base_url: Mutex::new(base_url),
        device_id,
        device_name: device_name(),
        platform_str: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        refresh_guard: Mutex::new(()),
        watcher_started: AtomicBool::new(false),
        gate: Arc::new(EntitlementGate::default()),
        last_ent_ver: AtomicI64::new(-1),
    })
}

fn device_name() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "Vance device".to_string())
}

fn http() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .expect("reqwest client")
}

impl AccountsState {
    fn base(&self) -> String {
        self.base_url.lock().clone()
    }

    pub fn signed_in(&self) -> bool {
        self.store.get("refresh").is_some()
    }

    fn wipe(&self) {
        self.store.delete("access");
        self.store.delete("refresh");
        self.store.delete("access_exp");
    }

    fn persist_tokens(&self, access: &str, refresh: &str, expires_in: i64) -> Result<(), String> {
        // Persist refresh FIRST so a crash can never leave us with a new access
        // token but the old (now-rotated-away) refresh.
        self.store.set("refresh", refresh)?;
        self.store.set("access", access)?;
        self.store
            .set("access_exp", &(now() + expires_in - ACCESS_MARGIN_SECS).to_string())?;
        Ok(())
    }

    /// A currently-valid access token, refreshing (single-flight) if needed.
    fn access_token(&self) -> Result<String, String> {
        let _guard = self.refresh_guard.lock();
        let access = self.store.get("access");
        let exp: i64 = self
            .store
            .get("access_exp")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if let Some(a) = access {
            if exp > now() {
                return Ok(a);
            }
        }
        // Refresh.
        let refresh = self
            .store
            .get("refresh")
            .ok_or_else(|| "signed out".to_string())?;
        let resp = http()
            .post(format!("{}/auth/refresh", self.base()))
            .json(&json!({ "refresh_token": refresh }))
            .send()
            .map_err(|e| format!("network: {e}"))?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            self.wipe();
            return Err("session ended".to_string());
        }
        let body: Value = resp.json().map_err(|e| e.to_string())?;
        let access = body["access_token"]
            .as_str()
            .ok_or("no access_token in refresh")?;
        let new_refresh = body["refresh_token"].as_str().unwrap_or(&refresh);
        let expires_in = body["expires_in"].as_i64().unwrap_or(900);
        self.persist_tokens(access, new_refresh, expires_in)?;
        Ok(access.to_string())
    }

    /// Authenticated request. On 401 the tokens are wiped (session revoked or
    /// ended) and an error is returned so the UI drops to signed-out.
    fn send_authed(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
        let token = self.access_token()?;
        let url = format!("{}{}", self.base(), path);
        let client = http();
        let mut req = match method {
            "POST" => client.post(&url),
            _ => client.get(&url),
        }
        .bearer_auth(token);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().map_err(|e| format!("network: {e}"))?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            self.wipe();
            return Err("session ended".to_string());
        }
        let text = resp.text().unwrap_or_default();
        let value: Value = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::Null)
        };
        if !status.is_success() {
            let msg = value
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("request failed")
                .to_string();
            return Err(msg);
        }
        Ok(value)
    }

    // --- flows -------------------------------------------------------------

    fn sign_in(&self, provider: &str) -> Result<Value, String> {
        use rand::RngCore;
        let mut rnd = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut rnd);
        let pkce = vance_publish::pkce_from_bytes(&rnd);

        // Bind the loopback FIRST so we know the port for the redirect URI.
        let listener =
            TcpListener::bind(("127.0.0.1", 0)).map_err(|e| format!("loopback bind: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| e.to_string())?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{port}/callback");

        let enc = vance_publish::form_encode;
        let url = format!(
            "{}/auth/start?provider={}&code_challenge={}&redirect_uri={}&device_id={}&device_name={}&platform={}",
            self.base(),
            enc(provider),
            enc(&pkce.challenge),
            enc(&redirect_uri),
            enc(&self.device_id),
            enc(&self.device_name),
            enc(&self.platform_str),
        );
        open_browser(&url);

        let code = accept_login_code(listener, Duration::from_secs(SIGN_IN_TIMEOUT_SECS))?;
        let resp = http()
            .post(format!("{}/auth/token", self.base()))
            .json(&json!({ "code": code, "code_verifier": pkce.verifier }))
            .send()
            .map_err(|e| format!("network: {e}"))?;
        if !resp.status().is_success() {
            let v: Value = resp.json().unwrap_or(Value::Null);
            return Err(v
                .get("error")
                .and_then(|s| s.as_str())
                .unwrap_or("sign-in failed")
                .to_string());
        }
        let body: Value = resp.json().map_err(|e| e.to_string())?;
        let access = body["access_token"].as_str().ok_or("no access_token")?;
        let refresh = body["refresh_token"].as_str().ok_or("no refresh_token")?;
        let expires_in = body["expires_in"].as_i64().unwrap_or(900);
        self.persist_tokens(access, refresh, expires_in)?;
        Ok(body.get("user").cloned().unwrap_or(Value::Null))
    }

    fn status(&self) -> Value {
        if !self.signed_in() {
            self.gate.set_state(HashSet::new(), HashSet::new());
            return json!({ "signed_in": false });
        }
        // Enforce the gate from the cached token straight away (offline-capable).
        self.refresh_gate();
        match self.send_authed("GET", "/v1/me", None) {
            Ok(user) => json!({ "signed_in": true, "user": user }),
            Err(_) => json!({ "signed_in": false }),
        }
    }

    fn sign_out(&self) {
        if let Some(refresh) = self.store.get("refresh") {
            let _ = http()
                .post(format!("{}/auth/logout", self.base()))
                .json(&json!({ "refresh_token": refresh }))
                .send();
        }
        self.wipe();
    }

    /// Background poll (spec §5 tier 2). On a revoked/ended session, wipe and
    /// tell the UI. Started once, after sign-in / on a signed-in startup.
    fn ensure_watcher(self: &Arc<Self>, app: AppHandle) {
        if self.watcher_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let this = self.clone();
        let _ = std::thread::Builder::new()
            .name("vance.account-heartbeat".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_secs(HEARTBEAT_SECS));
                    if !this.signed_in() {
                        break;
                    }
                    match this.send_authed("POST", "/v1/heartbeat", Some(json!({}))) {
                        Ok(v) => {
                            // Entitlement changed on the server → nudge the UI to
                            // reconcile (install new / uninstall revoked).
                            if let Some(ev) = v.get("ent_ver").and_then(|x| x.as_i64()) {
                                let prev = this.last_ent_ver.swap(ev, Ordering::SeqCst);
                                if prev != -1 && prev != ev {
                                    let _ = app.emit("account://entitlements-changed", ());
                                }
                            }
                        }
                        Err(_) => {
                            if !this.signed_in() {
                                // send_authed wiped tokens → session ended remotely.
                                let _ = app.emit("account://revoked", ());
                                break;
                            }
                            // Transient network error: keep polling.
                        }
                    }
                }
                this.watcher_started.store(false, Ordering::SeqCst);
            });
    }
}

// ===========================================================================
// M2 — entitlements + install pipeline (spec §6)
// ===========================================================================

/// Claims of the backend's signed entitlement token (verified OFFLINE here).
#[derive(Deserialize)]
struct EntTokenClaims {
    skus: Vec<String>,
    exp: i64,
    ent_ver: i64,
}

/// Local record of one installed marketplace effect (`installed.json`).
#[derive(Serialize, Deserialize, Clone)]
struct InstallRecord {
    dest: String,
    version: String,
    requires_entitlement: bool,
}
type Registry = HashMap<String, InstallRecord>; // sku -> record (sku == manifest id)

fn registry_path() -> PathBuf {
    settings::config_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("installed.json")
}
fn load_registry() -> Registry {
    std::fs::read(registry_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}
fn save_registry(reg: &Registry) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(reg).map_err(|e| e.to_string())?;
    std::fs::write(registry_path(), json).map_err(|e| e.to_string())
}

fn sha256_b64(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    B64.encode(h.finalize())
}

fn parse_vk(b64: &str) -> Option<VerifyingKey> {
    let bytes = B64.decode(b64).ok()?;
    let arr: [u8; 32] = bytes.try_into().ok()?;
    VerifyingKey::from_bytes(&arr).ok()
}

/// Pure core of entitlement-token verification: EdDSA signature + not-past-grace.
/// Must mirror the server's `sign_entitlement` (header.payload.sig, fixed header).
fn verify_ent_token_with(vk: &VerifyingKey, token: &str) -> Option<EntTokenClaims> {
    let mut parts = token.split('.');
    let (h, p, s) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let signing_input = format!("{h}.{p}");
    let sig = Signature::from_slice(&B64.decode(s).ok()?).ok()?;
    vk.verify(signing_input.as_bytes(), &sig).ok()?;
    let claims: EntTokenClaims = serde_json::from_slice(&B64.decode(p).ok()?).ok()?;
    if claims.exp < now() {
        return None; // offline grace window lapsed
    }
    Some(claims)
}

/// A plugin `dest` must be a plain filename (no path separators / traversal).
fn safe_plugin_dest(dir: &std::path::Path, dest: &str) -> Result<PathBuf, String> {
    if dest.is_empty() || dest.contains('/') || dest.contains('\\') || dest.contains("..") {
        return Err("invalid effect filename".into());
    }
    Ok(dir.join(dest))
}

fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("wasm.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

impl AccountsState {
    /// The shared gate (installed on the engine at startup).
    pub fn gate(&self) -> Arc<EntitlementGate> {
        self.gate.clone()
    }

    /// The backend's Ed25519 public key, cached, for offline token verification.
    fn server_pubkey(&self) -> Option<VerifyingKey> {
        if let Some(pk) = self.store.get("server_pubkey") {
            if let Some(vk) = parse_vk(&pk) {
                return Some(vk);
            }
        }
        let resp = http().get(format!("{}/auth/pubkey", self.base())).send().ok()?;
        let v: Value = resp.json().ok()?;
        let pk = v["public_key"].as_str()?.to_string();
        let vk = parse_vk(&pk)?;
        let _ = self.store.set("server_pubkey", &pk);
        Some(vk)
    }

    /// Verify a cached entitlement token (signature + not-past-grace) and return
    /// its claims. This is the OFFLINE boundary.
    fn verify_ent_token(&self, token: &str) -> Option<EntTokenClaims> {
        let vk = self.server_pubkey()?;
        verify_ent_token_with(&vk, token)
    }

    fn owned_offline(&self) -> Vec<String> {
        self.store
            .get("entitlement_token")
            .and_then(|t| self.verify_ent_token(&t))
            .map(|c| c.skus)
            .unwrap_or_default()
    }

    /// Fetch a fresh entitlement token (online), cache it, and return owned skus.
    /// Falls back to the cached token when offline.
    fn fetch_owned(&self) -> Vec<String> {
        if let Ok(v) = self.send_authed("GET", "/v1/entitlement-token", None) {
            if let Some(tok) = v["token"].as_str() {
                let _ = self.store.set("entitlement_token", tok);
                if let Some(c) = self.verify_ent_token(tok) {
                    self.last_ent_ver.store(c.ent_ver, Ordering::SeqCst);
                    return c.skus;
                }
            }
        }
        self.owned_offline()
    }

    fn refresh_gate_with(&self, owned: &HashSet<String>) {
        let reg = load_registry();
        let mut gated = HashSet::new();
        let mut entitled = HashSet::new();
        for (sku, rec) in &reg {
            if rec.requires_entitlement {
                gated.insert(sku.clone());
                if owned.contains(sku) {
                    entitled.insert(sku.clone());
                }
            }
        }
        self.gate.set_state(gated, entitled);
    }

    /// Recompute the gate from the cached token (offline, cheap).
    pub fn refresh_gate(&self) {
        let owned: HashSet<String> = self.owned_offline().into_iter().collect();
        self.refresh_gate_with(&owned);
    }

    /// Download + verify (sha256 pin) + atomically place an artifact response,
    /// recording it locally. Shared by paid (§6) and free-catalog (§9) install.
    fn place_from_artifact(
        &self,
        art: &Value,
        sku: &str,
        requires_entitlement: bool,
    ) -> Result<(), String> {
        let url = art["download_url"].as_str().ok_or("no download URL")?;
        let sha = art["sha256"].as_str().ok_or("no checksum")?;
        let dest = art["dest"].as_str().ok_or("no destination")?.to_string();
        let version = art["version"].as_str().unwrap_or("1").to_string();

        let bytes = http()
            .get(url)
            .send()
            .map_err(|e| format!("download: {e}"))?
            .error_for_status()
            .map_err(|e| format!("download: {e}"))?
            .bytes()
            .map_err(|e| format!("download: {e}"))?;
        if sha256_b64(bytes.as_ref()) != sha {
            return Err("the downloaded effect failed its checksum check".into());
        }
        let dir = vance_plugin_sdk::discovery::user_plugins_dir()
            .ok_or_else(|| "couldn't locate the add-ons folder".to_string())?;
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = safe_plugin_dest(&dir, &dest)?;
        write_atomic(&path, bytes.as_ref()).map_err(|e| e.to_string())?;

        let mut reg = load_registry();
        reg.insert(
            sku.to_string(),
            InstallRecord {
                dest,
                version,
                requires_entitlement,
            },
        );
        save_registry(&reg)?;
        Ok(())
    }

    /// Install an owned (paid/gated) effect via the auth'd artifact endpoint.
    fn install_sku(&self, sku: &str) -> Result<(), String> {
        let art = self.send_authed("POST", &format!("/v1/artifacts/{sku}"), Some(json!({})))?;
        self.place_from_artifact(&art, sku, true)
    }

    // --- catalog (§9) ------------------------------------------------------

    /// Unauthenticated GET (catalog browse/detail/reviews are public).
    fn get_public(&self, path: &str) -> Result<Value, String> {
        let resp = http()
            .get(format!("{}{}", self.base(), path))
            .send()
            .map_err(|e| format!("network: {e}"))?;
        if !resp.status().is_success() {
            let v: Value = resp.json().unwrap_or(Value::Null);
            return Err(v
                .get("error")
                .and_then(|s| s.as_str())
                .unwrap_or("request failed")
                .to_string());
        }
        resp.json().map_err(|e| e.to_string())
    }

    /// Install a FREE catalog effect (no entitlement). Sends the bearer when
    /// signed in so the install is recorded (enables reviewing).
    fn catalog_install(&self, sku: &str) -> Result<(), String> {
        let url = format!("{}/v1/catalog/{}/artifact", self.base(), sku);
        let mut req = http().post(&url).json(&json!({}));
        if let Ok(access) = self.access_token() {
            req = req.bearer_auth(access);
        }
        let resp = req.send().map_err(|e| format!("network: {e}"))?;
        if !resp.status().is_success() {
            let v: Value = resp.json().unwrap_or(Value::Null);
            return Err(v
                .get("error")
                .and_then(|s| s.as_str())
                .unwrap_or("install failed")
                .to_string());
        }
        let art: Value = resp.json().map_err(|e| e.to_string())?;
        self.place_from_artifact(&art, sku, false)
    }

    fn uninstall_sku(&self, sku: &str) {
        let mut reg = load_registry();
        if let Some(rec) = reg.remove(sku) {
            if let Some(dir) = vance_plugin_sdk::discovery::user_plugins_dir() {
                if let Ok(path) = safe_plugin_dest(&dir, &rec.dest) {
                    let _ = std::fs::remove_file(&path);
                }
            }
            let _ = save_registry(&reg);
        }
    }

    /// Install owned-but-missing (if auto), uninstall installed-but-revoked, and
    /// refresh the gate. Returns a report for the UI.
    fn reconcile(&self, auto_install: bool) -> Value {
        let owned_vec = self.fetch_owned();
        let owned: HashSet<String> = owned_vec.iter().cloned().collect();

        let mut uninstalled = vec![];
        for (sku, rec) in load_registry() {
            if rec.requires_entitlement && !owned.contains(&sku) {
                self.uninstall_sku(&sku);
                uninstalled.push(sku);
            }
        }
        let mut installed = vec![];
        if auto_install {
            let reg = load_registry();
            for sku in &owned {
                if !reg.contains_key(sku) {
                    match self.install_sku(sku) {
                        Ok(()) => installed.push(sku.clone()),
                        Err(e) => tracing::warn!("auto-install {sku}: {e}"),
                    }
                }
            }
        }
        self.refresh_gate_with(&owned);
        json!({ "installed": installed, "uninstalled": uninstalled, "owned": owned_vec })
    }

    /// Owned effects for the "My Effects" UI, with local install status.
    fn list_owned(&self) -> Result<Value, String> {
        let v = self.send_authed("GET", "/v1/entitlements", None)?;
        let reg = load_registry();
        let effects: Vec<Value> = v["entitlements"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|e| {
                        let sku = e["sku"].as_str().unwrap_or("");
                        json!({
                            "sku": sku,
                            "display_name": e.get("display_name").cloned().unwrap_or(Value::Null),
                            "installed": reg.contains_key(sku),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(json!({ "effects": effects }))
    }
}

// ===========================================================================
// Loopback helpers
// ===========================================================================

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

const OK_PAGE: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n<!doctype html><html><body style=\"font-family:sans-serif;background:#17191d;color:#dfe3e9;text-align:center;padding-top:20vh\"><h2>You're signed in to Vance</h2><p>You can close this tab and return to Vance.</p></body></html>";

/// Accept ONE loopback callback and return its `code` query param.
fn accept_login_code(listener: TcpListener, timeout: Duration) -> Result<String, String> {
    listener
        .set_nonblocking(true)
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            return Err("sign-in timed out (no response from the browser)".into());
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let req = read_request(&mut stream);
                if let Some(code) = parse_code(&req) {
                    let _ = stream.write_all(OK_PAGE.as_bytes());
                    return Ok(code);
                }
                let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

fn read_request(stream: &mut std::net::TcpStream) -> String {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(4)));
    let mut data = Vec::new();
    let mut buf = [0u8; 1024];
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                data.extend_from_slice(&buf[..n]);
                if data.windows(4).any(|w| w == b"\r\n\r\n") || data.contains(&b'\n') {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&data).into_owned()
}

/// Pull `code` out of the request line `GET /callback?code=XYZ&... HTTP/1.1`.
fn parse_code(request: &str) -> Option<String> {
    let line = request.lines().next()?;
    let path = line.split_whitespace().nth(1)?;
    let query = path.split('?').nth(1)?;
    for pair in query.split('&') {
        if let Some(v) = pair.strip_prefix("code=") {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

// ===========================================================================
// Tauri commands
// ===========================================================================

type St<'a> = State<'a, Arc<AccountsState>>;

#[tauri::command]
pub async fn account_status(app: AppHandle, state: St<'_>) -> Result<Value, String> {
    let st = state.inner().clone();
    let res = tokio::task::spawn_blocking(move || st.status())
        .await
        .map_err(|e| e.to_string())?;
    if res.get("signed_in").and_then(|v| v.as_bool()).unwrap_or(false) {
        state.inner().clone().ensure_watcher(app);
    }
    Ok(res)
}

#[tauri::command]
pub async fn account_sign_in(app: AppHandle, state: St<'_>, provider: String) -> Result<Value, String> {
    let st = state.inner().clone();
    let user = tokio::task::spawn_blocking(move || st.sign_in(&provider))
        .await
        .map_err(|e| e.to_string())??;
    state.inner().clone().ensure_watcher(app);
    Ok(user)
}

#[tauri::command]
pub async fn account_sign_out(state: St<'_>) -> Result<(), String> {
    let st = state.inner().clone();
    tokio::task::spawn_blocking(move || st.sign_out())
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn account_list_devices(state: St<'_>) -> Result<Value, String> {
    let st = state.inner().clone();
    tokio::task::spawn_blocking(move || st.send_authed("GET", "/v1/devices", None))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn account_stepup(
    state: St<'_>,
    action: String,
    target_ref: Option<String>,
    password: String,
) -> Result<Value, String> {
    let st = state.inner().clone();
    tokio::task::spawn_blocking(move || {
        st.send_authed(
            "POST",
            "/v1/stepup",
            Some(json!({ "action": action, "target_ref": target_ref, "password": password })),
        )
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn account_revoke_device(
    state: St<'_>,
    device_id: String,
    stepup_token: String,
) -> Result<(), String> {
    let st = state.inner().clone();
    tokio::task::spawn_blocking(move || {
        st.send_authed(
            "POST",
            &format!("/v1/devices/{device_id}/revoke"),
            Some(json!({ "stepup_token": stepup_token })),
        )
        .map(|_| ())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn account_set_password(
    state: St<'_>,
    password: String,
    stepup_token: Option<String>,
) -> Result<(), String> {
    let st = state.inner().clone();
    tokio::task::spawn_blocking(move || {
        st.send_authed(
            "POST",
            "/v1/password",
            Some(json!({ "password": password, "stepup_token": stepup_token })),
        )
        .map(|_| ())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub fn account_base_url(state: St<'_>) -> String {
    state.base()
}

#[tauri::command]
pub fn account_set_base_url(state: St<'_>, url: String) -> Result<(), String> {
    let url = url.trim().trim_end_matches('/').to_string();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("URL must start with http:// or https://".into());
    }
    *state.base_url.lock() = url.clone();
    let mut s = settings::load();
    s.accounts_base_url = Some(url);
    settings::save(&s).map_err(|e| e.to_string())
}

// --- M2 entitlement commands ----------------------------------------------

/// Owned effects + local install status, for the "My Effects" UI.
#[tauri::command]
pub async fn entitlements_list(state: St<'_>) -> Result<Value, String> {
    let st = state.inner().clone();
    tokio::task::spawn_blocking(move || st.list_owned())
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn entitlement_install(
    state: St<'_>,
    engine: State<'_, EngineHandle>,
    sku: String,
) -> Result<(), String> {
    let st = state.inner().clone();
    tokio::task::spawn_blocking(move || {
        st.install_sku(&sku)?;
        st.refresh_gate();
        Ok::<(), String>(())
    })
    .await
    .map_err(|e| e.to_string())??;
    engine.lock().rescan_plugins();
    Ok(())
}

#[tauri::command]
pub async fn entitlement_uninstall(
    state: St<'_>,
    engine: State<'_, EngineHandle>,
    sku: String,
) -> Result<(), String> {
    let st = state.inner().clone();
    tokio::task::spawn_blocking(move || {
        st.uninstall_sku(&sku);
        st.refresh_gate();
    })
    .await
    .map_err(|e| e.to_string())?;
    engine.lock().rescan_plugins();
    Ok(())
}

/// Reconcile with the server: (auto-)install owned, uninstall revoked, update the
/// gate, then rescan so the engine sees the change.
#[tauri::command]
pub async fn entitlements_reconcile(
    state: St<'_>,
    engine: State<'_, EngineHandle>,
) -> Result<Value, String> {
    let auto = settings::load().auto_install_purchases;
    let st = state.inner().clone();
    let report = tokio::task::spawn_blocking(move || st.reconcile(auto))
        .await
        .map_err(|e| e.to_string())?;
    engine.lock().rescan_plugins();
    Ok(report)
}

// --- M3 catalog commands ---------------------------------------------------

#[tauri::command]
pub async fn catalog_browse(
    state: St<'_>,
    q: Option<String>,
    category: Option<String>,
    sort: Option<String>,
    page: Option<i64>,
) -> Result<Value, String> {
    let mut path = format!("/v1/catalog?page={}", page.unwrap_or(0));
    if let Some(q) = q.filter(|s| !s.is_empty()) {
        path.push_str(&format!("&q={}", vance_publish::form_encode(&q)));
    }
    if let Some(c) = category.filter(|s| !s.is_empty()) {
        path.push_str(&format!("&category={}", vance_publish::form_encode(&c)));
    }
    if let Some(s) = sort {
        path.push_str(&format!("&sort={}", vance_publish::form_encode(&s)));
    }
    let st = state.inner().clone();
    tokio::task::spawn_blocking(move || st.get_public(&path))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn catalog_detail(state: St<'_>, sku: String) -> Result<Value, String> {
    let st = state.inner().clone();
    let path = format!("/v1/catalog/{}", vance_publish::form_encode(&sku));
    tokio::task::spawn_blocking(move || st.get_public(&path))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn catalog_reviews(state: St<'_>, sku: String) -> Result<Value, String> {
    let st = state.inner().clone();
    let path = format!("/v1/catalog/{}/reviews", vance_publish::form_encode(&sku));
    tokio::task::spawn_blocking(move || st.get_public(&path))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn catalog_install(
    state: St<'_>,
    engine: State<'_, EngineHandle>,
    sku: String,
) -> Result<(), String> {
    let st = state.inner().clone();
    tokio::task::spawn_blocking(move || st.catalog_install(&sku))
        .await
        .map_err(|e| e.to_string())??;
    engine.lock().rescan_plugins();
    Ok(())
}

#[tauri::command]
pub async fn catalog_review(
    state: St<'_>,
    sku: String,
    stars: i64,
    body: Option<String>,
) -> Result<(), String> {
    let st = state.inner().clone();
    tokio::task::spawn_blocking(move || {
        st.send_authed(
            "POST",
            &format!("/v1/catalog/{sku}/reviews"),
            Some(json!({ "stars": stars, "body": body })),
        )
        .map(|_| ())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_code_extracts_login_code() {
        let req = "GET /callback?code=abc-123_XYZ&foo=bar HTTP/1.1\r\nHost: x\r\n\r\n";
        assert_eq!(parse_code(req), Some("abc-123_XYZ".to_string()));
        assert_eq!(parse_code("GET /callback?state=x HTTP/1.1\r\n\r\n"), None);
    }

    #[test]
    fn entitlement_token_verifies_and_rejects_tamper_and_expiry() {
        use ed25519_dalek::{Signer, SigningKey};
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let vk = sk.verifying_key();

        // Build a token exactly the way the server's sign_entitlement does.
        let sign = |payload: serde_json::Value| {
            let header = B64.encode(br#"{"alg":"EdDSA","typ":"JWT"}"#);
            let p = B64.encode(serde_json::to_vec(&payload).unwrap());
            let input = format!("{header}.{p}");
            let sig = sk.sign(input.as_bytes());
            format!("{input}.{}", B64.encode(sig.to_bytes()))
        };

        let good = sign(json!({
            "sub": "u", "did": "d", "skus": ["com.x.glow"],
            "iat": 0, "exp": now() + 1000, "ent_ver": 2
        }));
        assert_eq!(
            verify_ent_token_with(&vk, &good).unwrap().skus,
            vec!["com.x.glow".to_string()]
        );

        // Tampered signing input → rejected.
        let mut chars: Vec<char> = good.chars().collect();
        chars[10] = if chars[10] == 'A' { 'B' } else { 'A' };
        let bad: String = chars.into_iter().collect();
        assert!(verify_ent_token_with(&vk, &bad).is_none());

        // Past the offline grace window → rejected.
        let expired = sign(json!({ "skus": ["x"], "exp": now() - 10, "ent_ver": 1 }));
        assert!(verify_ent_token_with(&vk, &expired).is_none());
    }

    #[test]
    fn encrypted_file_store_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.enc");
        let mut map = HashMap::new();
        map.insert("refresh".to_string(), "secret-token".to_string());
        save_file_map(&path, &map).unwrap();
        // On-disk bytes are not the plaintext.
        let raw = std::fs::read(&path).unwrap();
        assert!(!raw.windows(12).any(|w| w == b"secret-token"));
        let back = load_file_map(&path);
        assert_eq!(back.get("refresh").map(String::as_str), Some("secret-token"));
    }
}
