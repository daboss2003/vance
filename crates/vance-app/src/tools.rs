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
    let name = if cfg!(windows) {
        "python.exe"
    } else {
        "python"
    };
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

/// Vance's own standalone Python (downloaded when the system has none), if
/// present. `…/Vance/tools/python/{bin/python3 | python.exe}`.
fn managed_python() -> Option<PathBuf> {
    let dir = tools_dir().ok()?.join("python");
    let p = if cfg!(windows) {
        dir.join("python.exe")
    } else {
        dir.join("bin").join("python3")
    };
    p.is_file().then_some(p)
}

/// Whether the pip-based tools can be installed: a Python exists (system or
/// managed) OR Vance can download a standalone one for this platform. So on
/// supported platforms the AI tools are installable with NO prerequisites.
pub fn python_available() -> bool {
    system_python().is_some()
        || managed_python().is_some()
        || PBS_PYTHON.iter().any(|(k, _, _)| *k == platform_key())
}

/// The stable-diffusion model path for the LEGACY `sd` (stable-diffusion.cpp)
/// runner: `VANCE_SD_MODEL` if set, else a managed default if it exists. The
/// default install now uses `diffusers` instead (see [`diffusers_runner`]).
pub fn sd_model() -> Option<PathBuf> {
    if let Some(m) = std::env::var_os("VANCE_SD_MODEL") {
        return Some(PathBuf::from(m));
    }
    let managed = models_dir().ok()?.join("sd-model.gguf");
    managed.is_file().then_some(managed)
}

/// The bundled `diffusers` image-generation wrapper (installed into the managed
/// tools dir alongside its venv). Vance runs it as `<venv-python> <script>`.
const IMAGEGEN_PY: &str = include_str!("../assets/vance_imagegen.py");

/// A commercial-use-OK default model (CreativeML OpenRAIL-M), overridable with
/// `VANCE_SD_MODEL` (a Hugging Face model id or a local diffusers path). This
/// is the public, ungated SD-1.5 mirror — Stability's own SD-2.x repos are now
/// gated (need an HF login), so they can't be a zero-config default.
pub const DEFAULT_SD_MODEL: &str = "stable-diffusion-v1-5/stable-diffusion-v1-5";

/// `…/Vance/tools/imagegen` — holds the diffusers wrapper script.
pub fn imagegen_dir() -> std::io::Result<PathBuf> {
    let d = tools_dir()?.join("imagegen");
    std::fs::create_dir_all(&d)?;
    Ok(d)
}
/// Path to the diffusers wrapper script.
pub fn imagegen_script() -> std::io::Result<PathBuf> {
    Ok(imagegen_dir()?.join("vance_imagegen.py"))
}

/// Write the current bundled wrapper to the managed imagegen dir (overwrite).
/// Called before each run so a tool installed by an OLDER build picks up the
/// latest wrapper (new flags, resource limits, progress) automatically.
pub fn sync_imagegen_script() -> std::io::Result<PathBuf> {
    let script = imagegen_script()?;
    std::fs::write(&script, IMAGEGEN_PY)?;
    Ok(script)
}

/// The installed diffusers image generator, if present: `(venv python, script)`.
/// `None` until the "Generative images" tool is installed.
pub fn diffusers_runner() -> Option<(PathBuf, PathBuf)> {
    let py = venv_python()?;
    let script = imagegen_script().ok()?;
    script.is_file().then_some((py, script))
}

/// The model the diffusers wrapper should load: `VANCE_SD_MODEL` (HF id or local
/// path) if set, else [`DEFAULT_SD_MODEL`].
pub fn diffusers_model() -> String {
    std::env::var("VANCE_SD_MODEL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_SD_MODEL.to_string())
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
    /// The PyPI package(s) this tool installs into Vance's managed venv (no
    /// hosting needed — PyPI serves them; the model comes from PyPI/HF on use).
    /// Empty for a tool that installs as a native binary via the manifest.
    pub fn pip_packages(self) -> &'static [&'static str] {
        match self {
            Tool::Whisper => &["openai-whisper"],
            Tool::Demucs => &["demucs"],
            Tool::BackgroundRemover => &["backgroundremover"],
            // Image generation via Hugging Face diffusers + a bundled wrapper.
            Tool::StableDiffusion => &[
                "torch",
                "diffusers",
                "transformers",
                "accelerate",
                "safetensors",
            ],
        }
    }
    /// How Vance installs this tool: `"pip"` (into the managed venv) or
    /// `"download"` (native binary/model via the manifest).
    pub fn install_method(self) -> &'static str {
        if self.pip_packages().is_empty() {
            "download"
        } else {
            "pip"
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
            // SD has no `bin` — it's the diffusers wrapper; everything else
            // resolves by executable name.
            let path = if t == Tool::StableDiffusion {
                diffusers_runner().map(|(_, script)| script)
            } else {
                resolve(t.bin_name())
            };
            let managed = match (&path, &managed_root) {
                (Some(p), Some(root)) => p.starts_with(root),
                _ => false,
            };
            // Installing SD prewarms its model, so "installed" ⇒ ready.
            let model_ready = path.is_some() || t != Tool::StableDiffusion;
            let installable = if t.pip_packages().is_empty() {
                has_manifest
            } else {
                has_python
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

/// The manifest: `{ "<tool_id>": { "<platform>": [Artifact, ...], ... }, ... }`.
/// Platform keys are `os-arch`, e.g. `macos-aarch64`, `linux-x86_64`,
/// `windows-x86_64`. Each (tool, platform) maps to a LIST of artifacts so a
/// tool that needs several files installs atomically — e.g. `sd` ships both its
/// `bin/sd` executable and its `models/sd-model.gguf` weights.
pub type Manifest =
    std::collections::BTreeMap<String, std::collections::BTreeMap<String, Vec<Artifact>>>;

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
    let resp = client
        .get(&url)
        .send()
        .map_err(|e| InstallError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(InstallError::Http(format!("manifest {}", resp.status())));
    }
    resp.json::<Manifest>()
        .map_err(|e| InstallError::Http(e.to_string()))
}

/// A live progress reporter: `(phase, human-readable detail, optional 0..100 %)`.
/// Phases: `"setup"` (venv build), `"pip"` (package install), `"download"`
/// (native binary / model), `"done"`.
pub type Progress<'a> = dyn FnMut(&str, &str, Option<f32>) + 'a;

/// Install `tool` into Vance's managed tools dir. Blocking + slow (venv build,
/// large downloads) — run OFF the engine lock. Returns the installed path.
pub fn install(tool: Tool) -> Result<PathBuf, InstallError> {
    install_with_progress(tool, &mut |_, _, _| {})
}

/// Like [`install`], but streams live progress (venv build, each pip output
/// line, download bytes) through `on` so the UI can show it instead of
/// appearing frozen for minutes.
pub fn install_with_progress(tool: Tool, on: &mut Progress) -> Result<PathBuf, InstallError> {
    if tool.pip_packages().is_empty() {
        install_via_manifest(tool, on)
    } else {
        install_via_pip(tool, on)
    }
}

/// Bootstrap the managed venv if needed and return its Python interpreter.
fn ensure_venv(on: &mut Progress) -> Result<PathBuf, InstallError> {
    if let Some(p) = venv_python() {
        return Ok(p);
    }
    let python = ensure_python(on)?;
    on("setup", "Creating a private Python environment…", None);
    let venv = venv_dir()?;
    let out = vance_ipc::process::output(Command::new(&python).arg("-m").arg("venv").arg(&venv))?;
    if !out.status.success() {
        return Err(pip_err("creating venv", &out));
    }
    venv_python().ok_or(InstallError::NoPython)
}

// python-build-standalone (Astral) — self-contained, relocatable CPython builds
// used to give the pip tools a Python when the system has none. Hosted on
// GitHub (upstream — no hosting on our side), pinned by sha256.
const PBS_TAG: &str = "20260807";
const PBS_VERSION: &str = "3.12.13";
/// `(platform_key, release-triple, sha256)` for the `install_only` tarball.
const PBS_PYTHON: &[(&str, &str, &str)] = &[
    (
        "macos-aarch64",
        "aarch64-apple-darwin",
        "4201588fc5051c2ba988abbe1f033d318965ee378fadf7fb7ef79882ba7be84b",
    ),
    (
        "macos-x86_64",
        "x86_64-apple-darwin",
        "ce9dc826a3215d5deadf6d7ba409a882b8d431192c4c06deb34ff00f93ceb4f5",
    ),
    (
        "linux-aarch64",
        "aarch64-unknown-linux-gnu",
        "e2a33a26bae0f0975a9786c2e3beaee9cfeb35f856bdd273ff10ae35cf7e06ce",
    ),
    (
        "linux-x86_64",
        "x86_64-unknown-linux-gnu",
        "5bd6f36fd7ef02b909234c94dca9994ef0da06ace3bc3cece4fe27870e9cdbbe",
    ),
    (
        "windows-x86_64",
        "x86_64-pc-windows-msvc",
        "6cf2be701aa7e9470454c9c86285c1bcc1832518d63e39c3e34e9d8ea1cbb99f",
    ),
];

/// A Python to bootstrap the venv from: the system one, else Vance's managed
/// standalone build, else download+extract a standalone build (zero prereqs).
fn ensure_python(on: &mut Progress) -> Result<PathBuf, InstallError> {
    if let Some(p) = system_python() {
        return Ok(p);
    }
    if let Some(p) = managed_python() {
        return Ok(p);
    }
    provision_python(on)
}

/// Download + verify + extract a standalone CPython into the managed tools dir
/// so the pip tools work with no system Python. Returns its interpreter path.
fn provision_python(on: &mut Progress) -> Result<PathBuf, InstallError> {
    let key = platform_key();
    let (_, triple, sha) = PBS_PYTHON
        .iter()
        .find(|(k, _, _)| *k == key)
        .ok_or_else(|| {
            InstallError::Http(format!(
                "no prebuilt Python available for this platform ({key}); \
                 install Python 3 and retry"
            ))
        })?;
    let url = format!(
        "https://github.com/astral-sh/python-build-standalone/releases/download/\
         {PBS_TAG}/cpython-{PBS_VERSION}+{PBS_TAG}-{triple}-install_only.tar.gz"
    );
    let tools = tools_dir()?;
    let tgz = tools.join("python-download.tar.gz");
    on(
        "setup",
        &format!("Downloading a private Python {PBS_VERSION} runtime…"),
        Some(0.0),
    );
    download_to_file(&url, sha, &tgz, "Python runtime", "setup", on)?;

    on("setup", "Unpacking Python…", None);
    // `tar` handles .tar.gz on macOS/Linux and Windows 10+ (bsdtar). The archive
    // extracts to a top-level `python/` dir.
    let out = vance_ipc::process::output(
        Command::new("tar")
            .arg("-xzf")
            .arg(&tgz)
            .arg("-C")
            .arg(&tools),
    )?;
    let _ = std::fs::remove_file(&tgz);
    if !out.status.success() {
        return Err(InstallError::Pip {
            code: out.status.code(),
            stderr: format!(
                "unpacking Python: {}",
                String::from_utf8_lossy(&out.stderr)
                    .chars()
                    .take(500)
                    .collect::<String>()
            ),
        });
    }
    managed_python().ok_or_else(|| InstallError::MissingAfterInstall("python".into()))
}

/// Stream a URL to `dest`, hashing as it goes, and verify the sha256. Emits byte
/// progress under `phase`. Deletes `dest` on checksum mismatch.
fn download_to_file(
    url: &str,
    sha256_want: &str,
    dest: &std::path::Path,
    label: &str,
    phase: &str,
    on: &mut Progress,
) -> Result<(), InstallError> {
    use std::io::{Read, Write};
    let client = http_client()?;
    let mut resp = client
        .get(url)
        .send()
        .map_err(|e| InstallError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(InstallError::Http(format!("{url} {}", resp.status())));
    }
    let total = resp.content_length();
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::File::create(dest)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 65536];
    let mut done: u64 = 0;
    let mut last_emit: u64 = 0;
    loop {
        let read = resp
            .read(&mut buf)
            .map_err(|e| InstallError::Http(e.to_string()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        file.write_all(&buf[..read])?;
        done += read as u64;
        if done - last_emit >= 1_048_576 {
            last_emit = done;
            let pct = total.map(|t| {
                if t > 0 {
                    (done as f32 / t as f32 * 100.0).min(100.0)
                } else {
                    0.0
                }
            });
            let detail = match total {
                Some(t) => format!(
                    "Downloading {label} — {} / {}",
                    human_bytes(done),
                    human_bytes(t)
                ),
                None => format!("Downloading {label} — {}", human_bytes(done)),
            };
            on(phase, &detail, pct);
        }
    }
    file.flush()?;
    drop(file);
    let got_hex: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if !got_hex.eq_ignore_ascii_case(sha256_want) {
        let _ = std::fs::remove_file(dest);
        return Err(InstallError::Checksum {
            got: got_hex,
            want: sha256_want.to_string(),
        });
    }
    Ok(())
}

/// Install a pip tool into Vance's managed venv, creating the venv on first
/// use. Uses a system Python 3 only to bootstrap the venv; the tool itself
/// then lives entirely under `…/Vance/tools/venv` and resolves with no PATH
/// setup.
fn install_via_pip(tool: Tool, on: &mut Progress) -> Result<PathBuf, InstallError> {
    let vpython = ensure_venv(on)?;
    let packages = tool.pip_packages();

    // Keep pip itself current, then install the package(s). `--upgrade` makes a
    // re-install idempotent (and pulls a newer tool if one shipped). `--retries`
    // rides out transient network blips within a run; pip's wheel cache + the
    // venv persist across runs, so a re-click after an interruption RESUMES —
    // already-fetched wheels are reused rather than re-downloaded.
    const NET: [&str; 4] = ["--retries", "10", "--timeout", "60"];
    on("pip", "Updating the package installer (pip)…", None);
    let mut up = Command::new(&vpython);
    up.args(["-m", "pip", "install", "--upgrade"])
        .args(NET)
        .arg("pip");
    run_streaming(up, "pip", "upgrading pip", on)?;

    let joined = packages.join(" ");
    on(
        "pip",
        &format!("Installing {joined} (this can take a while)…"),
        None,
    );
    let mut inst = Command::new(&vpython);
    inst.args(["-m", "pip", "install", "--upgrade"])
        .args(NET)
        .args(packages);
    run_streaming(inst, "pip", &format!("installing {joined}"), on)?;

    // Image generation is a bundled diffusers wrapper (no CLI on PyPI): drop the
    // script and prewarm the model so the first generation isn't a big wait.
    if tool == Tool::StableDiffusion {
        return finish_diffusers(&vpython, on);
    }
    resolve(tool.bin_name()).ok_or_else(|| InstallError::MissingAfterInstall(tool.id().into()))
}

/// Write the diffusers wrapper into the managed tools dir and prewarm (download
/// + validate) the model. Returns the wrapper script path.
fn finish_diffusers(vpython: &std::path::Path, on: &mut Progress) -> Result<PathBuf, InstallError> {
    // The script must exist to be run for the prewarm; but if the prewarm fails
    // (bad model id, network) we REMOVE it again so the tool isn't left looking
    // installed — status()/diffusers_runner() key off the script's presence, and
    // a clean "not installed" lets the user simply re-click Install (which
    // resumes: torch/diffusers are cached, the model download resumes).
    let script = sync_imagegen_script()?;
    let model = diffusers_model();
    on(
        "download",
        &format!("Downloading the image model ({model})…"),
        Some(0.0),
    );
    let mut cmd = Command::new(vpython);
    cmd.arg(&script)
        .arg("--prewarm")
        .arg("--watch-parent")
        .arg("--model")
        .arg(&model);
    if let Err(e) = run_streaming(cmd, "download", "downloading the image model", on) {
        let _ = std::fs::remove_file(&script);
        return Err(e);
    }
    Ok(script)
}

/// Spawn a child, stream its stdout+stderr line-by-line to `on`, and turn a
/// non-zero exit into a `Pip` error carrying the tail of its output.
fn run_streaming(
    mut cmd: Command,
    phase: &str,
    what: &str,
    on: &mut Progress,
) -> Result<(), InstallError> {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    // Pipe stdin and DON'T take it: any child that watches its stdin (our
    // imagegen wrapper) then sees EOF the instant Vance dies (even on
    // force-quit), so long jobs can't outlive the app orphaned.
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = vance_ipc::process::spawn(&mut cmd)?;
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        let tx = tx.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        }));
    }
    if let Some(err) = child.stderr.take() {
        let tx = tx.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        }));
    }
    drop(tx); // rx closes once both reader threads finish
    let mut tail: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    for line in rx {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        on(phase, t, None);
        tail.push_back(t.to_string());
        if tail.len() > 40 {
            tail.pop_front();
        }
    }
    for h in readers {
        let _ = h.join();
    }
    let status = child.wait()?;
    if !status.success() {
        let joined: Vec<String> = tail.into_iter().collect();
        return Err(InstallError::Pip {
            code: status.code(),
            stderr: format!("{what}: {}", joined.join("\n")),
        });
    }
    Ok(())
}

/// Build a `Pip` error from a failed process, prefixing the phase.
fn pip_err(phase: &str, out: &std::process::Output) -> InstallError {
    let mut stderr: String = String::from_utf8_lossy(&out.stderr)
        .chars()
        .take(2000)
        .collect();
    if stderr.trim().is_empty() {
        stderr = String::from_utf8_lossy(&out.stdout)
            .chars()
            .take(2000)
            .collect();
    }
    InstallError::Pip {
        code: out.status.code(),
        stderr: format!("{phase}: {}", stderr.trim()),
    }
}

/// Download + verify + place every native-binary/model artifact for `tool` on
/// this platform into the managed tools dir. Returns the installed executable
/// path (a tool with only model artifacts resolves via its bin still).
fn install_via_manifest(tool: Tool, on: &mut Progress) -> Result<PathBuf, InstallError> {
    let manifest = fetch_manifest()?;
    let artifacts = manifest
        .get(tool.id())
        .and_then(|p| p.get(&platform_key()))
        .filter(|a| !a.is_empty())
        .ok_or_else(|| InstallError::NotInManifest(tool.id().into()))?;
    let client = http_client()?;
    let n = artifacts.len();
    for (i, art) in artifacts.iter().enumerate() {
        let name = art.dest.rsplit('/').next().unwrap_or(&art.dest).to_string();
        on(
            "download",
            &format!("Downloading {name} ({}/{n})…", i + 1),
            Some(0.0),
        );
        place_artifact(&client, art, &name, on)?;
    }
    // Validate via the SAME resolve() the rest of the app uses, so a manifest
    // whose bin dest doesn't match bin_name() fails loudly rather than reporting
    // a false success the app then can't act on.
    resolve(tool.bin_name()).ok_or_else(|| InstallError::MissingAfterInstall(tool.id().into()))
}

/// Stream-download one artifact (hashing as it goes so a multi-GB file never
/// buffers in memory), verify its sha256, and atomically place it under the
/// (sandboxed) managed tools dir. Emits byte progress through `on`.
fn place_artifact(
    client: &reqwest::blocking::Client,
    art: &Artifact,
    name: &str,
    on: &mut Progress,
) -> Result<PathBuf, InstallError> {
    use std::io::{Read, Write};
    let mut resp = client
        .get(&art.url)
        .send()
        .map_err(|e| InstallError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(InstallError::Http(format!("{} {}", art.url, resp.status())));
    }
    let total = resp.content_length();

    // Only `bin/` and `models/` are allowed dests (guards against a manifest
    // writing outside the sandbox).
    let dest = safe_dest(&art.dest)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = dest.with_extension("part");
    let mut file = std::fs::File::create(&tmp)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 65536];
    let mut done: u64 = 0;
    let mut last_emit: u64 = 0;
    loop {
        let read = resp
            .read(&mut buf)
            .map_err(|e| InstallError::Http(e.to_string()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        file.write_all(&buf[..read])?;
        done += read as u64;
        if done - last_emit >= 1_048_576 {
            last_emit = done;
            let pct = total.map(|t| {
                if t > 0 {
                    (done as f32 / t as f32 * 100.0).min(100.0)
                } else {
                    0.0
                }
            });
            let detail = match total {
                Some(t) => format!(
                    "Downloading {name} — {} / {}",
                    human_bytes(done),
                    human_bytes(t)
                ),
                None => format!("Downloading {name} — {}", human_bytes(done)),
            };
            on("download", &detail, pct);
        }
    }
    file.flush()?;
    drop(file);

    let got_hex: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if !got_hex.eq_ignore_ascii_case(&art.sha256) {
        let _ = std::fs::remove_file(&tmp);
        return Err(InstallError::Checksum {
            got: got_hex,
            want: art.sha256.clone(),
        });
    }
    #[cfg(unix)]
    if art.dest.starts_with("bin/") {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

/// Human-readable byte count for progress (e.g. `842.0 MB`).
fn human_bytes(n: u64) -> String {
    const U: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
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

fn http_client() -> Result<reqwest::blocking::Client, InstallError> {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(1800)) // large model downloads
        .build()
        .map_err(|e| InstallError::Http(e.to_string()))
}

// ---------------------------------------------------------------------
// Headless CLI (installer postinstall hooks + power users)
// ---------------------------------------------------------------------

/// Handle the headless AI-tool subcommands. This is the single primitive every
/// OS installer's postinstall step drives (`vance-app --install-ai-tools all`),
/// so a native yes/no prompt in the installer maps to the exact same
/// `tools::install` path the in-app "AI tools" panel uses.
///
/// Returns `Some(exit_code)` when it recognized and handled a subcommand — the
/// caller MUST then exit without starting the GUI — or `None` to fall through
/// to the normal window launch (so a plain double-click / OS-passed args still
/// open the editor).
///
/// Recognized flags (scanned anywhere in argv):
/// * `--list-ai-tools`       — print the catalog + install method, exit 0.
/// * `--ai-tools-status`     — print status as JSON, exit 0.
/// * `--install-ai-tools [all|id,id,…]` — install the given tools (default
///   `all`); exit 0 only if every requested tool ends up ready.
pub fn handle_cli(args: &[String]) -> Option<i32> {
    let rest = args.get(1..).unwrap_or(&[]);
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--list-ai-tools" => {
                print_catalog();
                return Some(0);
            }
            "--ai-tools-status" => {
                match serde_json::to_string_pretty(&status()) {
                    Ok(j) => println!("{j}"),
                    Err(e) => {
                        eprintln!("vance: serializing status: {e}");
                        return Some(1);
                    }
                }
                return Some(0);
            }
            "--install-ai-tools" => {
                // The optional value is the next arg unless it's another flag.
                let spec = rest
                    .get(i + 1)
                    .map(|s| s.as_str())
                    .filter(|s| !s.starts_with('-'))
                    .unwrap_or("all");
                return Some(install_cli(spec));
            }
            _ => i += 1,
        }
    }
    None
}

/// Parse an `--install-ai-tools` selector into the concrete tool list.
/// `all` → every catalog tool; otherwise a comma/space separated list of ids.
fn parse_selector(spec: &str) -> Result<Vec<Tool>, String> {
    let spec = spec.trim();
    if spec.is_empty() || spec.eq_ignore_ascii_case("all") {
        return Ok(Tool::all().to_vec());
    }
    let mut out = Vec::new();
    for id in spec.split([',', ' ']).filter(|s| !s.is_empty()) {
        match Tool::from_id(id) {
            Some(t) if !out.contains(&t) => out.push(t),
            Some(_) => {}
            None => {
                return Err(format!(
                    "unknown AI tool '{id}' (known: whisper, demucs, backgroundremover, sd)"
                ))
            }
        }
    }
    // A non-empty, non-"all" spec that resolves to nothing (e.g. "," or ", ,")
    // is a mistake — don't silently report success installing zero tools.
    if out.is_empty() {
        return Err(format!("no AI tools in selector '{spec}'"));
    }
    Ok(out)
}

fn print_catalog() {
    println!("Optional AI tools:");
    for t in Tool::all() {
        println!("  {:<18} {:<8} {}", t.id(), t.install_method(), t.label());
    }
    println!(
        "\nInstall:  vance-app --install-ai-tools all\n\
         Or a subset:  vance-app --install-ai-tools whisper,demucs\n\
         Status (JSON):  vance-app --ai-tools-status"
    );
}

/// Run the headless install for a selector, printing progress. Returns a
/// process exit code (0 = every requested tool ready, 1 = a failure).
fn install_cli(spec: &str) -> i32 {
    let tools = match parse_selector(spec) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("vance: {e}");
            return 2;
        }
    };
    // Fail fast with an actionable message if a pip tool is requested but no
    // system Python exists to bootstrap the managed venv.
    let needs_python = tools.iter().any(|t| !t.pip_packages().is_empty());
    if needs_python && !python_available() {
        eprintln!(
            "vance: Python 3 is required to install the pip-based tools \
             (whisper / demucs / backgroundremover) but was not found on PATH. \
             Install Python 3, then rerun."
        );
        // Don't hard-abort: a caller asking for `all` may still get `sd`.
    }
    let mut failures = 0;
    for t in &tools {
        println!(
            "vance: installing {} ({}, via {})…",
            t.id(),
            t.label(),
            t.install_method()
        );
        match install(*t) {
            Ok(path) => println!("vance:   ✓ {} → {}", t.id(), path.display()),
            Err(e) => {
                eprintln!("vance:   ✗ {} failed: {e}", t.id());
                failures += 1;
            }
        }
    }
    if failures == 0 {
        println!("vance: done — {} tool(s) ready.", tools.len());
        0
    } else {
        eprintln!("vance: {failures} tool(s) failed to install.");
        1
    }
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
        // sha256("") = e3b0c442… — mirrors the hex compare in place_artifact.
        let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let hex = |b: &[u8]| {
            let mut h = Sha256::new();
            h.update(b);
            h.finalize()
                .iter()
                .map(|x| format!("{x:02x}"))
                .collect::<String>()
        };
        assert!(hex(b"").eq_ignore_ascii_case(empty));
        assert!(!hex(b"x").eq_ignore_ascii_case(empty));
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
    fn install_method_matches_pip_packages() {
        // All four now install via pip into the managed venv (SD = diffusers).
        for t in Tool::all() {
            assert_eq!(t.install_method(), "pip");
            assert!(!t.pip_packages().is_empty());
        }
        assert!(Tool::StableDiffusion.pip_packages().contains(&"diffusers"));
        assert!(Tool::Whisper.pip_packages().contains(&"openai-whisper"));
    }

    #[test]
    fn run_streaming_captures_stdout_and_stderr() {
        if !cfg!(unix) {
            return;
        }
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("echo out-line; echo err-line 1>&2; exit 0");
        let mut lines: Vec<String> = Vec::new();
        let mut on = |_p: &str, detail: &str, _pct: Option<f32>| lines.push(detail.to_string());
        assert!(run_streaming(cmd, "pip", "test", &mut on).is_ok());
        assert!(
            lines.iter().any(|l| l == "out-line"),
            "missing stdout: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l == "err-line"),
            "missing stderr: {lines:?}"
        );
    }

    #[test]
    fn run_streaming_maps_nonzero_exit_to_error() {
        if !cfg!(unix) {
            return;
        }
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("echo boom 1>&2; exit 3");
        let mut on = |_: &str, _: &str, _: Option<f32>| {};
        match run_streaming(cmd, "pip", "installing x", &mut on) {
            Err(InstallError::Pip { code, stderr }) => {
                assert_eq!(code, Some(3));
                assert!(stderr.contains("boom"), "tail lost: {stderr}");
            }
            other => panic!("expected Pip error, got {other:?}"),
        }
    }

    #[test]
    fn pbs_covers_common_platforms_with_valid_hashes() {
        for key in [
            "macos-aarch64",
            "macos-x86_64",
            "linux-x86_64",
            "linux-aarch64",
            "windows-x86_64",
        ] {
            assert!(
                PBS_PYTHON.iter().any(|(k, _, _)| *k == key),
                "missing standalone-Python entry for {key}"
            );
        }
        for (_, _, sha) in PBS_PYTHON {
            assert_eq!(sha.len(), 64, "sha256 must be 64 hex chars");
            assert!(sha.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn python_available_when_platform_is_provisionable() {
        // On any platform we ship a standalone build for (incl. this host), the
        // pip tools are installable with no system Python required.
        if PBS_PYTHON.iter().any(|(k, _, _)| *k == platform_key()) {
            assert!(python_available());
        }
    }

    #[test]
    fn human_bytes_reads_naturally() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn venv_bin_dir_is_under_tools() {
        // The managed venv bin lives inside the tools dir (sandbox invariant).
        if let (Ok(root), Ok(vbin)) = (tools_dir(), venv_bin_dir()) {
            assert!(vbin.starts_with(&root));
        }
    }

    #[test]
    fn selector_parsing() {
        assert_eq!(parse_selector("all").unwrap().len(), 4);
        assert_eq!(parse_selector("").unwrap().len(), 4);
        assert_eq!(parse_selector("ALL").unwrap().len(), 4);
        assert_eq!(parse_selector("whisper").unwrap(), vec![Tool::Whisper]);
        assert_eq!(
            parse_selector("whisper,sd").unwrap(),
            vec![Tool::Whisper, Tool::StableDiffusion]
        );
        // De-dupes and accepts space separators.
        assert_eq!(parse_selector("demucs demucs").unwrap(), vec![Tool::Demucs]);
        assert!(parse_selector("nope").is_err());
        // A separators-only spec resolves to nothing → error, not silent 0-tool success.
        assert!(parse_selector(",").is_err());
        assert!(parse_selector(", ,").is_err());
    }

    #[test]
    fn handle_cli_only_intercepts_known_flags() {
        // A bare launch (no flags) or an OS-passed file path falls through.
        assert_eq!(handle_cli(&["vance-app".into()]), None);
        assert_eq!(
            handle_cli(&["vance-app".into(), "/some/project.vance".into()]),
            None
        );
        assert_eq!(
            handle_cli(&["vance-app".into(), "-psn_0_12345".into()]),
            None
        );
        // A recognized, read-only flag is handled (exit 0) without a GUI.
        assert_eq!(
            handle_cli(&["vance-app".into(), "--list-ai-tools".into()]),
            Some(0)
        );
        assert_eq!(
            handle_cli(&["vance-app".into(), "--ai-tools-status".into()]),
            Some(0)
        );
    }
}
