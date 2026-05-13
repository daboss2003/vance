//! Effect placeholder.
//!
//! v1 effects (the actual transform graph + parameter system) land in
//! Phase 5. This crate just defines the shape of an effect *reference*
//! so clips and tracks can carry a serializable list of them today —
//! when Phase 5 lands, it fills in the parameters and binds each
//! `Effect.kind` to a real implementation.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::id::EffectId;

/// A placeholder for an effect's typed parameters. v1 will replace this
/// with a typed parameter tree; we keep it as a string-keyed map for
/// now so projects from this milestone forward-migrate without losing
/// data.
pub type EffectParams = BTreeMap<String, EffectValue>;

/// A loosely-typed parameter value. Numbers can also be animated
/// via a `Keyframes` list — each entry pins a value at a
/// clip-relative time, evaluated by linear interpolation between
/// surrounding keyframes (5.9 will add bezier).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EffectValue {
    /// A plain number.
    Number(f64),
    /// A boolean toggle.
    Bool(bool),
    /// A free-form string.
    String(String),
    /// Animated numeric value — a list of keyframes in ascending
    /// time order, interpolated between samples.
    Keyframes(Vec<Keyframe>),
}

/// One keyframe pinning a numeric value at a specific clip-relative
/// time (seconds from the clip's `source_in`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Keyframe {
    pub time_seconds: f64,
    pub value: f64,
}

impl EffectValue {
    /// Evaluate this parameter as a number at clip-relative time
    /// `t_seconds`. `Number` returns its constant value; `Keyframes`
    /// linearly interpolates between the nearest neighbors,
    /// clamping at the endpoints. `Bool`/`String` return `None`.
    pub fn eval_number(&self, t_seconds: f64) -> Option<f64> {
        match self {
            EffectValue::Number(v) => Some(*v),
            EffectValue::Keyframes(kfs) => Some(eval_keyframes(kfs, t_seconds)),
            _ => None,
        }
    }
}

fn eval_keyframes(kfs: &[Keyframe], t: f64) -> f64 {
    if kfs.is_empty() {
        return 0.0;
    }
    if kfs.len() == 1 || t <= kfs[0].time_seconds {
        return kfs[0].value;
    }
    if t >= kfs[kfs.len() - 1].time_seconds {
        return kfs[kfs.len() - 1].value;
    }
    // Find the segment containing t.
    for i in 0..kfs.len() - 1 {
        let a = kfs[i];
        let b = kfs[i + 1];
        if t >= a.time_seconds && t <= b.time_seconds {
            let span = b.time_seconds - a.time_seconds;
            if span <= 0.0 {
                return a.value;
            }
            let frac = (t - a.time_seconds) / span;
            return a.value + (b.value - a.value) * frac;
        }
    }
    kfs[kfs.len() - 1].value
}

/// One effect attached to a clip or track.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Effect {
    /// Stable ID.
    pub id: EffectId,
    /// Kind identifier — e.g. `"transform"`, `"opacity"`, `"audio.gain"`.
    /// Looked up against the effect registry once Phase 5 lands.
    pub kind: String,
    /// Whether the effect contributes to the render. Disabled effects
    /// stay in the project for one-click re-enable.
    pub enabled: bool,
    /// Free-form parameter map for now (see module docs).
    pub params: EffectParams,
}

impl Effect {
    /// Create a new effect with a fresh ID, no parameters, enabled.
    pub fn new(kind: impl Into<String>) -> Self {
        Self {
            id: EffectId::new(),
            kind: kind.into(),
            enabled: true,
            params: EffectParams::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_effect_is_enabled_with_no_params() {
        let e = Effect::new("opacity");
        assert!(e.enabled);
        assert!(e.params.is_empty());
        assert_eq!(e.kind, "opacity");
    }
}
