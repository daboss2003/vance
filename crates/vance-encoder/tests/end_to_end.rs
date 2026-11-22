//! Phase 8.2 — end-to-end exercise of the sidecar.
//!
//! Spawns the sidecar binary, encodes a 1-second 64×64 test pattern
//! to ProRes 422 HQ + PCM audio, and asserts the output file exists,
//! is non-trivial in size, and probes as ProRes via `ffprobe`. The
//! test skips itself with `eprintln!` if `ffmpeg` / `ffprobe` are
//! missing on PATH so CI machines without FFmpeg don't fail.

use std::path::{Path, PathBuf};
use std::process::Command;
use vance_encoder::protocol::{
    AudioCodec, AudioConfig, AudioSampleFormat, EncoderPreset, EncoderRequest, EncoderResponse,
    HdrMetadata, VideoConfig, VideoPixelFormat, PROTOCOL_VERSION,
};
use vance_encoder::{EncoderClient, EncoderClientError};

/// An output file a test writes, deleted when the test ends, also when an
/// assertion fails or the test skips an iteration.
struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A private temp dir for one sidecar (its `TMPDIR`), removed at the end.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("vance-enc-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    /// The encoder temp files in it.
    fn encoder_files(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.0)
            .expect("read temp dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("vance-enc-"))
            .collect();
        names.sort();
        names
    }

    /// The encoder temp files, once there are `n` of them: ffmpeg creates
    /// the side video only after its first frame.
    fn wait_for_encoder_files(&self, n: usize) -> Vec<String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let files = self.encoder_files();
            if files.len() >= n || std::time::Instant::now() > deadline {
                return files;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// A sidecar keeping its temp files here.
    fn spawn(&self, binary: &Path) -> EncoderClient {
        let dir = self.0.as_os_str();
        EncoderClient::spawn_with_env(binary, [("TMPDIR", dir), ("TMP", dir), ("TEMP", dir)])
            .expect("spawn")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

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
        let _cleanup = Cleanup(out.clone());

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
        let _cleanup = Cleanup(out.clone());
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
    let _cleanup = Cleanup(out.clone());

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

/// A 4K `rgba64le` frame (66 MB) is bigger than an IPC message may be, so it
/// only gets through if pixels travel as a blob outside the message.
#[test]
fn frames_bigger_than_an_ipc_message_encode() {
    if !ffmpeg_present() || !ffprobe_present() {
        eprintln!("ffmpeg / ffprobe not on PATH — skipping 4K rgba64le test");
        return;
    }
    let Some(binary) = sidecar_binary() else {
        eprintln!("vance-encoder binary not built — skipping 4K rgba64le test");
        return;
    };
    let (w, h) = (3840u32, 2160u32);
    let frame_len = w as usize * h as usize * VideoPixelFormat::Rgba64Le.bytes_per_pixel();
    assert!(frame_len as u64 > u64::from(vance_ipc::MAX_FRAME_BYTES));

    let out = std::env::temp_dir().join(format!("vance-4k-rgba64-{}.mov", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let _cleanup = Cleanup(out.clone());
    let mut client = EncoderClient::spawn(&binary).expect("spawn");
    client
        .open(
            out.to_string_lossy().to_string(),
            Some(VideoConfig {
                width: w,
                height: h,
                frame_rate_num: 24,
                frame_rate_den: 1,
                pixel_format: VideoPixelFormat::Rgba64Le,
                // prores_ks is built into every ffmpeg, unlike libx265.
                preset: EncoderPreset::ProRes422HQ,
                time_base_num: 1,
                time_base_den: 24,
                hdr: None,
            }),
            None,
        )
        .expect("open 4K rgba64le");
    for f in 0..2u8 {
        let mut pixels = vec![0u8; frame_len];
        for (i, band) in pixels.chunks_mut(8192).enumerate() {
            band.fill((i as u8) ^ f.wrapping_mul(91));
        }
        client
            .push_video(i64::from(f), pixels)
            .unwrap_or_else(|e| panic!("push 4K rgba64le frame {f}: {e}"));
    }
    let (frames, _) = client.finish().expect("finish 4K rgba64le");
    let _ = client.shutdown();

    let probe = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0"])
        .args([
            "-show_entries",
            "stream=codec_name,width,height",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(&out)
        .output()
        .expect("ffprobe");
    let _ = std::fs::remove_file(&out);
    assert_eq!(frames, 2);
    let probe = String::from_utf8_lossy(&probe.stdout);
    assert_eq!(
        probe.lines().collect::<Vec<_>>(),
        ["prores", "3840", "2160"],
        "ffprobe of the 4K export"
    );
}

/// Every refused push still consumes its blob, so the pushes after it land
/// where they should and the counts at Finish only include accepted ones.
#[test]
fn refused_pushes_leave_the_session_usable() {
    if !ffmpeg_present() {
        eprintln!("ffmpeg not on PATH — skipping refused-push test");
        return;
    }
    let Some(binary) = sidecar_binary() else {
        eprintln!("vance-encoder binary not built — skipping refused-push test");
        return;
    };
    let out = std::env::temp_dir().join(format!("vance-refused-{}.mov", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let _cleanup = Cleanup(out.clone());
    let (w, h) = (64u32, 64u32);
    let frame_len = (w * h * 4) as usize;
    let encoder_message = |r: Result<(), EncoderClientError>| match r {
        Err(EncoderClientError::Encoder(e)) => e.message,
        other => panic!("expected a refused push, got {other:?}"),
    };

    let mut client = EncoderClient::spawn(&binary).expect("spawn");
    let msg = encoder_message(client.push_video(0, vec![0u8; frame_len]));
    assert!(msg.contains("encoder not open"), "{msg}");

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
            Some(AudioConfig {
                sample_rate: 48_000,
                channels: 2,
                sample_format: AudioSampleFormat::F32Interleaved,
                codec: AudioCodec::PcmS24,
                time_base_num: 1,
                time_base_den: 48_000,
            }),
        )
        .expect("open");

    // An rgba64le-sized frame for an Rgba8 config.
    let msg = encoder_message(client.push_video(0, vec![7u8; frame_len * 2]));
    assert!(
        msg.contains("video frame size mismatch: got 32768 bytes, expected 16384"),
        "{msg}"
    );
    // Three samples can't be stereo frames.
    let msg = encoder_message(client.push_audio(0, vec![0.25; 3]));
    assert!(msg.contains("not a whole number of 2-channel"), "{msg}");
    // Refused before anything is written: sending the request first would
    // leave the sidecar waiting for a blob that never comes.
    let too_big = vec![0u8; vance_ipc::MAX_BLOB_BYTES as usize + 1];
    assert!(matches!(
        client.push_video(0, too_big),
        Err(EncoderClientError::Ipc(vance_ipc::Error::BlobTooLarge))
    ));

    for f in 0..5u8 {
        client
            .push_video(i64::from(f), vec![f.wrapping_mul(40); frame_len])
            .unwrap_or_else(|e| panic!("push frame {f}: {e}"));
    }
    client
        .push_audio(0, vec![0.1; 4800 * 2])
        .expect("push 0.1 s of stereo");
    let (frames, samples) = client.finish().expect("finish");
    let _ = client.shutdown();
    let len = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    let _ = std::fs::remove_file(&out);
    assert_eq!((frames, samples), (5, 4800));
    assert!(len > 1_000, "output too small: {len} bytes");
}

/// A blob that disagrees with its message leaves the request stream out of
/// sync. Whatever follows must be refused, even bytes that decode as a valid
/// request.
#[test]
fn a_push_whose_blob_disagrees_with_its_message_refuses_everything_after() {
    let Some(binary) = sidecar_binary() else {
        eprintln!("vance-encoder binary not built — skipping out-of-sync test");
        return;
    };
    let mut sidecar: vance_ipc::Sidecar<EncoderRequest, EncoderResponse> =
        vance_ipc::Sidecar::spawn(&binary, std::iter::empty::<&str>()).expect("spawn");
    // The blob's bytes are a whole framed Hello: once the sidecar has lost
    // sync, they are what it reads as its next request.
    let mut smuggled = Vec::new();
    vance_ipc::write_frame(
        &mut smuggled,
        &EncoderRequest::Hello {
            protocol_version: PROTOCOL_VERSION,
        },
    )
    .unwrap();
    sidecar
        .send(&EncoderRequest::PushVideoFrame {
            pts: 0,
            len: smuggled.len() as u64 + 1,
        })
        .unwrap();
    sidecar.send_blob(&smuggled).unwrap();

    let failed = |resp| match resp {
        EncoderResponse::EncodeFailed(e) => e.message,
        other => panic!("expected EncodeFailed, got {other:?}"),
    };
    let msg = failed(sidecar.recv().expect("reply to the bad push"));
    assert!(msg.contains("differs from the length"), "{msg}");
    let msg = failed(sidecar.recv().expect("reply to the smuggled Hello"));
    assert!(msg.contains("out of sync"), "{msg}");
    let _ = sidecar.shutdown();
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
        EncoderPreset::ProRes422Proxy => "prores422proxy",
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
        let _cleanup = Cleanup(out.clone());

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
    let _cleanup = Cleanup(temp.clone());

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

// ---------------------------------------------------------------------
// Temp files: an export with audio writes a side video and a WAV to the
// temp dir; none may outlive the export, however it ends.
// ---------------------------------------------------------------------

/// A 64x64 ProRes + PCM export config.
fn av_configs() -> (VideoConfig, AudioConfig) {
    (
        VideoConfig {
            width: 64,
            height: 64,
            frame_rate_num: 24,
            frame_rate_den: 1,
            pixel_format: VideoPixelFormat::Rgba8,
            preset: EncoderPreset::ProRes422HQ,
            time_base_num: 1,
            time_base_den: 24,
            hdr: None,
        },
        AudioConfig {
            sample_rate: 48_000,
            channels: 2,
            sample_format: AudioSampleFormat::F32Interleaved,
            codec: AudioCodec::PcmS24,
            time_base_num: 1,
            time_base_den: 48_000,
        },
    )
}

/// Open a video + audio export to `out` and push a few frames and samples.
fn start_av_export(client: &mut EncoderClient, out: &Path) {
    let (video, audio) = av_configs();
    client
        .open(out.to_string_lossy().into_owned(), Some(video), Some(audio))
        .expect("open");
    for f in 0..3 {
        client
            .push_video(f, vec![f as u8 * 40; 64 * 64 * 4])
            .expect("push video");
    }
    client
        .push_audio(0, vec![0.25; 2 * 4800])
        .expect("push audio");
}

/// The ffmpeg encoder a fixed preset needs (H.264 / HEVC pick one at
/// runtime, and fail `Open` cleanly when there is none).
fn fixed_encoder(preset: &EncoderPreset) -> Option<&'static str> {
    match preset {
        EncoderPreset::ProRes422HQ
        | EncoderPreset::ProRes422
        | EncoderPreset::ProRes4444
        | EncoderPreset::ProRes422Proxy => Some("prores_ks"),
        EncoderPreset::DnxhrHq | EncoderPreset::DnxhrSq | EncoderPreset::DnxhrLb => Some("dnxhd"),
        EncoderPreset::Av1 { .. } => Some("libsvtav1"),
        EncoderPreset::H264 { .. } | EncoderPreset::Hevc { .. } => None,
    }
}

/// Whether the ffmpeg CLI lists `encoder`.
fn ffmpeg_has_encoder(encoder: &str) -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .is_ok_and(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .any(|l| l.split_whitespace().nth(1) == Some(encoder))
        })
}

fn skip_without_ffmpeg(what: &str) -> Option<PathBuf> {
    if !ffmpeg_present() {
        eprintln!("ffmpeg not on PATH — skipping {what}");
        return None;
    }
    let binary = sidecar_binary();
    if binary.is_none() {
        eprintln!("encoder binary missing — skipping {what}");
    }
    binary
}

/// The mux at `Finish` fails (MP4 can't hold ProRes): the export fails, and
/// its side video and WAV are gone anyway.
#[test]
fn a_failed_mux_leaves_no_temp_files() {
    let Some(binary) = skip_without_ffmpeg("failed mux test") else {
        return;
    };
    let tmp = TempDir::new("failed-mux");
    let mut client = tmp.spawn(&binary);
    start_av_export(&mut client, &tmp.0.join("out.mp4"));
    assert_eq!(
        tmp.wait_for_encoder_files(2).len(),
        2,
        "side video + WAV while exporting"
    );
    assert!(client.finish().is_err(), "the mux must fail");
    assert_eq!(
        tmp.encoder_files(),
        [] as [String; 0],
        "after the failed mux"
    );
    client.shutdown().expect("shutdown");
}

/// An export the parent never finishes: replaced by a new `Open` (its files
/// go, the new export's work), or left open at `Shutdown`.
#[test]
fn an_abandoned_export_leaves_no_temp_files() {
    let Some(binary) = skip_without_ffmpeg("abandoned export test") else {
        return;
    };
    let tmp = TempDir::new("abandoned");
    let out = tmp.0.join("out.mov");
    let mut client = tmp.spawn(&binary);
    start_av_export(&mut client, &out);
    let first = tmp.wait_for_encoder_files(2);
    assert_eq!(first.len(), 2, "side video + WAV: {first:?}");
    start_av_export(&mut client, &out);
    let second = tmp.wait_for_encoder_files(2);
    assert_eq!(second.len(), 2, "only the new export's files: {second:?}");
    assert!(
        second.iter().all(|f| !first.contains(f)),
        "{first:?} -> {second:?}"
    );
    let (frames, samples) = client.finish().expect("finish the second export");
    assert_eq!((frames, samples), (3, 4800));
    assert_eq!(tmp.encoder_files(), [] as [String; 0], "after finish");
    assert!(
        std::fs::metadata(&out).is_ok_and(|m| m.len() > 0),
        "export written"
    );

    start_av_export(&mut client, &out);
    assert_eq!(tmp.wait_for_encoder_files(2).len(), 2);
    client.shutdown().expect("shutdown");
    assert_eq!(tmp.encoder_files(), [] as [String; 0], "after Shutdown");
}

/// A killed sidecar can't clean up: the next one deletes what it left, at
/// startup, and leaves the files of an encoder that is still running.
#[test]
fn a_killed_encoders_temp_files_are_swept_by_the_next() {
    let Some(binary) = skip_without_ffmpeg("stale temp file sweep test") else {
        return;
    };
    let tmp = TempDir::new("killed");
    let mut client = tmp.spawn(&binary);
    start_av_export(&mut client, &tmp.0.join("out.mov"));
    assert_eq!(
        tmp.wait_for_encoder_files(2).len(),
        2,
        "side video + WAV while exporting"
    );
    client.kill().expect("kill");
    drop(client);
    let left = tmp.encoder_files();
    assert_eq!(left.len(), 2, "a killed sidecar's files stay: {left:?}");
    // This test process is alive: a file with its pid must survive.
    let live = format!("vance-enc-{}-1-video.mov", std::process::id());
    std::fs::write(tmp.0.join(&live), b"in use").expect("write");

    let next = tmp.spawn(&binary);
    assert_eq!(
        tmp.encoder_files(),
        [live],
        "after the next sidecar started"
    );
    next.shutdown().expect("shutdown");
}

// ---------------------------------------------------------------------
// Colour: RGB frames become YUV with the matrix the stream is tagged with.
// ---------------------------------------------------------------------

/// Limited-range Y'CbCr of `rgb` (0..=1) with luma weights `kr`, `kb`, at
/// `bits` bits.
fn ycbcr(rgb: [f64; 3], kr: f64, kb: f64, bits: u32) -> [f64; 3] {
    let [r, g, b] = rgb;
    let y = kr * r + (1.0 - kr - kb) * g + kb * b;
    let cb = (b - y) / (2.0 * (1.0 - kb));
    let cr = (r - y) / (2.0 * (1.0 - kr));
    let scale = f64::from(1u32 << (bits - 8));
    [
        (16.0 + 219.0 * y) * scale,
        (128.0 + 224.0 * cb) * scale,
        (128.0 + 224.0 * cr) * scale,
    ]
}

/// `key=value` lines of an ffprobe query of `path`.
fn probe(path: &Path, args: &[&str]) -> Vec<(String, String)> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0"])
        .args(args)
        .args(["-of", "default=nw=1"])
        .arg(path)
        .output()
        .expect("ffprobe");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// The first frame of `path` as decoded, unconverted: its pixel format and
/// the Y, Cb, Cr samples at each of `points` (luma coordinates).
fn decoded_samples(
    path: &Path,
    w: usize,
    h: usize,
    points: &[(usize, usize)],
) -> (String, Vec<[f64; 3]>) {
    let pix_fmt = probe(path, &["-show_entries", "stream=pix_fmt"])
        .into_iter()
        .find(|(k, _)| k == "pix_fmt")
        .map(|(_, v)| v)
        .expect("pix_fmt");
    let raw = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(path)
        .args(["-frames:v", "1", "-f", "rawvideo", "-"])
        .output()
        .expect("ffmpeg decode")
        .stdout;
    let (cw, ch) = if pix_fmt.contains("420") {
        (w.div_ceil(2), h.div_ceil(2))
    } else if pix_fmt.contains("422") {
        (w.div_ceil(2), h)
    } else {
        assert!(pix_fmt.contains("444"), "unexpected {pix_fmt}");
        (w, h)
    };
    let bps = if pix_fmt.contains("p10") || pix_fmt.contains("p12") {
        2
    } else {
        1
    };
    let sample = |offset: usize| -> f64 {
        if bps == 1 {
            f64::from(raw[offset])
        } else {
            f64::from(u16::from_le_bytes([raw[offset], raw[offset + 1]]))
        }
    };
    let (luma, chroma) = (w * h * bps, cw * ch * bps);
    let (sx, sy) = (w / cw, h / ch);
    let samples = points
        .iter()
        .map(|&(x, y)| {
            let c = ((y / sy) * cw + x / sx) * bps;
            [
                sample((y * w + x) * bps),
                sample(luma + c),
                sample(luma + chroma + c),
            ]
        })
        .collect();
    (pix_fmt, samples)
}

/// The bit depth of a planar `pix_fmt` name.
fn bit_depth(pix_fmt: &str) -> u32 {
    if pix_fmt.contains("p12") {
        12
    } else if pix_fmt.contains("p10") {
        10
    } else {
        8
    }
}

/// The colour tests' frame size, and a point in the middle of each half.
const COLOR_SIZE: usize = 256;
const COLOR_POINTS: [(usize, usize); 2] = [(64, 128), (192, 128)];

/// Encode a frame whose left half is `left` and right half `right` (8-bit
/// or 16-bit RGB) with `preset` / `hdr` to `out`, or `None` (skipped) when
/// this ffmpeg can't.
fn encode_halves(
    binary: &Path,
    out: &Path,
    preset: EncoderPreset,
    hdr: Option<HdrMetadata>,
    left: [u16; 3],
    right: [u16; 3],
) -> Option<()> {
    // DNxHR refuses frames as small as the other tests' 64x64.
    let (w, h) = (COLOR_SIZE, COLOR_SIZE);
    let wide = hdr.is_some();
    let mut frame = Vec::with_capacity(w * h * 8);
    for _y in 0..h {
        for x in 0..w {
            let rgb = if x < w / 2 { left } else { right };
            for c in rgb.into_iter().chain([u16::MAX]) {
                if wide {
                    frame.extend_from_slice(&c.to_le_bytes());
                } else {
                    frame.push(c as u8);
                }
            }
        }
    }
    let mut client = EncoderClient::spawn(binary).expect("spawn");
    let cfg = VideoConfig {
        width: w as u32,
        height: h as u32,
        frame_rate_num: 24,
        frame_rate_den: 1,
        pixel_format: if wide {
            VideoPixelFormat::Rgba64Le
        } else {
            VideoPixelFormat::Rgba8
        },
        preset,
        time_base_num: 1,
        time_base_den: 24,
        hdr,
    };
    if let Err(e) = client.open(out.to_string_lossy().into_owned(), Some(cfg), None) {
        // Only a missing encoder is a reason to skip; anything else is a
        // failure of the export itself.
        let message = e.to_string();
        assert!(
            message.contains("No working"),
            "{}: open: {message}",
            out.display()
        );
        eprintln!("{}: no encoder ({message}), skipping", out.display());
        return None;
    }
    for f in 0..4 {
        client.push_video(f, frame.clone()).expect("push");
    }
    client.finish().expect("finish");
    let _ = client.shutdown();
    Some(())
}

/// Every SDR preset converts RGB with BT.709 and tags the stream BT.709
/// (primaries, transfer, matrix) and limited range: 0x3080C0 and pure green
/// decode to their BT.709 Y'CbCr (±2 at 8 bits). BT.601, swscale's default,
/// gives Y' 111.6 for 0x3080C0 and 144.6 for green, against 115.3 and 172.6.
#[test]
fn sdr_exports_convert_with_bt709_and_say_so() {
    let Some(binary) = skip_without_ffmpeg("BT.709 export test") else {
        return;
    };
    if !ffprobe_present() {
        eprintln!("ffprobe not on PATH — skipping BT.709 export test");
        return;
    }
    let (left, right) = ([0x30, 0x80, 0xC0], [0, 0xFF, 0]);
    let cases = [
        (EncoderPreset::ProRes422HQ, "mov"),
        (EncoderPreset::ProRes422Proxy, "mov"),
        (EncoderPreset::ProRes4444, "mov"),
        (EncoderPreset::DnxhrLb, "mov"),
        (EncoderPreset::H264 { crf: 18 }, "mp4"),
        (EncoderPreset::Hevc { crf: 18 }, "mp4"),
        (EncoderPreset::Av1 { crf: 20 }, "mkv"),
    ];
    for (preset, ext) in cases {
        if let Some(encoder) = fixed_encoder(&preset).filter(|e| !ffmpeg_has_encoder(e)) {
            eprintln!("{preset:?}: ffmpeg has no {encoder} encoder, skipping");
            continue;
        }
        let out = std::env::temp_dir().join(format!(
            "vance-bt709-{}-{}.{ext}",
            std::process::id(),
            preset_slug(&preset)
        ));
        let _cleanup = Cleanup(out.clone());
        if encode_halves(&binary, &out, preset.clone(), None, left, right).is_none() {
            continue;
        }
        let what = format!("{preset:?}");
        let (pix_fmt, got) = decoded_samples(&out, COLOR_SIZE, COLOR_SIZE, &COLOR_POINTS);
        let bits = bit_depth(&pix_fmt);
        let tol = 2.0 * f64::from(1u32 << (bits - 8));
        for (rgb8, got) in [left, right].into_iter().zip(got) {
            let rgb = rgb8.map(|c| f64::from(c) / 255.0);
            let want = ycbcr(rgb, 0.2126, 0.0722, bits);
            for (c, (g, w)) in got.iter().zip(want).enumerate() {
                assert!(
                    (g - w).abs() <= tol,
                    "{what} ({pix_fmt}): {rgb8:02X?} component {c} is {g}, BT.709 gives {w:.1}"
                );
            }
        }
        assert_tags(&what, &out, ["bt709", "bt709", "bt709", "tv"]);
    }
}

/// HDR10 converts with BT.2020 NCL (not BT.601) and is tagged with it, PQ
/// and Rec.2020 primaries.
#[test]
fn hdr_exports_convert_with_bt2020() {
    let Some(binary) = skip_without_ffmpeg("BT.2020 export test") else {
        return;
    };
    if !ffprobe_present() {
        eprintln!("ffprobe not on PATH — skipping BT.2020 export test");
        return;
    }
    // HDR10 always encodes with libx265.
    if !ffmpeg_has_encoder("libx265") {
        eprintln!("ffmpeg has no libx265 encoder — skipping BT.2020 export test");
        return;
    }
    let out = std::env::temp_dir().join(format!("vance-bt2020-{}.mp4", std::process::id()));
    let _cleanup = Cleanup(out.clone());
    let hdr = HdrMetadata::hdr10(1000.0, 0.0001, 1000, 400);
    let (left, right) = ([0x2000, 0xC000, 0x4000], [0x3000, 0x8000, 0xC000]);
    if encode_halves(
        &binary,
        &out,
        EncoderPreset::Hevc { crf: 12 },
        Some(hdr),
        left,
        right,
    )
    .is_none()
    {
        return;
    }
    let (pix_fmt, got) = decoded_samples(&out, COLOR_SIZE, COLOR_SIZE, &COLOR_POINTS);
    assert_eq!(pix_fmt, "yuv420p10le");
    for (rgb16, got) in [left, right].into_iter().zip(got) {
        let rgb = rgb16.map(|c| f64::from(c) / 65535.0);
        let want = ycbcr(rgb, 0.2627, 0.0593, 10);
        for (c, (g, w)) in got.iter().zip(want).enumerate() {
            assert!(
                (g - w).abs() <= 8.0,
                "HDR10 {rgb16:04X?} component {c} is {g}, BT.2020 NCL gives {w:.1}"
            );
        }
    }
    assert_tags("HDR10", &out, ["bt2020", "smpte2084", "bt2020nc", "tv"]);
}

/// The stream (or, where the container has no field for it, its first
/// frame) says `[primaries, transfer, matrix, range]`.
fn assert_tags(what: &str, path: &Path, want: [&str; 4]) {
    let keys = [
        "color_primaries",
        "color_transfer",
        "color_space",
        "color_range",
    ];
    let entries = format!("stream={}", keys.join(","));
    let stream = probe(path, &["-show_entries", &entries]);
    let frames = format!("frame={}", keys.join(","));
    let frame = probe(path, &["-read_intervals", "%+#1", "-show_entries", &frames]);
    for (key, want) in keys.into_iter().zip(want) {
        let value =
            |tags: &[(String, String)]| tags.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        let (s, f) = (value(&stream), value(&frame));
        assert!(
            s.as_deref() == Some(want) || f.as_deref() == Some(want),
            "{what}: {key} is {s:?} (stream) / {f:?} (first frame), expected {want}"
        );
    }
}

// ---------------------------------------------------------------------
// Atomic exports: the container is written to a hidden partial file next
// to the destination and renamed over it only when `Finish` succeeds.
// ---------------------------------------------------------------------

/// What an earlier export left at a destination.
const EARLIER: &[u8] = b"an earlier export";

/// Every file in `dir` (hidden ones included), sorted.
fn dir_files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read dir")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The partial export files in `dir`.
fn partial_files(dir: &Path) -> Vec<String> {
    dir_files(dir)
        .into_iter()
        .filter(|n| n.contains(".vance-partial-"))
        .collect()
}

/// Wait until `dir` holds a partial file with content (`Open` creates it
/// empty, ffmpeg fills it) whose size has stopped changing, and return it.
fn settled_partial(dir: &Path) -> Option<String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let (mut last, mut steady) = (None, 0);
    while std::time::Instant::now() < deadline {
        let now = partial_files(dir).into_iter().next().map(|name| {
            let len = std::fs::metadata(dir.join(&name)).map_or(0, |m| m.len());
            (name, len)
        });
        let written = now.as_ref().is_some_and(|(_, len)| *len > 0);
        steady = if written && now == last {
            steady + 1
        } else {
            0
        };
        // Unchanged over three polls (~0.6 s): ffmpeg is done with it.
        if steady >= 3 {
            return now.map(|(name, _)| name);
        }
        last = now;
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    None
}

/// Push a few frames, ignoring a refusal (an ffmpeg that already failed).
fn push_frames(client: &mut EncoderClient, frames: i64) {
    for f in 0..frames {
        let _ = client.push_video(f, vec![f as u8 * 40; 64 * 64 * 4]);
    }
}

/// Exports that fail (the encoder refuses, the mux refuses) or are
/// abandoned (replaced by a new `Open`, then `Shutdown`) leave a file
/// already at the destination as it was, and no partial file. Without the
/// partial file ffmpeg's `-y` truncated it the moment it started.
#[test]
fn a_failed_export_leaves_the_destination_as_it_was() {
    let Some(binary) = skip_without_ffmpeg("atomic export failure test") else {
        return;
    };
    let tmp = TempDir::new("atomic-failed");
    let (mov, mp4) = (tmp.0.join("out.mov"), tmp.0.join("out.mp4"));
    for d in [&mov, &mp4] {
        std::fs::write(d, EARLIER).expect("write earlier export");
    }
    let untouched = |what: &str| {
        for d in [&mov, &mp4] {
            assert_eq!(
                std::fs::read(d).expect("destination"),
                EARLIER,
                "{what}: {} changed",
                d.display()
            );
        }
    };
    let (video, audio) = av_configs();
    let mut client = tmp.spawn(&binary);

    // DNxHR can't encode 64x64: ffmpeg fails, after opening its output.
    let dnxhr = VideoConfig {
        preset: EncoderPreset::DnxhrLb,
        ..video.clone()
    };
    client
        .open(mov.to_string_lossy().into_owned(), Some(dnxhr), None)
        .expect("open dnxhr");
    push_frames(&mut client, 3);
    assert!(client.finish().is_err(), "DNxHR at 64x64 must fail");
    untouched("failed encode");

    // MP4 can't hold ProRes: the mux at Finish fails.
    client
        .open(
            mp4.to_string_lossy().into_owned(),
            Some(video.clone()),
            Some(audio),
        )
        .expect("open prores + pcm");
    push_frames(&mut client, 3);
    client
        .push_audio(0, vec![0.25; 2 * 4800])
        .expect("push audio");
    assert!(client.finish().is_err(), "ProRes in MP4 must fail to mux");
    untouched("failed mux");

    // Abandoned while ffmpeg writes: replaced, then left open at Shutdown.
    client
        .open(
            mov.to_string_lossy().into_owned(),
            Some(video.clone()),
            None,
        )
        .expect("open");
    push_frames(&mut client, 3);
    assert!(
        settled_partial(&tmp.0).is_some(),
        "the export writes a partial file"
    );
    untouched("while exporting");
    client
        .open(mp4.to_string_lossy().into_owned(), Some(video), None)
        .expect("open replacing");
    push_frames(&mut client, 3);
    client.shutdown().expect("shutdown");
    untouched("abandoned exports");
    assert_eq!(
        dir_files(&tmp.0),
        ["out.mov", "out.mp4"],
        "nothing else left"
    );
}

/// A finished export, video only, with audio, or audio only, replaces the
/// destination (which holds the earlier file until then) and leaves no
/// other file.
#[test]
fn a_finished_export_replaces_the_destination_and_leaves_nothing_else() {
    let Some(binary) = skip_without_ffmpeg("atomic export test") else {
        return;
    };
    let tmp = TempDir::new("atomic-finished");
    let (video, audio) = av_configs();
    let mut client = tmp.spawn(&binary);
    for (name, video, audio) in [
        ("video.mov", Some(video.clone()), None),
        ("both.mov", Some(video), Some(audio.clone())),
        ("audio.wav", None, Some(audio)),
    ] {
        let destination = tmp.0.join(name);
        std::fs::write(&destination, EARLIER).expect("write earlier export");
        let video_only = audio.is_none();
        let has_video = video.is_some();
        client
            .open(destination.to_string_lossy().into_owned(), video, audio)
            .unwrap_or_else(|e| panic!("{name}: open: {e}"));
        if has_video {
            push_frames(&mut client, 3);
        }
        if !video_only {
            client
                .push_audio(0, vec![0.25; 2 * 4800])
                .expect("push audio");
        } else {
            assert!(settled_partial(&tmp.0).is_some(), "{name}: partial file");
        }
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            EARLIER,
            "{name}: while exporting"
        );
        client
            .finish()
            .unwrap_or_else(|e| panic!("{name}: finish: {e}"));
        let len = std::fs::metadata(&destination).map_or(0, |m| m.len());
        assert!(len > 1_000, "{name}: export is {len} bytes");
    }
    client.shutdown().expect("shutdown");
    assert_eq!(dir_files(&tmp.0), ["audio.wav", "both.mov", "video.mov"]);
}

/// A killed sidecar leaves its partial export behind. The next export into
/// that directory deletes it once nothing has written it for 10 minutes,
/// and leaves a running encoder's (this test process stands in for one) and
/// a dead one's that was written a moment ago.
#[test]
fn a_dead_encoders_partial_export_is_swept_by_the_next_export_there() {
    let Some(binary) = skip_without_ffmpeg("partial export sweep test") else {
        return;
    };
    let tmp = TempDir::new("atomic-killed");
    let (video, _) = av_configs();
    let mut client = tmp.spawn(&binary);
    client
        .open(
            tmp.0.join("out.mov").to_string_lossy().into_owned(),
            Some(video.clone()),
            None,
        )
        .expect("open");
    push_frames(&mut client, 3);
    client.kill().expect("kill");
    drop(client);
    // ffmpeg finishes the file on its own once the sidecar is gone.
    let dead = settled_partial(&tmp.0).expect("a killed export's partial file stays");
    // `.out.vance-partial-<host>-<pid>.mov`: this machine's host tag.
    let (_, owner) = dead.rsplit_once(".vance-partial-").expect("partial name");
    let (host, pid) = owner.split_once('-').expect("host-pid");
    let pid = pid.trim_end_matches(".mov");
    let aged = |name: &str| {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(tmp.0.join(name))
            .expect("open");
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(11 * 60);
        file.set_modified(old).expect("set mtime");
    };
    aged(&dead);
    let live = format!(".other.vance-partial-{host}-{}.mov", std::process::id());
    std::fs::write(tmp.0.join(&live), b"in use").expect("write");
    aged(&live);
    let recent = format!(".recent.vance-partial-{host}-{pid}.mov");
    std::fs::write(tmp.0.join(&recent), b"just written").expect("write");

    let mut next = tmp.spawn(&binary);
    assert!(
        tmp.0.join(&dead).exists(),
        "swept per export directory, not at startup"
    );
    next.open(
        tmp.0.join("next.mov").to_string_lossy().into_owned(),
        Some(video),
        None,
    )
    .expect("open");
    push_frames(&mut next, 3);
    next.finish().expect("finish");
    next.shutdown().expect("shutdown");
    assert_eq!(dir_files(&tmp.0), [live, recent, "next.mov".to_string()]);
}

/// A finished export that can't replace its destination (a folder appeared
/// there during the export) is kept next to it under a visible name, and the
/// error says where. A destination that can't be replaced or written next
/// to fails at `Open`, before any encoding.
#[test]
fn a_finished_export_that_cant_replace_its_destination_is_kept() {
    let Some(binary) = skip_without_ffmpeg("kept export test") else {
        return;
    };
    let tmp = TempDir::new("atomic-kept");
    let destination = tmp.0.join("out.mov");
    let (video, _) = av_configs();
    let mut client = tmp.spawn(&binary);
    client
        .open(
            destination.to_string_lossy().into_owned(),
            Some(video.clone()),
            None,
        )
        .expect("open");
    push_frames(&mut client, 3);
    std::fs::create_dir(&destination).expect("a folder takes the destination's name");
    let err = client
        .finish()
        .expect_err("the rename over a folder fails")
        .to_string();
    let kept = tmp.0.join("out (not replaced).mov");
    assert!(err.contains(&kept.display().to_string()), "{err}");
    let len = std::fs::metadata(&kept).map_or(0, |m| m.len());
    assert!(len > 1_000, "the finished export is kept: {len} bytes");
    assert!(destination.is_dir(), "the folder is left alone");
    assert_eq!(partial_files(&tmp.0), [] as [String; 0]);

    // Now a folder is there at Open: refused at once.
    let err = client
        .open(
            destination.to_string_lossy().into_owned(),
            Some(video.clone()),
            None,
        )
        .expect_err("a folder can't be replaced")
        .to_string();
    assert!(err.contains("is a folder"), "{err}");

    // A folder that takes no new files: refused at Open too.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let locked = tmp.0.join("locked");
        std::fs::create_dir(&locked).expect("mkdir");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        // Root writes anyway: then there's nothing to refuse.
        let writable = std::fs::write(locked.join("probe"), b"").is_ok();
        let opened = client.open(
            locked.join("out.mov").to_string_lossy().into_owned(),
            Some(video),
            None,
        );
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        if !writable {
            let err = opened
                .expect_err("a read-only folder is refused")
                .to_string();
            assert!(err.contains("can't write an export next to"), "{err}");
        }
    }
    client.shutdown().expect("shutdown");
    let mut left = dir_files(&tmp.0);
    left.retain(|n| n != "locked");
    assert_eq!(left, ["out (not replaced).mov", "out.mov"]);
}

/// Destination names the partial file's name used to break: one that is
/// only an extension (`.mp4`: Rust sees no extension, so the partial had
/// none and ffmpeg couldn't tell the container), and one near the 255-byte
/// name limit (the partial's tag made it too long).
#[test]
fn awkward_destination_names_export() {
    let Some(binary) = skip_without_ffmpeg("destination name test") else {
        return;
    };
    let tmp = TempDir::new("atomic-names");
    let (video, _) = av_configs();
    let mut names = vec![".mp4".to_string()];
    // Windows' 260-character path limit, which ffmpeg there may still keep
    // to, makes a 250-byte name an unfair test of this.
    if !cfg!(windows) {
        names.push(format!("{}.mov", "n".repeat(250)));
    }
    let mut client = tmp.spawn(&binary);
    for name in &names {
        let preset = if name.ends_with(".mp4") {
            EncoderPreset::H264 { crf: 23 }
        } else {
            EncoderPreset::ProRes422HQ
        };
        let cfg = VideoConfig {
            preset,
            ..video.clone()
        };
        client
            .open(
                tmp.0.join(name).to_string_lossy().into_owned(),
                Some(cfg),
                None,
            )
            .unwrap_or_else(|e| panic!("{} bytes: open: {e}", name.len()));
        push_frames(&mut client, 3);
        client
            .finish()
            .unwrap_or_else(|e| panic!("{} bytes: finish: {e}", name.len()));
        let len = std::fs::metadata(tmp.0.join(name)).map_or(0, |m| m.len());
        assert!(len > 1_000, "{} bytes: {len}-byte export", name.len());
    }
    client.shutdown().expect("shutdown");
    let mut want = names.clone();
    want.sort();
    assert_eq!(dir_files(&tmp.0), want);
}

/// An HDR proxy: PQ into ProRes 422 Proxy stays ProRes (no x265), 10-bit
/// 4:2:2, converted with BT.2020 NCL and tagged PQ / BT.2020 / BT.2020 NCL.
#[test]
fn a_pq_prores_proxy_is_prores_tagged_bt2020_pq() {
    let Some(binary) = skip_without_ffmpeg("PQ ProRes proxy test") else {
        return;
    };
    if !ffprobe_present() || !ffmpeg_has_encoder("prores_ks") {
        eprintln!("ffprobe or prores_ks missing — skipping PQ ProRes proxy test");
        return;
    }
    let out = std::env::temp_dir().join(format!("vance-pq-proxy-{}.mov", std::process::id()));
    let _cleanup = Cleanup(out.clone());
    let hdr = HdrMetadata::hdr10(1000.0, 0.0001, 1000, 400);
    let (left, right) = ([0x2000, 0xC000, 0x4000], [0x3000, 0x8000, 0xC000]);
    encode_halves(
        &binary,
        &out,
        EncoderPreset::ProRes422Proxy,
        Some(hdr),
        left,
        right,
    )
    .expect("encode");
    let codec = probe(&out, &["-show_entries", "stream=codec_name,profile"]);
    assert!(
        codec.contains(&("codec_name".into(), "prores".into())),
        "{codec:?}"
    );
    assert!(
        codec.contains(&("profile".into(), "Proxy".into())),
        "{codec:?}"
    );
    let (pix_fmt, got) = decoded_samples(&out, COLOR_SIZE, COLOR_SIZE, &COLOR_POINTS);
    assert_eq!(pix_fmt, "yuv422p10le");
    for (rgb16, got) in [left, right].into_iter().zip(got) {
        let rgb = rgb16.map(|c| f64::from(c) / 65535.0);
        let want = ycbcr(rgb, 0.2627, 0.0593, 10);
        for (c, (g, w)) in got.iter().zip(want).enumerate() {
            assert!(
                (g - w).abs() <= 8.0,
                "PQ proxy {rgb16:04X?} component {c} is {g}, BT.2020 NCL gives {w:.1}"
            );
        }
    }
    assert_tags(
        "PQ ProRes proxy",
        &out,
        ["bt2020", "smpte2084", "bt2020nc", "tv"],
    );
}

/// When the folder stops taking new names during the export (it went
/// read-only), the finished export can be neither renamed over the
/// destination nor kept beside it: it is copied, under a visible name that
/// no sweep matches, to the temp folder, and the error says where.
#[cfg(unix)]
#[test]
fn a_finished_export_that_cant_stay_in_its_folder_is_kept_elsewhere() {
    use std::os::unix::fs::PermissionsExt;
    let Some(binary) = skip_without_ffmpeg("kept-elsewhere export test") else {
        return;
    };
    let tmp = TempDir::new("atomic-kept-elsewhere");
    let (exports, temp) = (tmp.0.join("exports"), tmp.0.join("temp"));
    for d in [&exports, &temp] {
        std::fs::create_dir(d).expect("mkdir");
    }
    let dir = temp.as_os_str();
    let mut client =
        EncoderClient::spawn_with_env(&binary, [("TMPDIR", dir), ("TMP", dir), ("TEMP", dir)])
            .expect("spawn");
    let (video, _) = av_configs();
    let destination = exports.join(".hidden.mov");
    client
        .open(
            destination.to_string_lossy().into_owned(),
            Some(video),
            None,
        )
        .expect("open");
    push_frames(&mut client, 3);
    settled_partial(&exports).expect("a partial file");
    std::fs::set_permissions(&exports, std::fs::Permissions::from_mode(0o555)).unwrap();
    // Root writes anyway: then the export just succeeds.
    let read_only = std::fs::write(exports.join("probe"), b"").is_err();
    let finished = client.finish();
    std::fs::set_permissions(&exports, std::fs::Permissions::from_mode(0o755)).unwrap();
    client.shutdown().expect("shutdown");
    if !read_only {
        eprintln!("running as root: the folder stays writable, skipping");
        return;
    }
    let err = finished
        .expect_err("the rename into a read-only folder fails")
        .to_string();
    let kept = temp.join("hidden (not replaced).mov");
    assert!(err.contains(&kept.display().to_string()), "{err}");
    let len = std::fs::metadata(&kept).map_or(0, |m| m.len());
    assert!(len > 1_000, "the finished export is kept: {len} bytes");
    assert!(!destination.exists(), "nothing at the destination");
}
