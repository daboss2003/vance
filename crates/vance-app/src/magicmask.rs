//! GOAT-9 — "Magic Mask": AI subject isolation via a local CLI.
//!
//! Arbitrary-shape AI segmentation needs a model Vance does not bundle, so
//! this drives an EXTERNAL, user-installed background-removal CLI —
//! `backgroundremover` (`pip install backgroundremover`) by default — to
//! produce a matted video (subject isolated, background removed). Vance
//! imports the result; the subject then composites over lower tracks via
//! the existing chroma/luma keyer. The runner + import are fully built;
//! only the model + binary are the user's to provide (clear NotInstalled
//! error). True in-renderer arbitrary-matte compositing (an external alpha
//! texture slot) is a separate pipeline follow-up.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Errors from the magic-mask pipeline.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MagicMaskError {
    /// No segmentation CLI found on PATH.
    #[error(
        "no AI-mask CLI found. Install one and ensure it's on PATH:\n  \
         pip install -U backgroundremover\n\
         Then retry — Vance runs it and imports the isolated subject."
    )]
    NotInstalled,
    /// The CLI ran but failed.
    #[error("background remover failed (exit {code:?}): {stderr}")]
    Failed {
        /// Exit code.
        code: Option<i32>,
        /// Captured stderr (truncated).
        stderr: String,
    },
    /// Spawn / wait failure.
    #[error("running background remover: {0}")]
    Io(#[from] std::io::Error),
    /// The run produced no output file.
    #[error("background remover produced no output")]
    NoOutput,
}

/// Is a magic-mask CLI installed? (For the UI to enable the button.)
pub fn available() -> bool {
    which("backgroundremover").is_some()
}

/// Isolate the subject in `media_path`, returning the path to the produced
/// matted video. Blocking + slow (per-frame model inference, often many
/// minutes) — call OFF the engine lock.
pub fn isolate_subject(media_path: &Path) -> Result<PathBuf, MagicMaskError> {
    let bin = which("backgroundremover").ok_or(MagicMaskError::NotInstalled)?;
    let media = std::fs::canonicalize(media_path).unwrap_or_else(|_| media_path.to_path_buf());
    let out_dir = std::env::temp_dir().join(format!(
        "vance-magicmask-{}-{}",
        media.file_stem().and_then(|s| s.to_str()).unwrap_or("out"),
        std::fs::metadata(&media).map(|m| m.len()).unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&out_dir);
    std::fs::create_dir_all(&out_dir)?;
    let out_file = out_dir.join("subject.mov");

    // `backgroundremover -i <in> -tv -o <out>` → transparent/matted video.
    let mut cmd = Command::new(bin);
    cmd.arg("-i")
        .arg(&media)
        .arg("-tv")
        .arg("-o")
        .arg(&out_file);
    let out = cmd.output()?;
    if !out.status.success() {
        let mut stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        stderr.truncate(2000);
        return Err(MagicMaskError::Failed {
            code: out.status.code(),
            stderr,
        });
    }
    if out_file.is_file() {
        return Ok(out_file);
    }
    // Some builds pick a different extension — take the first video-ish file.
    find_output(&out_dir).ok_or(MagicMaskError::NoOutput)
}

/// First plausible video output in `dir` (recursive).
fn find_output(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(f) = find_output(&p) {
                return Some(f);
            }
            continue;
        }
        let ext = p.extension().and_then(|s| s.to_str()).map(|s| s.to_lowercase());
        if matches!(ext.as_deref(), Some("mov") | Some("webm") | Some("mp4") | Some("mkv")) {
            return Some(p);
        }
    }
    None
}

/// Find an executable on PATH (honours PATHEXT on Windows).
fn which(bin: &str) -> Option<PathBuf> {
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
    fn not_installed_message_is_actionable() {
        assert!(MagicMaskError::NotInstalled.to_string().contains("pip install"));
    }

    #[test]
    fn find_output_picks_a_video_file() {
        let base = std::env::temp_dir().join(format!("vance-mm-test-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("log.txt"), b"x").unwrap();
        std::fs::write(base.join("subject.mov"), b"x").unwrap();
        assert!(find_output(&base).unwrap().extension().unwrap() == "mov");
        let _ = std::fs::remove_dir_all(&base);
    }
}
