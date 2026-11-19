//! Plugin manifest + parameter schema.
//!
//! Every plugin declares a [`PluginManifest`] (serialized as RON) that
//! tells the host its name, kind (effect vs transition), and the
//! parameters it exposes to the UI. The host reads this once at load
//! time via the [`crate::abi::EXPORT_MANIFEST`] export, then renders a
//! parameter panel + marshals param values into the packed `f32`
//! array the process functions consume.
//!
//! RON example:
//! ```ron
//! PluginManifest(
//!     id: "com.example.stylize",
//!     name: "Stylize",
//!     version: "1.0.0",
//!     author: "Example",
//!     kind: Effect,
//!     params: [
//!         ParamSpec(key: "amount", label: "Amount", min: 0.0, max: 1.0, default: 0.5),
//!     ],
//! )
//! ```

use serde::{Deserialize, Serialize};

/// What a plugin does — determines which process export the host
/// calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PluginKind {
    /// Single-frame effect — host calls [`crate::abi::EXPORT_PROCESS`].
    Effect,
    /// Two-frame transition — host calls [`crate::abi::EXPORT_PROCESS2`].
    /// A `mix` parameter (0 = pure A, 1 = pure B) is always implicitly
    /// available even if not declared.
    Transition,
}

/// One tunable parameter exposed to the host UI + passed into the
/// guest's process function as a packed `f32`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ParamSpec {
    /// Stable machine key. Determines the param's index in the packed
    /// `f32` array (declaration order).
    pub key: String,
    /// Human-readable label for the UI.
    pub label: String,
    /// Inclusive minimum.
    pub min: f32,
    /// Inclusive maximum.
    pub max: f32,
    /// Default value.
    pub default: f32,
}

/// GPU resources a plugin declares it needs.
///
/// The host runs the CPU `process` path today; a plugin that sets
/// `requires_gpu` is rejected at load (rather than silently mis-run on the
/// CPU path) until GPU plugin execution lands. Declaring `textures` /
/// `shader_entry` without `requires_gpu` is a forward-compatible hint and
/// does not change behaviour.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GpuResources {
    /// The plugin's process runs as a GPU shader rather than the CPU path.
    #[serde(default)]
    pub requires_gpu: bool,
    /// Named storage textures the plugin would bind (declaration only).
    #[serde(default)]
    pub textures: Vec<String>,
    /// WGSL entry-point name for the plugin's compute/fragment stage.
    #[serde(default)]
    pub shader_entry: Option<String>,
}

/// Top-level plugin manifest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginManifest {
    /// Reverse-DNS unique id (e.g. `com.example.stylize`). The host
    /// uses this to de-duplicate + reference the plugin in projects.
    pub id: String,
    /// Display name for the effects browser.
    pub name: String,
    /// Semantic version string (informational).
    pub version: String,
    /// Author / vendor string.
    pub author: String,
    /// Effect or transition.
    pub kind: PluginKind,
    /// Parameters in declaration order. The order is the index order
    /// of the packed `f32` array the host sends to the guest.
    #[serde(default)]
    pub params: Vec<ParamSpec>,
    /// GPU resources the plugin declares (optional). Absent for the
    /// CPU-path plugins the host runs today.
    #[serde(default)]
    pub gpu: Option<GpuResources>,
}

/// Error parsing a manifest.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// RON deserialization failed.
    #[error("manifest ron parse: {0}")]
    Ron(String),
    /// Manifest violated an invariant (empty id, duplicate param key, …).
    #[error("manifest invalid: {0}")]
    Invalid(String),
}

impl PluginManifest {
    /// Parse + validate a manifest from a RON string.
    pub fn from_ron(s: &str) -> Result<Self, ManifestError> {
        let m: PluginManifest = ron::from_str(s).map_err(|e| ManifestError::Ron(e.to_string()))?;
        m.validate()?;
        Ok(m)
    }

    /// Serialize to a pretty RON string (used by the guest macro).
    pub fn to_ron(&self) -> String {
        ron::ser::to_string_pretty(self, ron::ser::PrettyConfig::default())
            .unwrap_or_else(|_| String::new())
    }

    /// Validate invariants: non-empty id/name, unique param keys,
    /// sane ranges.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.id.trim().is_empty() {
            return Err(ManifestError::Invalid("empty id".into()));
        }
        if self.name.trim().is_empty() {
            return Err(ManifestError::Invalid("empty name".into()));
        }
        let mut seen = std::collections::HashSet::new();
        for p in &self.params {
            if p.key.trim().is_empty() {
                return Err(ManifestError::Invalid("empty param key".into()));
            }
            if !seen.insert(p.key.as_str()) {
                return Err(ManifestError::Invalid(format!(
                    "duplicate param key `{}`",
                    p.key
                )));
            }
            // NaN bounds are rejected along with min > max.
            if p.min > p.max || p.min.is_nan() || p.max.is_nan() {
                return Err(ManifestError::Invalid(format!(
                    "param `{}` has min > max",
                    p.key
                )));
            }
            if !(p.default >= p.min && p.default <= p.max) {
                return Err(ManifestError::Invalid(format!(
                    "param `{}` default out of range",
                    p.key
                )));
            }
        }
        if let Some(gpu) = &self.gpu {
            let mut tex = std::collections::HashSet::new();
            for name in &gpu.textures {
                if name.trim().is_empty() {
                    return Err(ManifestError::Invalid("empty gpu texture name".into()));
                }
                if !tex.insert(name.as_str()) {
                    return Err(ManifestError::Invalid(format!(
                        "duplicate gpu texture `{name}`"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Build the packed `f32` parameter array from a map of
    /// `key → value`, in declaration order. Missing keys fall back to
    /// the param's `default`; values are clamped to `[min, max]`.
    pub fn pack_params(&self, values: &dyn Fn(&str) -> Option<f32>) -> Vec<f32> {
        self.params
            .iter()
            .map(|p| {
                let v = values(&p.key).unwrap_or(p.default);
                v.clamp(p.min, p.max)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"PluginManifest(
        id: "com.example.stylize",
        name: "Stylize",
        version: "1.0.0",
        author: "Example",
        kind: Effect,
        params: [
            ParamSpec(key: "amount", label: "Amount", min: 0.0, max: 1.0, default: 0.5),
            ParamSpec(key: "gamma", label: "Gamma", min: 0.1, max: 4.0, default: 1.0),
        ],
    )"#;

    #[test]
    fn parse_sample_manifest() {
        let m = PluginManifest::from_ron(SAMPLE).expect("parse");
        assert_eq!(m.id, "com.example.stylize");
        assert_eq!(m.kind, PluginKind::Effect);
        assert_eq!(m.params.len(), 2);
        assert_eq!(m.params[0].key, "amount");
    }

    #[test]
    fn round_trip_ron() {
        let m = PluginManifest::from_ron(SAMPLE).unwrap();
        let s = m.to_ron();
        let back = PluginManifest::from_ron(&s).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn gpu_resources_optional_and_default_none() {
        // A CPU manifest (no `gpu` field) parses with gpu = None.
        assert!(PluginManifest::from_ron(SAMPLE).unwrap().gpu.is_none());
    }

    #[test]
    fn gpu_resources_parse_and_round_trip() {
        let src = r#"PluginManifest(
            id: "com.example.glow", name: "Glow", version: "1", author: "a", kind: Effect,
            params: [],
            gpu: Some(GpuResources(
                requires_gpu: true,
                textures: ["src", "bloom"],
                shader_entry: Some("main"),
            )),
        )"#;
        let m = PluginManifest::from_ron(src).expect("parse gpu manifest");
        let gpu = m.gpu.as_ref().expect("gpu present");
        assert!(gpu.requires_gpu);
        assert_eq!(gpu.textures, vec!["src", "bloom"]);
        assert_eq!(gpu.shader_entry.as_deref(), Some("main"));
        // Round-trips through serialization.
        assert_eq!(PluginManifest::from_ron(&m.to_ron()).unwrap(), m);
    }

    #[test]
    fn gpu_rejects_duplicate_or_empty_textures() {
        let dup = r#"PluginManifest(
            id: "x", name: "X", version: "1", author: "a", kind: Effect, params: [],
            gpu: Some(GpuResources(requires_gpu: false, textures: ["t", "t"])),
        )"#;
        assert!(PluginManifest::from_ron(dup).is_err());
        let empty = r#"PluginManifest(
            id: "x", name: "X", version: "1", author: "a", kind: Effect, params: [],
            gpu: Some(GpuResources(requires_gpu: false, textures: [""])),
        )"#;
        assert!(PluginManifest::from_ron(empty).is_err());
    }

    #[test]
    fn rejects_empty_id() {
        let bad = SAMPLE.replace("com.example.stylize", "");
        assert!(PluginManifest::from_ron(&bad).is_err());
    }

    #[test]
    fn rejects_duplicate_param_keys() {
        let bad = r#"PluginManifest(
            id: "x", name: "X", version: "1", author: "a", kind: Effect,
            params: [
                ParamSpec(key: "k", label: "K", min: 0.0, max: 1.0, default: 0.0),
                ParamSpec(key: "k", label: "K2", min: 0.0, max: 1.0, default: 0.0),
            ],
        )"#;
        assert!(PluginManifest::from_ron(bad).is_err());
    }

    #[test]
    fn rejects_default_out_of_range() {
        let bad = r#"PluginManifest(
            id: "x", name: "X", version: "1", author: "a", kind: Effect,
            params: [ ParamSpec(key: "k", label: "K", min: 0.0, max: 1.0, default: 5.0) ],
        )"#;
        assert!(PluginManifest::from_ron(bad).is_err());
    }

    #[test]
    fn pack_params_clamps_and_defaults() {
        let m = PluginManifest::from_ron(SAMPLE).unwrap();
        // amount provided over-range → clamps to 1.0; gamma missing → default 1.0.
        let packed = m.pack_params(&|k| if k == "amount" { Some(99.0) } else { None });
        assert_eq!(packed, vec![1.0, 1.0]);
    }
}
