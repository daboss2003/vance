//! Phase 7.7 — per-track audio effect chain spec.
//!
//! Tracks carry an ordered insert chain of [`AudioEffectSpec`]s. The
//! mixer applies them per-track post-clip-fold, pre-master. Each spec
//! is a tagged enum so a project file can describe any chain length
//! and shape without bespoke EditOps per effect kind.
//!
//! Serialisation is `#[serde(tag = "kind")]` for stability — adding a
//! new effect variant in 7.8+ doesn't break older readers.

use serde::{Deserialize, Serialize};

/// One band in a 6-band parametric EQ.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct EqBandSpec {
    /// Filter shape (low shelf / peak / high shelf / low cut / high cut).
    #[serde(default)]
    pub kind: EqBandKind,
    /// Whether this band contributes to the output. Disabled bands
    /// stay in the spec so the user can A/B without losing settings.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Center frequency in Hz. Sanitised to `[10, 22 000]` at apply.
    pub freq_hz: f32,
    /// Gain in dB. Sanitised to `[-24, +24]`. Ignored by `LowCut` /
    /// `HighCut` shapes.
    #[serde(default)]
    pub gain_db: f32,
    /// Q (band width). Sanitised to `[0.1, 18]`. `0.707` ≈ Butterworth.
    #[serde(default = "default_q")]
    pub q: f32,
}

fn yes() -> bool {
    true
}

fn default_q() -> f32 {
    0.707
}

/// What kind of filter shape a band implements. Mirrors
/// `vance_audio::dsp::BiquadShape`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum EqBandKind {
    /// Bass tilt below `freq_hz`. Default for band 1.
    LowShelf,
    /// Resonant peak. Default for bands 2-5.
    #[default]
    Peak,
    /// Treble tilt above `freq_hz`. Default for band 6.
    HighShelf,
    /// 12 dB/oct high-pass.
    LowCut,
    /// 12 dB/oct low-pass.
    HighCut,
}

impl EqBandSpec {
    /// A neutral peak band at 1 kHz with 0 dB gain.
    pub fn neutral_peak() -> Self {
        Self {
            kind: EqBandKind::Peak,
            enabled: true,
            freq_hz: 1_000.0,
            gain_db: 0.0,
            q: 0.707,
        }
    }
}

/// 6 bands hard-coded — matches the PLAN.md deliverable. Default
/// spectrum: low shelf @ 80 Hz, peaks @ 250 / 800 / 2.5 k / 6 k Hz,
/// high shelf @ 12 kHz — a sensible starting curve for music + dialog.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Eq6Spec {
    pub bands: [EqBandSpec; 6],
}

impl Default for Eq6Spec {
    fn default() -> Self {
        Self {
            bands: [
                EqBandSpec {
                    kind: EqBandKind::LowShelf,
                    enabled: true,
                    freq_hz: 80.0,
                    gain_db: 0.0,
                    q: 0.707,
                },
                EqBandSpec {
                    kind: EqBandKind::Peak,
                    enabled: true,
                    freq_hz: 250.0,
                    gain_db: 0.0,
                    q: 1.0,
                },
                EqBandSpec {
                    kind: EqBandKind::Peak,
                    enabled: true,
                    freq_hz: 800.0,
                    gain_db: 0.0,
                    q: 1.0,
                },
                EqBandSpec {
                    kind: EqBandKind::Peak,
                    enabled: true,
                    freq_hz: 2_500.0,
                    gain_db: 0.0,
                    q: 1.0,
                },
                EqBandSpec {
                    kind: EqBandKind::Peak,
                    enabled: true,
                    freq_hz: 6_000.0,
                    gain_db: 0.0,
                    q: 1.0,
                },
                EqBandSpec {
                    kind: EqBandKind::HighShelf,
                    enabled: true,
                    freq_hz: 12_000.0,
                    gain_db: 0.0,
                    q: 0.707,
                },
            ],
        }
    }
}

/// Compressor params. RMS-detector, decoupled smoother, soft-knee
/// gain computer. See `vance_audio::dsp::Compressor` for the algorithm.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompressorSpec {
    /// Threshold in dBFS. Sanitised to `[-60, 0]`.
    pub threshold_db: f32,
    /// Compression ratio. Sanitised to `[1, 20]`. 1 = bypass, 20 = brickwall.
    pub ratio: f32,
    /// Attack time in milliseconds. Sanitised to `[0.1, 1000]`.
    pub attack_ms: f32,
    /// Release time in milliseconds. Sanitised to `[0.1, 5000]`.
    pub release_ms: f32,
    /// Knee width in dB. 0 = hard knee. Sanitised to `[0, 18]`.
    #[serde(default)]
    pub knee_db: f32,
    /// Makeup gain in dB. Sanitised to `[-12, 24]`.
    #[serde(default)]
    pub makeup_db: f32,
}

impl Default for CompressorSpec {
    fn default() -> Self {
        Self {
            threshold_db: -18.0,
            ratio: 3.0,
            attack_ms: 5.0,
            release_ms: 100.0,
            knee_db: 6.0,
            makeup_db: 0.0,
        }
    }
}

/// Look-ahead peak limiter params. See `vance_audio::dsp::Limiter`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LimiterSpec {
    /// Output ceiling in dBFS. Sanitised to `[-30, 0]`. The limiter
    /// guarantees the peak output stays at or below this value (with
    /// the typical sub-sample-inter-peak caveat).
    pub ceiling_db: f32,
    /// Release time in milliseconds. Sanitised to `[1, 5000]`.
    pub release_ms: f32,
}

impl Default for LimiterSpec {
    fn default() -> Self {
        Self {
            ceiling_db: -0.3,
            release_ms: 50.0,
        }
    }
}

/// GOAT-8 — noise-gate params. See `vance_audio::dsp::NoiseGate`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct NoiseGateSpec {
    /// Open threshold in dBFS (sanitised `[-90, 0]`).
    pub threshold_db: f32,
    /// Closed attenuation in dB (sanitised `[-90, 0]`).
    pub range_db: f32,
    /// Attack (open) time, ms.
    pub attack_ms: f32,
    /// Release (close) time, ms.
    pub release_ms: f32,
}

impl Default for NoiseGateSpec {
    fn default() -> Self {
        Self {
            threshold_db: -45.0,
            range_db: -60.0,
            attack_ms: 1.0,
            release_ms: 120.0,
        }
    }
}

/// GOAT-8 — de-hum (mains-hum notch comb) params.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeHumSpec {
    /// Mains fundamental, 50 or 60 Hz (sanitised `[40, 120]`).
    pub base_hz: f32,
    /// Number of notches (fundamental + harmonics), sanitised `1..=10`.
    pub harmonics: u8,
    /// Notch Q (narrowness), sanitised `[0.5, 40]`.
    pub q: f32,
}

impl Default for DeHumSpec {
    fn default() -> Self {
        Self {
            base_hz: 60.0,
            harmonics: 4,
            q: 12.0,
        }
    }
}

/// Tagged-enum serde so adding a new variant stays backward-compat readable.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum AudioEffectSpec {
    /// 6-band parametric EQ.
    Eq6 {
        /// Whether the effect contributes to the output.
        #[serde(default = "yes")]
        enabled: bool,
        /// The 6 bands, in display order.
        eq: Eq6Spec,
    },
    /// Single-band compressor.
    Compressor {
        /// Whether the effect contributes to the output.
        #[serde(default = "yes")]
        enabled: bool,
        /// Compressor configuration.
        spec: CompressorSpec,
    },
    /// Look-ahead peak limiter.
    Limiter {
        /// Whether the effect contributes to the output.
        #[serde(default = "yes")]
        enabled: bool,
        /// Limiter configuration.
        spec: LimiterSpec,
    },
    /// GOAT-8 — downward noise gate / expander.
    NoiseGate {
        /// Whether the effect contributes to the output.
        #[serde(default = "yes")]
        enabled: bool,
        /// Gate configuration.
        spec: NoiseGateSpec,
    },
    /// GOAT-8 — mains-hum remover (harmonic notch comb).
    DeHum {
        /// Whether the effect contributes to the output.
        #[serde(default = "yes")]
        enabled: bool,
        /// De-hum configuration.
        spec: DeHumSpec,
    },
    /// Phase 7.10 / 7.11 — third-party plugin (LV2 / CLAP) loaded by
    /// the sandboxed [`vance-plugin-host`] sidecar. The spec records
    /// just the descriptor handle (format + plugin id + name) plus
    /// the user-customised parameter snapshot; the actual runtime
    /// state lives on the fold thread (see vance-audio's
    /// `TrackDspState`).
    Plugin {
        /// Whether the effect contributes to the output.
        #[serde(default = "yes")]
        enabled: bool,
        /// Plugin descriptor recorded at instantiation time. Re-used
        /// to re-instantiate after a project reload or sandbox crash.
        descriptor: PluginRefSpec,
        /// Parameter overrides: `param_id → value`. Empty = defaults.
        #[serde(default)]
        params: Vec<PluginParamValue>,
    },
}

/// Project-stored snapshot of a plugin's identity. Mirror of
/// `vance_audio_plugins::PluginDescriptor` so the timeline crate stays
/// decoupled from the sandbox library.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginRefSpec {
    /// Plugin format — `"lv2"` or `"clap"`.
    pub format: String,
    /// Format-specific identifier (LV2 URI / `bundle:id`).
    pub id: String,
    /// Display name (captured at instantiation for offline display).
    pub name: String,
    /// Vendor / author display string.
    #[serde(default)]
    pub vendor: String,
}

/// One parameter override stored on the project.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginParamValue {
    /// Plugin-published param id from `PluginParam::id`.
    pub param_id: u32,
    /// Value (clamped to the plugin's reported range at apply time).
    pub value: f32,
}

impl AudioEffectSpec {
    /// Display label for the chain UI.
    pub fn kind_label(&self) -> &'static str {
        match self {
            AudioEffectSpec::Eq6 { .. } => "EQ",
            AudioEffectSpec::Compressor { .. } => "Comp",
            AudioEffectSpec::Limiter { .. } => "Limit",
            AudioEffectSpec::NoiseGate { .. } => "Gate",
            AudioEffectSpec::DeHum { .. } => "DeHum",
            AudioEffectSpec::Plugin { .. } => "Plug",
        }
    }

    /// Is this effect contributing to the output right now?
    pub fn is_enabled(&self) -> bool {
        match self {
            AudioEffectSpec::Eq6 { enabled, .. }
            | AudioEffectSpec::Compressor { enabled, .. }
            | AudioEffectSpec::Limiter { enabled, .. }
            | AudioEffectSpec::NoiseGate { enabled, .. }
            | AudioEffectSpec::DeHum { enabled, .. }
            | AudioEffectSpec::Plugin { enabled, .. } => *enabled,
        }
    }

    /// Toggle the chain entry's `enabled` bit without rebuilding the
    /// spec (preserves nested params).
    pub fn set_enabled(&mut self, on: bool) {
        match self {
            AudioEffectSpec::Eq6 { enabled, .. }
            | AudioEffectSpec::Compressor { enabled, .. }
            | AudioEffectSpec::Limiter { enabled, .. }
            | AudioEffectSpec::NoiseGate { enabled, .. }
            | AudioEffectSpec::DeHum { enabled, .. }
            | AudioEffectSpec::Plugin { enabled, .. } => *enabled = on,
        }
    }

    /// Default for "Add Plugin" with a captured descriptor — the UI
    /// passes in the plugin chosen from the enumeration.
    pub fn plugin(
        format: impl Into<String>,
        id: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        AudioEffectSpec::Plugin {
            enabled: true,
            descriptor: PluginRefSpec {
                format: format.into(),
                id: id.into(),
                name: name.into(),
                vendor: String::new(),
            },
            params: Vec::new(),
        }
    }

    /// Default for "Add EQ" button — a neutral 6-band EQ.
    pub fn default_eq6() -> Self {
        AudioEffectSpec::Eq6 {
            enabled: true,
            eq: Eq6Spec::default(),
        }
    }

    /// Default for "Add Compressor" button — gentle bus compression.
    pub fn default_compressor() -> Self {
        AudioEffectSpec::Compressor {
            enabled: true,
            spec: CompressorSpec::default(),
        }
    }

    /// Default for "Add Limiter" button — transparent ceiling at -0.3 dBFS.
    pub fn default_limiter() -> Self {
        AudioEffectSpec::Limiter {
            enabled: true,
            spec: LimiterSpec::default(),
        }
    }

    /// Default for "Add Noise Gate" — dialogue-friendly gate.
    pub fn default_noise_gate() -> Self {
        AudioEffectSpec::NoiseGate {
            enabled: true,
            spec: NoiseGateSpec::default(),
        }
    }

    /// Default for "Add De-Hum" — 60 Hz mains + 3 harmonics.
    pub fn default_dehum() -> Self {
        AudioEffectSpec::DeHum {
            enabled: true,
            spec: DeHumSpec::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_eq6_has_six_bands_centered_on_useful_freqs() {
        let eq = Eq6Spec::default();
        assert_eq!(eq.bands.len(), 6);
        assert_eq!(eq.bands[0].kind, EqBandKind::LowShelf);
        assert_eq!(eq.bands[5].kind, EqBandKind::HighShelf);
        // Bands ordered ascending by frequency — the chain UI relies on this.
        for w in eq.bands.windows(2) {
            assert!(w[0].freq_hz < w[1].freq_hz, "EQ bands must be ascending");
        }
    }

    #[test]
    fn audio_effect_kind_labels_are_stable() {
        assert_eq!(AudioEffectSpec::default_eq6().kind_label(), "EQ");
        assert_eq!(AudioEffectSpec::default_compressor().kind_label(), "Comp");
        assert_eq!(AudioEffectSpec::default_limiter().kind_label(), "Limit");
        assert_eq!(AudioEffectSpec::default_noise_gate().kind_label(), "Gate");
        assert_eq!(AudioEffectSpec::default_dehum().kind_label(), "DeHum");
    }

    #[test]
    fn goat8_effects_round_trip_through_ron() {
        // GOAT-8: the new gate/de-hum specs must survive the .vance RON path
        // and toggle without disturbing params.
        for mut e in [
            AudioEffectSpec::default_noise_gate(),
            AudioEffectSpec::default_dehum(),
        ] {
            let ron = ron::ser::to_string_pretty(&e, ron::ser::PrettyConfig::default()).unwrap();
            let back: AudioEffectSpec = ron::from_str(&ron).unwrap();
            assert_eq!(e, back, "spec survives RON round-trip");
            let snap = e.clone();
            e.set_enabled(false);
            e.set_enabled(true);
            assert_eq!(e, snap, "toggle preserves params");
        }
    }

    #[test]
    fn enabled_toggle_does_not_disturb_params() {
        let mut e = AudioEffectSpec::default_compressor();
        let snapshot = e.clone();
        e.set_enabled(false);
        assert!(!e.is_enabled());
        // Re-enabling restores the exact original spec.
        e.set_enabled(true);
        assert_eq!(e, snapshot);
    }

    #[test]
    fn audio_effect_round_trips_through_serde() {
        let chain = vec![
            AudioEffectSpec::default_eq6(),
            AudioEffectSpec::default_compressor(),
            AudioEffectSpec::default_limiter(),
        ];
        let s = serde_json::to_string(&chain).unwrap();
        let back: Vec<AudioEffectSpec> = serde_json::from_str(&s).unwrap();
        assert_eq!(chain, back);
    }
}
