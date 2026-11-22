//! Helpers shared by the colour-hint, alpha, frame-step and keyframe test
//! modules. (The older modules keep their own copies.)

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use vance_decoder::{DecoderClient, FrameData};

pub fn sidecar_binary_path() -> PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.push(if cfg!(windows) {
        "vance-decoder.exe"
    } else {
        "vance-decoder"
    });
    p
}

/// A generated fixture file, deleted on drop.
pub struct Fixture(pub PathBuf);

impl Fixture {
    pub fn path(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Whether the local ffmpeg CLI can encode with `encoder`.
pub fn has_encoder(encoder: &str) -> bool {
    let Ok(out) = Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
    else {
        return false;
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|line| line.split_whitespace().nth(1) == Some(encoder))
}

static SEQ: AtomicUsize = AtomicUsize::new(0);

/// A unique temp path ending in `name` (its extension picks the muxer).
pub fn temp_path(name: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("vance-it-{}-{n}-{name}", std::process::id()))
}

/// Run the ffmpeg CLI with `args` (after the common flags), or panic.
pub fn ffmpeg(what: &str, args: &[&str]) {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .output()
        .expect("run ffmpeg");
    assert!(
        out.status.success(),
        "ffmpeg failed to {what}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `ffmpeg -f lavfi -i <lavfi> <out_args> <temp file named name>`, or `None`
/// (skip, logged) when ffmpeg can't encode with `encoder`.
pub fn lavfi_fixture(name: &str, encoder: &str, lavfi: &str, out_args: &[&str]) -> Option<Fixture> {
    if !has_encoder(encoder) {
        eprintln!("{name}: ffmpeg with the {encoder} encoder not available, skipping");
        return None;
    }
    let fixture = Fixture(temp_path(name));
    let path = fixture.path();
    let mut args = vec!["-f", "lavfi", "-i", lavfi];
    args.extend_from_slice(out_args);
    args.push(&path);
    ffmpeg(&format!("generate {name}"), &args);
    Some(fixture)
}

/// A sidecar with `env` set.
pub fn spawn(env: &[(&str, &str)]) -> DecoderClient {
    let bin = sidecar_binary_path();
    assert!(bin.exists(), "decoder binary missing at {}", bin.display());
    DecoderClient::spawn_with_env(&bin, env.iter().copied()).expect("spawn decoder")
}

/// Same pts, geometry, format, strides, colour hints and pixels.
pub fn assert_same(what: &str, a: &FrameData, b: &FrameData) {
    assert_eq!(
        (
            a.pts,
            a.width,
            a.height,
            a.pixel_format,
            &a.strides,
            a.color
        ),
        (
            b.pts,
            b.width,
            b.height,
            b.pixel_format,
            &b.strides,
            b.color
        ),
        "{what}: frame description"
    );
    assert_eq!(a.bytes.len(), b.bytes.len(), "{what}: payload size");
    if let Some(i) = a.bytes.iter().zip(b.bytes.iter()).position(|(x, y)| x != y) {
        panic!("{what} (pts {}): pixels differ from byte {i}", a.pts);
    }
}

/// Every frame of the stream started with `start_stream`, as owned copies.
pub fn drain(what: &str, client: &mut DecoderClient) -> Vec<FrameData> {
    let mut frames = Vec::new();
    while let Some(f) = client
        .next_frame()
        .unwrap_or_else(|e| panic!("{what}: frame {}: {e}", frames.len()))
    {
        frames.push(f.clone());
        assert!(frames.len() <= 5000, "{what}: stream never ends");
    }
    frames
}
