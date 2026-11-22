//! End-to-end multi-track compositing through the Engine (ADR 0007): layer
//! order, opacity, picture-in-picture, keying, titles and Fusion over video,
//! aspect fit, track edits + undo, hidden tracks and gaps.
//!
//! Every check reads the frame back through BOTH `preview_frame_rgba` (scrub
//! path) and `export_frame_rgba` (export per-frame function).
//!
//! Source clips are generated with ffmpeg as lossless H.264, converted to
//! Rec.709 limited range explicitly. Solid colours use only 0 / 255 channels:
//! the engine decodes untagged video as Rec.709 and re-encodes sRGB, and the
//! two transfer curves agree only at the ends.
//!
//! Skips (eprintln) without ffmpeg / ffprobe, the sidecars or libx264.

use support::*;
use vance_timeline::{TitleParams, TrackKind};

/// Shared by the compositing modules (`lockstep`, `hdr_composite`,
/// `perf_compositing`).
pub(crate) mod support {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use vance_app_lib::engine::Engine;
    use vance_timeline::{ClipId, EffectValue, MediaItemId, TitleParams, TrackId, TrackKind};

    /// Canvas of the small tests.
    pub(crate) const W: u32 = 320;
    pub(crate) const H: u32 = 180;

    pub(crate) const BLACK: [u8; 3] = [0, 0, 0];
    pub(crate) const WHITE: [u8; 3] = [255, 255, 255];
    pub(crate) const RED: [u8; 3] = [255, 0, 0];
    pub(crate) const BLUE: [u8; 3] = [0, 0, 255];
    /// Primaries survive 4:2:0 YUV + Rec.709 → sRGB only roughly (the
    /// curves' linear toes differ ~3×), so colour checks are coarse; grey
    /// checks that matter use their own tight bounds.
    pub(crate) const COARSE: u8 = 40;

    fn tool(n: &str) -> bool {
        Command::new(n).arg("-version").output().is_ok()
    }

    pub(crate) fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn sidecars_present() -> bool {
        let d = workspace_root().join("target/debug");
        let ext = if cfg!(windows) { ".exe" } else { "" };
        ["vance-decoder", "vance-encoder", "vance-audio-decoder"]
            .iter()
            .all(|b| d.join(format!("{b}{ext}")).exists())
    }

    /// ffmpeg / ffprobe and the sidecars are present. Moves to the workspace
    /// root, where the engine looks for the encoder (every test in `it`
    /// moves to this same directory).
    pub(crate) fn ready(what: &str) -> bool {
        if !tool("ffmpeg") || !tool("ffprobe") {
            eprintln!("{what}: ffmpeg/ffprobe absent — skipping");
            return false;
        }
        if !sidecars_present() {
            eprintln!("{what}: sidecars missing — run `cargo build --workspace --bins`; skipping");
            return false;
        }
        std::env::set_current_dir(workspace_root()).expect("chdir workspace root");
        true
    }

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// Temp files with unique names (module tag + pid + counter), deleted
    /// on drop, including when an assert fails.
    pub(crate) struct Scratch {
        tag: &'static str,
        files: Vec<PathBuf>,
    }

    impl Scratch {
        pub(crate) fn new(tag: &'static str) -> Self {
            Scratch {
                tag,
                files: Vec::new(),
            }
        }

        pub(crate) fn path(&mut self, label: &str, ext: &str) -> PathBuf {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!(
                "vance-{}-{}-{n}-{label}.{ext}",
                self.tag,
                std::process::id()
            ));
            let _ = std::fs::remove_file(&p);
            self.files.push(p.clone());
            p
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            for f in &self.files {
                let _ = std::fs::remove_file(f);
                // A proxy made at a scratch path leaves its marker beside it.
                let mut marker = f.clone().into_os_string();
                marker.push(".vance-proxy");
                let _ = std::fs::remove_file(marker);
            }
        }
    }

    /// Encode the filter graph `graph` (lavfi sources inside; its last
    /// chain's output is the clip) as `secs` of lossless H.264 at `fps`.
    // Intentional: the explicit bt709 conversion. ffmpeg's default RGB→YUV
    // matrix is BT.601, but the engine decodes untagged / bt709 video as
    // Rec.709, so a default conversion would shift every colour.
    pub(crate) fn encode(out: &Path, graph: &str, secs: f64, fps: u32) -> bool {
        let graph = format!(
            "{graph},fps={fps},format=rgb24,scale=out_color_matrix=bt709:out_range=tv,format=yuv420p[v]"
        );
        let status = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(["-filter_complex", &graph, "-map", "[v]"])
            .args(["-t", &secs.to_string()])
            .args(["-c:v", "libx264", "-preset", "ultrafast", "-qp", "0"])
            .args(["-colorspace", "bt709", "-color_primaries", "bt709"])
            .args(["-color_trc", "bt709", "-color_range", "tv"])
            .arg(out)
            .status();
        let ok = status.is_ok_and(|s| s.success()) && out.exists();
        if !ok {
            eprintln!("ffmpeg could not encode `{graph}` (libx264 missing?) — skipping");
        }
        ok
    }

    /// `0xRRGGBB` → an ffmpeg `color` source of `w × h`, `secs` long.
    pub(crate) fn solid_src(rgb: u32, w: u32, h: u32, secs: f64) -> String {
        format!("color=c=0x{rgb:06X}:s={w}x{h}:r=24:d={secs}")
    }

    /// A white-card title (`bg_alpha` 1, no text) of `w × h`.
    pub(crate) fn card(rgb: [f32; 3], w: u32, h: u32) -> TitleParams {
        TitleParams {
            text: String::new(),
            size_px: 24.0,
            fg: rgb,
            bg: rgb,
            width: w,
            height: h,
            bg_alpha: 1.0,
        }
    }

    pub(crate) fn num(v: f64) -> EffectValue {
        EffectValue::Number(v)
    }

    /// An Engine with a fresh project ("Sequence 01", 24 fps, V1 + A1).
    pub(crate) struct Rig {
        pub(crate) engine: Engine,
        pub(crate) scratch: Scratch,
        canvas: (u32, u32),
    }

    impl Rig {
        pub(crate) fn new(tag: &'static str) -> Option<Rig> {
            if !ready(tag) {
                return None;
            }
            let mut engine = Engine::default();
            engine.new_project(tag.into()).expect("new project");
            Some(Rig {
                engine,
                scratch: Scratch::new(tag),
                canvas: (W, H),
            })
        }

        /// Encode `graph` and import it; `None` (skip) when ffmpeg can't.
        pub(crate) fn clip(&mut self, label: &str, graph: &str, secs: f64) -> Option<MediaItemId> {
            self.clip_at_fps(label, graph, secs, 24)
        }

        pub(crate) fn clip_at_fps(
            &mut self,
            label: &str,
            graph: &str,
            secs: f64,
            fps: u32,
        ) -> Option<MediaItemId> {
            let out = self.scratch.path(label, "mp4");
            if !encode(&out, graph, secs, fps) {
                return None;
            }
            Some(self.import(&out))
        }

        pub(crate) fn solid(
            &mut self,
            label: &str,
            rgb: u32,
            w: u32,
            h: u32,
        ) -> Option<MediaItemId> {
            self.clip(label, &solid_src(rgb, w, h, 2.0), 2.0)
        }

        pub(crate) fn import(&mut self, path: &Path) -> MediaItemId {
            let path = path.canonicalize().expect("canonical media path");
            let info = self
                .engine
                .import_media_file(&path.to_string_lossy())
                .unwrap_or_else(|e| panic!("import {}: {e}", path.display()));
            MediaItemId::from_uuid(info.id.parse().unwrap())
        }

        pub(crate) fn title(&mut self, name: &str, p: TitleParams, secs: f64) -> MediaItemId {
            let info = self
                .engine
                .create_title_media(name.into(), secs, p)
                .expect("create title");
            MediaItemId::from_uuid(info.id.parse().unwrap())
        }

        pub(crate) fn media_name(&self, id: MediaItemId) -> String {
            self.engine.media_get_info(id).expect("media info").name
        }

        /// Video tracks, bottom (V1) → top.
        pub(crate) fn video_tracks(&self) -> Vec<TrackId> {
            self.engine
                .current_sequence_contents()
                .expect("contents")
                .tracks
                .iter()
                .filter(|t| t.kind == "video")
                .map(|t| TrackId::from_uuid(t.id.parse().unwrap()))
                .collect()
        }

        /// `(kind, name)` of every track, in sequence order.
        pub(crate) fn track_names(&self) -> Vec<(String, String)> {
            self.engine
                .current_sequence_contents()
                .expect("contents")
                .tracks
                .iter()
                .map(|t| (t.kind.clone(), t.name.clone()))
                .collect()
        }

        /// The `n`th video track (0 = V1), adding tracks on top as needed.
        pub(crate) fn track(&mut self, n: usize) -> TrackId {
            while self.video_tracks().len() <= n {
                self.engine
                    .add_track(TrackKind::Video, None, None)
                    .expect("add video track");
            }
            self.video_tracks()[n]
        }

        pub(crate) fn place(&mut self, track: TrackId, media: MediaItemId, at: f64) -> ClipId {
            let id = self
                .engine
                .insert_clip_at(track, media, at)
                .expect("insert clip");
            ClipId::from_uuid(id.parse().unwrap())
        }

        pub(crate) fn fx(
            &mut self,
            track: TrackId,
            clip: ClipId,
            kind: &str,
            params: &[(&str, f64)],
        ) -> String {
            let params: BTreeMap<String, EffectValue> = params
                .iter()
                .map(|(k, v)| ((*k).to_string(), num(*v)))
                .collect();
            self.engine
                .add_effect_to_clip(track, clip, kind.into(), params)
                .unwrap_or_else(|e| panic!("add {kind}: {e}"))
        }

        /// Set the sequence canvas, which the frame reads also use.
        // Intentional: always set explicitly, after the clips are placed, so
        // the canvas doesn't depend on which clip was added first.
        pub(crate) fn canvas(&mut self, w: u32, h: u32) {
            self.engine
                .set_frame_size(Some((w, h)))
                .expect("set frame size");
            self.canvas = (w, h);
        }

        pub(crate) fn preview(&mut self, t: f64) -> Vec<u8> {
            let (w, h) = self.canvas;
            let buf = self
                .engine
                .preview_frame_rgba(t, w, h)
                .unwrap_or_else(|e| panic!("preview_frame_rgba({t}): {e}"));
            assert_eq!(buf.len(), (w * h * 4) as usize, "preview buffer size");
            buf
        }

        pub(crate) fn export(&mut self, t: f64) -> Vec<u8> {
            let (w, h) = self.canvas;
            let buf = self
                .engine
                .export_frame_rgba(t, w, h)
                .unwrap_or_else(|e| panic!("export_frame_rgba({t}): {e}"));
            assert_eq!(buf.len(), (w * h * 4) as usize, "export buffer size");
            buf
        }

        /// The frame at `t` through the scrub path and the export path.
        pub(crate) fn frames(&mut self, t: f64) -> [(&'static str, Vec<u8>); 2] {
            [("preview", self.preview(t)), ("export", self.export(t))]
        }

        pub(crate) fn width(&self) -> u32 {
            self.canvas.0
        }
    }

    pub(crate) fn px(buf: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * w + x) * 4) as usize;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    pub(crate) fn near(p: [u8; 4], want: [u8; 3], tol: u8) -> bool {
        p[3] == 255 && (0..3).all(|c| p[c].abs_diff(want[c]) <= tol)
    }

    /// Assert the opaque colour at each `(x, y)` is within `tol` of `want`.
    pub(crate) fn expect_at(
        buf: &[u8],
        w: u32,
        points: &[(u32, u32)],
        want: [u8; 3],
        tol: u8,
        what: &str,
    ) {
        for &(x, y) in points {
            let p = px(buf, w, x, y);
            assert!(
                near(p, want, tol),
                "{what}: pixel ({x},{y}) = {p:?}, want ≈{want:?} (±{tol})"
            );
        }
    }
}

const T: f64 = 0.5;

/// Corners and edge midpoints, 10 px in.
fn border_points() -> Vec<(u32, u32)> {
    vec![
        (10, 10),
        (W - 10, 10),
        (10, H - 10),
        (W - 10, H - 10),
        (W / 2, 10),
        (10, H / 2),
    ]
}

/// (a) Display-referred blending (user decision 2): 50% white over black is
/// 0.5 × 255 = 127.5 encoded, not the linear-light sRGB(0.5) = 188.
#[test]
fn half_opacity_white_title_over_black_video_is_display_mid_grey() {
    let Some(mut rig) = Rig::new("e2e-opacity") else {
        return;
    };
    let Some(black) = rig.solid("black", 0x000000, W, H) else {
        return;
    };
    let white = rig.title("white", card([1.0; 3], W, H), 2.0);
    let (v1, v2) = (rig.track(0), rig.track(1));
    rig.place(v1, black, 0.0);
    let c = rig.place(v2, white, 0.0);
    rig.fx(v2, c, "opacity", &[("value", 0.5)]);
    rig.canvas(W, H);
    for (path, buf) in rig.frames(T) {
        for (i, p) in buf.chunks_exact(4).enumerate() {
            let ok = p[3] == 255 && p[..3].iter().all(|c| (127..=128).contains(c));
            assert!(
                ok,
                "{path}: pixel ({}, {}) = {p:?}, want 127/128 grey (188 = linear-light blend)",
                i as u32 % W,
                i as u32 / W
            );
        }
    }
}

/// (b) A V2 clip at scale 0.5 sits centred over V1, which shows around it.
#[test]
fn picture_in_picture_shows_the_lower_track_around_it() {
    let Some(mut rig) = Rig::new("e2e-pip") else {
        return;
    };
    let (Some(blue), Some(red)) = (
        rig.solid("blue", 0x0000FF, W, H),
        rig.solid("red", 0xFF0000, W, H),
    ) else {
        return;
    };
    let (v1, v2) = (rig.track(0), rig.track(1));
    rig.place(v1, blue, 0.0);
    let pip = rig.place(v2, red, 0.0);
    rig.fx(v2, pip, "transform", &[("scale_x", 0.5), ("scale_y", 0.5)]);
    rig.canvas(W, H);
    // The PiP covers [80, 240) × [45, 135).
    let inside = [(160, 90), (90, 55), (230, 125)];
    let outside = [
        (40, 20),
        (300, 170),
        (160, 20),
        (60, 90),
        (260, 90),
        (160, 160),
    ];
    for (path, buf) in rig.frames(T) {
        expect_at(&buf, W, &inside, RED, COARSE, &format!("{path} PiP"));
        expect_at(
            &buf,
            W,
            &outside,
            BLUE,
            COARSE,
            &format!("{path} V1 around PiP"),
        );
    }
}

/// Green-screen plate: a red 96 × 96 square at (112, 42) on pure green.
fn green_screen(rig: &mut Rig) -> Option<vance_timeline::MediaItemId> {
    let graph = format!(
        "{}[bg];{}[fg];[bg][fg]overlay=112:42",
        solid_src(0x00FF00, W, H, 2.0),
        solid_src(0xFF0000, 96, 96, 2.0)
    );
    rig.clip("greenscreen", &graph, 2.0)
}

const SQUARE: [(u32, u32); 3] = [(160, 90), (125, 55), (195, 125)];
const KEYED: [(u32, u32); 4] = [(30, 30), (290, 150), (60, 90), (260, 20)];

/// (c) A keyed green-screen clip reveals V1 where it was green and keeps
/// the foreground square.
#[test]
fn keyed_green_screen_reveals_the_lower_track() {
    let Some(mut rig) = Rig::new("e2e-key") else {
        return;
    };
    let Some(blue) = rig.solid("blue", 0x0000FF, W, H) else {
        return;
    };
    let Some(plate) = green_screen(&mut rig) else {
        return;
    };
    let (v1, v2) = (rig.track(0), rig.track(1));
    rig.place(v1, blue, 0.0);
    let c = rig.place(v2, plate, 0.0);
    rig.fx(v2, c, vance_timeline::keyer::KEYER_KIND, &[]);
    rig.canvas(W, H);
    for (path, buf) in rig.frames(T) {
        expect_at(
            &buf,
            W,
            &SQUARE,
            RED,
            COARSE,
            &format!("{path} kept square"),
        );
        expect_at(&buf, W, &KEYED, BLUE, COARSE, &format!("{path} keyed → V1"));
    }
}

/// Matte view solos the keyer's matte in the preview only (export never
/// renders a matte), and clearing it restores the composite.
#[test]
fn matte_view_is_preview_only() {
    let Some(mut rig) = Rig::new("e2e-matte") else {
        return;
    };
    let Some(blue) = rig.solid("blue", 0x0000FF, W, H) else {
        return;
    };
    let Some(plate) = green_screen(&mut rig) else {
        return;
    };
    let (v1, v2) = (rig.track(0), rig.track(1));
    rig.place(v1, blue, 0.0);
    let c = rig.place(v2, plate, 0.0);
    rig.fx(v2, c, vance_timeline::keyer::KEYER_KIND, &[]);
    rig.canvas(W, H);

    rig.engine.set_matte_view(Some(c)).expect("set matte view");
    let matte = rig.preview(T);
    expect_at(&matte, W, &SQUARE, WHITE, 8, "matte: kept");
    expect_at(&matte, W, &KEYED, BLACK, 8, "matte: keyed");
    let export = rig.export(T);
    expect_at(
        &export,
        W,
        &KEYED,
        BLUE,
        COARSE,
        "export ignores matte view",
    );
    expect_at(
        &export,
        W,
        &SQUARE,
        RED,
        COARSE,
        "export ignores matte view",
    );

    rig.engine.set_matte_view(None).expect("clear matte view");
    let back = rig.preview(T);
    expect_at(&back, W, &KEYED, BLUE, COARSE, "matte view cleared");
}

/// (d) A transparent-background title shows the video around its text.
#[test]
fn transparent_title_shows_video_around_the_text() {
    let Some(mut rig) = Rig::new("e2e-title") else {
        return;
    };
    let Some(blue) = rig.solid("blue", 0x0000FF, W, H) else {
        return;
    };
    let title = rig.title(
        "text",
        TitleParams {
            text: "VANCE".into(),
            size_px: 96.0,
            fg: [1.0; 3],
            bg: [0.0; 3],
            width: W,
            height: H,
            bg_alpha: 0.0,
        },
        2.0,
    );
    let (v1, v2) = (rig.track(0), rig.track(1));
    rig.place(v1, blue, 0.0);
    rig.place(v2, title, 0.0);
    rig.canvas(W, H);
    for (path, buf) in rig.frames(T) {
        // A black (opaque-background) card would fail here.
        expect_at(
            &buf,
            W,
            &border_points(),
            BLUE,
            COARSE,
            &format!("{path} around text"),
        );
        let px_count = |want: [u8; 3]| {
            buf.chunks_exact(4)
                .filter(|p| near([p[0], p[1], p[2], p[3]], want, COARSE))
                .count()
        };
        let (white, blue) = (px_count(WHITE), px_count(BLUE));
        assert!(white > 300, "{path}: text missing ({white} white px)");
        assert!(
            blue > (W * H / 2) as usize,
            "{path}: video should fill most of the frame ({blue} blue px)"
        );
    }
}

/// A Fusion comp of one `MediaIn` naming `name` (by id or display name).
fn media_in_comp(name: &str) -> vance_fusion::Comp {
    use vance_fusion::{Comp, Fit, MediaSpec, Node, NodeKind};
    let mut comp = Comp::empty(W, H);
    comp.fps = 24.0;
    comp.duration_s = 2.0;
    comp.nodes.push(Node::new(
        "plate",
        "Plate",
        NodeKind::MediaIn(MediaSpec {
            name: name.into(),
            fit: Fit::Contain,
        }),
    ));
    comp.output = "plate".into();
    comp
}

/// (e) Fusion `MediaIn` resolves named media items (user decision 4: by id,
/// then display name) and shows their footage; an unknown name is empty.
#[test]
fn fusion_media_in_shows_the_named_footage() {
    let Some(mut rig) = Rig::new("e2e-fusion") else {
        return;
    };
    // Only in the library, not on the timeline.
    let Some(red) = rig.solid("plate", 0xFF0000, W, H) else {
        return;
    };
    let info = rig
        .engine
        .create_fusion_media(Some("Comp".into()), W, H, 2.0)
        .expect("create fusion media");
    let comp_id = vance_timeline::MediaItemId::from_uuid(info.id.parse().unwrap());
    let v1 = rig.track(0);
    rig.place(v1, comp_id, 0.0);
    rig.canvas(W, H);
    let everywhere = [(160, 90), (10, 10), (W - 10, H - 10), (40, 150)];
    for (how, name) in [
        ("display name", rig.media_name(red)),
        ("id", red.to_string()),
    ] {
        rig.engine
            .set_fusion_comp(comp_id, media_in_comp(&name))
            .expect("set fusion comp");
        for (path, buf) in rig.frames(T) {
            expect_at(
                &buf,
                W,
                &everywhere,
                RED,
                COARSE,
                &format!("{path} MediaIn by {how}"),
            );
        }
    }
    rig.engine
        .set_fusion_comp(comp_id, media_in_comp("no such media"))
        .expect("set fusion comp");
    for (path, buf) in rig.frames(T) {
        expect_at(
            &buf,
            W,
            &everywhere,
            BLACK,
            2,
            &format!("{path} unresolved MediaIn"),
        );
    }
}

/// (f) Decision 1: a 9:16 clip in a 16:9 sequence is fitted without
/// distortion; the bars are transparent, so the lower track (or black)
/// shows.
#[test]
fn portrait_clip_is_fitted_with_transparent_bars() {
    let Some(mut rig) = Rig::new("e2e-fit") else {
        return;
    };
    let (Some(blue), Some(red)) = (
        rig.solid("blue", 0x0000FF, W, H),
        rig.solid("portrait", 0xFF0000, 90, 160),
    ) else {
        return;
    };
    let (v1, v2) = (rig.track(0), rig.track(1));
    rig.place(v1, blue, 0.0);
    rig.place(v2, red, 0.0);
    rig.canvas(W, H);
    // 90×160 fitted into 320×180: 101.25 × 180, centred → x ∈ [109.4, 210.6].
    let picture = [(160, 90), (115, 10), (205, 170)];
    let bars = [(100, 90), (220, 90), (20, 10), (300, 170)];
    for (path, buf) in rig.frames(T) {
        expect_at(
            &buf,
            W,
            &picture,
            RED,
            COARSE,
            &format!("{path} fitted clip"),
        );
        expect_at(
            &buf,
            W,
            &bars,
            BLUE,
            COARSE,
            &format!("{path} bars show V1"),
        );
        // Not stretched: the full height is covered, the width is not.
        let row: Vec<bool> = (0..W)
            .map(|x| near(px(&buf, W, x, 90), RED, COARSE))
            .collect();
        let red_px = row.iter().filter(|r| **r).count();
        assert!(
            (99..=103).contains(&red_px),
            "{path}: fitted width {red_px} px, want ≈101 (aspect kept)"
        );
    }
    rig.engine.set_track_enabled(v1, false).expect("hide V1");
    for (path, buf) in rig.frames(T) {
        expect_at(
            &buf,
            W,
            &bars,
            BLACK,
            2,
            &format!("{path} bars over nothing"),
        );
        expect_at(
            &buf,
            W,
            &picture,
            RED,
            COARSE,
            &format!("{path} fitted clip"),
        );
    }
}

/// (g) Track add / move / remove (+ undo) change the layer order and the
/// rendered frame, and undo restores both. Automatic names follow position.
#[test]
fn track_edits_and_undo_restore_sequence_and_frame() {
    let Some(mut rig) = Rig::new("e2e-tracks") else {
        return;
    };
    let (Some(blue), Some(red), Some(green)) = (
        rig.solid("blue", 0x0000FF, W, H),
        rig.solid("red", 0xFF0000, W, H),
        rig.solid("green", 0x00FF00, W, H),
    ) else {
        return;
    };
    let v1 = rig.track(0);
    rig.place(v1, blue, 0.0);
    let v2 = rig
        .engine
        .add_track(TrackKind::Video, None, None)
        .expect("add V2");
    rig.place(v2, red, 0.0);
    rig.canvas(W, H);
    let names = |r: &Rig| r.track_names();
    let s = |v: &[(&str, &str)]| -> Vec<(String, String)> {
        v.iter()
            .map(|(k, n)| (k.to_string(), n.to_string()))
            .collect()
    };
    let centre = [(160, 90), (20, 20)];
    let top_is = |rig: &mut Rig, want: [u8; 3], what: &str| {
        for (path, buf) in rig.frames(T) {
            expect_at(&buf, W, &centre, want, COARSE, &format!("{path}: {what}"));
        }
    };
    let base = s(&[("video", "V1"), ("video", "V2"), ("audio", "A1")]);
    assert_eq!(names(&rig), base, "video tracks stay before audio");
    assert_eq!(rig.video_tracks(), vec![v1, v2]);
    top_is(&mut rig, RED, "V2 is the top layer");

    // Move V2 to the bottom: blue is on top, and the names renumber.
    rig.engine.move_track(v2, 0).expect("move track");
    assert_eq!(rig.video_tracks(), vec![v2, v1]);
    assert_eq!(names(&rig), base, "automatic names follow position");
    top_is(&mut rig, BLUE, "after move");
    assert!(rig.engine.undo().expect("undo move"));
    assert_eq!(rig.video_tracks(), vec![v1, v2], "undo restores the order");
    assert_eq!(names(&rig), base);
    top_is(&mut rig, RED, "after undoing the move");

    // Remove the top track (with its clip); undo brings both back.
    rig.engine.remove_track(v2).expect("remove track");
    assert_eq!(rig.video_tracks(), vec![v1]);
    top_is(&mut rig, BLUE, "after removing V2");
    assert!(rig.engine.undo().expect("undo remove"));
    assert_eq!(rig.video_tracks(), vec![v1, v2]);
    let clips = rig.engine.current_sequence_contents().unwrap().tracks[1]
        .clips
        .len();
    assert_eq!(clips, 1, "undo restores the removed track's clip");
    top_is(&mut rig, RED, "after undoing the remove");

    // Insert at the bottom: the top layer doesn't change; undo removes it.
    let under = rig
        .engine
        .add_track(TrackKind::Video, None, Some(0))
        .expect("add bottom track");
    rig.place(under, green, 0.0);
    assert_eq!(rig.video_tracks(), vec![under, v1, v2]);
    assert_eq!(
        names(&rig),
        s(&[
            ("video", "V1"),
            ("video", "V2"),
            ("video", "V3"),
            ("audio", "A1")
        ])
    );
    top_is(&mut rig, RED, "a new bottom track stays below");
    rig.engine.set_track_enabled(v2, false).expect("hide top");
    rig.engine
        .set_track_enabled(v1, false)
        .expect("hide middle");
    top_is(&mut rig, [0, 255, 0], "only the new bottom track visible");
    for _ in 0..4 {
        assert!(rig.engine.undo().expect("undo"));
    }
    assert_eq!(rig.video_tracks(), vec![v1, v2]);
    assert_eq!(names(&rig), base);
    top_is(&mut rig, RED, "after undoing the insert");
}

/// (g, rename) Custom names survive moves while automatic ones renumber;
/// undo restores the previous name. Renaming doesn't touch the picture.
#[test]
fn rename_track_and_undo() {
    let Some(mut rig) = Rig::new("e2e-rename") else {
        return;
    };
    let (Some(blue), Some(red)) = (
        rig.solid("blue", 0x0000FF, W, H),
        rig.solid("red", 0xFF0000, W, H),
    ) else {
        return;
    };
    let (v1, v2) = (rig.track(0), rig.track(1));
    rig.place(v1, blue, 0.0);
    rig.place(v2, red, 0.0);
    rig.canvas(W, H);
    let names = |r: &Rig| -> Vec<String> { r.track_names().into_iter().map(|(_, n)| n).collect() };
    let top_is = |rig: &mut Rig, want: [u8; 3], what: &str| {
        for (path, buf) in rig.frames(T) {
            expect_at(
                &buf,
                W,
                &[(160, 90)],
                want,
                COARSE,
                &format!("{path}: {what}"),
            );
        }
    };
    rig.engine
        .rename_track(v2, "  Overlay ".into())
        .expect("rename track");
    assert_eq!(names(&rig), ["V1", "Overlay", "A1"], "trimmed custom name");
    top_is(&mut rig, RED, "rename keeps the picture");
    rig.engine.move_track(v2, 0).expect("move renamed track");
    assert_eq!(
        names(&rig),
        ["Overlay", "V2", "A1"],
        "custom name kept, automatic one renumbered"
    );
    top_is(&mut rig, BLUE, "renamed track moved down");
    assert!(rig.engine.undo().expect("undo move"));
    assert_eq!(names(&rig), ["V1", "Overlay", "A1"]);
    assert!(rig.engine.undo().expect("undo rename"));
    assert_eq!(names(&rig), ["V1", "V2", "A1"], "undo restores the name");
    top_is(&mut rig, RED, "after undoing rename + move");
    // An empty name reverts to the automatic one.
    rig.engine
        .rename_track(v2, "Overlay".into())
        .expect("rename track");
    rig.engine
        .rename_track(v2, String::new())
        .expect("clear name");
    assert_eq!(names(&rig), ["V1", "V2", "A1"]);
}

/// (h) A hidden track doesn't render; showing it again does.
#[test]
fn hidden_track_does_not_render() {
    let Some(mut rig) = Rig::new("e2e-hidden") else {
        return;
    };
    let (Some(blue), Some(red)) = (
        rig.solid("blue", 0x0000FF, W, H),
        rig.solid("red", 0xFF0000, W, H),
    ) else {
        return;
    };
    let (v1, v2) = (rig.track(0), rig.track(1));
    rig.place(v1, blue, 0.0);
    rig.place(v2, red, 0.0);
    rig.canvas(W, H);
    let pts = [(160, 90), (20, 20), (300, 160)];
    rig.engine.set_track_enabled(v2, false).expect("hide V2");
    let enabled = |rig: &Rig| -> Vec<bool> {
        let c = rig.engine.current_sequence_contents().unwrap();
        c.tracks
            .iter()
            .filter(|t| t.kind == "video")
            .map(|t| t.enabled)
            .collect()
    };
    assert_eq!(enabled(&rig), vec![true, false]);
    for (path, buf) in rig.frames(T) {
        expect_at(&buf, W, &pts, BLUE, COARSE, &format!("{path} V2 hidden"));
    }
    rig.engine.set_track_enabled(v1, false).expect("hide V1");
    for (path, buf) in rig.frames(T) {
        expect_at(&buf, W, &pts, BLACK, 2, &format!("{path} all hidden"));
    }
    rig.engine.set_track_enabled(v1, true).expect("show V1");
    rig.engine.set_track_enabled(v2, true).expect("show V2");
    for (path, buf) in rig.frames(T) {
        expect_at(&buf, W, &pts, RED, COARSE, &format!("{path} V2 shown"));
    }
}

/// (i) Gaps render opaque black: between clips, after the last clip, and
/// where an upper track has a gap over a lower one.
#[test]
fn gaps_render_black() {
    let Some(mut rig) = Rig::new("e2e-gaps") else {
        return;
    };
    let (Some(blue), Some(red)) = (
        rig.solid("blue", 0x0000FF, W, H),
        rig.solid("red", 0xFF0000, W, H),
    ) else {
        return;
    };
    let (v1, v2) = (rig.track(0), rig.track(1));
    // V1: blue [0, 2) and [3, 5); V2: red [1, 2) only.
    rig.place(v1, blue, 0.0);
    rig.place(v1, blue, 3.0);
    let c = rig.place(v2, red, 1.0);
    rig.engine.trim_clip_out(v2, c, 1.0).expect("trim V2 clip");
    rig.canvas(W, H);
    let mut pts = border_points();
    pts.push((W / 2, H / 2));
    for (t, want, what) in [
        (0.5, BLUE, "V2 gap over V1"),
        (1.5, RED, "both tracks"),
        (2.5, BLACK, "gap on every track"),
        (3.5, BLUE, "second V1 clip"),
        (6.0, BLACK, "past the end"),
    ] {
        let tol = if want == BLACK { 0 } else { COARSE };
        for (path, buf) in rig.frames(t) {
            expect_at(
                &buf,
                rig.width(),
                &pts,
                want,
                tol,
                &format!("{path} t={t}: {what}"),
            );
        }
    }
}

/// Decisions 3 + 4: the first file clip on a video track sets the sequence
/// canvas to its source size; later clips don't change it; an export with no
/// explicit size renders at it. Here the canvas is 9:16, so a 16:9 clip on V2
/// is fitted with bars top and bottom that show V1.
#[test]
fn first_clip_sets_the_canvas_and_export_defaults_to_it() {
    let Some(mut rig) = Rig::new("e2e-canvas") else {
        return;
    };
    let (Some(red), Some(blue)) = (
        rig.solid("portrait", 0xFF0000, 90, 160),
        rig.solid("landscape", 0x0000FF, W, H),
    ) else {
        return;
    };
    let size = |r: &Rig| r.engine.current_sequence_contents().unwrap().frame_size;
    assert_eq!(size(&rig), None, "no canvas before the first clip");
    let (v1, v2) = (rig.track(0), rig.track(1));
    rig.place(v1, red, 0.0);
    assert_eq!(size(&rig), Some([90, 160]), "the first clip's source size");
    rig.place(v2, blue, 0.0);
    assert_eq!(size(&rig), Some([90, 160]), "only the first clip sets it");

    let out = rig.scratch.path("default-size", "mov");
    rig.engine
        .render_sequence(vance_app_lib::engine::RenderOptions {
            output_path: out.to_string_lossy().to_string(),
            preset: vance_encoder::EncoderPreset::ProRes422HQ,
            width: None,
            height: None,
            render_and_replace_track: None,
            start_seconds: Some(0.0),
            end_seconds: Some(0.25),
            hdr: false,
        })
        .expect("render_sequence");
    let probe = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0"])
        .args(["-show_entries", "stream=width,height", "-of", "csv=p=0"])
        .arg(&out)
        .output()
        .expect("ffprobe");
    assert_eq!(
        String::from_utf8_lossy(&probe.stdout).trim(),
        "90,160",
        "export defaults to the sequence frame size"
    );

    // 320×180 fitted into 90×160: 90 × 50.6, centred → y ∈ [54.7, 105.3].
    // (Re-setting the same size only points the frame reads at it.)
    rig.canvas(90, 160);
    for (path, buf) in rig.frames(T) {
        assert_eq!(buf.len(), 90 * 160 * 4, "{path} canvas size");
        expect_at(
            &buf,
            90,
            &[(45, 80), (5, 60), (85, 100)],
            BLUE,
            COARSE,
            path,
        );
        expect_at(&buf, 90, &[(45, 20), (45, 140), (5, 5)], RED, COARSE, path);
    }
}
