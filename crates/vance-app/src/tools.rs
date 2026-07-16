//! Managed AI-tool directory + installer ("Chrome-style" bootstrap).
//!
//! The heavy, optional AI helpers (whisper / demucs / background-remover /
//! stable-diffusion) are too large and license-encumbered to bundle in the
//! app binary. Instead Vance keeps a MANAGED tools directory under its config
//! dir and looks there FIRST (before `PATH`), so a downloaded/installed tool
//! "just works" with zero user setup. A small manifest-driven downloader
//! fetches a tool + verifies its checksum into that directory.
//!
//! The download SOURCE is not baked in: the manifest is fetched from
//! `VANCE_TOOLS_MANIFEST_URL` (a JSON the maintainer hosts) — that host + the
//! artifacts + model licensing are the deployment decision. Everything else
//! (resolution, verify, placement, catalog, status) is fully local.

use std::path::PathBuf;
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The managed tools root: `…/Vance/tools`.
pub fn tools_dir() -> std::io::Result<PathBuf> {
    let dir = crate::settings::config_dir()?.join("tools");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
/// Managed helper binaries live here (checked before `PATH`).
pub fn bin_dir() -> std::io::Result<PathBuf> {
    let dir = tools_dir()?.join("bin");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
/// Managed model files (weights) live here.
pub fn models_dir() -> std::io::Result<PathBuf> {
    let dir = tools_dir()?.join("models");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Vance's managed Python virtual-env — where the pip-installed AI tools
/// (whisper / demucs / backgroundremover) live so they run with zero PATH
/// setup. `…/Vance/tools/venv`.
pub fn venv_dir() -> std::io::Result<PathBuf> {
    Ok(tools_dir()?.join("venv"))
}
/// The venv's console-scripts dir (`bin` on unix, `Scripts` on Windows).
pub fn venv_bin_dir() -> std::io::Result<PathBuf> {
    let sub = if cfg!(windows) { "Scripts" } else { "bin" };
    Ok(venv_dir()?.join(sub))
}
/// The venv's own Python interpreter, if the venv has been created.
fn venv_python() -> Option<PathBuf> {
    let name = if cfg!(windows) { "python.exe" } else { "python" };
    let p = venv_bin_dir().ok()?.join(name);
    p.is_file().then_some(p)
}

/// Resolve an executable by name, preferring Vance's managed locations so an
/// installed tool wins with no PATH setup: the managed `bin/` dir, then the
/// managed Python venv, then finally the system `PATH`.
pub fn resolve(name: &str) -> Option<PathBuf> {
    for ext in exe_exts() {
        if let Ok(bin) = bin_dir() {
            let candidate = bin.join(format!("{name}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        if let Ok(vbin) = venv_bin_dir() {
            let candidate = vbin.join(format!("{name}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    which_on_path(name)
}

/// A Python 3 interpreter to bootstrap the managed venv from, if one exists on
/// `PATH` (`python3`, then `python`).
pub fn system_python() -> Option<PathBuf> {
    which_on_path("python3").or_else(|| which_on_path("python"))
}

/// Whether a system Python is available to install the pip-based tools.
pub fn python_available() -> bool {
    system_python().is_some()
}

/// The stable-diffusion model path: `VANCE_SD_MODEL` if set, else a managed
/// default (`…/tools/models/sd-model.gguf`) if it exists.
pub fn sd_model() -> Option<PathBuf> {
    if let Some(m) = std::env::var_os("VANCE_SD_MODEL") {
        return Some(PathBuf::from(m));
    }
    let managed = models_dir().ok()?.join("sd-model.gguf");
    managed.is_file().then_some(managed)
}

// ---------------------------------------------------------------------
// Catalog + status
// ---------------------------------------------------------------------

/// A helper tool Vance can drive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tool {
    /// Speech-to-text (captions) — `whisper` / whisper.cpp.
    Whisper,
    /// Voice isolation — `demucs`.
    Demucs,
    /// Subject mask — `backgroundremover`.
    BackgroundRemover,
    /// Generative images — `sd` (stable-diffusion.cpp) + a model.
    StableDiffusion,
}

impl Tool {
    /// Stable id used in the manifest + IPC.
    pub fn id(self) -> &'static str {
        match self {
            Tool::Whisper => "whisper",
            Tool::Demucs => "demucs",
            Tool::BackgroundRemover => "backgroundremover",
            Tool::StableDiffusion => "sd",
        }
    }
    /// The executable name to resolve.
    pub fn bin_name(self) -> &'static str {
        match self {
            Tool::Whisper => "whisper",
            Tool::Demucs => "demucs",
            Tool::BackgroundRemover => "backgroundremover",
            Tool::StableDiffusion => "sd",
        }
    }
    /// Every catalog tool.
    pub fn all() -> [Tool; 4] {
        [
            Tool::Whisper,
            Tool::Demucs,
            Tool::BackgroundRemover,
            Tool::StableDiffusion,
        ]
    }
    /// Parse from an id.
    pub fn from_id(id: &str) -> Option<Tool> {
        Tool::all().into_iter().find(|t| t.id() == id)
    }
    /// The PyPI package that provides this tool's CLI, if it installs via pip
    /// into Vance's managed venv (no hosting needed — PyPI serves it). `None`
    /// for tools that are a native binary + model download instead.
    pub fn pip_package(self) -> Option<&'static str> {
        match self {
            Tool::Whisper => Some("openai-whisper"),
            Tool::Demucs => Some("demucs"),
            Tool::BackgroundRemover => Some("backgroundremover"),
            Tool::StableDiffusion => None,
        }
    }
    /// How Vance installs this tool: `"pip"` (into the managed venv) or
    /// `"download"` (native binary/model via the manifest).
    pub fn install_method(self) -> &'static str {
        if self.pip_package().is_some() {
            "pip"
        } else {
            "download"
        }
    }
    /// A short human label for the UI.
    pub fn label(self) -> &'static str {
        match self {
            Tool::Whisper => "Speech-to-text (captions)",
            Tool::Demucs => "Voice isolation",
            Tool::BackgroundRemover => "Subject mask",
            Tool::StableDiffusion => "Generative images",
        }
    }
}

/// The install/availability state of one tool.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolStatus {
    /// Tool id.
    pub id: String,
    /// Human label for the UI.
    pub label: String,
    /// How Vance installs it: `"pip"` or `"download"`.
    pub method: String,
    /// Whether Vance can find the executable (managed dir or PATH).
    pub installed: bool,
    /// True when it lives in Vance's managed dir/venv (vs. the system PATH).
    pub managed: bool,
    /// Whether a required model is present (only meaningful for `sd`).
    pub model_ready: bool,
    /// Whether Vance can install it right now (pip tools need Python;
    /// download tools need a manifest URL).
    pub installable: bool,
    /// Resolved executable path, when found.
    pub path: Option<String>,
}

/// Status of every catalog tool.
pub fn status() -> Vec<ToolStatus> {
    let managed_root = tools_dir().ok();
    let has_python = python_available();
    let has_manifest = std::env::var_os("VANCE_TOOLS_MANIFEST_URL").is_some();
    Tool::all()
        .into_iter()
        .map(|t| {
            let path = resolve(t.bin_name());
            let managed = match (&path, &managed_root) {
                (Some(p), Some(root)) => p.starts_with(root),
                _ => false,
            };
            let model_ready = match t {
                Tool::StableDiffusion => sd_model().is_some(),
                _ => true,
            };
            let installable = match t.pip_package() {
                Some(_) => has_python,
                None => has_manifest,
            };
            ToolStatus {
                id: t.id().into(),
                label: t.label().into(),
                method: t.install_method().into(),
                installed: path.is_some(),
                managed,
                model_ready,
                installable,
                path: path.map(|p| p.to_string_lossy().into_owned()),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------
// Manifest-driven installer
// ---------------------------------------------------------------------

/// Errors from the tool installer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum InstallError {
    /// No `VANCE_TOOLS_MANIFEST_URL` configured — there's no download source.
    #[error(
        "no tool download source configured. Set VANCE_TOOLS_MANIFEST_URL to a \
         JSON manifest URL (the maintainer hosts the binaries/models), or install \
         the tool yourself. Vance finds a tool placed in its managed tools dir."
    )]
    NoSource,
    /// The tool id isn't in the manifest.
    #[error("no download for '{0}' on this platform in the manifest")]
    NotInManifest(String),
    /// A network / HTTP failure.
    #[error("download failed: {0}")]
    Http(String),
    /// The downloaded bytes didn't match the manifest checksum.
    #[error("checksum mismatch (got {got}, expected {want}) — refusing to install")]
    Checksum {
        /// Computed sha256.
        got: String,
        /// Manifest sha256.
        want: String,
    },
    /// No system Python 3 to bootstrap the managed venv from.
    #[error(
        "Python 3 is required to install this tool but was not found. Install \
         Python 3 (python.org or your OS package manager), then retry."
    )]
    NoPython,
    /// `pip` / `venv` ran but failed.
    #[error("pip install failed (exit {code:?}): {stderr}")]
    Pip {
        /// Process exit code.
        code: Option<i32>,
        /// Captured stderr (truncated).
        stderr: String,
    },
    /// The install reported success but the expected executable wasn't found.
    #[error("'{0}' installed but its executable wasn't found afterward")]
    MissingAfterInstall(String),
    /// A filesystem failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// One downloadable artifact for a (tool, platform).
#[derive(Clone, Debug, Deserialize)]
pub struct Artifact {
    /// Download URL.
    pub url: String,
    /// Lowercase hex sha256 of the downloaded bytes.
    pub sha256: String,
    /// Where to place it relative to the managed tools dir (e.g.
    /// `bin/sd`, `models/sd-model.gguf`). Executable bit is set for `bin/*`.
    pub dest: String,
}

/// The manifest: `{ "<tool_id>": { "<platform>": Artifact, ... }, ... }`.
/// Platform keys are `os-arch`, e.g. `macos-aarch64`, `linux-x86_64`,
/// `windows-x86_64`.
pub type Manifest = std::collections::BTreeMap<String, std::collections::BTreeMap<String, Artifact>>;

/// The current platform key used to select a manifest artifact.
pub fn platform_key() -> String {
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    };
    format!("{os}-{arch}")
}

/// Fetch + parse the tool manifest from `VANCE_TOOLS_MANIFEST_URL`.
pub fn fetch_manifest() -> Result<Manifest, InstallError> {
    let url = std::env::var("VANCE_TOOLS_MANIFEST_URL").map_err(|_| InstallError::NoSource)?;
    let client = http_client()?;
    let resp = client.get(&url).send().map_err(|e| InstallError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(InstallError::Http(format!("manifest {}", resp.status())));
    }
    resp.json::<Manifest>().map_err(|e| InstallError::Http(e.to_string()))
}

/// Install `tool` into Vance's managed tools dir, picking the right method:
/// pip-into-managed-venv for the Python tools (no hosting needed), or a
/// manifest download for native-binary tools. Blocking + slow (venv build,
/// large downloads) — run OFF the engine lock. Returns the installed path.
pub fn install(tool: Tool) -> Result<PathBuf, InstallError> {
    match tool.pip_package() {
        Some(package) => install_via_pip(tool, package),
        None => install_via_manifest(tool),
    }
}

/// Install a pip tool into Vance's managed venv, creating the venv on first
/// use. Uses a system Python 3 only to bootstrap the venv; the tool itself
/// then lives entirely under `…/Vance/tools/venv` and resolves with no PATH
/// setup.
fn install_via_pip(tool: Tool, package: &str) -> Result<PathBuf, InstallError> {
    // Bootstrap the managed venv if it doesn't exist yet.
    let vpython = match venv_python() {
        Some(p) => p,
        None => {
            let sys = system_python().ok_or(InstallError::NoPython)?;
            let venv = venv_dir()?;
            let out = Command::new(&sys)
                .arg("-m")
                .arg("venv")
                .arg(&venv)
                .output()?;
            if !out.status.success() {
                return Err(pip_err("creating venv", &out));
            }
            venv_python().ok_or(InstallError::NoPython)?
        }
    };
    // Keep pip itself current, then install the package. `--upgrade` makes a
    // re-install idempotent (and pulls a newer tool if one shipped).
    let up = Command::new(&vpython)
        .args(["-m", "pip", "install", "--upgrade", "pip"])
        .output()?;
    if !up.status.success() {
        return Err(pip_err("upgrading pip", &up));
    }
    let out = Command::new(&vpython)
        .args(["-m", "pip", "install", "--upgrade", package])
        .output()?;
    if !out.status.success() {
        return Err(pip_err(&format!("installing {package}"), &out));
    }
    resolve(tool.bin_name()).ok_or_else(|| InstallError::MissingAfterInstall(tool.id().into()))
}

/// Build a `Pip` error from a failed process, prefixing the phase.
fn pip_err(phase: &str, out: &std::process::Output) -> InstallError {
    let mut stderr: String = String::from_utf8_lossy(&out.stderr).chars().take(2000).collect();
    if stderr.trim().is_empty() {
        stderr = String::from_utf8_lossy(&out.stdout).chars().take(2000).collect();
    }
    InstallError::Pip {
        code: out.status.code(),
        stderr: format!("{phase}: {}", stderr.trim()),
    }
}

/// Download + verify + place the native-binary/model artifact for `tool` on
/// this platform into the managed tools dir. Returns the installed path.
fn install_via_manifest(tool: Tool) -> Result<PathBuf, InstallError> {
    let manifest = fetch_manifest()?;
    let entry = manifest
        .get(tool.id())
        .and_then(|p| p.get(&platform_key()))
        .ok_or_else(|| InstallError::NotInManifest(tool.id().into()))?;
    let client = http_client()?;
    let resp = client
        .get(&entry.url)
        .send()
        .map_err(|e| InstallError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(InstallError::Http(format!("{} {}", entry.url, resp.status())));
    }
    let bytes = resp.bytes().map_err(|e| InstallError::Http(e.to_string()))?;
    verify_sha256(&bytes, &entry.sha256)?;

    // Place under the managed tools dir; only `bin/` and `models/` are allowed
    // dests (guards against a manifest writing outside the sandbox).
    let dest = safe_dest(&entry.dest)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = dest.with_extension("part");
    std::fs::write(&tmp, &bytes)?;
    #[cfg(unix)]
    if entry.dest.starts_with("bin/") {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

/// Resolve a manifest `dest` under the managed tools dir, rejecting anything
/// that escapes `bin/` or `models/`.
fn safe_dest(dest: &str) -> Result<PathBuf, InstallError> {
    let rel = dest.trim_start_matches('/');
    if rel.contains("..") || !(rel.starts_with("bin/") || rel.starts_with("models/")) {
        return Err(InstallError::Http(format!("unsafe manifest dest: {dest}")));
    }
    Ok(tools_dir()?.join(rel))
}

fn verify_sha256(bytes: &[u8], want: &str) -> Result<(), InstallError> {
    let mut h = Sha256::new();
    h.update(bytes);
    let got = h.finalize();
    let got_hex: String = got.iter().map(|b| format!("{b:02x}")).collect();
    if got_hex.eq_ignore_ascii_case(want) {
        Ok(())
    } else {
        Err(InstallError::Checksum {
            got: got_hex,
            want: want.to_string(),
        })
    }
}

fn http_client() -> Result<reqwest::blocking::Client, InstallError> {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(1800)) // large model downloads
        .build()
        .map_err(|e| InstallError::Http(e.to_string()))
}

// ---------------------------------------------------------------------
// PATH resolution (shared)
// ---------------------------------------------------------------------

fn exe_exts() -> Vec<String> {
    if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
            .split(';')
            .map(|s| s.to_string())
            .collect()
    } else {
        vec![String::new()]
    }
}

fn which_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for ext in exe_exts() {
            let candidate = dir.join(format!("{name}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_key_is_os_arch() {
        let k = platform_key();
        assert!(k.contains('-'));
        assert!(k.starts_with("macos") || k.starts_with("linux") || k.starts_with("windows"));
    }

    #[test]
    fn tool_ids_round_trip() {
        for t in Tool::all() {
            assert_eq!(Tool::from_id(t.id()), Some(t));
        }
        assert_eq!(Tool::from_id("nope"), None);
    }

    #[test]
    fn safe_dest_rejects_escapes() {
        assert!(safe_dest("bin/sd").is_ok());
        assert!(safe_dest("models/sd-model.gguf").is_ok());
        assert!(safe_dest("../evil").is_err());
        assert!(safe_dest("bin/../../etc/passwd").is_err());
        assert!(safe_dest("/etc/passwd").is_err());
        assert!(safe_dest("elsewhere/x").is_err());
    }

    #[test]
    fn checksum_verify() {
        // sha256("") = e3b0c442...
        let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert!(verify_sha256(b"", empty).is_ok());
        assert!(verify_sha256(b"x", empty).is_err());
    }

    #[test]
    fn no_manifest_url_is_no_source() {
        // Only meaningful when the env var is unset in the test env.
        if std::env::var_os("VANCE_TOOLS_MANIFEST_URL").is_none() {
            assert!(matches!(fetch_manifest(), Err(InstallError::NoSource)));
        }
    }

    #[test]
    fn status_lists_all_tools() {
        let s = status();
        assert_eq!(s.len(), 4);
        for id in ["whisper", "demucs", "backgroundremover", "sd"] {
            assert!(s.iter().any(|t| t.id == id));
        }
    }

    #[test]
    fn install_method_matches_pip_package() {
        // The three Python tools install via pip; SD is a native download.
        assert_eq!(Tool::Whisper.install_method(), "pip");
        assert_eq!(Tool::Demucs.install_method(), "pip");
        assert_eq!(Tool::BackgroundRemover.install_method(), "pip");
        assert_eq!(Tool::StableDiffusion.install_method(), "download");
        assert_eq!(Tool::StableDiffusion.pip_package(), None);
        assert!(Tool::Whisper.pip_package().is_some());
    }

    #[test]
    fn venv_bin_dir_is_under_tools() {
        // The managed venv bin lives inside the tools dir (sandbox invariant).
        if let (Ok(root), Ok(vbin)) = (tools_dir(), venv_bin_dir()) {
            assert!(vbin.starts_with(&root));
        }
    }
}
