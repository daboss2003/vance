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
use vance_core::{Rational, RationalTime};

use crate::id::TransitionId;

/// What kind of transition this is. Drives which renderer pass is
/// used at the engine layer. v1 only ships `Dissolve` (linear
/// cross-dissolve via the renderer's `mix_factor` uniform).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum TransitionKind {
    /// Linear cross-dissolve. Mix factor ramps 0 → 1 over the
    /// transition's duration.
    Dissolve,
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
}

impl Transition {
    /// New dissolve at `position` of length `duration`.
    pub fn dissolve(position: RationalTime, duration: RationalTime) -> Self {
        Self {
            id: TransitionId::new(),
            kind: TransitionKind::Dissolve,
            position,
            duration,
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
