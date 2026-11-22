//! Frame steps: a `decode_at` a little ahead of the frame the last one
//! returned decodes on from where the decoder is instead of seeking, and
//! gives exactly the frames independent seeks give.
//!
//! "Independent seeks" here are `decode_at` calls in descending order, so
//! every one is behind the last and seeks. Every case also checks what must
//! leave the decoder's position unusable: a target past the end, a backward
//! target, a stream in between, and a `Seek` request.
//!
//! Fixtures are generated with the `ffmpeg` CLI; a case is skipped when it
//! or the encoder it needs isn't available.

use crate::common::{assert_same, drain, lavfi_fixture, spawn, Fixture};
use std::process::Command;
use std::time::Instant;
use vance_decoder::{DecoderClient, FrameData};

/// The sidecar configurations every case runs in: software with one decode
/// thread (a stream then runs on the one-shot decoder) and with eight, and
/// the default (VideoToolbox for H.264 / HEVC on a Mac).
const MODES: [(&str, &[(&str, &str)]); 3] = [
    (
        "1 thread",
        &[("VANCE_DECODE_THREADS", "1"), ("VANCE_FORCE_SW", "1")],
    ),
    (
        "8 threads",
        &[("VANCE_DECODE_THREADS", "8"), ("VANCE_FORCE_SW", "1")],
    ),
    ("default", &[]),
];

fn decode(what: &str, client: &mut DecoderClient, pts: i64) -> FrameData {
    client
        .decode_at(pts)
        .unwrap_or_else(|e| panic!("{what}: decode_at {pts}: {e}"))
        .clone()
}

/// 50 frames of 320x240 `testsrc2` at 25 fps in 12-frame GOPs, encoded with
/// `encode` and written by `muxer`.
fn gop_fixture(name: &str, encoder: &str, encode: &[&str]) -> Option<Fixture> {
    let mut args = encode.to_vec();
    args.extend(["-g", "12", "-sc_threshold", "0", "-pix_fmt", "yuv420p"]);
    lavfi_fixture(name, encoder, "testsrc2=s=320x240:r=25:d=2", &args)
}

fn assert_steps_match_seeks(name: &str, path: &str) {
    for (mode, env) in MODES {
        let what = format!("{name}, {mode}");
        let mut client = spawn(env);
        client
            .open(path)
            .unwrap_or_else(|e| panic!("{what}: open: {e}"));
        client.start_stream(0, 4).expect("start_stream");
        let frames = drain(&what, &mut client);
        let n = frames.len();
        assert!(n >= 40, "{what}: only {n} frames");

        let mut seeked: Vec<FrameData> = (0..n)
            .rev()
            .map(|k| decode(&what, &mut client, frames[k].pts))
            .collect();
        seeked.reverse();
        for k in 0..n {
            let at = format!("{what}: step to frame {k}");
            let got = decode(&at, &mut client, frames[k].pts);
            assert_same(&at, &got, &seeked[k]);
            assert_same(&at, &got, &frames[k]);
        }

        // Past the last frame: an error, after which the decoder still
        // decodes backwards and then forwards again.
        assert!(
            client.decode_at(frames[n - 1].pts + 1).is_err(),
            "{what}: a target past the last frame must fail"
        );
        for k in [10, 11, 12, 13, 20] {
            let at = format!("{what}: frame {k} after the end");
            assert_same(&at, &decode(&at, &mut client, frames[k].pts), &frames[k]);
        }
        // Between frames: the next frame.
        for k in 20..30 {
            let at = format!("{what}: just after frame {k}");
            assert_same(
                &at,
                &decode(&at, &mut client, frames[k].pts + 1),
                &frames[k + 1],
            );
        }
        // A jump within the window that crosses a keyframe, then back.
        for k in [31, 38, 30] {
            let at = format!("{what}: jump to frame {k}");
            assert_same(&at, &decode(&at, &mut client, frames[k].pts), &frames[k]);
        }

        // A stream in between moves the one-shot decoder (with one thread
        // it streams on it): the next target ahead must not decode on from
        // where the stream stopped.
        client
            .start_stream(frames[33].pts, 4)
            .expect("start_stream at frame 33");
        for (k, want) in frames.iter().enumerate().skip(33).take(3) {
            let f = client.next_frame().expect("next_frame").expect("a frame");
            assert_same(&format!("{what}: stream frame {k}"), &f, want);
        }
        client.stop_stream().expect("stop_stream");
        let at = format!("{what}: frame 32 after a stream");
        assert_same(&at, &decode(&at, &mut client, frames[32].pts), &frames[32]);

        // Same for a `Seek` request.
        client.seek(frames[40].pts).expect("seek");
        let at = format!("{what}: frame 35 after a seek to frame 40");
        assert_same(&at, &decode(&at, &mut client, frames[35].pts), &frames[35]);
        client.shutdown().expect("shutdown");
    }
}

#[test]
fn h264_b_frame_steps_match_seeks() {
    let Some(fx) = gop_fixture(
        "steps-h264.mp4",
        "libx264",
        &["-c:v", "libx264", "-bf", "3"],
    ) else {
        return;
    };
    assert_steps_match_seeks("h264 b-frames mp4", &fx.path());
}

#[test]
fn hevc_steps_match_seeks() {
    let params = "log-level=error:keyint=12:min-keyint=12:scenecut=0:open-gop=0:bframes=3";
    let Some(fx) = gop_fixture(
        "steps-hevc.mp4",
        "libx265",
        &["-c:v", "libx265", "-x265-params", params, "-tag:v", "hvc1"],
    ) else {
        return;
    };
    assert_steps_match_seeks("hevc b-frames mp4", &fx.path());
}

/// MPEG-TS has no keyframe index: the read-ahead finds the keyframes.
#[test]
fn h264_ts_steps_match_seeks() {
    let Some(fx) = gop_fixture(
        "steps-h264.ts",
        "libx264",
        &["-c:v", "libx264", "-bf", "2", "-f", "mpegts"],
    ) else {
        return;
    };
    assert_steps_match_seeks("h264 ts", &fx.path());
}

/// AVI stores no pts: the frames' timestamps come from the packets sent
/// since the last flush, which must carry over from one step to the next.
/// MPEG-4 Part 2 there has open GOPs.
#[test]
fn avi_steps_match_seeks() {
    for (name, encoder, encode) in [
        (
            "steps-h264.avi",
            "libx264",
            &["-c:v", "libx264", "-bf", "2"][..],
        ),
        (
            "steps-mpeg4.avi",
            "mpeg4",
            &["-c:v", "mpeg4", "-bf", "2"][..],
        ),
    ] {
        let mut args = encode.to_vec();
        args.extend(["-f", "avi"]);
        let Some(fx) = gop_fixture(name, encoder, &args) else {
            continue;
        };
        assert_steps_match_seeks(name, &fx.path());
    }
}

const STEP_PROBE_ENV: &str = "VANCE_TEST_STEP_PROBE";
const STEP_PROBE_TARGETS: &str = "VANCE_TEST_STEP_TARGETS";

/// The sidecar counts how each `decode_at` ran and logs it when the file
/// closes. 4 s at 25 fps in 12-frame GOPs without B-frames (keyframes at
/// 0, 12, 24, ... 96), so every count is exact:
/// - frames 0, 1, 2: a seek, then two steps;
/// - 20, 21: a jump (restarted at keyframe 12), then a step;
/// - 40, 49: jumps restarted at 36 and 48;
/// - 10: backwards, a seek; 90: more than 2 s ahead, a seek.
///
/// MJPEG (every frame a keyframe) always seeks: decoding on would only
/// read the frames in between for nothing.
///
/// Intentional: each case re-runs this test binary with only
/// `step_log_probe`, as `threading::convert_threads_override_is_logged`
/// does, to read the sidecar's stderr.
#[test]
fn steps_decode_forward_and_restart_at_keyframes() {
    let targets = "0,1,2,20,21,40,49,10,90";
    let cases = [
        (
            "probe-h264.mp4",
            "libx264",
            &["-c:v", "libx264", "-bf", "0"][..],
            "decode_at: 6 decoded forward (3 restarted at a keyframe), 3 after a seek",
        ),
        (
            "probe-h264.ts",
            "libx264",
            &["-c:v", "libx264", "-bf", "0", "-f", "mpegts"][..],
            "decode_at: 6 decoded forward (3 restarted at a keyframe), 3 after a seek",
        ),
        (
            "probe-mjpeg.avi",
            "mjpeg",
            &["-c:v", "mjpeg", "-f", "avi"][..],
            "decode_at: 0 decoded forward (0 restarted at a keyframe), 9 after a seek",
        ),
    ];
    for (name, encoder, encode, expected) in cases {
        let mut args = encode.to_vec();
        args.extend(["-g", "12", "-sc_threshold", "0"]);
        if encoder != "mjpeg" {
            args.extend(["-pix_fmt", "yuv420p"]);
        }
        let Some(fx) = lavfi_fixture(name, encoder, "testsrc2=s=320x240:r=25:d=4", &args) else {
            continue;
        };
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "frame_step::step_log_probe",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(STEP_PROBE_ENV, &fx.0)
            .env(STEP_PROBE_TARGETS, targets)
            .output()
            .expect("re-run the test binary");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "{name}: probe failed:\n{stdout}\n{stderr}"
        );
        assert!(
            stderr.contains(expected),
            "{name}: no {expected:?} in the sidecar's log:\n{stderr}"
        );
    }
}

#[test]
#[ignore = "run in a process of its own by steps_decode_forward_and_restart_at_keyframes"]
fn step_log_probe() {
    let Some(path) = std::env::var_os(STEP_PROBE_ENV) else {
        return;
    };
    let targets: Vec<usize> = std::env::var(STEP_PROBE_TARGETS)
        .expect("targets")
        .split(',')
        .map(|k| k.parse().unwrap())
        .collect();
    let path = path.to_string_lossy().into_owned();
    // Software, so the counts don't depend on VideoToolbox.
    let mut client = spawn(&[("VANCE_FORCE_SW", "1")]);
    client.open(&path).expect("open");
    client.start_stream(0, 4).expect("start_stream");
    let frames = drain("probe", &mut client);
    for k in targets {
        let got = decode("probe", &mut client, frames[k].pts);
        assert_same(&format!("probe frame {k}"), &got, &frames[k]);
    }
    // Closing the file logs the counts.
    client.close().expect("close");
    client.shutdown().expect("shutdown");
}

/// What a frame step costs: ms per `decode_at` stepping one frame forward
/// at a time, against ms per independent seek to the same frames (each
/// target behind the last), for 1080p H.264 and HEVC with B-frames in 2 s
/// GOPs, in software and with the default decoder. A manual benchmark:
///   cargo test -p vance-decoder --test it -- --ignored --nocapture frame_step::step_cost
#[test]
#[ignore = "manual benchmark: run with --ignored --nocapture"]
fn step_cost() {
    let lavfi = "testsrc2=s=1920x1080:r=25:d=4";
    let x265 = "log-level=error:keyint=50:min-keyint=50:scenecut=0:bframes=3";
    let fixtures = [
        lavfi_fixture(
            "cost-h264.mp4",
            "libx264",
            lavfi,
            &[
                "-c:v", "libx264", "-bf", "3", "-g", "50", "-pix_fmt", "yuv420p",
            ],
        ),
        lavfi_fixture(
            "cost-hevc.mp4",
            "libx265",
            lavfi,
            &[
                "-c:v",
                "libx265",
                "-x265-params",
                x265,
                "-pix_fmt",
                "yuv420p",
                "-tag:v",
                "hvc1",
            ],
        ),
    ];
    for fx in fixtures.iter().flatten() {
        for (mode, env) in [MODES[1], MODES[2]] {
            let mut client = spawn(env);
            client.open(fx.path()).expect("open");
            client.start_stream(0, 4).expect("start_stream");
            let pts: Vec<i64> = drain("cost", &mut client).iter().map(|f| f.pts).collect();
            // Frames 30..78: mid-GOP to the end of the next GOP and beyond.
            let steps: Vec<i64> = pts[30..78].to_vec();
            let timed = |client: &mut DecoderClient, order: &[i64]| {
                let start = Instant::now();
                for &p in order {
                    client.decode_at(p).expect("decode_at");
                }
                start.elapsed().as_secs_f64() * 1000.0 / order.len() as f64
            };
            client.decode_at(pts[29]).expect("decode_at");
            let forward = timed(&mut client, &steps);
            let mut back = steps.clone();
            back.reverse();
            client.decode_at(pts[90]).expect("decode_at");
            let seeks = timed(&mut client, &back);
            eprintln!(
                "step_cost {}, {mode}: {forward:.1} ms per step forward, {seeks:.1} ms per seek",
                fx.0.file_name().unwrap().to_string_lossy()
            );
            client.shutdown().expect("shutdown");
        }
    }
}
