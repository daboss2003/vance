//! Phase 8 (real export) — offline audio mixdown helpers.
//!
//! The live mixer ([`vance_audio::AudioMixer`]) is driven by the cpal
//! callback and can't be run headlessly. For export we re-decode each
//! audio clip and fold it with the SAME gain / pan / fade / per-track
//! DSP / master model the live fold thread uses — but offline, at the
//! export sample rate. The orchestration (decoding via the audio
//! sidecar) lives in `engine.rs::offline_audio_bounce`; this module
//! holds the pure, unit-testable resampling kernel.

/// Linear-interpolation resample of interleaved-stereo f32 PCM from
/// `src_rate` to `dst_rate`, producing exactly `dst_frames` output
/// frames. Quality is adequate for an export bounce (and a no-op when
/// the rates already match, which is the common 48 kHz case);
/// higher-quality polyphase resampling can replace it later without
/// changing callers.
pub fn resample_linear_stereo(
    src: &[f32],
    src_rate: u32,
    dst_rate: u32,
    dst_frames: usize,
) -> Vec<f32> {
    let mut out = vec![0.0f32; dst_frames * 2];
    let src_frames = src.len() / 2;
    if src_frames == 0 || dst_frames == 0 {
        return out;
    }
    // Fast path: identical rates → straight copy (clamped to dst_frames).
    if src_rate == dst_rate {
        let n = src_frames.min(dst_frames);
        out[..n * 2].copy_from_slice(&src[..n * 2]);
        return out;
    }
    let ratio = src_rate as f64 / dst_rate as f64;
    for j in 0..dst_frames {
        let pos = j as f64 * ratio;
        let i0 = pos.floor() as usize;
        if i0 >= src_frames {
            break; // ran past the source; remaining frames stay silent
        }
        let frac = (pos - i0 as f64) as f32;
        let i1 = (i0 + 1).min(src_frames - 1);
        let l0 = src[i0 * 2];
        let r0 = src[i0 * 2 + 1];
        let l1 = src[i1 * 2];
        let r1 = src[i1 * 2 + 1];
        out[j * 2] = l0 + (l1 - l0) * frac;
        out[j * 2 + 1] = r0 + (r1 - r0) * frac;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_rate_is_copy() {
        let src = vec![1.0, -1.0, 0.5, -0.5];
        let out = resample_linear_stereo(&src, 48_000, 48_000, 2);
        assert_eq!(out, src);
    }

    #[test]
    fn downsample_halves_frame_count() {
        // 4 source frames at 96k → 2 frames at 48k.
        let src = vec![0.0, 0.0, 1.0, 1.0, 2.0, 2.0, 3.0, 3.0];
        let out = resample_linear_stereo(&src, 96_000, 48_000, 2);
        assert_eq!(out.len(), 4);
        // frame 0 = src frame 0; frame 1 = src frame 2 (ratio 2.0).
        assert_eq!(out[0], 0.0);
        assert_eq!(out[2], 2.0);
    }

    #[test]
    fn upsample_interpolates_midpoints() {
        // 2 source frames at 24k → 4 frames at 48k. ratio = 0.5.
        let src = vec![0.0, 0.0, 1.0, 1.0];
        let out = resample_linear_stereo(&src, 24_000, 48_000, 4);
        assert_eq!(out.len(), 8);
        assert!((out[0] - 0.0).abs() < 1e-6);
        // frame 1 @ pos 0.5 → midpoint 0.5.
        assert!((out[2] - 0.5).abs() < 1e-6);
        assert!((out[4] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn empty_source_is_silence() {
        let out = resample_linear_stereo(&[], 44_100, 48_000, 8);
        assert_eq!(out, vec![0.0f32; 16]);
    }

    #[test]
    fn short_source_pads_with_silence() {
        // 1 source frame, ask for 4 output frames; the tail stays 0.
        let src = vec![1.0, 1.0];
        let out = resample_linear_stereo(&src, 48_000, 48_000, 4);
        assert_eq!(out[0], 1.0);
        assert_eq!(&out[2..], &[0.0; 6]);
    }
}
