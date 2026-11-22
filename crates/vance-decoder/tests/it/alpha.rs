//! Sources with an alpha channel arrive as `PixelFormat::Rgba8`: packed
//! 8-bit RGBA with straight alpha, one plane of `width * 4` bytes per row,
//! converted from YUV with the stream's own matrix and range.
//!
//! Every case uses the default spawn, so on a Mac a codec VideoToolbox
//! handles (ProRes) must still decode in software: VideoToolbox would give
//! NV12 without the alpha. Fixtures are generated with the `ffmpeg` CLI and
//! skipped when it or the encoder isn't available.

use crate::common::{drain, lavfi_fixture, spawn};
use vance_decoder::protocol::{ColorMatrixHint, ColorRangeHint};
use vance_decoder::{DecoderClient, FrameData, PixelFormat};

/// The RGBA pixel at (`x`, `y`).
fn rgba(f: &FrameData, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * f.width + x) * 4) as usize;
    f.bytes[i..i + 4].try_into().unwrap()
}

/// `f` is an `Rgba8` frame whose every pixel's colour is within `tol` of
/// `rgb` and whose alpha is within `alpha_tol` of `alpha(x)`.
fn assert_rgba(
    what: &str,
    f: &FrameData,
    rgb: [u8; 3],
    tol: u8,
    alpha: impl Fn(u32) -> u8,
    alpha_tol: u8,
) {
    assert_eq!(f.pixel_format, PixelFormat::Rgba8, "{what}");
    assert_eq!(f.strides, vec![f.width * 4], "{what}: strides");
    assert_eq!(
        f.bytes.len(),
        PixelFormat::Rgba8.frame_size(f.width, f.height),
        "{what}: payload size"
    );
    for y in 0..f.height {
        for x in 0..f.width {
            let px = rgba(f, x, y);
            let close = (0..3).all(|c| px[c].abs_diff(rgb[c]) <= tol);
            assert!(
                close,
                "{what}: ({x}, {y}) is {px:?}, expected {rgb:?}±{tol}"
            );
            let a = alpha(x);
            assert!(
                px[3].abs_diff(a) <= alpha_tol,
                "{what}: ({x}, {y}) alpha {}, expected {a}±{alpha_tol}",
                px[3]
            );
        }
    }
}

/// Open `path`: `Rgba8`, hints full range / no matrix / square pixels, and
/// the first frame and every streamed frame as `assert_rgba` checks them.
fn assert_alpha_source(
    what: &str,
    client: &mut DecoderClient,
    path: &str,
    rgb: [u8; 3],
    tol: u8,
    alpha: impl Fn(u32) -> u8 + Copy,
    alpha_tol: u8,
) {
    let info = client
        .open(path)
        .unwrap_or_else(|e| panic!("{what}: open: {e}"));
    assert_eq!(
        info.pixel_format,
        PixelFormat::Rgba8,
        "{what}: stream format"
    );
    assert_eq!(
        (info.color_range, info.color_matrix, info.sample_aspect),
        (ColorRangeHint::Full, ColorMatrixHint::Unknown, (1, 1)),
        "{what}: hints of RGBA frames"
    );
    let f = client.decode_at(0).expect("decode_at 0");
    assert_eq!(f.color.range, ColorRangeHint::Full, "{what}: frame hints");
    assert_rgba(what, &f, rgb, tol, alpha, alpha_tol);
    client.start_stream(0, 4).expect("start_stream");
    let frames = drain(what, client);
    assert!(!frames.is_empty(), "{what}: no frames");
    for (i, f) in frames.iter().enumerate() {
        assert_rgba(
            &format!("{what}: streamed frame {i}"),
            f,
            rgb,
            tol,
            alpha,
            alpha_tol,
        );
    }
}

/// An alpha gradient across 64 columns: 0, 4, ... 252.
fn ramp(x: u32) -> u8 {
    (x * 4) as u8
}

/// ProRes 4444 with a gradient alpha, pure green encoded with each matrix:
/// decoded with the stream's matrix it is green again. (Read as BT.709, the
/// BT.601 samples give green ≈ 215; read as BT.601, the BT.709 ones ≈ 20
/// red.) Tagged in the container (a `colr` atom, written when primaries,
/// transfer and matrix are all given), or only in the frame headers; untagged,
/// the sidecar assumes BT.709, as the renderer does for untagged YUV.
#[test]
fn prores_4444_alpha_decodes_to_rgba_with_its_matrix() {
    for (name, matrix, tags) in [
        ("alpha-prores-709.mov", "bt709", Some(("bt709", true))),
        ("alpha-prores-601.mov", "bt601", Some(("smpte170m", true))),
        (
            "alpha-prores-601-frames.mov",
            "bt601",
            Some(("smpte170m", false)),
        ),
        ("alpha-prores-untagged.mov", "bt709", None),
    ] {
        let mut lavfi = format!(
            "color=c=0x00FF00:s=64x48:r=25:d=0.2,format=rgba,\
             geq=r='r(X,Y)':g='g(X,Y)':b='b(X,Y)':a='X*4',\
             scale=out_color_matrix={matrix}:out_range=tv,format=yuva444p10le"
        );
        let mut args = vec![
            "-c:v",
            "prores_ks",
            "-profile:v",
            "4444",
            "-pix_fmt",
            "yuva444p10le",
        ];
        if let Some((tag, in_container)) = tags {
            lavfi.push_str(&format!(",setparams=range=tv:colorspace={tag}"));
            args.extend(["-colorspace", tag, "-color_range", "tv"]);
            if in_container {
                args.extend(["-color_primaries", tag, "-color_trc", "bt709"]);
            }
        }
        let Some(fx) = lavfi_fixture(name, "prores_ks", &lavfi, &args) else {
            return;
        };
        let mut client = spawn(&[]);
        assert_alpha_source(name, &mut client, &fx.path(), [0, 255, 0], 4, ramp, 2);
        client.shutdown().expect("shutdown");
    }
}

/// FFV1 4:4:4 + alpha with exact code values (Y 180, Cb 100, Cr 150, A 128),
/// tagged full and limited range: the YUV → RGB step uses the tagged range.
#[test]
fn yuva_range_is_the_streams() {
    // BT.709 from Y 180, Cb 100, Cr 150, full range: R = Y + 1.5748 Cr',
    // G = Y - 0.1873 Cb' - 0.4681 Cr', B = Y + 1.8556 Cb' (Cb' = Cb - 128);
    // limited range scales Y' = (Y - 16) * 255/219 and Cb', Cr' by 255/224.
    for (name, range, rgb) in [
        ("alpha-ffv1-full.mkv", "pc", [215, 175, 128]),
        ("alpha-ffv1-limited.mkv", "tv", [230, 185, 132]),
    ] {
        let lavfi = format!(
            "color=c=gray:s=64x48:r=25:d=0.2,format=yuva444p,\
             lutyuv=y=180:u=100:v=150:a=128,setparams=range={range}:colorspace=bt709"
        );
        let args = [
            "-c:v",
            "ffv1",
            "-pix_fmt",
            "yuva444p",
            "-color_range",
            range,
            "-colorspace",
            "bt709",
        ];
        let Some(fx) = lavfi_fixture(name, "ffv1", &lavfi, &args) else {
            return;
        };
        let mut client = spawn(&[]);
        assert_alpha_source(name, &mut client, &fx.path(), rgb, 2, |_| 128, 0);
        client.shutdown().expect("shutdown");
    }
}

/// RGB sources with alpha: a PNG already in the wire layout (sent as is) and
/// QuickTime Animation's ARGB (reordered). Lossless, so exact.
#[test]
fn rgb_alpha_sources_arrive_exactly() {
    for (name, encoder, pix_fmt, extra) in [
        ("alpha-rgba.png", "png", "rgba", &["-frames:v", "1"][..]),
        ("alpha-qtrle.mov", "qtrle", "argb", &[][..]),
    ] {
        let lavfi =
            format!("color=c=red:s=64x48:r=25:d=0.2,format={pix_fmt},geq=r=255:g=0:b=0:a='X*4'");
        let mut args = vec!["-c:v", encoder, "-pix_fmt", pix_fmt];
        args.extend_from_slice(extra);
        let Some(fx) = lavfi_fixture(name, encoder, &lavfi, &args) else {
            return;
        };
        let mut client = spawn(&[]);
        assert_alpha_source(name, &mut client, &fx.path(), [255, 0, 0], 0, ramp, 0);
        client.shutdown().expect("shutdown");
    }
}

/// What an alpha layer costs to play: 4K ProRes 4444 with alpha streamed as
/// RGBA (software decode, swscale to RGBA, shared memory), ms per frame. A
/// manual benchmark:
///   cargo test -p vance-decoder --test it -- --ignored --nocapture alpha::prores_4444_4k
#[test]
#[ignore = "manual benchmark: run with --ignored --nocapture"]
fn prores_4444_4k_rgba_throughput() {
    let Some(fx) = lavfi_fixture(
        "alpha-4k.mov",
        "prores_ks",
        "testsrc2=s=3840x2160:r=24:d=1,format=yuva444p10le",
        &[
            "-c:v",
            "prores_ks",
            "-profile:v",
            "4444",
            "-pix_fmt",
            "yuva444p10le",
        ],
    ) else {
        return;
    };
    let mut client = spawn(&[]);
    let info = client.open(fx.path()).expect("open");
    assert_eq!(info.pixel_format, PixelFormat::Rgba8);
    let mut scratch = vec![0u8; PixelFormat::Rgba8.frame_size(info.width, info.height)];
    let start = std::time::Instant::now();
    client.start_stream(0, 8).expect("start_stream");
    let mut frames = 0u32;
    while let Some(f) = client.next_frame().expect("next_frame") {
        scratch.copy_from_slice(&f.bytes);
        frames += 1;
    }
    let ms = start.elapsed().as_secs_f64() * 1000.0 / f64::from(frames);
    eprintln!("prores_4444_4k_rgba_throughput: {frames} frames, {ms:.1} ms/frame");
    client.shutdown().expect("shutdown");
}

/// Open `path`: an opaque YUV stream (`Yuv420P`, BT.709 limited from the
/// RGB the sidecar converts) whose first frame's luma is flat at `y` ±2.
fn assert_opaque(what: &str, client: &mut DecoderClient, path: &str, y: u8) {
    let info = client
        .open(path)
        .unwrap_or_else(|e| panic!("{what}: open: {e}"));
    assert_eq!(
        info.pixel_format,
        PixelFormat::Yuv420P,
        "{what}: stream format"
    );
    assert_eq!(
        (info.color_range, info.color_matrix),
        (ColorRangeHint::Limited, ColorMatrixHint::Bt709),
        "{what}: hints"
    );
    let f = client.decode_at(0).expect("decode_at 0");
    assert_eq!(f.pixel_format, PixelFormat::Yuv420P, "{what}: frame format");
    let luma = &f.bytes[..(f.width * f.height) as usize];
    let (min, max) = (luma.iter().min().unwrap(), luma.iter().max().unwrap());
    assert!(
        min.abs_diff(y) <= 2 && max.abs_diff(y) <= 2,
        "{what}: luma {min}..={max}, expected {y}"
    );
}

/// Where the 4th byte is commonly padding, an all-zero alpha channel means
/// nothing: uncompressed RGB (AVI, QuickTime, Matroska) and QuickTime
/// Animation, as screen captures write them. FFmpeg decodes it as alpha 0,
/// which made the clip fully transparent; such streams take the opaque path
/// (pure blue, Y' 32). The Matroska stream doesn't know its own duration
/// (the file's is used for the middle and last samples).
#[test]
fn alpha_that_is_padding_is_opaque() {
    let blue = "color=c=blue:s=64x48:r=25:d=0.2,format=bgra,geq=r=0:g=0:b=255:a=0";
    let raw = |muxer| vec!["-c:v", "rawvideo", "-pix_fmt", "bgra", "-f", muxer];
    let mut mkv = raw("matroska");
    mkv.extend(["-allow_raw_vfw", "1"]);
    for (name, encoder, args) in [
        ("padding-zero.avi", "rawvideo", raw("avi")),
        ("padding-zero-raw.mov", "rawvideo", raw("mov")),
        ("padding-zero.mkv", "rawvideo", mkv),
        (
            "padding-zero-qtrle.mov",
            "qtrle",
            vec!["-c:v", "qtrle", "-pix_fmt", "argb", "-f", "mov"],
        ),
    ] {
        let Some(fx) = lavfi_fixture(name, encoder, blue, &args) else {
            return;
        };
        let mut client = spawn(&[]);
        assert_opaque(name, &mut client, &fx.path(), 32);
        client.shutdown().expect("shutdown");
    }
}

/// A truncated file (a copy cut short): its index promises frames that
/// aren't there, so no frame starts where the last one should. The last
/// frame that does decode is checked instead, and the padding clip is
/// still opaque.
#[test]
fn a_truncated_padding_clip_is_opaque() {
    let Some(fx) = lavfi_fixture(
        "padding-truncated.mov",
        "rawvideo",
        "color=c=blue:s=64x48:r=25:d=0.4,format=bgra,geq=r=0:g=0:b=255:a=0",
        &[
            "-c:v",
            "rawvideo",
            "-pix_fmt",
            "bgra",
            "-movflags",
            "+faststart",
        ],
    ) else {
        return;
    };
    // 10 frames of 64 x 48 x 4 bytes, the index first: cut 3 and a bit.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&fx.0)
        .expect("open");
    let len = file.metadata().expect("metadata").len();
    file.set_len(len - 3 * 64 * 48 * 4 - 100).expect("truncate");
    drop(file);
    let mut client = spawn(&[]);
    assert_opaque("truncated", &mut client, &fx.path(), 32);
    client.shutdown().expect("shutdown");
}

/// Uncompressed 32-bit AVI ("BI_RGB") can carry real alpha (After Effects'
/// and Premiere's "Millions of Colors+"): a gradient or a matte stays RGBA.
#[test]
fn bi_rgb_avi_keeps_real_alpha() {
    let raw = ["-c:v", "rawvideo", "-pix_fmt", "bgra", "-f", "avi"];
    for (name, alpha, expect) in [
        ("bi-rgb-ramp.avi", "'X*4'", ramp as fn(u32) -> u8),
        ("bi-rgb-matte.avi", "'if(lt(X,32),0,255)'", |x| {
            if x < 32 {
                0
            } else {
                255
            }
        }),
    ] {
        let lavfi =
            format!("color=c=blue:s=64x48:r=25:d=0.2,format=bgra,geq=r=0:g=0:b=255:a={alpha}");
        let Some(fx) = lavfi_fixture(name, "rawvideo", &lavfi, &raw) else {
            return;
        };
        let mut client = spawn(&[]);
        assert_alpha_source(name, &mut client, &fx.path(), [0, 0, 255], 0, expect, 0);
        client.shutdown().expect("shutdown");
    }
}

/// In a codec made for alpha, all-zero alpha is real transparency, kept as
/// such (an overlay that is blank at the frames looked at would otherwise
/// become an opaque block over the video).
#[test]
fn zero_alpha_in_alpha_codecs_stays_transparent() {
    let blue = "color=c=blue:s=64x48:r=25:d=0.2,format=bgra,geq=r=0:g=0:b=255:a=0";
    for (name, encoder, args) in [
        (
            "zero-alpha.mkv",
            "ffv1",
            &["-c:v", "ffv1", "-pix_fmt", "bgra"][..],
        ),
        (
            "zero-alpha.png",
            "png",
            &["-c:v", "png", "-pix_fmt", "rgba", "-frames:v", "1"][..],
        ),
    ] {
        let Some(fx) = lavfi_fixture(name, encoder, blue, args) else {
            return;
        };
        let mut client = spawn(&[]);
        assert_alpha_source(name, &mut client, &fx.path(), [0, 0, 255], 0, |_| 0, 0);
        client.shutdown().expect("shutdown");
    }
}

/// An animation that starts fully transparent keeps its alpha, also in a
/// format whose 4th byte may be padding (QuickTime Animation): only a stream
/// all-zero at its first, middle and last frame is taken for padding.
#[test]
fn a_fade_in_from_transparent_keeps_its_alpha() {
    let Some(fx) = lavfi_fixture(
        "alpha-fade-in.mov",
        "qtrle",
        "color=c=0x00FF00:s=64x48:r=25:d=0.4,format=argb,\
         geq=r=0:g=255:b=0:a='if(lt(N,3),0,255)'",
        &["-c:v", "qtrle", "-pix_fmt", "argb"],
    ) else {
        return;
    };
    let mut client = spawn(&[]);
    let info = client.open(fx.path()).expect("open");
    assert_eq!(
        info.pixel_format,
        PixelFormat::Rgba8,
        "a fade-in keeps its alpha"
    );
    let (tb, fr) = (info.time_base, info.frame_rate);
    let frame_pts = |k: i64| k * tb.1 * fr.1 / (tb.0 * fr.0);
    let first = client.decode_at(frame_pts(0)).expect("frame 0");
    assert_rgba("fade-in frame 0", &first, [0, 255, 0], 0, |_| 0, 0);
    let later = client.decode_at(frame_pts(5)).expect("frame 5");
    assert_rgba("fade-in frame 5", &later, [0, 255, 0], 0, |_| 255, 0);
    client.shutdown().expect("shutdown");
}

/// The same for uncompressed RGB in Matroska, whose stream has no duration
/// of its own: the middle and last samples come from the file's duration,
/// so the clip isn't judged by its transparent first frame alone.
#[test]
fn a_fade_in_without_a_stream_duration_keeps_its_alpha() {
    let Some(fx) = lavfi_fixture(
        "alpha-fade-in.mkv",
        "rawvideo",
        "color=c=blue:s=64x48:r=25:d=0.4,format=bgra,geq=r=0:g=0:b=255:a='if(lt(N,3),0,255)'",
        &[
            "-c:v",
            "rawvideo",
            "-pix_fmt",
            "bgra",
            "-allow_raw_vfw",
            "1",
        ],
    ) else {
        return;
    };
    let mut client = spawn(&[]);
    let info = client.open(fx.path()).expect("open");
    assert_eq!(
        info.duration_pts, None,
        "the fixture's stream has no duration"
    );
    assert_eq!(
        info.pixel_format,
        PixelFormat::Rgba8,
        "a fade-in keeps its alpha"
    );
    client.shutdown().expect("shutdown");
}
