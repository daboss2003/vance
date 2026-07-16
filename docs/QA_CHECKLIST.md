# Vance — Manual QA Checklist

This is the "does it actually work when a human uses it" pass. Everything
below has passing automated tests (engine logic, real-media export, decode,
serialization, error paths), **but the GUI itself has never been launched** —
that's what this checklist is for. Work top-down: if the smoke tests fail,
stop and report; nothing else matters until the app opens and renders.

Legend: **[core]** should just work · **[tool]** needs an external CLI you
install · **[acct]** needs an account/keys · **[hw]** needs specific hardware.

---

## How to run the build

The release build produces everything in `target/release/`:

```
target/release/vance-app          # the app (double-click or run in a terminal)
target/release/vance-decoder      # sidecars — must sit next to vance-app
target/release/vance-encoder
target/release/vance-audio-decoder
```

Run it from a terminal the first time so you can see logs:

```
cd "/Users/mac/Desktop/video editor/vance"
./target/release/vance-app
```

Keep that terminal visible — Rust panics and `tracing` warnings print there,
and a crash also writes `~/Library/Application Support/Vance/crashes/last-crash.log`.

FFmpeg must be on PATH (`brew install ffmpeg`) for export.

---

## 0 · Smoke test (do these first) [core]

- [ ] The app window opens (titled "Vance", ~1280×800) within a few seconds.
- [ ] The UI renders — you see the header, a viewport area, a timeline, and
      the row of tool panels along the bottom. No blank white/black window.
- [ ] The terminal shows **no panic** and no repeating error spam.
- [ ] The **first-run wizard** appears (Welcome → What Vance found → Privacy →
      You're set). Its capability list shows FFmpeg + the bundled sidecars as
      present, and the optional AI tools as present/absent.
- [ ] "Skip setup" and "Get started" both dismiss it, and it does **not**
      reappear on the next launch (a `settings.json` was written).
- [ ] Resize the window — layout reflows, nothing overlaps off-screen.

If any of §0 fails, capture the terminal output and stop here.

---

## 1 · Import & media browser [core]

- [ ] **File ▸ Open** (or drag a real video file in) imports it; it appears in
      the Browser with a name, duration, and thumbnail/label.
- [ ] Import a few different files: an MP4/H.264, a MOV, an image (PNG/JPG),
      an audio-only file (MP3/WAV). Each lands with the right "kind".
- [ ] A **10-bit HDR clip** (HEVC Main10, PQ or HLG) imports without error and
      is tagged Rec.2020 PQ/HLG (check its input color space in the item's
      details). *This is new — it used to be rejected.*
- [ ] Import something that isn't a video (a `.txt`) — you get a clear error
      message, not a crash.

## 2 · Timeline editing [core]

- [ ] Drag a clip from the Browser onto a video track — it places at the drop
      point and shows a thumbnail/waveform.
- [ ] Play (Space) — the playhead moves and the viewport shows frames.
- [ ] Trim a clip edge; split at the playhead; ripple-delete a clip. The
      timeline updates correctly and neighbors behave as expected.
- [ ] Toggle **magnetic mode** — edits ripple across tracks; a connected clip
      follows its anchor when you move it.
- [ ] **Undo/redo** (Cmd-Z / Shift-Cmd-Z) reverts and re-applies each edit,
      including caption and review-comment changes.
- [ ] Roll / slip / slide trims behave (adjust the cut, the shown range, or
      the position without gaps).
- [ ] Markers: drop a marker, add a note, navigate to it.

## 3 · Version history & bins [core]

- [ ] The edit history is visible; you can step back through edits.
- [ ] **Branch** the history, make a different edit, switch back — both
      branches are intact.
- [ ] Organize media into bins; add a custom column (Scene/Take/Rating); a
      **smart bin** auto-collects items matching a query.

## 4 · Color page [core]

- [ ] Switch to the **Color** page. Scopes (waveform/parade/vectorscope/
      histogram) render and update as you scrub.
- [ ] Add grade nodes; adjust lift/gamma/gain, curves, a qualifier/secondary.
      The viewport updates live.
- [ ] Load a `.cube` LUT — it applies; toggling it on/off changes the image.
- [ ] Keyframe a grade parameter or drive it with an expression — it animates
      across the shot.
- [ ] **HDR grade** *(needs an HDR clip)*: with an HDR source on the timeline,
      the SDR viewport tone-maps it (highlights roll off, not flat white).

## 5 · Audio & the mixer [core]

- [ ] The mixer console shows per-track channel strips.
- [ ] Adjust a track fader / clip gain / pan — playback level changes.
- [ ] Add an insert effect (EQ/comp/limiter) to a track; it audibly applies.
- [ ] Create a **submix bus**, route a track to it, add an aux send — routing
      works and no feedback/cycle is allowed.
- [ ] Waveforms draw on audio clips.
- [ ] **[tool]** If built with CLAP/LV2 support, load a third-party plugin;
      a plugin crash should drop the plugin, not the app.

## 6 · Titles, Motion Graphics & Fusion [core]

- [ ] **Titles/MoGraph panel**: create a title from a built-in template; edit
      its controls (text, color, size, font). Drag it to the timeline — it
      renders and animates as the playhead crosses it.
- [ ] **Fusion panel**: create a composition; add nodes (Background, Text,
      Rectangle, Blur, Merge, etc.); wire input slots; set the output node.
      Edit node params. Drag the comp to the timeline — it renders animated.
- [ ] A deep/complex Fusion graph doesn't hang or crash (it's cycle- and
      memory-guarded).

## 7 · Captions [core]

- [ ] Add caption cues manually; edit text and timing; split/merge cues.
- [ ] **Import** a `.srt` / `.vtt` file — cues appear, sorted.
- [ ] **Export** captions to `.srt` and `.vtt` — re-importing yields the same
      cues (multi-line, accents, and `<`/`&` survive).
- [ ] **[tool]** With `whisper` (or whisper.cpp) installed, auto-transcribe a
      clip — cues are generated. Without it, you get a clear "install whisper"
      message, not a crash.

## 8 · AI-assisted tools (each needs an external CLI)

- [ ] **[tool]** Voice isolation (`pip install demucs`): isolate vocals from a
      clip → an isolated stem imports.
- [ ] **[tool]** Magic Mask (`pip install -U backgroundremover`): isolate the
      subject → matted media imports.
- [ ] **[core]** Smart Reframe: reframe a 16:9 clip to 9:16 — a keyframed
      transform tracks the subject; re-running is idempotent (no double-shift).
- [ ] Dialogue leveling: normalizes a spoken clip's loudness.
- [ ] **[tool]** Generative image: set `VANCE_IMAGEGEN_CMD` (or `sd` +
      `VANCE_SD_MODEL`), enter a prompt → an image lands in the Browser.
      Without configuration, you get a clear setup message.
- [ ] For **every** AI tool: with the helper absent, the button shows an
      actionable "install X" error — never a crash.

## 9 · Export [core]

- [ ] **Export panel**: pick ProRes 422 HQ, set an output path, export a short
      range. The file is produced and plays in QuickTime/VLC.
- [ ] Export H.264 and HEVC — both produce valid, playable files.
- [ ] Export a range that includes a **title/MoGraph/Fusion** clip — the
      synthetic content appears correctly and animates in the file.
- [ ] Export with an **odd requested size** (e.g. 1921×1081) — it succeeds and
      the file is even-dimensioned (no failure on 4:2:0 codecs).
- [ ] Audio in the export is the real mix (not silence), matching playback.
- [ ] **HDR10 export**: tick **HDR10**, export an HDR (or any) clip → the file
      is HEVC Main10; `ffprobe` shows `color_transfer=smpte2084`,
      `color_primaries=bt2020`.
- [ ] Export to an un-writable path — clean error, not a crash.

## 10 · Publish [acct]

- [ ] **[acct]** Publish panel ▸ YouTube: enter your own OAuth Client ID +
      redirect URI, click **Connect** → browser consent opens → after you
      approve, the panel shows "connected".
- [ ] Fill title/description/privacy, choose a rendered file, **Publish** →
      the video uploads and you get a watch URL. (Large files upload in
      resumable chunks.)
- [ ] Vimeo/TikTok connect via OAuth but their uploaders are not wired yet —
      expect a clear "not wired" message, not a crash.

## 11 · Review comments (collaboration) [core]

- [ ] **Review panel**: type a name + a comment, click **＋ Comment** — it
      anchors at the current playhead position and shows an "N open" badge.
- [ ] Click a comment's timestamp — the playhead jumps there.
- [ ] Resolve a comment (checkbox); toggle "show resolved"; reply in a thread;
      delete a comment.
- [ ] Save the project, reopen it — all comments, replies, and resolved states
      are intact (they ride in the `.vance` bundle).

## 12 · Save / load projects [core]

- [ ] Save a project containing clips, a Fusion comp, a title, captions, and
      review comments to a `.vance` bundle.
- [ ] Close and reopen it — **everything** is restored (media, clips, grades,
      the Fusion graph, captions, comments). *Automated test covers this, but
      confirm via the UI.*
- [ ] Open a corrupt/garbage file as a project — clean error, not a crash.

## 13 · Onboarding & diagnostics [core]

- [ ] The capability list (wizard / diagnostics) correctly reflects what's
      installed (install/uninstall an optional tool and re-check).
- [ ] Privacy opt-ins default OFF and persist across launches.
- [ ] Force a panic path if you can (or just note): a crash writes
      `last-crash.log` under `…/Vance/crashes/`.

---

## Things that are expected / NOT bugs

- **HDR on an SDR display** looks tone-mapped (slightly filmic highlights),
  not identical to the source — correct until viewed on an HDR monitor.
- **HDR highlight round-trip** isn't bit-exact (a documented ~0.04-code
  approximation) — verify true fidelity on **[hw]** an HDR monitor.
- **10-bit HDR clips force software decode** (slower than 8-bit hardware
  decode) — intentional; the hardware path can't carry 10-bit correctly.
- **Vimeo / TikTok** uploaders, **img2img/inpaint**, and **Speed Editor HID**
  device I/O are stubbed/flagged follow-ups.
- **macOS only** so far — Windows/Linux have never been compiled or run.

## Known gated (can't be tested without your resources)

- **[hw]** HDR display fidelity sign-off.
- **[acct]** Dolby Vision RPU / Atmos (license + SDK), RAW vendor SDKs
  (BRAW/R3D/ARRIRAW), real-time cloud collaboration sync (server infra).

---

## Report back

For each failure, note: the step, what you expected, what happened, and paste
the terminal output + `last-crash.log` if it crashed. Screen-recording the
repro helps most.
