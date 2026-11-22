//! Per-clip blend modes for multi-track compositing (ADR 0007).
//!
//! The discriminant order is frozen: it is the id the GPU composite pass
//! switches on ([`BlendMode::gpu_id`]), so new modes may only be appended.
//!
//! Serialized as a snake_case string (`"soft_light"`) in every format.
//! Deserializing accepts any string and maps unknown names to
//! [`BlendMode::Normal`], so a bundle written by a newer build that knows
//! more modes still opens.

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// How a clip's layer combines with the layers below it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum BlendMode {
    /// Source over backdrop.
    #[default]
    Normal,
    /// Backdrop + source.
    Add,
    /// Backdrop × source.
    Multiply,
    /// Inverse of multiplying the inverses.
    Screen,
    /// Hard light with backdrop and source swapped.
    Overlay,
    /// Per-channel minimum.
    Darken,
    /// Per-channel maximum.
    Lighten,
    /// Absolute per-channel difference.
    Difference,
    /// W3C soft light.
    SoftLight,
    /// W3C hard light.
    HardLight,
    /// Brightens the backdrop to reflect the source.
    ColorDodge,
    /// Darkens the backdrop to reflect the source.
    ColorBurn,
}

impl BlendMode {
    /// Every mode, in `gpu_id` order.
    pub const ALL: [BlendMode; 12] = [
        BlendMode::Normal,
        BlendMode::Add,
        BlendMode::Multiply,
        BlendMode::Screen,
        BlendMode::Overlay,
        BlendMode::Darken,
        BlendMode::Lighten,
        BlendMode::Difference,
        BlendMode::SoftLight,
        BlendMode::HardLight,
        BlendMode::ColorDodge,
        BlendMode::ColorBurn,
    ];

    /// The id the composite shader switches on (the discriminant).
    pub fn gpu_id(self) -> u32 {
        self as u32
    }

    /// Stable snake_case name, used for serialization and the IPC layer.
    pub fn as_str(self) -> &'static str {
        match self {
            BlendMode::Normal => "normal",
            BlendMode::Add => "add",
            BlendMode::Multiply => "multiply",
            BlendMode::Screen => "screen",
            BlendMode::Overlay => "overlay",
            BlendMode::Darken => "darken",
            BlendMode::Lighten => "lighten",
            BlendMode::Difference => "difference",
            BlendMode::SoftLight => "soft_light",
            BlendMode::HardLight => "hard_light",
            BlendMode::ColorDodge => "color_dodge",
            BlendMode::ColorBurn => "color_burn",
        }
    }

    /// Parse a mode name. Case, `_`, `-` and spaces are ignored, so
    /// `"soft_light"`, `"SoftLight"` and `"soft-light"` all match. Unknown
    /// names map to [`BlendMode::Normal`].
    pub fn parse_lossy(s: &str) -> BlendMode {
        let key = normalize(s);
        BlendMode::ALL
            .into_iter()
            .find(|m| normalize(m.as_str()) == key)
            .unwrap_or_default()
    }
}

fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, '_' | '-' | ' '))
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

impl Serialize for BlendMode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for BlendMode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(BlendModeVisitor)
    }
}

struct BlendModeVisitor;

impl<'de> Visitor<'de> for BlendModeVisitor {
    type Value = BlendMode;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a blend mode name")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<BlendMode, E> {
        Ok(BlendMode::parse_lossy(v))
    }

    // Intentional: a bare RON identifier (`blend_mode: Screen`) reaches
    // `deserialize_any` as a unit — ron 0.8 consumes the identifier and drops
    // its text — so it loads as Normal instead of failing the whole bundle.
    // We always write the quoted string form, which round-trips exactly.
    fn visit_unit<E: de::Error>(self) -> Result<BlendMode, E> {
        Ok(BlendMode::Normal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Wrap {
        m: BlendMode,
    }

    #[test]
    fn order_matches_gpu_ids_and_all() {
        for (i, m) in BlendMode::ALL.iter().enumerate() {
            assert_eq!(m.gpu_id(), i as u32, "{m:?}");
        }
        assert_eq!(BlendMode::Normal.gpu_id(), 0);
        assert_eq!(BlendMode::Screen.gpu_id(), 3);
        assert_eq!(BlendMode::SoftLight.gpu_id(), 8);
        assert_eq!(BlendMode::ColorBurn.gpu_id(), 11);
        assert_eq!(BlendMode::default(), BlendMode::Normal);
    }

    #[test]
    fn names_round_trip_through_parse_lossy() {
        for m in BlendMode::ALL {
            assert_eq!(BlendMode::parse_lossy(m.as_str()), m);
        }
        assert_eq!(BlendMode::parse_lossy("SoftLight"), BlendMode::SoftLight);
        assert_eq!(BlendMode::parse_lossy("color-dodge"), BlendMode::ColorDodge);
        assert_eq!(BlendMode::parse_lossy("bogus"), BlendMode::Normal);
        assert_eq!(BlendMode::parse_lossy(""), BlendMode::Normal);
    }

    #[test]
    fn json_is_a_snake_case_string() {
        let j = serde_json::to_string(&BlendMode::HardLight).unwrap();
        assert_eq!(j, "\"hard_light\"");
        for m in BlendMode::ALL {
            let back: BlendMode =
                serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
            assert_eq!(back, m);
        }
        let unknown: BlendMode = serde_json::from_str("\"vivid_light\"").unwrap();
        assert_eq!(unknown, BlendMode::Normal);
    }

    #[test]
    fn ron_writes_a_string_and_round_trips() {
        let w = Wrap {
            m: BlendMode::ColorBurn,
        };
        let s = ron::to_string(&w).unwrap();
        assert!(s.contains("\"color_burn\""), "{s}");
        let back: Wrap = ron::from_str(&s).unwrap();
        assert_eq!(back, w);
        let unknown: Wrap = ron::from_str("(m: \"vivid_light\")").unwrap();
        assert_eq!(unknown.m, BlendMode::Normal);
    }

    #[test]
    fn ron_identifier_loads_without_failing() {
        // Pins the Intentional on `visit_unit`: ron 0.8 drops the identifier
        // text, so the value is lost but the bundle still opens.
        let w: Wrap = ron::from_str("(m: Screen)").unwrap();
        assert_eq!(w.m, BlendMode::Normal);
    }
}
