//! `vance-time` — frame-accurate time conversions.
//!
//! Builds on `vance-core`'s `Rational`, `RationalTime`, `FrameRate`, and
//! `Timecode`. This crate adds:
//!
//! - **Drop-frame** ↔ frame-index conversions for the NTSC family
//!   (29.97, 59.94 fps). The classic "drop 2 frames every minute except every
//!   tenth minute" trick that keeps timecode aligned to wall-clock time.
//! - **Non-drop-frame (NDF)** conversions for integer rates.
//!
//! Reference: SMPTE ST 12-1.

#![warn(missing_docs)]

use vance_core::{Error, FrameRate, RationalTime, Result, Timecode};

pub use vance_core::nominal_fps;

/// Convert a `Timecode` to an absolute frame index.
///
/// Handles both NDF (non-drop-frame, integer rates) and drop-frame
/// (29.97 / 59.94 / 119.88 family).
pub fn timecode_to_frame(tc: Timecode) -> Result<i64> {
    let nominal = nominal_fps(tc.rate) as i64;
    let h = tc.hours as i64;
    let m = tc.minutes as i64;
    let s = tc.seconds as i64;
    let f = tc.frames as i64;

    if !tc.drop_frame {
        // NDF: simple base-N conversion.
        Ok(((h * 3600 + m * 60 + s) * nominal) + f)
    } else {
        // Drop-frame: SMPTE drop-frame algorithm. We "drop" `drop_per_min`
        // frame numbers at the start of every minute *except* every 10th
        // minute. The frame numbers themselves are dropped; the timecode
        // labels skip them but the underlying frame index does not.
        let drop_per_min = drop_frames_per_minute(tc.rate)?;
        let total_minutes = h * 60 + m;
        let frame_index = (h * 3600 + m * 60 + s) * nominal + f
            - drop_per_min * (total_minutes - total_minutes / 10);
        Ok(frame_index)
    }
}

/// Convert an absolute frame index to a `Timecode` at the given rate.
pub fn frame_to_timecode(frame: i64, rate: FrameRate, drop_frame: bool) -> Result<Timecode> {
    if frame < 0 {
        return Err(Error::invalid_input("negative frame index"));
    }
    let nominal = nominal_fps(rate) as i64;

    let (h, m, s, f) = if !drop_frame {
        let total_seconds = frame / nominal;
        let f = frame % nominal;
        let s = total_seconds % 60;
        let total_minutes = total_seconds / 60;
        let m = total_minutes % 60;
        let h = total_minutes / 60;
        (h, m, s, f)
    } else {
        // SMPTE drop-frame inverse. We re-introduce the dropped frame
        // numbers so that the (h, m, s, f) tuple's labels increment correctly.
        let drop_per_min = drop_frames_per_minute(rate)?;
        let frames_per_10min = nominal * 60 * 10 - drop_per_min * 9;
        let frames_per_min = nominal * 60 - drop_per_min;

        let d = frame / frames_per_10min;
        let m_in_10 = frame % frames_per_10min;

        // Within a 10-minute block: first minute is "long" (no drop), the
        // following nine minutes each drop `drop_per_min` frames.
        let mut adjusted = frame;
        if m_in_10 > drop_per_min {
            // Add back the dropped frame numbers from the per-minute drops
            // *within* this 10-minute group.
            adjusted +=
                drop_per_min * ((m_in_10 - drop_per_min) / frames_per_min) + drop_per_min * 9 * d;
        } else {
            adjusted += drop_per_min * 9 * d;
        }

        let f = adjusted % nominal;
        let total_seconds = adjusted / nominal;
        let s = total_seconds % 60;
        let total_minutes = total_seconds / 60;
        let m = total_minutes % 60;
        let h = total_minutes / 60;
        (h, m, s, f)
    };

    Timecode::new(h as u32, m as u32, s as u32, f as u32, drop_frame, rate)
}

/// Convert a `RationalTime` (seconds) to a `Timecode` at the given rate.
pub fn time_to_timecode(t: RationalTime, rate: FrameRate, drop_frame: bool) -> Result<Timecode> {
    let frame = t.to_frame_floor(rate)?;
    frame_to_timecode(frame, rate, drop_frame)
}

/// Convert a `Timecode` to a `RationalTime` (seconds since 00:00:00).
pub fn timecode_to_time(tc: Timecode) -> Result<RationalTime> {
    let frame = timecode_to_frame(tc)?;
    RationalTime::from_frame(frame, tc.rate)
}

/// How many frame numbers SMPTE drop-frame elides at the start of each
/// non-tenth minute, for a given drop-capable rate.
///
/// - 30000/1001 fps (29.97) drops 2 frames per minute (except every 10th).
/// - 60000/1001 fps (59.94) drops 4 frames per minute.
/// - 120000/1001 fps (119.88) drops 8 frames per minute (rare).
///
/// Returns `Err` for non-NTSC rates.
pub fn drop_frames_per_minute(rate: FrameRate) -> Result<i64> {
    if !rate.is_drop_frame_family() {
        return Err(Error::invalid_input(format!(
            "rate {rate} does not use drop-frame timecode"
        )));
    }
    let nominal = nominal_fps(rate) as i64;
    // Nominal of 30 → drop 2; 60 → drop 4; 120 → drop 8. Generalize: 2/30 of nominal.
    if nominal % 30 != 0 {
        return Err(Error::invalid_input(format!(
            "drop-frame nominal {nominal} not a multiple of 30"
        )));
    }
    Ok(nominal * 2 / 30)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use vance_core::{FrameRate, Timecode};

    // ---- NDF ----------------------------------------------------------------

    #[test]
    fn ndf_round_trip_24fps() {
        let rate = FrameRate::FPS_24;
        for frame in [0i64, 1, 23, 24, 25, 3600, 3601, 86_400, 86_401, 1_000_000] {
            let tc = frame_to_timecode(frame, rate, false).unwrap();
            assert_eq!(timecode_to_frame(tc).unwrap(), frame, "frame {frame}");
        }
    }

    #[test]
    fn ndf_round_trip_30fps() {
        let rate = FrameRate::FPS_30;
        for frame in [0i64, 29, 30, 31, 1799, 1800, 1801, 107_999, 108_000] {
            let tc = frame_to_timecode(frame, rate, false).unwrap();
            assert_eq!(timecode_to_frame(tc).unwrap(), frame, "frame {frame}");
        }
    }

    #[test]
    fn ndf_known_values() {
        let rate = FrameRate::FPS_24;
        // 1 hour at 24fps = 24 * 3600 = 86400 frames.
        let tc = frame_to_timecode(86400, rate, false).unwrap();
        assert_eq!(tc.to_string(), "01:00:00:00");
        // 23:59:59:23 at 24fps:
        let tc = Timecode::new(23, 59, 59, 23, false, rate).unwrap();
        assert_eq!(
            timecode_to_frame(tc).unwrap(),
            24 * (23 * 3600 + 59 * 60 + 59) + 23
        );
    }

    // ---- Drop-frame ---------------------------------------------------------

    #[test]
    fn drop_per_minute_values() {
        assert_eq!(drop_frames_per_minute(FrameRate::FPS_29_97).unwrap(), 2);
        assert_eq!(drop_frames_per_minute(FrameRate::FPS_59_94).unwrap(), 4);
        assert!(drop_frames_per_minute(FrameRate::FPS_30).is_err());
        assert!(drop_frames_per_minute(FrameRate::FPS_24).is_err());
    }

    #[test]
    fn dropframe_minute_boundary_29_97() {
        // At 00:00:59;29 (frame 1799), the next *frame* is 1800 — but its
        // *label* in DF is 00:01:00;02, not 00:01:00;00, because labels :00
        // and :01 are skipped at the start of every non-10th minute.
        // So 00:01:00;02 (DF) maps to frame 1800.
        let rate = FrameRate::FPS_29_97;
        let tc = Timecode::new(0, 1, 0, 2, true, rate).unwrap();
        let frame = timecode_to_frame(tc).unwrap();
        assert_eq!(frame, 1800);
    }

    #[test]
    fn dropframe_tenth_minute_no_drop_29_97() {
        // At 00:10:00;00, no drop is applied (tenth minute).
        let rate = FrameRate::FPS_29_97;
        let tc = Timecode::new(0, 10, 0, 0, true, rate).unwrap();
        let frame = timecode_to_frame(tc).unwrap();
        // 10 minutes' worth of nominal frames, minus 9 drops of 2 each
        // (minutes 1..9 dropped, minute 10 did not).
        assert_eq!(frame, 30 * 60 * 10 - 2 * 9);
    }

    #[test]
    fn dropframe_one_hour_29_97() {
        // After exactly one hour of wall-clock at 29.97 fps, the timecode
        // should read 01:00:00;00 (drop-frame keeps it aligned).
        // One hour = 3600 sec * 30000/1001 fps = 107892 + 3600/1001 ≈ 107892.108…
        // The number of frames at 01:00:00;00 (DF) is 107892.
        let rate = FrameRate::FPS_29_97;
        let tc = Timecode::new(1, 0, 0, 0, true, rate).unwrap();
        let frame = timecode_to_frame(tc).unwrap();
        assert_eq!(frame, 107892);
    }

    #[test]
    fn dropframe_round_trip_29_97() {
        let rate = FrameRate::FPS_29_97;
        let frames = [0i64, 1, 1797, 1798, 1799, 17981, 17982, 107_892, 215_784];
        for &f in &frames {
            let tc = frame_to_timecode(f, rate, true).unwrap();
            assert_eq!(timecode_to_frame(tc).unwrap(), f, "frame {f}");
        }
    }

    // ---- time_to_timecode / timecode_to_time --------------------------------

    #[test]
    fn time_to_timecode_at_24fps() {
        let t = RationalTime::new(1, 1).unwrap(); // 1 second
        let tc = time_to_timecode(t, FrameRate::FPS_24, false).unwrap();
        assert_eq!(tc.to_string(), "00:00:01:00");
    }

    #[test]
    fn timecode_to_time_at_29_97_df() {
        let tc = Timecode::new(1, 0, 0, 0, true, FrameRate::FPS_29_97).unwrap();
        let t = timecode_to_time(tc).unwrap();
        // 107892 frames at 30000/1001 fps = 107892 * 1001 / 30000 seconds.
        let expected = RationalTime::from_frame(107892, FrameRate::FPS_29_97).unwrap();
        assert_eq!(t, expected);
    }

    // ---- properties ---------------------------------------------------------

    proptest! {
        #[test]
        fn ndf_round_trip_property(frame in 0i64..200_000_000) {
            let rate = FrameRate::FPS_30;
            let tc = frame_to_timecode(frame, rate, false).unwrap();
            prop_assert_eq!(timecode_to_frame(tc).unwrap(), frame);
        }

        #[test]
        fn dropframe_round_trip_property(frame in 0i64..200_000_000) {
            let rate = FrameRate::FPS_29_97;
            let tc = frame_to_timecode(frame, rate, true).unwrap();
            prop_assert_eq!(timecode_to_frame(tc).unwrap(), frame);
        }
    }
}
