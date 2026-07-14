//! Phase 11 — persisted app settings (first-run flag + privacy opt-ins).
//!
//! A tiny JSON file in the app config dir. Deliberately minimal and
//! `#[serde(default)]` throughout so the schema can grow without breaking
//! existing installs, and a corrupt/missing file falls back to defaults
//! (never blocks startup).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// App-wide settings, independent of any project.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AppSettings {
    /// Set once the user has finished (or skipped) the first-run wizard.
    #[serde(default)]
    pub first_run_complete: bool,
    /// Opt-in to sending crash reports (local crash logs are always written;
    /// this gates any future upload). Defaults OFF.
    #[serde(default)]
    pub crash_reports_opt_in: bool,
    /// Opt-in to anonymous usage telemetry. Defaults OFF.
    #[serde(default)]
    pub telemetry_opt_in: bool,
    /// Forward-looking: the AI assistant provider the user chose in the
    /// wizard (e.g. "anthropic", "ollama"), if any. The key itself is NOT
    /// stored here — it belongs in the OS keychain (a later milestone).
    #[serde(default)]
    pub ai_provider: Option<String>,
}

/// The `…/Vance` config base directory (created if missing).
pub fn config_dir() -> std::io::Result<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Library/Application Support"))
            .unwrap_or_else(std::env::temp_dir)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(std::env::temp_dir)
    };
    let dir = base.join("Vance");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn settings_path() -> std::io::Result<PathBuf> {
    Ok(config_dir()?.join("settings.json"))
}

/// Load settings, falling back to defaults on any error (missing/corrupt).
pub fn load() -> AppSettings {
    let Ok(path) = settings_path() else {
        return AppSettings::default();
    };
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => AppSettings::default(),
    }
}

/// Persist settings (pretty JSON) atomically-ish (write temp, then rename).
pub fn save(settings: &AppSettings) -> std::io::Result<()> {
    let path = settings_path()?;
    let json = serde_json::to_vec_pretty(settings)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_conservative() {
        let s = AppSettings::default();
        assert!(!s.first_run_complete);
        assert!(!s.crash_reports_opt_in, "privacy opt-ins default OFF");
        assert!(!s.telemetry_opt_in, "privacy opt-ins default OFF");
        assert!(s.ai_provider.is_none());
    }

    #[test]
    fn round_trips_through_json() {
        let s = AppSettings {
            first_run_complete: true,
            crash_reports_opt_in: true,
            telemetry_opt_in: false,
            ai_provider: Some("ollama".into()),
        };
        let j = serde_json::to_string(&s).unwrap();
        let back: AppSettings = serde_json::from_str(&j).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn missing_fields_fall_back_to_default() {
        // A forward/older file with only one field still loads.
        let back: AppSettings = serde_json::from_str(r#"{"first_run_complete":true}"#).unwrap();
        assert!(back.first_run_complete);
        assert!(!back.telemetry_opt_in);
    }
}
