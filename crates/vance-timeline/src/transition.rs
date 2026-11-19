//! Transition placed at a cut between two adjacent clips on a track.
//!
//! v1 only models cross-dissolve. The transition has a duration and is
//! anchored at a cut point (the timeline position where one clip's
//! `timeline_out` equals the next clip's `timeline_in`). At render
//! time, the transition causes the engine to blend the two
//! surrounding clips over its duration: the outgoing clip fades out,
//! the incoming clip fades in.
//!
//! Transitions are stored *separately* from clips (on the `Track`), so
//! adding or removing a transition does not require rewriting the
//! clip stream. This matches the conceptual model "clips stay where
//! they are; the transition just changes how the cut renders" and
//! keeps the data model minimal.
//!
//! Boundary semantics:
//! - `position` is the cut's timeline position (i.e., the shared edge
//!   of the two clips).
//! - The transition extends `duration / 2` on each side: from
//!   `position - duration/2` to `position + duration/2`.
//! - The outgoing clip must extend at least `duration / 2` past the
//!   cut (otherwise the dissolve has nothing to fade from). v1
//!   tolerates clips that don't have that extra material — the
//!   missing frames render as the last available frame.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use vance_core::{Rational, RationalTime};

use crate::effect::EffectValue;
use crate::id::TransitionId;

/// What kind of transition this is. Drives which engine path renders
/// the transition window.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum TransitionKind {
    /// Linear cross-dissolve. Mix factor ramps 0 → 1 over the
    /// transition's duration, blended on the GPU via the renderer's
    /// `mix_factor` uniform.
    Dissolve,
    /// A WASM transition plugin (manifest `kind: Transition`). The string
    /// is the plugin manifest id (e.g. `com.vance.sample.dissolve`). The
    /// engine renders both clips to RGBA and calls the plugin's
    /// `process_transition(a, b, ...)`, feeding the linear progress as the
    /// `mix`/`progress` parameter.
    Wasm(String),
}

/// A transition anchored at a cut between two clips on a track.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transition {
    /// Stable ID.
    pub id: TransitionId,
    /// The kind of transition (dissolve, etc.). Drives the engine /
    /// renderer path used during the transition window.
    pub kind: TransitionKind,
    /// Cut position on the timeline. The transition's window is
    /// `[position - duration/2, position + duration/2]`.
    pub position: RationalTime,
    /// How long the transition lasts. Must be > 0.
    pub duration: RationalTime,
    /// Plugin parameter overrides for a [`TransitionKind::Wasm`]
    /// transition (keyed by manifest param key; values may be keyframed).
    /// Empty for built-in kinds. The engine supplies the linear progress
    /// as the `mix`/`progress` param and fills the rest from here (or the
    /// manifest defaults).
    #[serde(default)]
    pub params: BTreeMap<String, EffectValue>,
}

impl Transition {
    /// New dissolve at `position` of length `duration`.
    pub fn dissolve(position: RationalTime, duration: RationalTime) -> Self {
        Self {
            id: TransitionId::new(),
            kind: TransitionKind::Dissolve,
            position,
            duration,
            params: BTreeMap::new(),
        }
    }

    /// New WASM-plugin transition at `position` of length `duration`,
    /// driven by the plugin with manifest id `plugin_id`.
    pub fn wasm(
        plugin_id: impl Into<String>,
        position: RationalTime,
        duration: RationalTime,
    ) -> Self {
        Self {
            id: TransitionId::new(),
            kind: TransitionKind::Wasm(plugin_id.into()),
            position,
            duration,
            params: BTreeMap::new(),
        }
    }

    /// Half the transition's duration (used as the left/right
    /// extent around `position`).
    fn half_duration(&self) -> Rational {
        let half = Rational::new(1, 2).expect("1/2 is valid");
        self.duration.0.checked_mul(half).unwrap_or(Rational::ZERO)
    }

    /// Start of the transition's window on the timeline. Clamped at
    /// zero — transitions placed near t=0 (with a long duration)
    /// don't extend into negative time.
    pub fn window_in(&self) -> RationalTime {
        let half = self.half_duration();
        let start = self.position.0 - half;
        if start < Rational::ZERO {
            RationalTime::ZERO
        } else {
            RationalTime(start)
        }
    }

    /// End of the transition's window on the timeline.
    pub fn window_out(&self) -> RationalTime {
        let half = self.half_duration();
        RationalTime(self.position.0 + half)
    }

    /// `true` iff the given timeline time falls inside this
    /// transition's window (inclusive at both ends).
    pub fn contains(&self, t: RationalTime) -> bool {
        t >= self.window_in() && t <= self.window_out()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(num: i64) -> RationalTime {
        RationalTime::new(num, 1).unwrap()
    }

    #[test]
    fn window_is_symmetric_around_position() {
        // 2s transition centered at 10s → window [9, 11]
        let tr = Transition::dissolve(t(10), t(2));
        assert_eq!(tr.window_in(), t(9));
        assert_eq!(tr.window_out(), t(11));
    }

    #[test]
    fn contains_is_inclusive() {
        let tr = Transition::dissolve(t(10), t(2));
        assert!(tr.contains(t(9)));
        assert!(tr.contains(t(11)));
        assert!(tr.contains(t(10)));
        assert!(!tr.contains(RationalTime::new(89, 10).unwrap()));
    }

    #[test]
    fn window_in_clamps_at_zero() {
        // 4s transition centered at 1s would put window_in at -1; clamp to 0.
        let tr = Transition::dissolve(t(1), t(4));
        assert_eq!(tr.window_in(), RationalTime::ZERO);
    }
}
