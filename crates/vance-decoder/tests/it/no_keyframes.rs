//! Streams whose packets carry no keyframe flags.
//!
//! `seek` looks for a keyframe at or before the target, stepping further
//! back up to the stream start; when even the start has none it decodes from
//! the stream's first packet. A Matroska file whose blocks are all marked
//! non-key (its cues still list where the clusters are) reaches that last
//! step on every seek. FFmpeg's Matroska demuxer drops non-key packets after
//! an ordinary seek until a keyframe comes, which here is never: the fall
//! back to the first packet must seek without that (`AVSEEK_FLAG_ANY`).
//!
//! FFV1 decodes from its first frame whatever the container says (each
//! frame says itself whether it is a keyframe).

use crate::common::{assert_same, drain, lavfi_fixture, spawn, Fixture};

/// The value of the EBML variable-length integer at `data[p..]` and its
/// length; `marker` keeps the length marker bit (element IDs do).
fn vint(data: &[u8], p: usize, marker: bool) -> (u64, usize) {
    let len = data[p].leading_zeros() as usize + 1;
    assert!(len <= 8, "bad EBML vint at {p}");
    let first = if marker {
        u64::from(data[p])
    } else {
        u64::from(data[p]) & (0xff >> len)
    };
    let value = data[p + 1..p + len]
        .iter()
        .fold(first, |v, &b| (v << 8) | u64::from(b));
    (value, len)
}

/// Clear the keyframe flag of every SimpleBlock in `data[start..end]`,
/// descending into the Segment and its Clusters. Returns how many.
fn clear_keyframe_flags(data: &mut [u8], start: usize, end: usize) -> usize {
    const SEGMENT: u64 = 0x1853_8067;
    const CLUSTER: u64 = 0x1F43_B675;
    const SIMPLE_BLOCK: u64 = 0xA3;
    let mut cleared = 0;
    let mut p = start;
    while p < end {
        let (id, id_len) = vint(data, p, true);
        let (size, size_len) = vint(data, p + id_len, false);
        let body = p + id_len + size_len;
        // All ones: an element of unknown size runs to its parent's end.
        let unknown = size == (1u64 << (7 * size_len)) - 1;
        let stop = if unknown { end } else { body + size as usize };
        match id {
            SEGMENT | CLUSTER => cleared += clear_keyframe_flags(data, body, stop),
            SIMPLE_BLOCK => {
                // Track number (a vint), a 16-bit timecode, then the flags.
                let (_, track_len) = vint(data, body, false);
                data[body + track_len + 2] &= 0x7f;
                cleared += 1;
            }
            _ => {}
        }
        p = stop;
    }
    cleared
}

/// 2 s of 25 fps FFV1 in 12-frame GOPs, in Matroska with every block's
/// keyframe flag cleared.
fn keyframeless_mkv() -> Option<Fixture> {
    let fx = lavfi_fixture(
        "no-keyframes.mkv",
        "ffv1",
        "testsrc2=s=320x240:r=25:d=2",
        &["-c:v", "ffv1", "-g", "12"],
    )?;
    let mut data = std::fs::read(&fx.0).expect("read fixture");
    let len = data.len();
    assert_eq!(
        clear_keyframe_flags(&mut data, 0, len),
        50,
        "blocks patched"
    );
    std::fs::write(&fx.0, data).expect("write fixture");
    // Not checked with the CLI's ffprobe: FFmpeg 8 has an FFV1 parser that
    // flags keyframes from the frames themselves, the 7.1 the sidecar links
    // doesn't, so only the container's flags count there.
    Some(fx)
}

/// With 1 and with 8 decode threads: a stream from 0 gives all 50 frames,
/// `decode_at` anywhere (forwards and back) gives the streamed frame, and a
/// stream started mid-file gives the rest.
#[test]
fn a_stream_without_keyframe_flags_decodes_from_its_first_packet() {
    let Some(fx) = keyframeless_mkv() else {
        return;
    };
    for threads in ["1", "8"] {
        let what = format!("no keyframes, {threads} decode thread(s)");
        let mut client = spawn(&[("VANCE_DECODE_THREADS", threads)]);
        client.open(fx.path()).expect("open");
        client.start_stream(0, 4).expect("start_stream");
        let frames = drain(&what, &mut client);
        assert_eq!(frames.len(), 50, "{what}: frames from 0");
        for pair in frames.windows(2) {
            assert!(pair[0].pts < pair[1].pts, "{what}: pts not increasing");
        }
        for k in [30, 5, 49, 0, 13, 14, 40] {
            let at = format!("{what}: decode_at frame {k}");
            let got = client
                .decode_at(frames[k].pts)
                .unwrap_or_else(|e| panic!("{at}: {e}"));
            assert_same(&at, &got, &frames[k]);
        }
        client
            .start_stream(frames[20].pts, 4)
            .expect("start_stream mid-file");
        let tail = drain(&format!("{what}: stream from frame 20"), &mut client);
        assert_eq!(tail.len(), 30, "{what}: frames from frame 20");
        for (i, (got, want)) in tail.iter().zip(&frames[20..]).enumerate() {
            assert_same(&format!("{what}: stream frame {}", 20 + i), got, want);
        }
        client.shutdown().expect("shutdown");
    }
}
