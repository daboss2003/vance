//! Phase 8.2 — end-to-end exercise of the sidecar.
//!
//! Spawns the sidecar binary, encodes a 1-second 64×64 test pattern
//! to ProRes 422 HQ + PCM audio, and asserts the output file exists,
//! is non-trivial in size, and probes as ProRes via `ffprobe`. The
//! test skips itself with `eprintln!` if `ffmpeg` / `ffprobe` are
//! missing on PATH so CI machines without FFmpeg don't fail.

use std::path::PathBuf;
use std::process::Command;
use vance_encoder::protocol::{
    AudioCodec, AudioConfig, AudioSampleFormat, EncoderPreset, VideoConfig, VideoPixelFormat,
};
use vance_encoder::EncoderClient;

fn ffmpeg_present() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok()
}

fn ffprobe_present() -> bool {
    Command::new("ffprobe").arg("-version").output().is_ok()
}

fn sidecar_binary() -> Option<PathBuf> {
    // Cargo puts integration-test deps' binaries in target/<profile>/.
    // CARGO_BIN_EXE_<name> is set by cargo when the binary is part of
    // the same crate's [[bin]] target.
    if let Some(p) = option_env!("CARGO_BIN_EXE_vance-encoder") {
        return Some(PathBuf::from(p));
    }
    let candidates = [
        PathBuf::from("target/debug/vance-encoder"),
        PathBuf::from("target/release/vance-encoder"),
    ];
    for c in candidates {
        if c.exists() {
            return Some(c);
        }
    }
    None
}

#[test]
fn prores_422_hq_one_second_round_trip() {
    if !ffmpeg_present() || !ffprobe_present() {
        eprintln!("ffmpeg / ffprobe not on PATH — skipping prores e2e test");
        return;
    }
    let Some(binary) = sidecar_binary() else {
        eprintln!("vance-encoder binary not built — skipping prores e2e test");
        return;
    };

    let temp = std::env::temp_dir().join(format!(
        "vance-prores-test-{}.mov",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&temp);

    let mut client = EncoderClient::spawn(&binary).expect("sidecar spawn");
    assert!(!client.available_presets().is_empty());

    let width = 64u32;
    let height = 64u32;
    let frame_rate_num = 30u32;
    let frame_rate_den = 1u32;
    let sample_rate = 48_000u32;

    client
        .open(
            temp.to_string_lossy().to_string(),
            Some(VideoConfig {
                width,
                height,
                frame_rate_num,
                frame_rate_den,
                pixel_format: VideoPixelFormat::Rgba8,
                preset: EncoderPreset::ProRes422HQ,
                time_base_num: frame_rate_den,
                time_base_den: frame_rate_num,
            }),
            Some(AudioConfig {
                sample_rate,
                channels: 2,
                sample_format: AudioSampleFormat::F32Interleaved,
                codec: AudioCodec::PcmS24,
                time_base_num: 1,
                time_base_den: sample_rate,
            }),
        )
        .expect("open");

    // Push 30 frames of a moving-stripe pattern.
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    for f in 0..30 {
        for y in 0..height as usize {
            for x in 0..width as usize {
                let i = (y * width as usize + x) * 4;
                let stripe = ((x + f as usize) / 4) as u8;
                pixels[i] = stripe.wrapping_mul(17);
                pixels[i + 1] = (stripe.wrapping_add(80)).wrapping_mul(13);
                pixels[i + 2] = stripe.wrapping_mul(5);
                pixels[i + 3] = 255;
            }
        }
        client.push_video(f as i64, pixels.clone()).expect("push_video");
    }
    // 1 second of stereo silence (48_000 frames * 2 channels).
    let samples = vec![0f32; (sample_rate * 2) as usize];
    client.push_audio(0, samples).expect("push_audio");

    let (vf, _as) = client.finish().expect("finish");
    assert_eq!(vf, 30, "video frames");
    let _ = client.shutdown();

    let meta = std::fs::metadata(&temp).expect("output exists");
    assert!(meta.len() > 1_000, "output too small: {} bytes", meta.len());

    // ffprobe the file to confirm it's a valid ProRes MOV.
    let probe = Command::new("ffprobe")
        .arg("-v")
        .arg("error")
        .arg("-select_streams")
        .arg("v:0")
        .arg("-show_entries")
        .arg("stream=codec_name,width,height")
        .arg("-of")
        .arg("default=nw=1:nk=1")
        .arg(&temp)
        .output()
        .expect("ffprobe");
    let probe_str = String::from_utf8_lossy(&probe.stdout);
    let mut lines = probe_str.lines();
    let codec = lines.next().unwrap_or_default();
    let w = lines.next().unwrap_or_default();
    let h = lines.next().unwrap_or_default();
    assert_eq!(codec, "prores", "expected prores codec, got `{codec}`");
    assert_eq!(w, "64");
    assert_eq!(h, "64");

    let _ = std::fs::remove_file(&temp);
}
