//! Sources whose decoded pixel format isn't a wire format (4:2:2 / 4:4:4 /
//! 12-bit / RGB / full-range JPEG) must still open, and their frames must
//! arrive converted to a wire format: `Yuv420P10Le` for a >8-bit source,
//! `Yuv420P` otherwise. Fixtures are generated with the `ffmpeg` CLI at test
//! time (built-in encoders only); the tests skip when it isn't on PATH.
//!
//! Every test uses the default spawn, so on macOS the VideoToolbox codec
//! allowlist is in play: codecs VT can't decode must stay on the software
//! path and report `Yuv420P`, not `Nv12`.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use vance_decoder::{DecoderClient, FrameData, PixelFormat};

fn sidecar_binary_path() -> PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    let exe = if cfg!(windows) {
        "vance-decoder.exe"
    } else {
        "vance-decoder"
    };
    p.push(exe);
    p
}

static FIXTURE_SEQ: AtomicUsize = AtomicUsize::new(0);

/// A generated fixture file, deleted on drop.
struct Fixture(PathBuf);

impl Fixture {
    fn path(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// `ffmpeg -f lavfi -i <lavfi> <out_args> <unique temp file>.<ext>`.
/// `None` means ffmpeg isn't on PATH and the caller should skip.
fn make_fixture(ext: &str, lavfi: &str, out_args: &[&str]) -> Option<Fixture> {
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("ffmpeg not on PATH — skipping");
        return None;
    }
    let n = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
    let fixture = Fixture(
        std::env::temp_dir().join(format!("vance-pixfmt-{}-{n}.{ext}", std::process::id())),
    );
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", lavfi])
        .args(out_args)
        .arg(&fixture.0)
        .status()
        .expect("run ffmpeg");
    assert!(
        status.success(),
        "ffmpeg failed to generate {}",
        fixture.0.display()
    );
    Some(fixture)
}

fn spawn_default() -> DecoderClient {
    let bin = sidecar_binary_path();
    assert!(bin.exists(), "decoder binary missing at {}", bin.display());
    DecoderClient::spawn(&bin).expect("spawn decoder")
}

/// Asserts the frame's 4:2:0 wire layout (chroma = ceil(w/2) x ceil(h/2))
/// and returns its Y, U, V samples (8-bit samples widened to u16; NV12's
/// interleaved UV split into U and V).
fn yuv_planes(f: &FrameData) -> [Vec<u16>; 3] {
    let (w, h) = (f.width as usize, f.height as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let bps = f.pixel_format.bytes_per_sample();
    assert_eq!(
        f.bytes.len(),
        (w * h + 2 * cw * ch) * bps,
        "{:?} {w}x{h}: payload size",
        f.pixel_format
    );
    let samples = |bytes: &[u8]| -> Vec<u16> {
        if bps == 1 {
            bytes.iter().map(|&b| u16::from(b)).collect()
        } else {
            bytes
                .chunks_exact(2)
                .map(|s| u16::from_le_bytes([s[0], s[1]]))
                .collect()
        }
    };
    let (y, chroma) = f.bytes.split_at(w * h * bps);
    match f.pixel_format {
        PixelFormat::Yuv420P | PixelFormat::Yuv420P10Le => {
            assert_eq!(f.strides, vec![w as u32, cw as u32, cw as u32]);
            let (u, v) = chroma.split_at(cw * ch * bps);
            [samples(y), samples(u), samples(v)]
        }
        PixelFormat::Nv12 => {
            assert_eq!(f.strides, vec![w as u32, (2 * cw) as u32]);
            let uv = samples(chroma);
            let u = uv.iter().step_by(2).copied().collect();
            let v = uv.iter().skip(1).step_by(2).copied().collect();
            [samples(y), u, v]
        }
    }
}

/// Every sample of a flat-color plane (`width` samples per row) must be
/// within `tol` of `expected`.
fn assert_flat(plane: &str, samples: &[u16], width: usize, expected: u16, tol: u16) {
    assert!(!samples.is_empty(), "{plane}: empty plane");
    let range = expected.saturating_sub(tol)..=expected + tol;
    if let Some(i) = samples.iter().position(|s| !range.contains(s)) {
        let min = samples.iter().min().unwrap();
        let max = samples.iter().max().unwrap();
        panic!(
            "{plane}: sample {} at (x={}, y={}) outside {expected}±{tol}; plane spans {min}..={max}",
            samples[i],
            i % width,
            i / width
        );
    }
}

fn assert_flat_yuv(f: &FrameData, (y, u, v): (u16, u16, u16), tol: u16) {
    let [py, pu, pv] = yuv_planes(f);
    let cw = (f.width as usize).div_ceil(2);
    assert_flat("Y", &py, f.width as usize, y, tol);
    assert_flat("U", &pu, cw, u, tol);
    assert_flat("V", &pv, cw, v, tol);
}

/// Streams the whole file and checks every frame, exercising the cached
/// converter across frames and on the decode thread.
fn assert_stream_flat(
    client: &mut DecoderClient,
    expected_pf: PixelFormat,
    yuv: (u16, u16, u16),
    tol: u16,
    expected_frames: usize,
) {
    client.start_stream(0, 4).expect("start_stream");
    let mut count = 0;
    while let Some(f) = client.next_frame().expect("next_frame") {
        assert_eq!(f.pixel_format, expected_pf, "frame {count}");
        assert_flat_yuv(&f, yuv, tol);
        count += 1;
    }
    assert_eq!(count, expected_frames, "streamed frame count");
}

#[test]
fn prores_422_10bit_decodes_as_yuv420p10le() {
    // yuv422p10le, flat Y/U/V code values set directly in 10-bit.
    let Some(fx) = make_fixture(
        "mov",
        "color=c=gray:s=320x180:r=25:d=0.2,format=yuv422p10le,lutyuv=y=600:u=300:v=700",
        &[
            "-c:v",
            "prores_ks",
            "-profile:v",
            "3",
            "-pix_fmt",
            "yuv422p10le",
        ],
    ) else {
        return;
    };
    // Default spawn: a >8-bit source must take the software path (and stay
    // 10-bit) on every platform, including macOS where VideoToolbox exists.
    let mut client = spawn_default();
    let info = client.open(fx.path()).expect("open ProRes 422 HQ");
    assert_eq!(info.codec_name, "prores");
    assert_eq!(info.pixel_format, PixelFormat::Yuv420P10Le);

    let f = client.decode_at(0).expect("decode frame 0");
    assert_eq!((f.width, f.height), (320, 180));
    assert_eq!(f.pixel_format, PixelFormat::Yuv420P10Le);
    // 600 > 255: proves the samples are 10-bit code values, not 8-bit.
    assert_flat_yuv(&f, (600, 300, 700), 4);

    assert_stream_flat(&mut client, PixelFormat::Yuv420P10Le, (600, 300, 700), 4, 5);
    client.shutdown().expect("shutdown");
}

#[test]
fn prores_4444_12bit_is_carried_as_10bit() {
    // The ProRes 4444 decoder outputs yuv444p12le (12-bit 4:4:4).
    let Some(fx) = make_fixture(
        "mov",
        "color=c=gray:s=320x180:r=25:d=0.2,format=yuv444p10le,lutyuv=y=600:u=300:v=700",
        &[
            "-c:v",
            "prores_ks",
            "-profile:v",
            "4",
            "-pix_fmt",
            "yuv444p10le",
        ],
    ) else {
        return;
    };
    let mut client = spawn_default();
    let info = client.open(fx.path()).expect("open ProRes 4444");
    assert_eq!(info.pixel_format, PixelFormat::Yuv420P10Le);

    let f = client.decode_at(0).expect("decode frame 0");
    assert_eq!(f.pixel_format, PixelFormat::Yuv420P10Le);
    // 12-bit 2400 → 10-bit 600 (not left at 12-bit scale).
    assert_flat_yuv(&f, (600, 300, 700), 4);
    client.shutdown().expect("shutdown");
}

#[test]
fn ffv1_444_8bit_decodes_as_yuv420p_in_software() {
    // Odd size: 4:2:0 chroma must be ceil(w/2) x ceil(h/2). FFV1 is
    // lossless, so the code values survive exactly. VideoToolbox can't decode
    // FFV1, so on macOS too it must not be attached (and reported as Nv12).
    let Some(fx) = make_fixture(
        "mkv",
        "color=c=gray:s=161x91:r=25:d=0.2,format=yuv444p,lutyuv=y=180:u=90:v=200",
        &["-c:v", "ffv1", "-pix_fmt", "yuv444p"],
    ) else {
        return;
    };
    let mut client = spawn_default();
    let info = client.open(fx.path()).expect("open FFV1 yuv444p");
    assert_eq!(info.codec_name, "ffv1");
    assert_eq!(info.pixel_format, PixelFormat::Yuv420P);

    let f = client.decode_at(0).expect("decode frame 0");
    assert_eq!((f.width, f.height), (161, 91));
    assert_eq!(f.pixel_format, PixelFormat::Yuv420P);
    assert_flat_yuv(&f, (180, 90, 200), 1);

    assert_stream_flat(&mut client, PixelFormat::Yuv420P, (180, 90, 200), 1, 5);
    client.shutdown().expect("shutdown");
}

#[test]
fn png_rgb24_converts_with_bt709_limited_range() {
    // Pure red (255, 0, 0) RGB.
    let Some(fx) = make_fixture(
        "png",
        "color=c=red:s=161x91,format=rgb24,lutrgb=r=255:g=0:b=0",
        &["-frames:v", "1", "-pix_fmt", "rgb24"],
    ) else {
        return;
    };
    // Not a VideoToolbox codec: Yuv420P on macOS too.
    let mut client = spawn_default();
    let info = client.open(fx.path()).expect("open PNG rgb24");
    assert_eq!(info.pixel_format, PixelFormat::Yuv420P);

    let f = client.decode_at(0).expect("decode the image");
    assert_eq!(f.pixel_format, PixelFormat::Yuv420P);
    // Rec.709 limited: Y = 16 + 219*0.2126 ≈ 63, Cb ≈ 102, Cr = 240.
    // (Rec.601 would give Y ≈ 82 / Cb ≈ 90; full range would give Y ≈ 54.)
    assert_flat_yuv(&f, (63, 102, 240), 2);
    client.shutdown().expect("shutdown");
}

#[test]
fn mjpeg_full_range_422_keeps_its_range() {
    // yuvj422p (full-range 4:2:2). Only the chroma subsampling may change:
    // Y=250 must stay 250, not be squeezed to limited range (≈231).
    let Some(fx) = make_fixture(
        "avi",
        "color=c=gray:s=161x91:r=25:d=0.2,format=yuvj422p,lutyuv=y=250:u=90:v=200",
        &["-c:v", "mjpeg", "-pix_fmt", "yuvj422p", "-q:v", "2"],
    ) else {
        return;
    };
    // Not a VideoToolbox codec: Yuv420P on macOS too.
    let mut client = spawn_default();
    let info = client.open(fx.path()).expect("open MJPEG yuvj422p");
    assert_eq!(info.pixel_format, PixelFormat::Yuv420P);

    let f = client.decode_at(0).expect("decode frame 0");
    assert_eq!(f.pixel_format, PixelFormat::Yuv420P);
    assert_flat_yuv(&f, (250, 90, 200), 3);

    assert_stream_flat(&mut client, PixelFormat::Yuv420P, (250, 90, 200), 3, 5);
    client.shutdown().expect("shutdown");
}

#[test]
fn videotoolbox_codec_decoded_in_software_is_converted_to_nv12() {
    // MPEG-4 Part 2 is on the VideoToolbox allowlist, so on macOS the device
    // attaches and StreamInfo says Nv12. Apple Silicon's VideoToolbox has no
    // MPEG-4 Part 2 decoder, so FFmpeg falls back to software and the frames
    // arrive as yuv420p; they must be converted to the reported Nv12 (where
    // VT does decode it they arrive as NV12 already). Elsewhere this is the
    // plain software path. Odd size: NV12 rows carry 2 * ceil(w/2) UV bytes.
    let Some(fx) = make_fixture(
        "mkv",
        "color=c=gray:s=161x91:r=25:d=0.2,format=yuv420p,lutyuv=y=180:u=90:v=200",
        &["-c:v", "mpeg4", "-pix_fmt", "yuv420p", "-q:v", "2"],
    ) else {
        return;
    };
    let expected = if cfg!(target_os = "macos") {
        PixelFormat::Nv12
    } else {
        PixelFormat::Yuv420P
    };
    let mut client = spawn_default();
    let info = client.open(fx.path()).expect("open MPEG-4 Part 2");
    assert_eq!(info.codec_name, "mpeg4");
    assert_eq!(info.pixel_format, expected);

    let f = client.decode_at(0).expect("decode frame 0");
    assert_eq!(f.pixel_format, expected);
    assert_flat_yuv(&f, (180, 90, 200), 2);

    assert_stream_flat(&mut client, expected, (180, 90, 200), 2, 5);
    client.shutdown().expect("shutdown");
}

/// Playback-path throughput for 4K 10-bit ProRes 422 HQ: software decode,
/// swscale 4:2:2 -> 4:2:0, payload packing and the IPC hop, streamed with
/// the engine's playback buffer depth, once per frame transport. The
/// consumer copies each frame into a reused buffer, standing in for the
/// renderer's texture upload. Real-time 24-30 fps needs < ~33 ms per frame.
/// A manual benchmark, not a CI check:
///   cargo test -p vance-decoder --test it -- --ignored --nocapture prores_4k
/// The dev profile builds this crate and vance-ipc at opt-level 2 (see the
/// workspace Cargo.toml), so a plain debug build gives representative numbers.
/// On an M1 (Nov 2026), with frame-threaded decode: ~18 ms/frame (~55 fps)
/// with shared memory, ~21-26 ms/frame inline. Before ADR 0006 it was
/// ~42 ms/frame with raw blobs, and ~88-98 ms with the bytes inside bincode.
#[test]
#[ignore = "manual benchmark: run with --ignored --nocapture"]
fn prores_4k_decode_throughput() {
    let Some(fx) = make_fixture(
        "mov",
        "testsrc2=s=3840x2160:r=24:d=3",
        &[
            "-c:v",
            "prores_ks",
            "-profile:v",
            "3",
            "-pix_fmt",
            "yuv422p10le",
        ],
    ) else {
        return;
    };
    let bin = sidecar_binary_path();
    for (transport, env) in [
        ("shared memory", &[][..]),
        ("inline (VANCE_IPC_SHM=0)", &[("VANCE_IPC_SHM", "0")][..]),
    ] {
        let mut client =
            DecoderClient::spawn_with_env(&bin, env.iter().copied()).expect("spawn decoder");
        let info = client.open(fx.path()).expect("open 4K ProRes");
        assert_eq!((info.width, info.height), (3840, 2160));
        assert_eq!(info.pixel_format, PixelFormat::Yuv420P10Le);
        // The consumer copies every frame once, as the renderer's texture
        // upload does; without that a lease would cost nothing to "use".
        let mut scratch = vec![0u8; info.pixel_format.frame_size(info.width, info.height)];

        let start = Instant::now();
        client.start_stream(0, 8).expect("start_stream");
        let mut frames = 0u32;
        while let Some(f) = client.next_frame().expect("next_frame") {
            assert_eq!(f.pixel_format, PixelFormat::Yuv420P10Le, "frame {frames}");
            scratch.copy_from_slice(&f.bytes);
            std::hint::black_box(&scratch);
            frames += 1;
        }
        let elapsed = start.elapsed();
        let stats = client.transport_stats();
        client.shutdown().expect("shutdown");
        assert_eq!(frames, 72, "3 s at 24 fps");
        let sent_shared = stats.shared + stats.spilled;
        if env.is_empty() {
            assert_eq!((sent_shared, stats.inline), (72, 0), "{stats:?}");
        } else {
            assert_eq!((sent_shared, stats.inline), (0, 72), "{stats:?}");
        }

        let ms_per_frame = elapsed.as_secs_f64() * 1000.0 / f64::from(frames);
        eprintln!(
            "prores_4k_decode_throughput, {transport}: {frames} frames in {:.0} ms = \
             {ms_per_frame:.1} ms/frame, {:.1} fps ({stats:?})",
            elapsed.as_secs_f64() * 1000.0,
            1000.0 / ms_per_frame
        );
    }
}
