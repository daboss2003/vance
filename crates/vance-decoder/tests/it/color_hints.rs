//! Protocol v6 stream hints: the colour range, YUV matrix and sample aspect
//! ratio `StreamInfo` reports, which the client stamps on every frame.
//!
//! The hints describe the bytes as sent. Each case checks the samples too:
//! a full-range source keeps its full-range code values (also through the
//! 4:2:2 / 4:4:4 → 4:2:0 conversion and on VideoToolbox), and an RGB source,
//! which the sidecar converts to BT.709 limited-range YUV, is reported as
//! exactly that whatever its own tags say.
//!
//! Fixtures are flat colours generated with the `ffmpeg` CLI; `setparams`
//! tags the frames so the CLI doesn't convert their range for the encoder.

use crate::common::{drain, lavfi_fixture, spawn};
use vance_decoder::protocol::{ColorMatrixHint as M, ColorRangeHint as R};
use vance_decoder::{DecoderClient, FrameColor, FrameData, PixelFormat, StreamInfo};

/// The Y plane's samples (8-bit widened; 10-bit little-endian).
fn luma(f: &FrameData) -> Vec<u16> {
    assert!(
        !f.pixel_format.is_rgba(),
        "{:?} has no luma",
        f.pixel_format
    );
    let n = f.width as usize * f.height as usize;
    match f.pixel_format.bytes_per_sample() {
        1 => f.bytes[..n].iter().map(|&b| u16::from(b)).collect(),
        _ => f.bytes[..2 * n]
            .chunks_exact(2)
            .map(|s| u16::from_le_bytes([s[0], s[1]]))
            .collect(),
    }
}

fn assert_luma(what: &str, f: &FrameData, y: u16, tol: u16) {
    let samples = luma(f);
    let (min, max) = (samples.iter().min().unwrap(), samples.iter().max().unwrap());
    assert!(
        y.abs_diff(*min) <= tol && y.abs_diff(*max) <= tol,
        "{what}: luma spans {min}..={max}, expected {y}±{tol}"
    );
}

/// `info`'s hints, the first frame's and every streamed frame's are
/// `(range, matrix, sample_aspect)`, and the luma is `y`.
fn assert_hints(
    what: &str,
    client: &mut DecoderClient,
    info: &StreamInfo,
    hints: (R, M, (u32, u32)),
    y: Option<u16>,
) {
    let (range, matrix, sample_aspect) = hints;
    assert_eq!(
        (info.color_range, info.color_matrix, info.sample_aspect),
        hints,
        "{what}: stream info hints"
    );
    let want = FrameColor {
        range,
        matrix,
        sample_aspect,
    };
    assert_eq!(FrameColor::from_stream(info), want, "{what}");
    let f = client
        .decode_at(0)
        .unwrap_or_else(|e| panic!("{what}: decode_at 0: {e}"));
    assert_eq!(f.color, want, "{what}: decode_at frame's hints");
    assert_eq!(f.pixel_format, info.pixel_format, "{what}");
    if let Some(y) = y {
        assert_luma(what, &f, y, 2);
    }
    client.start_stream(0, 4).expect("start_stream");
    let frames = drain(what, client);
    assert!(!frames.is_empty(), "{what}: no frames streamed");
    for (i, f) in frames.iter().enumerate() {
        assert_eq!(f.color, want, "{what}: streamed frame {i}'s hints");
    }
}

/// H.264 tagged with each range and matrix: the hints follow the tags, and
/// the samples are what was encoded (a full-range Y of 250 is not squeezed),
/// in software and with the default decoder (VideoToolbox on a Mac).
#[test]
fn h264_range_and_matrix_follow_the_stream() {
    let cases = [
        ("pc", "bt470bg", 250, (R::Full, M::Bt601)),
        ("tv", "bt709", 200, (R::Limited, M::Bt709)),
        ("tv", "smpte170m", 200, (R::Limited, M::Bt601)),
        ("tv", "bt2020nc", 200, (R::Limited, M::Bt2020Ncl)),
        ("", "", 200, (R::Unknown, M::Unknown)),
    ];
    for (range, matrix, y, (want_range, want_matrix)) in cases {
        let name = format!("hints-{range}-{matrix}.mp4");
        let mut lavfi =
            format!("color=c=gray:s=320x240:r=25:d=0.4,format=yuv420p,lutyuv=y={y}:u=100:v=150");
        let mut args = vec!["-c:v", "libx264"];
        if !range.is_empty() {
            lavfi.push_str(&format!(",setparams=range={range}:colorspace={matrix}"));
            args.extend(["-color_range", range, "-colorspace", matrix]);
        }
        let Some(fx) = lavfi_fixture(&name, "libx264", &lavfi, &args) else {
            return;
        };
        for (mode, env) in [
            ("software", &[("VANCE_FORCE_SW", "1")][..]),
            ("default", &[]),
        ] {
            let what = format!("{name}, {mode}");
            let mut client = spawn(env);
            let info = client.open(fx.path()).expect("open");
            assert_hints(
                &what,
                &mut client,
                &info,
                (want_range, want_matrix, (1, 1)),
                Some(y),
            );
            client.shutdown().expect("shutdown");
        }
    }
}

/// Anamorphic 1440x1080 at 4:3 pixels (HDV) in MP4 and MKV, and PAL 16:9
/// at 64:45: the sample aspect ratio, and the pixel aspect it gives.
#[test]
fn sample_aspect_ratio_is_reported() {
    let cases = [
        ("sar-hdv.mp4", "1440x1080", "4/3", (4, 3)),
        ("sar-hdv.mkv", "1440x1080", "4/3", (4, 3)),
        ("sar-pal.mp4", "720x576", "64/45", (64, 45)),
        ("sar-square.mp4", "320x240", "1", (1, 1)),
    ];
    for (name, size, sar, want) in cases {
        let lavfi = format!("testsrc2=s={size}:r=25:d=0.2,setsar={sar}");
        let Some(fx) = lavfi_fixture(
            name,
            "libx264",
            &lavfi,
            &["-c:v", "libx264", "-pix_fmt", "yuv420p"],
        ) else {
            return;
        };
        let mut client = spawn(&[("VANCE_FORCE_SW", "1")]);
        let info = client.open(fx.path()).expect("open");
        let range = info.color_range;
        assert_hints(name, &mut client, &info, (range, M::Unknown, want), None);
        let f = client.decode_at(0).expect("decode_at");
        let pixel_aspect = want.0 as f32 / want.1 as f32;
        assert!(
            (f.color.pixel_aspect() - pixel_aspect).abs() < 1e-6,
            "{name}: pixel aspect {}",
            f.color.pixel_aspect()
        );
        client.shutdown().expect("shutdown");
    }
}

/// Sources the sidecar converts with swscale. Between YUV formats it keeps
/// the samples' range (and matrix), so the hints are the source's: a
/// full-range 4:4:4 or 10-bit 4:2:2 source keeps its full-range values in
/// 4:2:0, a full-range JPEG too. An RGB image becomes BT.709 limited-range
/// YUV, and says so; a gray image keeps its (full-range) values.
#[test]
fn converted_sources_report_the_wire_range_and_matrix() {
    struct Case {
        name: &'static str,
        encoder: &'static str,
        lavfi: &'static str,
        args: &'static [&'static str],
        wire: PixelFormat,
        hints: (R, M),
        y: u16,
    }
    let cases = [
        Case {
            name: "conv-ffv1-444-full.mkv",
            encoder: "ffv1",
            lavfi: "color=c=gray:s=161x91:r=25:d=0.2,format=yuv444p,lutyuv=y=250:u=90:v=200,\
                    setparams=range=pc:colorspace=bt709",
            args: &["-c:v", "ffv1", "-color_range", "pc", "-colorspace", "bt709"],
            wire: PixelFormat::Yuv420P,
            hints: (R::Full, M::Bt709),
            y: 250,
        },
        Case {
            name: "conv-ffv1-444-limited.mkv",
            encoder: "ffv1",
            lavfi: "color=c=gray:s=161x91:r=25:d=0.2,format=yuv444p,lutyuv=y=200:u=90:v=200,\
                    setparams=range=tv:colorspace=smpte170m",
            args: &[
                "-c:v",
                "ffv1",
                "-color_range",
                "tv",
                "-colorspace",
                "smpte170m",
            ],
            wire: PixelFormat::Yuv420P,
            hints: (R::Limited, M::Bt601),
            y: 200,
        },
        Case {
            name: "conv-ffv1-422p10-full.mkv",
            encoder: "ffv1",
            lavfi: "color=c=gray:s=161x91:r=25:d=0.2,format=yuv422p10le,\
                    lutyuv=y=1000:u=300:v=700,setparams=range=pc:colorspace=bt2020nc",
            args: &[
                "-c:v",
                "ffv1",
                "-color_range",
                "pc",
                "-colorspace",
                "bt2020nc",
            ],
            wire: PixelFormat::Yuv420P10Le,
            hints: (R::Full, M::Bt2020Ncl),
            y: 1000,
        },
        Case {
            name: "conv-mjpeg-422.avi",
            encoder: "mjpeg",
            lavfi: "color=c=gray:s=161x91:r=25:d=0.2,format=yuvj422p,lutyuv=y=250:u=90:v=200",
            args: &["-c:v", "mjpeg", "-pix_fmt", "yuvj422p", "-q:v", "2"],
            wire: PixelFormat::Yuv420P,
            hints: (R::Full, M::Bt601),
            y: 250,
        },
        // Pure red, whose own tags say full range and an RGB "matrix".
        Case {
            name: "conv-rgb24.png",
            encoder: "png",
            lavfi: "color=c=red:s=161x91,format=rgb24,lutrgb=r=255:g=0:b=0",
            args: &["-frames:v", "1", "-pix_fmt", "rgb24"],
            wire: PixelFormat::Yuv420P,
            hints: (R::Limited, M::Bt709),
            y: 63,
        },
        Case {
            name: "conv-gray.png",
            encoder: "png",
            lavfi: "color=c=black:s=64x48,format=gray,geq=lum=200",
            args: &["-frames:v", "1", "-pix_fmt", "gray"],
            wire: PixelFormat::Yuv420P,
            hints: (R::Full, M::Unknown),
            y: 200,
        },
    ];
    for c in cases {
        let Some(fx) = lavfi_fixture(c.name, c.encoder, c.lavfi, c.args) else {
            continue;
        };
        let mut client = spawn(&[]);
        let info = client.open(fx.path()).expect("open");
        assert_eq!(info.pixel_format, c.wire, "{}: wire format", c.name);
        assert_hints(
            c.name,
            &mut client,
            &info,
            (c.hints.0, c.hints.1, (1, 1)),
            Some(c.y),
        );
        client.shutdown().expect("shutdown");
    }
}
