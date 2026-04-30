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

/// A loosely-typed parameter value. Just enough to serialize a stub
/// effect today without committing to the eventual schema.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EffectValue {
    /// A plain number.
    Number(f64),
    /// A boolean toggle.
    Bool(bool),
    /// A free-form string.
    String(String),
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
