//! GOAT-gated — generative image synthesis via a user-supplied model CLI.
//!
//! Text-to-image generation needs a model Vance does not (and legally cannot)
//! bundle, so this drives an EXTERNAL, user-configured generator — exactly
//! like the whisper / demucs / background-remover runners. Two ways to wire
//! it:
//!
//!  1. `VANCE_IMAGEGEN_CMD` — a full command template with placeholders
//!     (`{prompt}`, `{negative}`, `{width}`, `{height}`, `{steps}`,
//!     `{seed}`, `{out}`). The template is tokenized into argv (quote-aware)
//!     and each placeholder is substituted into ONE argument, so a prompt
//!     with spaces/quotes can never break out into the shell — there is no
//!     shell. Put a flag and its optional value in a SINGLE token
//!     (`--negative={negative}`) so an absent value drops the whole token.
//!
//!  2. Default `sd` (stable-diffusion.cpp) on PATH + `VANCE_SD_MODEL`
//!     pointing at the weights — Vance builds the txt2img args itself.
//!
//! The runner + import are fully built; the model + binary are the user's to
//! provide (clear `NotConfigured` error). img2img / inpaint (feeding a clip
//! frame + mask) is a follow-up — it needs source-frame + mask PNG encoding.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A text-to-image generation request.
#[derive(Clone, Debug)]
pub struct GenRequest {
    /// Positive prompt.
    pub prompt: String,
    /// Optional negative prompt.
    pub negative: Option<String>,
    /// Output width in px.
    pub width: u32,
    /// Output height in px.
    pub height: u32,
    /// Diffusion steps (`None` → a sane default).
    pub steps: Option<u32>,
    /// Seed (`None` → random / `-1`).
    pub seed: Option<i64>,
    /// Where the model must write the PNG.
    pub out_path: PathBuf,
}

/// Errors from the generative pipeline.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GenError {
    /// No generator configured.
    #[error(
        "no image generator installed. Easiest: open the AI tools panel and \
         install “Generative images” — Vance sets up diffusers + a model for you.\n\
         Advanced alternatives:\n  \
         • set VANCE_IMAGEGEN_CMD to a command template using \
         {{prompt}} {{width}} {{height}} {{out}} (and optionally {{negative}} {{steps}} {{seed}}), or\n  \
         • install `sd` (stable-diffusion.cpp) on PATH and set VANCE_SD_MODEL to the model file."
    )]
    NotConfigured,
    /// The command template had no program token.
    #[error("VANCE_IMAGEGEN_CMD is empty or has no program")]
    EmptyTemplate,
    /// The generator ran but failed.
    #[error("image generator failed (exit {code:?}): {stderr}")]
    Failed {
        /// Exit code.
        code: Option<i32>,
        /// Captured stderr (truncated).
        stderr: String,
    },
    /// Spawn / wait failure.
    #[error("running image generator: {0}")]
    Io(#[from] std::io::Error),
    /// The run produced no output file.
    #[error("image generator produced no output at {0}")]
    NoOutput(PathBuf),
}

/// Is a generator configured? (For the UI to enable the button.)
pub fn available() -> bool {
    std::env::var_os("VANCE_IMAGEGEN_CMD").is_some()
        || crate::tools::diffusers_runner().is_some()
        || (which("sd").is_some() && crate::tools::sd_model().is_some())
}

/// Generate an image for `req`, returning the output path. Blocking + slow
/// (model inference) — call OFF the engine lock.
pub fn generate(req: &GenRequest) -> Result<PathBuf, GenError> {
    let (program, args) = resolve_command(req)?;
    if let Some(parent) = req.out_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::remove_file(&req.out_path);

    let mut cmd = Command::new(&program);
    cmd.args(&args);
    let out = cmd.output()?;
    if !out.status.success() {
        let stderr: String = String::from_utf8_lossy(&out.stderr).chars().take(2000).collect();
        return Err(GenError::Failed {
            code: out.status.code(),
            stderr,
        });
    }
    if req.out_path.is_file() {
        Ok(req.out_path.clone())
    } else {
        Err(GenError::NoOutput(req.out_path.clone()))
    }
}

/// Resolve the program + argv for a request (template or default `sd`).
fn resolve_command(req: &GenRequest) -> Result<(PathBuf, Vec<String>), GenError> {
    if let Some(tmpl) = std::env::var_os("VANCE_IMAGEGEN_CMD") {
        let tmpl = tmpl.to_string_lossy();
        let toks = tokenize(&tmpl);
        let mut argv: Vec<String> = Vec::new();
        for tok in &toks {
            if let Some(sub) = substitute(tok, req) {
                argv.push(sub);
            }
        }
        if argv.is_empty() {
            return Err(GenError::EmptyTemplate);
        }
        let program = PathBuf::from(argv.remove(0));
        return Ok((program, argv));
    }
    // Installed via the AI tools panel: the diffusers wrapper in the managed venv.
    if let Some((python, script)) = crate::tools::diffusers_runner() {
        return Ok((python, diffusers_args(&script, req)));
    }
    // Legacy: stable-diffusion.cpp `sd` + a model file from the environment.
    if let (Some(sd), Some(model)) = (which("sd"), std::env::var_os("VANCE_SD_MODEL")) {
        let args = default_sd_args(&PathBuf::from(model), req);
        return Ok((sd, args));
    }
    Err(GenError::NotConfigured)
}

/// Build the argv for the bundled diffusers wrapper: `<script> --model … --prompt
/// … --out … --width … --height … --steps … [--negative …] [--seed …]`.
fn diffusers_args(script: &Path, req: &GenRequest) -> Vec<String> {
    let mut a = vec![
        script.to_string_lossy().into_owned(),
        "--model".into(),
        crate::tools::diffusers_model(),
        "--prompt".into(),
        req.prompt.clone(),
        "--out".into(),
        req.out_path.to_string_lossy().into_owned(),
        "--width".into(),
        req.width.to_string(),
        "--height".into(),
        req.height.to_string(),
        "--steps".into(),
        req.steps.unwrap_or(25).to_string(),
    ];
    if let Some(neg) = req.negative.as_ref().filter(|s| !s.is_empty()) {
        a.push("--negative".into());
        a.push(neg.clone());
    }
    if let Some(seed) = req.seed.filter(|s| *s >= 0) {
        a.push("--seed".into());
        a.push(seed.to_string());
    }
    a
}

/// Build the default stable-diffusion.cpp txt2img argv.
fn default_sd_args(model: &Path, req: &GenRequest) -> Vec<String> {
    let mut a = vec![
        "-m".to_string(),
        model.to_string_lossy().into_owned(),
        "-p".to_string(),
        req.prompt.clone(),
        "-W".to_string(),
        req.width.to_string(),
        "-H".to_string(),
        req.height.to_string(),
        "--steps".to_string(),
        req.steps.unwrap_or(20).to_string(),
        "-o".to_string(),
        req.out_path.to_string_lossy().into_owned(),
    ];
    if let Some(neg) = req.negative.as_ref().filter(|s| !s.is_empty()) {
        a.push("-n".to_string());
        a.push(neg.clone());
    }
    if let Some(seed) = req.seed {
        a.push("-s".to_string());
        a.push(seed.to_string());
    }
    a
}

/// Substitute placeholders in one template token. Returns `None` to DROP the
/// token when it reduces to an empty string (e.g. a lone `{negative}` with no
/// negative prompt), so an optional argument disappears cleanly.
///
/// SINGLE-PASS over the ORIGINAL token: a substituted value is written to the
/// output and never re-scanned, so a `{seed}`/`{out}` that happens to appear
/// literally inside the user's prompt text is NOT re-expanded (that would
/// mangle the prompt or leak the internal output path into it). Unknown
/// `{...}` names are left verbatim.
fn substitute(token: &str, req: &GenRequest) -> Option<String> {
    let neg = req.negative.clone().unwrap_or_default();
    let seed = req.seed.map(|s| s.to_string()).unwrap_or_else(|| "-1".to_string());
    let steps = req.steps.unwrap_or(20).to_string();
    let width = req.width.to_string();
    let height = req.height.to_string();
    let out = req.out_path.to_string_lossy();

    let mut result = String::with_capacity(token.len());
    let mut rest = token;
    while let Some(open) = rest.find('{') {
        result.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) => {
                let name = &after[..close];
                let value: Option<&str> = match name {
                    "prompt" => Some(&req.prompt),
                    "negative" => Some(&neg),
                    "width" => Some(&width),
                    "height" => Some(&height),
                    "steps" => Some(&steps),
                    "seed" => Some(&seed),
                    "out" => Some(&out),
                    _ => None,
                };
                match value {
                    Some(v) => {
                        result.push_str(v);
                        rest = &after[close + 1..];
                    }
                    None => {
                        // Unknown placeholder — keep the literal `{` and rescan
                        // from just after it.
                        result.push('{');
                        rest = after;
                    }
                }
            }
            None => {
                // No closing brace — keep `{` literally and stop scanning.
                result.push('{');
                rest = after;
            }
        }
    }
    result.push_str(rest);
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

/// Split a command template into argv, honoring single/double quotes and
/// whitespace. Quotes group a run into ONE token; there is no other shell
/// processing (no variable/glob/pipe expansion), so substituted values are
/// never re-interpreted.
fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut has_token = false;
    for c in s.chars() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                has_token = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                has_token = true;
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if has_token {
                    out.push(std::mem::take(&mut cur));
                    has_token = false;
                }
            }
            c => {
                cur.push(c);
                has_token = true;
            }
        }
    }
    if has_token {
        out.push(cur);
    }
    out
}

/// Find an executable on PATH (honours PATHEXT on Windows).
/// Find an executable: Vance's managed tools dir first, then `PATH`.
fn which(bin: &str) -> Option<PathBuf> {
    crate::tools::resolve(bin)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> GenRequest {
        GenRequest {
            prompt: "a red fox in snow".into(),
            negative: None,
            width: 1024,
            height: 576,
            steps: Some(30),
            seed: Some(42),
            out_path: PathBuf::from("/tmp/gen.png"),
        }
    }

    #[test]
    fn not_configured_message_is_actionable() {
        assert!(GenError::NotConfigured.to_string().contains("VANCE_IMAGEGEN_CMD"));
        assert!(GenError::NotConfigured.to_string().contains("VANCE_SD_MODEL"));
    }

    #[test]
    fn tokenize_respects_quotes() {
        let t = tokenize(r#"gen --prompt "{prompt}" -o {out}"#);
        assert_eq!(t, vec!["gen", "--prompt", "{prompt}", "-o", "{out}"]);
        // Empty string → no tokens.
        assert!(tokenize("   ").is_empty());
    }

    #[test]
    fn substitute_fills_placeholders_as_single_args() {
        let r = req();
        // A lone {prompt} becomes ONE arg carrying the whole (spaced) prompt.
        assert_eq!(substitute("{prompt}", &r).as_deref(), Some("a red fox in snow"));
        assert_eq!(substitute("--size={width}x{height}", &r).as_deref(), Some("--size=1024x576"));
        assert_eq!(substitute("{seed}", &r).as_deref(), Some("42"));
        // Absent negative → the token drops entirely.
        assert_eq!(substitute("{negative}", &r), None);
    }

    #[test]
    fn prompt_placeholders_are_not_re_expanded() {
        // Review fix: a {seed}/{out} literally inside the prompt must survive
        // verbatim — substitution is single-pass over the original token.
        let mut r = req();
        r.prompt = "a photo at {seed} dpi near {out}".into();
        assert_eq!(
            substitute("{prompt}", &r).as_deref(),
            Some("a photo at {seed} dpi near {out}"),
            "user prompt content must not be re-scanned for placeholders"
        );
        // Unknown placeholders are left verbatim.
        assert_eq!(substitute("{init}", &r).as_deref(), Some("{init}"));
        // Unterminated brace is kept literally.
        assert_eq!(substitute("{prompt", &r).as_deref(), Some("{prompt"));
    }

    #[test]
    fn template_prompt_cannot_break_out_of_its_arg() {
        // A malicious-looking prompt stays a single argument — no shell.
        let mut r = req();
        r.prompt = "foo; rm -rf / #".into();
        let toks = tokenize("gen -p {prompt} -o {out}");
        let argv: Vec<String> = toks.iter().filter_map(|t| substitute(t, &r)).collect();
        assert_eq!(argv, vec!["gen", "-p", "foo; rm -rf / #", "-o", "/tmp/gen.png"]);
    }

    #[test]
    fn default_sd_args_include_model_prompt_size() {
        let r = req();
        let a = default_sd_args(Path::new("/models/sd.gguf"), &r);
        assert!(a.windows(2).any(|w| w[0] == "-m" && w[1] == "/models/sd.gguf"));
        assert!(a.windows(2).any(|w| w[0] == "-p" && w[1] == "a red fox in snow"));
        assert!(a.windows(2).any(|w| w[0] == "-W" && w[1] == "1024"));
        assert!(a.windows(2).any(|w| w[0] == "-s" && w[1] == "42"));
    }
}
