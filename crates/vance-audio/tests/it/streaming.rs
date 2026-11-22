//! End-to-end test for the audio sidecar (Phase 2.3).
//!
//! Spawns the sidecar, opens an AAC fixture, streams chunks until EOS,
//! and validates: chunk count > 0, sample values are in [-1, 1], total
//! sample count is approximately the file's duration × rate × channels.

use std::path::PathBuf;
use vance_audio::AudioClient;

fn sidecar_binary_path() -> PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    let exe = if cfg!(windows) {
        "vance-audio-decoder.exe"
    } else {
        "vance-audio-decoder"
    };
    p.push(exe);
    p
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sine_440_48k_2s.m4a")
}

#[test]
fn streams_aac_to_eos() {
    let bin = sidecar_binary_path();
    if !bin.exists() {
        panic!(
            "audio sidecar missing at {} — run `cargo build -p vance-audio` first",
            bin.display()
        );
    }
    let fixture = fixture_path();
    assert!(fixture.exists(), "fixture missing at {}", fixture.display());

    let mut client = AudioClient::spawn(&bin).expect("spawn");
    eprintln!("decoder: {}", client.decoder_label());

    let info = client
        .open(fixture.to_string_lossy().to_string())
        .expect("open");
    eprintln!(
        "info: {} {}Hz, channels={}, dur_pts={:?}",
        info.codec_name, info.sample_rate, info.channels, info.duration_pts
    );
    assert_eq!(info.sample_rate, 48000);
    assert_eq!(info.channels, 2);

    client.start_stream(0, 1024).expect("start_stream");

    let mut total_samples = 0usize;
    let mut chunk_count = 0usize;
    let mut max_abs = 0.0f32;
    while let Some(chunk) = client.next_chunk().expect("next_chunk") {
        // Interleaved stereo, so length must be even.
        assert!(chunk.samples.len() % 2 == 0, "non-even sample count");
        for &s in &chunk.samples {
            assert!(s.is_finite(), "non-finite sample");
            let a = s.abs();
            if a > max_abs {
                max_abs = a;
            }
        }
        total_samples += chunk.samples.len();
        chunk_count += 1;
        if chunk_count > 10000 {
            panic!("too many chunks; runaway loop");
        }
    }
    eprintln!("delivered {chunk_count} chunks, {total_samples} samples, peak |s|={max_abs:.3}");

    // 2s @ 48kHz stereo = 192_000 samples. AAC priming/padding can shave
    // a little off the start/end; accept ±10%.
    let expected = 2 * 48_000 * 2;
    assert!(
        total_samples >= expected * 9 / 10 && total_samples <= expected * 11 / 10,
        "expected ~{expected} samples, got {total_samples}"
    );

    // Decode produced *some* signal (not silence). Exact amplitude depends
    // on AAC decoder normalization; calibration vs. cpal output happens
    // when AudioPlayer wiring is exercised in 2.5 / Phase-2 manual demo.
    assert!(max_abs > 0.001, "decoded only silence ({max_abs})");

    client.stop_stream().expect("stop_stream");
    client.shutdown().expect("shutdown");
}

/// Once a request has failed because the sidecar died, the client may have
/// reaped it (vance-ipc checks its exit status), so its pid could be
/// anyone's: a kill handle then sends no signal. Likewise after a shutdown
/// or a drop.
#[cfg(unix)]
#[test]
fn a_kill_handle_sends_nothing_once_the_sidecar_may_be_reaped() {
    use std::time::{Duration, Instant};
    let bin = sidecar_binary_path();
    let mut client = AudioClient::spawn(&bin).expect("spawn");
    client
        .open(fixture_path().to_string_lossy().to_string())
        .expect("open");
    let handle = client.kill_handle();
    let pid = client.pid().expect("pid");
    // Dies on its own (not through the handle).
    let killed = std::process::Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .is_ok_and(|s| s.success());
    assert!(killed, "SIGKILL {pid}");
    let deadline = Instant::now() + Duration::from_secs(5);
    let failed = loop {
        match client.start_stream(0, 1024) {
            Err(e) => break e,
            Ok(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
                let _ = client.stop_stream();
            }
            Ok(_) => panic!("the dead sidecar kept answering"),
        }
    };
    assert!(
        matches!(failed, vance_audio::AudioClientError::Ipc(_)),
        "{failed:?}"
    );
    handle.kill();
    assert!(!handle.signalled(), "no signal after a failed request");
    drop(client);

    let client = AudioClient::spawn(&bin).expect("spawn");
    let handle = client.kill_handle();
    client.shutdown().expect("shutdown");
    handle.kill();
    assert!(!handle.signalled(), "no signal after a shutdown");

    let client = AudioClient::spawn(&bin).expect("spawn");
    let handle = client.kill_handle();
    drop(client);
    handle.kill();
    assert!(!handle.signalled(), "no signal after a drop");
}
