//! GOAT-7 — auto-transcription via a local Whisper CLI.
//!
//! Transcription itself runs an ML model that Vance does NOT bundle (the
//! models are hundreds of MB to multiple GB). This module drives an
//! EXTERNAL, user-installed Whisper-compatible CLI — by default OpenAI's
//! `whisper` (which decodes any media via its own ffmpeg and writes an
//! `.srt`), with `whisper.cpp`'s `whisper-cli` auto-detected as a
//! fallback. The Vance side (audio handoff → SRT → cue model → editor) is
//! fully built; only the binary + model are the user's to provide, and we
//! return a clear, actionable error when they're missing.

use std::path::{Path, PathBuf};
use std::process::Command;

use vance_captions::Cue;

/// A transcription request.
#[derive(Clone, Debug)]
pub struct TranscribeConfig {
    /// Whisper model name (e.g. "base", "small", "medium", "large-v3").
    pub model: String,
    /// BCP-47 / Whisper language code, or `None` to auto-detect.
    pub language: Option<String>,
}

impl Default for TranscribeConfig {
    fn default() -> Self {
        TranscribeConfig {
            model: "base".into(),
            language: None,
        }
    }
}

/// Errors from the transcription pipeline.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TranscribeError {
    /// No Whisper CLI found on PATH.
    #[error(
        "no Whisper CLI found. Install one and ensure it's on PATH:\n  \
         • OpenAI Whisper:  pip install -U openai-whisper\n  \
         • whisper.cpp:     build `whisper-cli` and a model (set VANCE_WHISPER_CPP_MODEL)\n\
         Then retry — Vance handles the rest (audio → SRT → caption editor)."
    )]
    NotInstalled,
    /// The CLI ran but failed.
    #[error("whisper failed (exit {code:?}): {stderr}")]
    Failed {
        /// Process exit code.
        code: Option<i32>,
        /// Captured stderr (truncated).
        stderr: String,
    },
    /// Couldn't spawn / wait on the process.
    #[error("running whisper: {0}")]
    Io(#[from] std::io::Error),
    /// The CLI produced no parseable `.srt`.
    #[error("whisper produced no readable .srt output")]
    NoOutput,
    /// The produced SRT couldn't be parsed.
    #[error("parsing whisper SRT: {0}")]
    Parse(#[from] vance_captions::CaptionError),
}

/// Which Whisper CLI we found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WhisperKind {
    /// OpenAI `whisper` (Python) — decodes media itself, writes `<stem>.srt`.
    OpenAi(PathBuf),
    /// whisper.cpp `whisper-cli` — needs a model path (`VANCE_WHISPER_CPP_MODEL`).
    Cpp(PathBuf),
}

/// Detect an installed Whisper CLI, preferring OpenAI's `whisper`.
pub fn detect() -> Option<WhisperKind> {
    if let Some(p) = which("whisper") {
        return Some(WhisperKind::OpenAi(p));
    }
    for name in ["whisper-cli", "whisper-cpp", "main"] {
        if let Some(p) = which(name) {
            // `main` is ambiguous; only accept it if a cpp model is configured.
            if name == "main" && std::env::var_os("VANCE_WHISPER_CPP_MODEL").is_none() {
                continue;
            }
            return Some(WhisperKind::Cpp(p));
        }
    }
    None
}

/// Is any Whisper CLI available? (For the UI to enable/disable the button.)
pub fn available() -> bool {
    detect().is_some()
}

/// Transcribe `media_path` into caption cues. Blocking + potentially slow
/// (minutes) — call OFF the engine lock / UI thread.
pub fn transcribe_file(
    media_path: &Path,
    config: &TranscribeConfig,
) -> Result<Vec<Cue>, TranscribeError> {
    let kind = detect().ok_or(TranscribeError::NotInstalled)?;
    let tmp = std::env::temp_dir().join(format!(
        "vance-whisper-{}",
        media_path.file_stem().and_then(|s| s.to_str()).unwrap_or("out")
    ));
    std::fs::create_dir_all(&tmp)?;

    let srt_text = match kind {
        WhisperKind::OpenAi(bin) => run_openai(&bin, media_path, &tmp, config)?,
        WhisperKind::Cpp(bin) => run_cpp(&bin, media_path, &tmp, config)?,
    };
    let cues = vance_captions::srt::from_srt(&srt_text)?;
    Ok(cues)
}

fn run_openai(
    bin: &Path,
    media: &Path,
    out_dir: &Path,
    config: &TranscribeConfig,
) -> Result<String, TranscribeError> {
    let mut cmd = Command::new(bin);
    cmd.arg(media)
        .arg("--model")
        .arg(&config.model)
        .arg("--output_format")
        .arg("srt")
        .arg("--output_dir")
        .arg(out_dir)
        .arg("--task")
        .arg("transcribe");
    if let Some(lang) = &config.language {
        cmd.arg("--language").arg(lang);
    }
    run_and_collect(cmd)?;
    // OpenAI writes `<media stem>.srt` into out_dir.
    read_first_srt(out_dir)
}

fn run_cpp(
    bin: &Path,
    media: &Path,
    out_dir: &Path,
    _config: &TranscribeConfig,
) -> Result<String, TranscribeError> {
    let model = std::env::var_os("VANCE_WHISPER_CPP_MODEL")
        .map(PathBuf::from)
        .ok_or(TranscribeError::NotInstalled)?;
    let of = out_dir.join("out");
    // whisper.cpp wants 16 kHz mono WAV; we pass the media directly and rely
    // on the user's build having ffmpeg support, else this surfaces a clear
    // failure the UI shows verbatim.
    let mut cmd = Command::new(bin);
    cmd.arg("-m")
        .arg(model)
        .arg("-f")
        .arg(media)
        .arg("-osrt")
        .arg("-of")
        .arg(&of);
    run_and_collect(cmd)?;
    let srt = of.with_extension("srt");
    std::fs::read_to_string(&srt).map_err(|_| TranscribeError::NoOutput)
}

fn run_and_collect(mut cmd: Command) -> Result<(), TranscribeError> {
    let out = cmd.output()?;
    if !out.status.success() {
        let mut stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        stderr.truncate(2000);
        return Err(TranscribeError::Failed {
            code: out.status.code(),
            stderr,
        });
    }
    Ok(())
}

/// Read the first `.srt` file in `dir`.
fn read_first_srt(dir: &Path) -> Result<String, TranscribeError> {
    let entries = std::fs::read_dir(dir)?;
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) == Some("srt") {
            if let Ok(s) = std::fs::read_to_string(&p) {
                return Ok(s);
            }
        }
    }
    Err(TranscribeError::NoOutput)
}

/// Find an executable on PATH (cross-platform, honouring PATHEXT on Windows).
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
    fn detect_is_total_and_consistent() {
        // detect() never panics; available() agrees with it.
        assert_eq!(available(), detect().is_some());
    }

    #[test]
    fn not_installed_error_is_actionable() {
        let msg = TranscribeError::NotInstalled.to_string();
        assert!(msg.contains("openai-whisper") && msg.contains("whisper.cpp"));
    }

    #[test]
    fn which_finds_a_known_binary() {
        // `sh` exists on every unix CI box; on Windows skip.
        if cfg!(unix) {
            assert!(which("sh").is_some());
        }
    }
}
