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
