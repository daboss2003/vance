//! Frame-accurate time types.
//!
//! `RationalTime` is a duration in seconds expressed exactly. `FrameRate` is
//! a positive rational frames-per-second. `Timecode` is a wall-clock-style
//! HH:MM:SS:FF (or HH:MM:SS;FF for drop-frame) representation.
//!
//! Drop-frame conversions and other rate-specific math live in `vance-time` —
//! this crate just defines the types.

use crate::{Error, Rational, Result};
use core::fmt;

/// A duration, in seconds, stored exactly as a rational number.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct RationalTime(pub Rational);

impl RationalTime {
    /// `0/1` seconds.
    pub const ZERO: RationalTime = RationalTime(Rational::ZERO);

    /// Construct from raw `(num, den)` seconds.
    pub fn new(num: i64, den: i64) -> Result<Self> {
        Ok(RationalTime(Rational::new(num, den)?))
    }

    /// Construct from a frame index at a given rate. `index / rate`.
    pub fn from_frame(index: i64, rate: FrameRate) -> Result<Self> {
        let r = Rational::new(index, 1)?.checked_mul(rate.0.recip()?)?;
        Ok(RationalTime(r))
    }

    /// Floor-convert to a frame index at the given rate. `floor(self * rate)`.
    pub fn to_frame_floor(self, rate: FrameRate) -> Result<i64> {
        let prod = self.0.checked_mul(rate.0)?;
        // floor(num / den) for positive den.
        let (n, d) = (prod.numer(), prod.denom());
        let q = n.div_euclid(d);
        Ok(q)
    }

    /// Round-half-up convert to a frame index at the given rate.
    pub fn to_frame_round(self, rate: FrameRate) -> Result<i64> {
        let prod = self.0.checked_mul(rate.0)?;
        let (n, d) = (prod.numer(), prod.denom());
        // Add d/2 then floor: (n + d/2) div d, but careful with negatives.
        let half = d / 2;
        let bumped = if n >= 0 { n + half } else { n - half };
        Ok(bumped.div_euclid(d))
    }
}

impl fmt::Display for RationalTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}s", self.0)
    }
}

/// A positive rational frame rate (frames per second).
///
/// Common values: `FrameRate::FPS_24`, `FrameRate::FPS_25`,
/// `FrameRate::FPS_30`, `FrameRate::FPS_23_976`, `FrameRate::FPS_29_97`,
/// `FrameRate::FPS_59_94`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct FrameRate(pub Rational);

impl FrameRate {
    /// 24 fps (cinema).
    pub const FPS_24: FrameRate = FrameRate(Rational { num: 24, den: 1 });
    /// 25 fps (PAL).
    pub const FPS_25: FrameRate = FrameRate(Rational { num: 25, den: 1 });
    /// 30 fps (NTSC integer).
    pub const FPS_30: FrameRate = FrameRate(Rational { num: 30, den: 1 });
    /// 50 fps (PAL high).
    pub const FPS_50: FrameRate = FrameRate(Rational { num: 50, den: 1 });
    /// 60 fps.
    pub const FPS_60: FrameRate = FrameRate(Rational { num: 60, den: 1 });
    /// 23.976 fps = 24000/1001.
    pub const FPS_23_976: FrameRate = FrameRate(Rational {
        num: 24000,
        den: 1001,
    });
    /// 29.97 fps = 30000/1001 (NTSC).
    pub const FPS_29_97: FrameRate = FrameRate(Rational {
        num: 30000,
        den: 1001,
    });
    /// 59.94 fps = 60000/1001.
    pub const FPS_59_94: FrameRate = FrameRate(Rational {
        num: 60000,
        den: 1001,
    });

    /// Construct from `(num, den)`. Both must be positive.
    pub fn new(num: i64, den: i64) -> Result<Self> {
        if num <= 0 || den <= 0 {
            return Err(Error::invalid_input("FrameRate must be positive"));
        }
        Ok(FrameRate(Rational::new(num, den)?))
    }

    /// Returns true if this rate is in the NTSC family (denominator divisible
    /// by 1001) and therefore uses drop-frame timecode by convention.
    pub fn is_drop_frame_family(self) -> bool {
        self.0.denom() % 1001 == 0
    }

    /// Frame duration in seconds (1/rate).
    pub fn frame_duration(self) -> RationalTime {
        // rate is positive, so recip is safe.
        RationalTime(self.0.recip().expect("FrameRate guaranteed nonzero"))
    }
}

impl fmt::Display for FrameRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} fps", self.0)
    }
}

/// SMPTE timecode representation. `HH:MM:SS:FF` for non-drop-frame,
/// `HH:MM:SS;FF` for drop-frame (semicolon between SS and FF).
///
/// A `Timecode` value carries the rate it was authored at; conversions to a
/// frame index require that rate.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Timecode {
    /// Hours (0..=23 typically; not modulo'd).
    pub hours: u32,
    /// Minutes (0..=59).
    pub minutes: u32,
    /// Seconds (0..=59).
    pub seconds: u32,
    /// Frames (0 .. nominal_rate).
    pub frames: u32,
    /// True if the source timecode used drop-frame conventions.
    pub drop_frame: bool,
    /// The frame rate this timecode was authored at.
    pub rate: FrameRate,
}

impl Timecode {
    /// Construct, returning `Err` if the field values are out of range for the rate.
    pub fn new(
        hours: u32,
        minutes: u32,
        seconds: u32,
        frames: u32,
        drop_frame: bool,
        rate: FrameRate,
    ) -> Result<Self> {
        if minutes > 59 || seconds > 59 {
            return Err(Error::invalid_input(format!(
                "minutes/seconds out of range in {hours:02}:{minutes:02}:{seconds:02}:{frames:02}"
            )));
        }
        // Nominal frames-per-second is the rate rounded to nearest integer.
        let nominal = nominal_fps(rate);
        if frames >= nominal {
            return Err(Error::invalid_input(format!(
                "frame field {frames} >= nominal rate {nominal}"
            )));
        }
        Ok(Self {
            hours,
            minutes,
            seconds,
            frames,
            drop_frame,
            rate,
        })
    }
}

impl fmt::Display for Timecode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sep = if self.drop_frame { ';' } else { ':' };
        write!(
            f,
            "{:02}:{:02}:{:02}{}{:02}",
            self.hours, self.minutes, self.seconds, sep, self.frames
        )
    }
}

/// The nominal integer frames-per-second for SMPTE field math (e.g. 30 for 29.97).
pub fn nominal_fps(rate: FrameRate) -> u32 {
    let f = rate.0.as_f64().round();
    f as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_rate_constants() {
        assert_eq!(FrameRate::FPS_29_97.0.numer(), 30000);
        assert_eq!(FrameRate::FPS_29_97.0.denom(), 1001);
        assert!(FrameRate::FPS_29_97.is_drop_frame_family());
        assert!(!FrameRate::FPS_30.is_drop_frame_family());
    }

    #[test]
    fn nominal_fps_for_common_rates() {
        assert_eq!(nominal_fps(FrameRate::FPS_24), 24);
        assert_eq!(nominal_fps(FrameRate::FPS_23_976), 24);
        assert_eq!(nominal_fps(FrameRate::FPS_29_97), 30);
        assert_eq!(nominal_fps(FrameRate::FPS_30), 30);
        assert_eq!(nominal_fps(FrameRate::FPS_59_94), 60);
    }

    #[test]
    fn frame_round_trip_at_integer_rate() {
        let rate = FrameRate::FPS_24;
        for i in [0i64, 1, 23, 24, 25, 100, 1_000_000] {
            let t = RationalTime::from_frame(i, rate).unwrap();
            assert_eq!(t.to_frame_floor(rate).unwrap(), i, "frame {i}");
            assert_eq!(t.to_frame_round(rate).unwrap(), i, "frame {i}");
        }
    }

    #[test]
    fn frame_round_trip_at_ntsc_rate() {
        let rate = FrameRate::FPS_29_97;
        // After 1001 seconds, frame index should be exactly 30000.
        let t = RationalTime::new(1001, 1).unwrap();
        assert_eq!(t.to_frame_floor(rate).unwrap(), 30000);
    }

    #[test]
    fn frame_duration_correct() {
        let d = FrameRate::FPS_24.frame_duration();
        assert_eq!(d.0, Rational::new(1, 24).unwrap());
        let d = FrameRate::FPS_29_97.frame_duration();
        assert_eq!(d.0, Rational::new(1001, 30000).unwrap());
    }

    #[test]
    fn timecode_format() {
        let tc = Timecode::new(1, 2, 3, 4, false, FrameRate::FPS_24).unwrap();
        assert_eq!(tc.to_string(), "01:02:03:04");
        let tc = Timecode::new(1, 2, 3, 4, true, FrameRate::FPS_29_97).unwrap();
        assert_eq!(tc.to_string(), "01:02:03;04");
    }

    #[test]
    fn timecode_rejects_out_of_range() {
        assert!(Timecode::new(0, 60, 0, 0, false, FrameRate::FPS_24).is_err());
        assert!(Timecode::new(0, 0, 60, 0, false, FrameRate::FPS_24).is_err());
        assert!(Timecode::new(0, 0, 0, 24, false, FrameRate::FPS_24).is_err());
        assert!(Timecode::new(0, 0, 0, 30, false, FrameRate::FPS_29_97).is_err());
    }

    #[test]
    fn invalid_frame_rate() {
        assert!(FrameRate::new(0, 1).is_err());
        assert!(FrameRate::new(-24, 1).is_err());
        assert!(FrameRate::new(24, 0).is_err());
    }
}
