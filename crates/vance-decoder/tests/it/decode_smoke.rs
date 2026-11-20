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

/// The raw v3 protocol: a region that doesn't exist can't be attached, a
/// release for a region the sidecar never attached is all stale, and
/// neither disturbs decoding. The `Frame` reply's pixels follow it as
/// exactly one blob of the announced length.
#[test]
fn shm_requests_without_a_region_leave_frames_inline() {
    use vance_decoder::protocol::{
        DecoderRequest as Req, DecoderResponse as Resp, FramePayload, ShmRegionDesc, SlotRef,
        PROTOCOL_VERSION,
    };
    let bin = sidecar_binary_path();
    assert!(bin.exists(), "decoder binary missing at {}", bin.display());
    let mut sc: vance_ipc::Sidecar<Req, Resp> =
        vance_ipc::Sidecar::spawn(&bin, std::iter::empty::<&str>()).expect("spawn");
    let hello = sc
        .request(&Req::Hello {
            protocol_version: PROTOCOL_VERSION,
        })
        .unwrap();
    assert!(
        matches!(hello, Resp::Hello { protocol_version, .. } if protocol_version == PROTOCOL_VERSION),
        "{hello:?}"
    );
    let info = match sc
        .request(&Req::Open {
            path: fixture_path().to_string_lossy().into_owned(),
        })
        .unwrap()
    {
        Resp::Opened(info) => info,
        other => panic!("open: {other:?}"),
    };

    let missing = ShmRegionDesc {
        epoch: 1,
        os_name: vance_ipc::shm::object_name(std::process::id(), 1, 0x5eed_0000),
        slot_count: 3,
        slot_size: vance_ipc::shm::SLOT_ALIGN,
    };
    let reply = sc.request(&Req::AttachShm(missing)).unwrap();
    assert!(matches!(reply, Resp::ShmUnavailable { .. }), "{reply:?}");

    let slot = SlotRef {
        epoch: 1,
        slot: 0,
        generation: 1,
        len: 10,
    };
    let reply = sc
        .request(&Req::Release {
            slots: vec![slot, SlotRef { slot: 1, ..slot }],
        })
        .unwrap();
    assert!(matches!(reply, Resp::Released { stale: 2 }), "{reply:?}");

    for _ in 0..2 {
        let (meta, len) = match sc.request(&Req::DecodeAt { target_pts: 0 }).unwrap() {
            Resp::Frame {
                meta,
                payload: FramePayload::Inline { len },
            } => (meta, len),
            other => panic!("decode_at: {other:?}"),
        };
        assert_eq!((meta.width, meta.height), (info.width, info.height));
        let expected = meta.pixel_format.frame_size(meta.width, meta.height) as u64;
        assert_eq!(len, expected);
        assert_eq!(sc.recv_blob(len).unwrap().len() as u64, expected);
    }
    assert!(matches!(sc.request(&Req::Shutdown).unwrap(), Resp::Goodbye));
}

/// The raw v3 protocol with a real region: once attached, frames arrive as
/// `Shared` slot references with no blob, byte-identical to the inline
/// frames of a sidecar without a region. Released slots are reused with a
/// new generation; stale releases are counted; stopping a stream hands its
/// unsent slots back; with no slot free the main thread goes inline; and a
/// newer region replaces the old one.
#[test]
fn shm_frames_match_inline_frames_and_slots_come_back() {
    use std::sync::Arc;
    use vance_decoder::protocol::{
        DecoderRequest as Req, DecoderResponse as Resp, FrameMeta, FramePayload, SlotRef,
        StreamInfo, PROTOCOL_VERSION,
    };
    use vance_ipc::shm::{Access, Adopted, LeaseTable, ReleaseQueue, ShmRegion, SLOT_ALIGN};
    type Sc = vance_ipc::Sidecar<Req, Resp>;

    let bin = sidecar_binary_path();
    assert!(bin.exists(), "decoder binary missing at {}", bin.display());
    let spawn_open = || -> (Sc, StreamInfo) {
        let mut sc: Sc = vance_ipc::Sidecar::spawn(&bin, std::iter::empty::<&str>()).unwrap();
        let hello = sc
            .request(&Req::Hello {
                protocol_version: PROTOCOL_VERSION,
            })
            .unwrap();
        assert!(matches!(hello, Resp::Hello { .. }), "{hello:?}");
        let open = Req::Open {
            path: fixture_path().to_string_lossy().into_owned(),
        };
        match sc.request(&open).unwrap() {
            Resp::Opened(info) => (sc, info),
            other => panic!("open: {other:?}"),
        }
    };
    let decode = |sc: &mut Sc, target_pts| match sc.request(&Req::DecodeAt { target_pts }) {
        Ok(Resp::Frame { meta, payload }) => (meta, payload),
        other => panic!("decode_at {target_pts}: {other:?}"),
    };
    let release = |sc: &mut Sc, slots: Vec<SlotRef>| match sc.request(&Req::Release { slots }) {
        Ok(Resp::Released { stale }) => stale,
        other => panic!("release: {other:?}"),
    };

    // The reference: an inline frame from a sidecar that never gets a region.
    let (mut inline_sc, _) = spawn_open();
    let (meta0, bytes0) = match decode(&mut inline_sc, 0) {
        (meta, FramePayload::Inline { len }) => (meta, inline_sc.recv_blob(len).unwrap()),
        other => panic!("expected an inline frame, got {other:?}"),
    };

    let (mut sc, info) = spawn_open();
    let frame_size = info.pixel_format.frame_size(info.width, info.height) as u64;
    let slot_size = frame_size.div_ceil(SLOT_ALIGN) * SLOT_ALIGN;
    // The parent's mapping is read-only, as in the real client.
    let mut region = ShmRegion::create(1, 3, slot_size, Access::ReadOnly).expect("create region");
    let reply = sc.request(&Req::AttachShm(region.desc().clone())).unwrap();
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

    let (meta, r) = shared(decode(&mut sc, 0));
    assert_eq!(meta, meta0);
    assert_eq!((r.epoch, r.len), (1, frame_size));
    let lease = read(r);
    assert!(lease[..] == bytes0[..], "shared frame differs from inline");
    drop(lease);
    // No blob followed the shared frame, or this reply would be garbage.
    assert_eq!(release(&mut sc, queue.drain()), 0);

    // The released slot is the next one used, under a new generation.
    let (_, r2) = shared(decode(&mut sc, 0));
    assert_eq!((r2.slot, r2.generation), (r.slot, r.generation + 1));
    let lease2 = read(r2);
    assert!(
        lease2[..] == bytes0[..],
        "reused slot holds the wrong frame"
    );
    // A second release of the first use is stale.
    assert_eq!(release(&mut sc, vec![r]), 1);

    // Stopping a stream returns every slot its decode thread had filled
    // but not sent. The parent keeps holding `r2`.
    let started = sc.request(&Req::StartStream {
        from_pts: 0,
        buffer_frames: 8,
    });
    assert!(matches!(started, Ok(Resp::Streaming { .. })), "{started:?}");
    let (_, streamed) = match sc.request(&Req::NextFrame) {
        Ok(Resp::Frame { meta, payload }) => shared((meta, payload)),
        other => panic!("next_frame: {other:?}"),
    };
    assert_eq!(release(&mut sc, vec![streamed]), 0);
    // Let the decode thread fill both free slots and wait for a third.
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(matches!(sc.request(&Req::StopStream), Ok(Resp::Stopped)));
    // Two slots are free again (one is still leased): two shared frames,
    // then the main thread, which never waits, sends the next one inline.
    let (_, a) = shared(decode(&mut sc, 0));
    let (_, b) = shared(decode(&mut sc, 0));
    let lease_a = read(a);
    assert!(lease_a[..] == bytes0[..]);
    match decode(&mut sc, 0) {
        (meta, FramePayload::Inline { len }) => {
            assert_eq!(meta, meta0);
            assert!(
                sc.recv_blob(len).unwrap() == bytes0,
                "inline fallback differs"
            );
        }
        other => panic!("expected an inline frame with every slot out, got {other:?}"),
    }

    // A bigger region (new epoch) replaces this one while its slots are
    // still out; releases for the old epoch are then stale.
    let new_region =
        ShmRegion::create(2, 3, 2 * slot_size, Access::ReadOnly).expect("create region 2");
    let reply = sc
        .request(&Req::AttachShm(new_region.desc().clone()))
        .unwrap();
    assert!(matches!(reply, Resp::ShmAttached { epoch: 2 }), "{reply:?}");
    let (_, r3) = shared(decode(&mut sc, 0));
    assert_eq!(r3.epoch, 2);
    drop((lease2, lease_a));
    let mut old = queue.drain();
    old.push(b);
    assert_eq!(release(&mut sc, old), 3);
    assert_eq!(release(&mut sc, vec![r3]), 0);

    assert!(matches!(sc.request(&Req::Shutdown), Ok(Resp::Goodbye)));
    assert!(matches!(
        inline_sc.request(&Req::Shutdown),
        Ok(Resp::Goodbye)
    ));
}
