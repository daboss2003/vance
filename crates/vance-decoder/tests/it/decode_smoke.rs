//! End-to-end smoke test: spawn the sidecar binary, open a tiny H.264
//! fixture, decode a frame, validate dimensions and pixel format.

use std::path::PathBuf;
use vance_decoder::{DecoderClient, PixelFormat};

fn sidecar_binary_path() -> PathBuf {
    // current_exe = target/<profile>/deps/<test_binary>
    let mut p = std::env::current_exe().unwrap();
    p.pop(); // deps/
    p.pop(); // <profile>/
    let exe = if cfg!(windows) {
        "vance-decoder.exe"
    } else {
        "vance-decoder"
    };
    p.push(exe);
    p
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/testsrc_320x240_5s.mp4")
}

#[test]
fn handshake_open_decode_close() {
    let bin = sidecar_binary_path();
    if !bin.exists() {
        panic!(
            "sidecar binary not built at {} — run `cargo build -p vance-decoder` first",
            bin.display()
        );
    }
    let fixture = fixture_path();
    assert!(
        fixture.exists(),
        "fixture missing: {} (regenerate with the ffmpeg command in tests/fixtures/README)",
        fixture.display()
    );

    let mut client = DecoderClient::spawn(&bin).expect("spawn decoder sidecar");
    eprintln!("ffmpeg version: {}", client.ffmpeg_version());

    let info = client
        .open(fixture.to_string_lossy().to_string())
        .expect("open fixture");
    assert_eq!(info.width, 320);
    assert_eq!(info.height, 240);
    // The decoder reports the format FFmpeg negotiated. On macs with
    // a VideoToolbox-backed h264 path that's NV12; on a software path
    // it's YUV420P. Both are 4:2:0 and the rest of the pipeline
    // (renderer upload_frame, proxy converter) handles either, so the
    // smoke test accepts both rather than pinning one platform's pick.
    assert!(
        matches!(info.pixel_format, PixelFormat::Yuv420P | PixelFormat::Nv12),
        "unexpected pixel format: {:?}",
        info.pixel_format
    );
    assert!(info.duration_pts.unwrap_or(0) > 0);
    assert_eq!(info.codec_name, "h264");

    // Decode the first frame (PTS 0).
    let frame0 = client.decode_at(0).expect("decode frame 0");
    assert_eq!(frame0.width, 320);
    assert_eq!(frame0.height, 240);
    assert_eq!(
        frame0.pixel_format, info.pixel_format,
        "frame format should match the stream's reported format"
    );
    let expected_size = frame0.pixel_format.frame_size(320, 240);
    assert_eq!(
        frame0.bytes.len(),
        expected_size,
        "frame 0 byte count mismatch"
    );
    // YUV420P → 3 planes (Y/U/V); NV12 → 2 planes (Y + interleaved UV).
    let expected_planes = match frame0.pixel_format {
        PixelFormat::Yuv420P | PixelFormat::Yuv420P10Le => 3,
        PixelFormat::Nv12 => 2,
    };
    assert_eq!(
        frame0.strides.len(),
        expected_planes,
        "plane count should match {:?}",
        frame0.pixel_format
    );

    // Decode a frame ~1 second in. Stream time-base for libx264 is typically
    // (1, 15360) — but we pass PTS in the stream's time base, so use the
    // info.time_base to translate. ~1 second = time_base.den / time_base.num.
    let one_sec_pts = info.time_base.1 / info.time_base.0;
    let frame_at_1s = client.decode_at(one_sec_pts).expect("decode at ~1 second");
    assert!(
        frame_at_1s.pts >= one_sec_pts,
        "got pts {} expected >= {}",
        frame_at_1s.pts,
        one_sec_pts
    );

    client.close().expect("close");
    client.shutdown().expect("shutdown");
}

/// A sidecar driven in the raw protocol, stderr captured. No reply says how
/// many released slots were stale, so the sidecar logs it there and
/// [`RawSidecar::finish`] reads the counts back.
struct RawSidecar {
    child: std::process::Child,
    /// `None` once closed, which ends the sidecar's request loop.
    stdin: Option<std::process::ChildStdin>,
    stdout: std::io::BufReader<std::process::ChildStdout>,
    /// Collects all of stderr until the sidecar exits.
    stderr: Option<std::thread::JoinHandle<String>>,
}

impl RawSidecar {
    /// Start the sidecar without a handshake.
    fn start() -> Self {
        use std::io::Read;
        use std::process::{Command, Stdio};
        let bin = sidecar_binary_path();
        assert!(bin.exists(), "decoder binary missing at {}", bin.display());
        let mut cmd = Command::new(bin);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = vance_ipc::process::spawn(&mut cmd).expect("spawn decoder");
        let stdin = child.stdin.take();
        let stdout = std::io::BufReader::new(child.stdout.take().expect("stdout piped"));
        let mut err = child.stderr.take().expect("stderr piped");
        let stderr = std::thread::spawn(move || {
            let mut log = String::new();
            let _ = err.read_to_string(&mut log);
            log
        });
        Self {
            child,
            stdin,
            stdout,
            stderr: Some(stderr),
        }
    }

    /// Start the sidecar, shake hands and open the H.264 fixture.
    fn open_fixture() -> (Self, vance_decoder::StreamInfo) {
        use vance_decoder::protocol::{DecoderRequest as Req, DecoderResponse as Resp};
        let mut sc = Self::start();
        let hello = sc.request(
            vec![],
            Req::Hello {
                protocol_version: vance_decoder::protocol::PROTOCOL_VERSION,
            },
        );
        assert!(
            matches!(hello, Resp::Hello { protocol_version, .. }
                if protocol_version == vance_decoder::protocol::PROTOCOL_VERSION),
            "{hello:?}"
        );
        let open = Req::Open {
            path: fixture_path().to_string_lossy().into_owned(),
        };
        match sc.request(vec![], open) {
            Resp::Opened(info) => (sc, info),
            other => panic!("open: {other:?}"),
        }
    }

    fn send(&mut self, msg: &impl serde::Serialize) {
        use std::io::Write;
        let stdin = self.stdin.as_mut().expect("stdin open");
        vance_ipc::write_frame(stdin, msg).expect("send");
        stdin.flush().expect("flush");
    }

    fn recv(&mut self) -> vance_decoder::DecoderResponse {
        vance_ipc::read_frame(&mut self.stdout).expect("reply")
    }

    /// Send `request` with `releases` riding along, and wait for the reply.
    fn request(
        &mut self,
        releases: Vec<vance_decoder::protocol::SlotRef>,
        request: vance_decoder::DecoderRequest,
    ) -> vance_decoder::DecoderResponse {
        self.send(&vance_decoder::DecoderEnvelope { releases, request });
        self.recv()
    }

    fn recv_blob(&mut self, len: u64) -> Vec<u8> {
        vance_ipc::read_blob(&mut self.stdout, len).expect("blob")
    }

    /// Close stdin and wait for the sidecar to exit: its exit status and
    /// everything it wrote to stderr.
    fn exit(mut self) -> (std::process::ExitStatus, String) {
        drop(self.stdin.take());
        let status = self.child.wait().expect("wait for the sidecar");
        let log = self.stderr.take().expect("not yet joined").join().unwrap();
        eprint!("{log}");
        (status, log)
    }

    /// Shut down cleanly, with `releases` riding on the `Shutdown`. Returns
    /// the stale-release counts the sidecar logged, in order (it logs only
    /// non-zero counts).
    fn finish(mut self, releases: Vec<vance_decoder::protocol::SlotRef>) -> Vec<u32> {
        use vance_decoder::protocol::{DecoderRequest as Req, DecoderResponse as Resp};
        let bye = self.request(releases, Req::Shutdown);
        assert!(matches!(bye, Resp::Goodbye), "{bye:?}");
        let (status, log) = self.exit();
        assert!(status.success(), "sidecar exited with {status}");
        // The line `log_stale_releases` in src/bin/main.rs writes.
        log.lines()
            .filter_map(|line| {
                line.strip_prefix("[decoder] ignored ")?
                    .strip_suffix(" stale shared-memory slot releases")?
                    .parse()
                    .ok()
            })
            .collect()
    }
}

impl Drop for RawSidecar {
    fn drop(&mut self) {
        // A failed test must not leave the sidecar running.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The raw protocol: a region that doesn't exist can't be attached,
/// releases for a region the sidecar never attached are all stale, and
/// neither disturbs decoding (the releases ride on a `DecodeAt`). The
/// `Frame` reply's pixels follow it as exactly one blob of the announced
/// length.
#[test]
fn shm_requests_without_a_region_leave_frames_inline() {
    use vance_decoder::protocol::{
        DecoderRequest as Req, DecoderResponse as Resp, FramePayload, ShmRegionDesc, SlotRef,
    };
    let (mut sc, info) = RawSidecar::open_fixture();

    let missing = ShmRegionDesc {
        epoch: 1,
        os_name: vance_ipc::shm::object_name(std::process::id(), 1, 0x5eed_0000),
        slot_count: 3,
        slot_size: vance_ipc::shm::SLOT_ALIGN,
    };
    let reply = sc.request(vec![], Req::AttachShm(missing));
    assert!(matches!(reply, Resp::ShmUnavailable { .. }), "{reply:?}");

    let slot = SlotRef {
        epoch: 1,
        slot: 0,
        generation: 1,
        len: 10,
    };
    let mut releases = vec![slot, SlotRef { slot: 1, ..slot }];
    for _ in 0..2 {
        let decode = Req::DecodeAt { target_pts: 0 };
        let (meta, len) = match sc.request(std::mem::take(&mut releases), decode) {
            Resp::Frame {
                meta,
                payload: FramePayload::Inline { len },
            } => (meta, len),
            other => panic!("decode_at: {other:?}"),
        };
        assert_eq!((meta.width, meta.height), (info.width, info.height));
        let expected = meta.pixel_format.frame_size(meta.width, meta.height) as u64;
        assert_eq!(len, expected);
        assert_eq!(sc.recv_blob(len).len() as u64, expected);
    }
    assert_eq!(sc.finish(vec![]), [2], "stale releases logged");
}

/// The raw protocol with a real region: once attached, frames arrive as
/// `Shared` slot references with no blob, byte-identical to the inline
/// frames of a sidecar without a region. A release riding on a request is
/// taken back before that request is served, so its slot is reused at once
/// under a new generation; stale releases are ignored and logged; stopping
/// a stream hands its unsent slots back; with no slot free the main thread
/// goes inline; and a newer region replaces the old one.
#[test]
fn shm_frames_match_inline_frames_and_slots_come_back() {
    use std::sync::Arc;
    use vance_decoder::protocol::{
        DecoderRequest as Req, DecoderResponse as Resp, FrameMeta, FramePayload, SlotRef,
    };
    use vance_ipc::shm::{Access, Adopted, LeaseTable, ReleaseQueue, ShmRegion, SLOT_ALIGN};

    let decode = |sc: &mut RawSidecar, releases: Vec<SlotRef>, target_pts| match sc
        .request(releases, Req::DecodeAt { target_pts })
    {
        Resp::Frame { meta, payload } => (meta, payload),
        other => panic!("decode_at {target_pts}: {other:?}"),
    };

    // The reference: an inline frame from a sidecar that never gets a region.
    let (mut inline_sc, _) = RawSidecar::open_fixture();
    let (meta0, bytes0) = match decode(&mut inline_sc, vec![], 0) {
        (meta, FramePayload::Inline { len }) => (meta, inline_sc.recv_blob(len)),
        other => panic!("expected an inline frame, got {other:?}"),
    };

    let (mut sc, info) = RawSidecar::open_fixture();
    let frame_size = info.pixel_format.frame_size(info.width, info.height) as u64;
    let slot_size = frame_size.div_ceil(SLOT_ALIGN) * SLOT_ALIGN;
    // The parent's mapping is read-only, as in the real client.
    let mut region = ShmRegion::create(1, 3, slot_size, Access::ReadOnly).expect("create region");
    let reply = sc.request(vec![], Req::AttachShm(region.desc().clone()));
    assert!(matches!(reply, Resp::ShmAttached { epoch: 1 }), "{reply:?}");
    // The parent owns the name: the child must not have unlinked it.
    region
        .unlink_name()
        .expect("parent unlinks after the attach");
    let region = Arc::new(region);
    let queue = Arc::new(ReleaseQueue::default());
    // Lease budget = slot count, so every adopt here is a zero-copy view.
    let table = LeaseTable::new(Arc::clone(&region), Arc::clone(&queue), 3);
    let read = |r: SlotRef| match table.adopt(r).unwrap() {
        Adopted::Leased(lease) => lease,
        other => panic!("{other:?}"),
    };
    let shared = |(meta, payload): (FrameMeta, FramePayload)| match payload {
        FramePayload::Shared(r) => (meta, r),
        other => panic!("expected a shared frame, got {other:?}"),
    };

    let (meta, r) = shared(decode(&mut sc, vec![], 0));
    assert_eq!(meta, meta0);
    assert_eq!((r.epoch, r.len), (1, frame_size));
    let lease = read(r);
    assert!(lease[..] == bytes0[..], "shared frame differs from inline");
    drop(lease);
    let released = queue.drain();
    assert_eq!(released, [r]);

    // The release rides on the next decode and is taken back first, so that
    // decode reuses the slot, under a new generation. (No blob followed the
    // shared frame, or this reply would be garbage.)
    let (_, r2) = shared(decode(&mut sc, released, 0));
    assert_eq!((r2.slot, r2.generation), (r.slot, r.generation + 1));
    let lease2 = read(r2);
    assert!(
        lease2[..] == bytes0[..],
        "reused slot holds the wrong frame"
    );

    // A second release of the first use is stale (logged: 1). It rides on
    // the stream start, which it doesn't disturb.
    let started = sc.request(
        vec![r],
        Req::StartStream {
            from_pts: 0,
            buffer_frames: 8,
        },
    );
    assert!(matches!(started, Resp::Streaming { .. }), "{started:?}");
    let (_, streamed) = match sc.request(vec![], Req::NextFrame) {
        Resp::Frame { meta, payload } => shared((meta, payload)),
        other => panic!("next_frame: {other:?}"),
    };
    // Let the decode thread fill the free slot and wait for another.
    std::thread::sleep(std::time::Duration::from_millis(300));
    // Stopping a stream returns every slot its decode thread had filled but
    // not sent; `streamed` rides on the stop. The parent keeps `r2`.
    let stopped = sc.request(vec![streamed], Req::StopStream);
    assert!(matches!(stopped, Resp::Stopped), "{stopped:?}");
    // Two slots are free again (one is still leased): two shared frames,
    // then the main thread, which never waits, sends the next one inline.
    let (_, a) = shared(decode(&mut sc, vec![], 0));
    let (_, b) = shared(decode(&mut sc, vec![], 0));
    let lease_a = read(a);
    assert!(lease_a[..] == bytes0[..]);
    match decode(&mut sc, vec![], 0) {
        (meta, FramePayload::Inline { len }) => {
            assert_eq!(meta, meta0);
            assert!(sc.recv_blob(len) == bytes0, "inline fallback differs");
        }
        other => panic!("expected an inline frame with every slot out, got {other:?}"),
    }

    // A bigger region (new epoch) replaces this one while its slots are
    // still out; releases for the old epoch are then stale (logged: 3), and
    // the decode they ride on is served from the new region.
    let new_region =
        ShmRegion::create(2, 3, 2 * slot_size, Access::ReadOnly).expect("create region 2");
    let reply = sc.request(vec![], Req::AttachShm(new_region.desc().clone()));
    assert!(matches!(reply, Resp::ShmAttached { epoch: 2 }), "{reply:?}");
    let (_, r3) = shared(decode(&mut sc, vec![], 0));
    assert_eq!(r3.epoch, 2);
    drop((lease2, lease_a));
    let mut old = queue.drain();
    old.push(b);
    let (_, r4) = shared(decode(&mut sc, old, 0));
    assert_eq!(r4.epoch, 2);

    // Releases of the new region's slots, riding on the shutdown, are not.
    assert_eq!(sc.finish(vec![r3, r4]), [1, 3], "stale releases logged");
    assert_eq!(inline_sc.finish(vec![]), [] as [u32; 0]);
}

/// A client on protocol v3 sends a bare `Hello` (no envelope), which this
/// sidecar can't read. It still gets this sidecar's `Hello` back, so its
/// handshake fails as a version mismatch instead of on a dead pipe.
#[test]
fn a_v3_client_hears_this_sidecars_version() {
    use vance_decoder::protocol::{DecoderResponse as Resp, PROTOCOL_VERSION};
    // Variant 0 of v3's `DecoderRequest`.
    #[derive(serde::Serialize)]
    enum V3Request {
        Hello { protocol_version: u32 },
    }
    let mut sc = RawSidecar::start();
    sc.send(&V3Request::Hello {
        protocol_version: 3,
    });
    let reply = sc.recv();
    assert!(
        matches!(
            reply,
            Resp::Hello {
                protocol_version: PROTOCOL_VERSION,
                ..
            }
        ),
        "{reply:?}"
    );
    let (status, log) = sc.exit();
    assert!(!status.success(), "an unreadable request ends the sidecar");
    assert!(
        log.contains(&format!(
            "unreadable request (this sidecar speaks protocol v{PROTOCOL_VERSION})"
        )),
        "{log}"
    );
}
