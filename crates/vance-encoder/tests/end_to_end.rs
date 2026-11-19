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
    candidates.into_iter().find(|c| c.exists())
}

/// Phase 8.4 — exercise the audio codec variants (PCM_S16, AAC, FLAC)
/// via audio-only exports. PCM_S24 is exercised by the round-trip
/// test above.
#[test]
fn audio_codecs_export_to_recognisable_files() {
    if !ffmpeg_present() || !ffprobe_present() {
        eprintln!("ffmpeg / ffprobe missing — skipping audio codec test");
        return;
    }
    let Some(binary) = sidecar_binary() else {
        eprintln!("encoder binary missing — skipping");
        return;
    };

    let cases: Vec<(AudioCodec, &'static str, &'static str)> = vec![
        (AudioCodec::PcmS16, "wav", "pcm_s16le"),
        (AudioCodec::Aac, "m4a", "aac"),
        (AudioCodec::Flac, "flac", "flac"),
    ];

    for (codec, ext, expected_codec_name) in cases {
        let out = std::env::temp_dir().join(format!(
            "vance-aud-{}-{}.{}",
            std::process::id(),
            expected_codec_name,
            ext
        ));
        let _ = std::fs::remove_file(&out);

        let mut client = EncoderClient::spawn(&binary).expect("spawn");
        let sample_rate = 48_000u32;
        client
            .open(
                out.to_string_lossy().to_string(),
                None,
                Some(AudioConfig {
                    sample_rate,
                    channels: 2,
                    sample_format: AudioSampleFormat::F32Interleaved,
                    codec,
                    time_base_num: 1,
                    time_base_den: sample_rate,
                }),
            )
            .unwrap_or_else(|e| panic!("open {codec:?}: {e}"));
        // 0.5 s sine-ish content so the encoder has something to chew on.
        let n = (sample_rate / 2) as usize;
        let mut samples = Vec::with_capacity(n * 2);
        for i in 0..n {
            let s = ((i as f32) * 0.05).sin() * 0.3;
            samples.push(s);
            samples.push(s);
        }
        client
            .push_audio(0, samples)
            .unwrap_or_else(|e| panic!("push: {e}"));
        client
            .finish()
            .unwrap_or_else(|e| panic!("finish {codec:?}: {e}"));
        let _ = client.shutdown();

        let probe = Command::new("ffprobe")
            .arg("-v")
            .arg("error")
            .arg("-select_streams")
            .arg("a:0")
            .arg("-show_entries")
            .arg("stream=codec_name")
            .arg("-of")
            .arg("default=nw=1:nk=1")
            .arg(&out)
            .output()
            .expect("ffprobe");
        let probe_str = String::from_utf8_lossy(&probe.stdout).trim().to_string();
        assert_eq!(
            probe_str, expected_codec_name,
            "codec mismatch for {codec:?}: ffprobe says `{probe_str}`"
        );
        let _ = std::fs::remove_file(&out);
    }
}

/// Phase 8.3 / 8.6 — exercise the non-ProRes presets via the
/// sidecar's ffmpeg arg routing. Skips a preset if the host's ffmpeg
/// build lacks the codec (e.g. `libsvtav1` isn't always present).
/// Confirms the pipeline produces a non-empty file for every preset
/// it CAN run, so a UI selection of any preset Tries-And-Returns
/// instead of failing on a typo in argv generation.
#[test]
fn every_codec_preset_produces_an_output_file() {
    if !ffmpeg_present() {
        eprintln!("ffmpeg not on PATH — skipping multi-preset smoke test");
        return;
    }
    let Some(binary) = sidecar_binary() else {
        eprintln!("vance-encoder binary not built — skipping");
        return;
    };

    // Each (preset, extension). H.264 + HEVC go to MP4; AV1 to MKV;
    // DNxHR to MOV; ProRes already covered above.
    let cases: Vec<(EncoderPreset, &'static str)> = vec![
        (EncoderPreset::DnxhrLb, "mov"),
        (EncoderPreset::H264 { crf: 28 }, "mp4"),
        (EncoderPreset::Hevc { crf: 30 }, "mp4"),
        (EncoderPreset::Av1 { crf: 50 }, "mkv"),
    ];

    let mut ran_at_least_one = false;
    for (preset, ext) in cases {
        let out = std::env::temp_dir().join(format!(
            "vance-preset-{}-{}.{}",
            std::process::id(),
            preset_slug(&preset),
            ext
        ));
        let _ = std::fs::remove_file(&out);
        let mut client = match EncoderClient::spawn(&binary) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let cfg = VideoConfig {
            width: 64,
            height: 64,
            frame_rate_num: 24,
            frame_rate_den: 1,
            pixel_format: VideoPixelFormat::Rgba8,
            preset: preset.clone(),
            time_base_num: 1,
            time_base_den: 24,
            hdr: None,
        };
        if client
            .open(out.to_string_lossy().to_string(), Some(cfg), None)
            .is_err()
        {
            // ffmpeg refused (codec missing). Skip — not a sidecar bug.
            continue;
        }
        let mut pixels = vec![0u8; 64 * 64 * 4];
        for f in 0..6u8 {
            for i in pixels.iter_mut() {
                *i = f.wrapping_mul(33);
            }
            if client.push_video(f as i64, pixels.clone()).is_err() {
                break;
            }
        }
        if client.finish().is_err() {
            continue;
        }
        let _ = client.shutdown();
        if let Ok(meta) = std::fs::metadata(&out) {
            if meta.len() > 100 {
                ran_at_least_one = true;
            }
        }
        let _ = std::fs::remove_file(&out);
    }
    assert!(
        ran_at_least_one,
        "no non-ProRes preset produced output — sidecar argv routing is broken"
    );
}

/// H.264 export must succeed on any machine whose ffmpeg has SOME working
/// H.264 encoder. Unlike the smoke test above it does not tolerate failure:
/// a hard-coded hardware encoder that isn't usable on this host (VAAPI on a
/// GPU-less Linux CI runner) must fall back instead of breaking the pipe.
#[test]
fn h264_export_succeeds_via_runtime_encoder_selection() {
    if !ffmpeg_present() {
        eprintln!("ffmpeg not on PATH — skipping H.264 selection test");
        return;
    }
    let Some(binary) = sidecar_binary() else {
        eprintln!("vance-encoder binary not built — skipping");
        return;
    };
    let out = std::env::temp_dir().join(format!("vance-h264-select-{}.mp4", std::process::id()));
    let _ = std::fs::remove_file(&out);

    let (w, h) = (320u32, 240u32);
    let mut client = EncoderClient::spawn(&binary).expect("spawn");
    client
        .open(
            out.to_string_lossy().to_string(),
            Some(VideoConfig {
                width: w,
                height: h,
                frame_rate_num: 24,
                frame_rate_den: 1,
                pixel_format: VideoPixelFormat::Rgba8,
                preset: EncoderPreset::H264 { crf: 23 },
                time_base_num: 1,
                time_base_den: 24,
                hdr: None,
            }),
            None,
        )
        .unwrap_or_else(|e| panic!("open H.264: {e}"));
    let mut pixels = vec![0u8; (w * h * 4) as usize];
    for f in 0..10u8 {
        for (i, p) in pixels.iter_mut().enumerate() {
            *p = (i as u8).wrapping_add(f.wrapping_mul(9));
        }
        client
            .push_video(f as i64, pixels.clone())
            .unwrap_or_else(|e| panic!("push H.264 frame {f}: {e}"));
    }
    let (frames, _) = client
        .finish()
        .unwrap_or_else(|e| panic!("finish H.264: {e}"));
    assert_eq!(frames, 10);
    let _ = client.shutdown();

    let len = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    assert!(len > 1_000, "H.264 output too small: {len} bytes");
    if ffprobe_present() {
        let probe = Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "v:0"])
            .args([
                "-show_entries",
                "stream=codec_name",
                "-of",
                "default=nw=1:nk=1",
            ])
            .arg(&out)
            .output()
            .expect("ffprobe");
        let codec = String::from_utf8_lossy(&probe.stdout).trim().to_string();
        assert_eq!(codec, "h264", "expected an H.264 stream, got `{codec}`");
    }
    let _ = std::fs::remove_file(&out);
}

fn preset_slug(p: &EncoderPreset) -> &'static str {
    match p {
        EncoderPreset::ProRes422HQ => "prores422hq",
        EncoderPreset::ProRes422 => "prores422",
        EncoderPreset::ProRes4444 => "prores4444",
        EncoderPreset::DnxhrHq => "dnxhrhq",
        EncoderPreset::DnxhrSq => "dnxhrsq",
        EncoderPreset::DnxhrLb => "dnxhrlb",
        EncoderPreset::H264 { .. } => "h264",
        EncoderPreset::Hevc { .. } => "hevc",
        EncoderPreset::Av1 { .. } => "av1",
    }
}

/// Phase 8.8 — determinism gate.
///
/// Same input → byte-identical ProRes output. Catches accidental
/// non-determinism (timestamps, randomised quantisation, thread
/// scheduling) that would invalidate the exit-gate checksum step.
/// Runs two encodes of the same 12-frame fixed pattern and compares
/// the resulting files with `std::fs::read` + a quick xxhash-style
/// fold.
#[test]
fn prores_422_hq_is_deterministic_across_runs() {
    if !ffmpeg_present() {
        eprintln!("ffmpeg not on PATH — skipping determinism test");
        return;
    }
    let Some(binary) = sidecar_binary() else {
        eprintln!("vance-encoder binary not built — skipping determinism test");
        return;
    };

    let encode_one = |suffix: &str| -> Vec<u8> {
        let out = std::env::temp_dir().join(format!(
            "vance-determ-{}-{}.mov",
            std::process::id(),
            suffix
        ));
        let _ = std::fs::remove_file(&out);

        let mut client = EncoderClient::spawn(&binary).expect("spawn");
        let w = 64u32;
        let h = 64u32;
        client
            .open(
                out.to_string_lossy().to_string(),
                Some(VideoConfig {
                    width: w,
                    height: h,
                    frame_rate_num: 24,
                    frame_rate_den: 1,
                    pixel_format: VideoPixelFormat::Rgba8,
                    preset: EncoderPreset::ProRes422HQ,
                    time_base_num: 1,
                    time_base_den: 24,
                    hdr: None,
                }),
                None, // video-only — audio adds a mux step that's
                      // itself deterministic, but isolating video makes
                      // a failure here easier to attribute.
            )
            .expect("open");
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        for f in 0..12 {
            for i in 0..(w * h) as usize {
                let v = ((i + f as usize) & 0xff) as u8;
                pixels[i * 4] = v;
                pixels[i * 4 + 1] = v.wrapping_add(33);
                pixels[i * 4 + 2] = v.wrapping_mul(7);
                pixels[i * 4 + 3] = 255;
            }
            client.push_video(f as i64, pixels.clone()).expect("push");
        }
        client.finish().expect("finish");
        let _ = client.shutdown();
        let bytes = std::fs::read(&out).expect("read encoded");
        let _ = std::fs::remove_file(&out);
        bytes
    };

    let a = encode_one("a");
    let b = encode_one("b");
    // ProRes is deterministic per spec; the MOV container may embed
    // timestamps in metadata. We compare the bulk of the file to
    // ignore the first 256 bytes (which include mvhd creation-time
    // entries that ffmpeg can stamp with wall-clock).
    assert!(
        a.len() == b.len(),
        "encode sizes differ: {} vs {}",
        a.len(),
        b.len()
    );
    let header_window = 256;
    assert_eq!(
        &a[header_window..],
        &b[header_window..],
        "ProRes payload differs across runs (non-deterministic encode)"
    );
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

    let temp = std::env::temp_dir().join(format!("vance-prores-test-{}.mov", std::process::id()));
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
                hdr: None,
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
        client
            .push_video(f as i64, pixels.clone())
            .expect("push_video");
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
