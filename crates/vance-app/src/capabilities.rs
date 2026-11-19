//! Phase 11 — runtime capability detection for the onboarding wizard and
//! the Help ▸ Diagnostics view.
//!
//! Reports which external tools + bundled sidecars are present so a new user
//! immediately sees what works and what to install, rather than hitting an
//! opaque error later. Optional AI features degrade gracefully — the editor
//! is fully usable without any of them.

use serde::Serialize;

/// One detected capability.
#[derive(Clone, Debug, Serialize)]
pub struct Capability {
    /// Stable id (`ffmpeg`, `decoder`, `whisper`, …).
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Whether it was found.
    pub present: bool,
    /// Whether core editing/export needs it (vs. an optional enhancement).
    pub required: bool,
    /// What the user can do about it when absent.
    pub hint: String,
}

impl Capability {
    fn new(id: &str, name: &str, present: bool, required: bool, hint: &str) -> Self {
        Capability {
            id: id.into(),
            name: name.into(),
            present,
            required,
            hint: hint.into(),
        }
    }
}

/// Detect all capabilities. Cheap (PATH stats + env), safe to call on the UI
/// thread at first run and from a Diagnostics refresh.
pub fn detect() -> Vec<Capability> {
    let (decoder, encoder, audio) = crate::engine::sidecar_presence();
    vec![
        Capability::new(
            "ffmpeg",
            "FFmpeg",
            which("ffmpeg").is_some(),
            true,
            "Install FFmpeg and ensure it's on PATH (macOS: `brew install ffmpeg`; \
             Windows: winget install Gyan.FFmpeg; Linux: your package manager).",
        ),
        Capability::new(
            "decoder",
            "Media decoder (bundled)",
            decoder,
            true,
            "The decoder sidecar wasn't found next to the app — reinstall Vance.",
        ),
        Capability::new(
            "encoder",
            "Media encoder (bundled)",
            encoder,
            true,
            "The encoder sidecar wasn't found next to the app — reinstall Vance.",
        ),
        Capability::new(
            "audio",
            "Audio engine (bundled)",
            audio,
            true,
            "The audio sidecar wasn't found next to the app — reinstall Vance.",
        ),
        Capability::new(
            "whisper",
            "Speech-to-text (captions)",
            crate::transcribe::available(),
            false,
            "Optional. Add it from the AI tools panel — Vance installs it for you \
             (or install `whisper` on PATH yourself).",
        ),
        Capability::new(
            "demucs",
            "Voice isolation (AI audio)",
            crate::separate::available(),
            false,
            "Optional. Add it from the AI tools panel — Vance installs it for you \
             (or `pip install demucs` yourself).",
        ),
        Capability::new(
            "matte",
            "Subject mask (Magic Mask)",
            crate::magicmask::available(),
            false,
            "Optional. Add it from the AI tools panel — Vance installs it for you \
             (or `pip install -U backgroundremover` yourself).",
        ),
        Capability::new(
            "imagegen",
            "Generative images",
            crate::generate::available(),
            false,
            "Optional. Add it from the AI tools panel — Vance installs diffusers + \
             a model for you (or set VANCE_IMAGEGEN_CMD / VANCE_SD_MODEL yourself).",
        ),
    ]
}

/// Find an executable on PATH (honours PATHEXT on Windows).
fn which(bin: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
            .split(';')
            .map(|s| s.to_string())
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path) {
        for ext in &exts {
            let candidate = dir.join(format!("{bin}{ext}"));
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
    fn detect_reports_all_capabilities_with_hints() {
        let caps = detect();
        // The fixed set is always reported (present or not), each with an id,
        // a name, and a non-empty hint.
        for id in [
            "ffmpeg", "decoder", "encoder", "audio", "whisper", "demucs", "matte", "imagegen",
        ] {
            let c = caps
                .iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("missing {id}"));
            assert!(!c.name.is_empty());
            assert!(!c.hint.is_empty());
        }
        // The bundled sidecars are marked required; the AI tools optional.
        assert!(caps.iter().find(|c| c.id == "decoder").unwrap().required);
        assert!(!caps.iter().find(|c| c.id == "whisper").unwrap().required);
    }
}
