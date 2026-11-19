//! GOAT-8 — AI voice isolation / source separation via a local CLI.
//!
//! Like transcription, the separation MODEL is not bundled (Demucs models
//! are hundreds of MB). This drives an EXTERNAL, user-installed CLI —
//! Facebook's `demucs` (`pip install demucs`) by default — and returns the
//! produced stem files (vocals + accompaniment) for the editor to import.
//! The Vance side (run → locate stems → import) is fully built; only the
//! binary + model are the user's to provide, with a clear NotInstalled
//! error when missing.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Errors from the separation pipeline.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SeparateError {
    /// No separation CLI found on PATH.
    #[error(
        "no voice-isolation CLI found. Install Demucs and ensure it's on PATH:\n  \
         pip install -U demucs\n\
         Then retry — Vance runs it and imports the isolated stems."
    )]
    NotInstalled,
    /// The CLI ran but failed.
    #[error("demucs failed (exit {code:?}): {stderr}")]
    Failed {
        /// Exit code.
        code: Option<i32>,
        /// Captured stderr (truncated).
        stderr: String,
    },
    /// Spawn / wait failure.
    #[error("running demucs: {0}")]
    Io(#[from] std::io::Error),
    /// The run produced no `vocals.*` stem.
    #[error("demucs produced no vocals stem")]
    NoOutput,
}

/// Paths to the produced stems.
#[derive(Clone, Debug)]
pub struct Stems {
    /// The isolated vocal/dialogue stem.
    pub vocals: PathBuf,
    /// The accompaniment (everything else), when two-stem mode produced it.
    pub accompaniment: Option<PathBuf>,
}

/// Is a separation CLI installed? (For the UI to enable the button.)
pub fn available() -> bool {
    which("demucs").is_some()
}

/// Separate `media_path` into a vocals + accompaniment stem (two-stem
/// mode). Blocking + slow (often minutes) — call OFF the engine lock.
pub fn isolate(media_path: &Path) -> Result<Stems, SeparateError> {
    let bin = which("demucs").ok_or(SeparateError::NotInstalled)?;
    let media = std::fs::canonicalize(media_path).unwrap_or_else(|_| media_path.to_path_buf());
    let out_dir = std::env::temp_dir().join(format!(
        "vance-demucs-{}-{}",
        media.file_stem().and_then(|s| s.to_str()).unwrap_or("out"),
        std::fs::metadata(&media).map(|m| m.len()).unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&out_dir);
    std::fs::create_dir_all(&out_dir)?;

    let mut cmd = Command::new(bin);
    cmd.arg("--two-stems")
        .arg("vocals")
        .arg("-o")
        .arg(&out_dir)
        .arg("--")
        .arg(&media);
    let out = cmd.output()?;
    if !out.status.success() {
        let stderr: String = String::from_utf8_lossy(&out.stderr)
            .chars()
            .take(2000)
            .collect();
        return Err(SeparateError::Failed {
            code: out.status.code(),
            stderr,
        });
    }

    // Demucs writes `<out>/<model>/<track>/{vocals,no_vocals}.{wav,mp3,...}`.
    let vocals = find_stem(&out_dir, "vocals").ok_or(SeparateError::NoOutput)?;
    let accompaniment = find_stem(&out_dir, "no_vocals");
    Ok(Stems {
        vocals,
        accompaniment,
    })
}

/// Recursively find a stem file whose name (sans extension) is `stem`.
fn find_stem(dir: &Path, stem: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(found) = find_stem(&p, stem) {
                return Some(found);
            }
        } else if p.file_stem().and_then(|s| s.to_str()) == Some(stem) {
            return Some(p);
        }
    }
    None
}

/// Find an executable on PATH (honours PATHEXT on Windows).
/// Find an executable: Vance's managed tools dir first, then `PATH`.
fn which(bin: &str) -> Option<PathBuf> {
    crate::tools::resolve(bin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_installed_message_is_actionable() {
        assert!(SeparateError::NotInstalled
            .to_string()
            .contains("pip install"));
    }

    #[test]
    fn find_stem_locates_nested_file() {
        // Build a fake demucs output tree in a temp dir.
        let base = std::env::temp_dir().join(format!("vance-sep-test-{}", std::process::id()));
        let nested = base.join("htdemucs").join("clip");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("vocals.wav"), b"x").unwrap();
        std::fs::write(nested.join("no_vocals.wav"), b"x").unwrap();
        assert!(find_stem(&base, "vocals").is_some());
        assert!(find_stem(&base, "no_vocals").is_some());
        assert!(find_stem(&base, "drums").is_none());
        let _ = std::fs::remove_dir_all(&base);
    }
}
