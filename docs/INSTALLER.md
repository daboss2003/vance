# Building Vance installers + the optional AI-tools flow

This covers how Vance is bundled into platform installers and how the optional
AI tools (captions, voice isolation, subject mask, generative images) are
installed — at setup time or later. For the release/versioning/signing recipe
see [RELEASING.md](RELEASING.md); for the end-user view see
[manual/src/install.md](manual/src/install.md).

## TL;DR

```sh
cargo xtask stage-sidecars      # build + stage the 4 sidecars for externalBin
cargo xtask bundle              # stage, build UI, and run `tauri build` with the overlay
cargo xtask bundle --bundles app        # just the macOS .app (fast, no dmg/sign)
cargo xtask bundle --target x86_64-pc-windows-msvc --bundles nsis   # Windows .exe
```

`cargo xtask bundle` requires the Tauri CLI (`cargo install tauri-cli --version '^2'`).

## Why two config files

Tauri's `generate_context!` validates `bundle.externalBin` and the icon set **at
compile time**. If those lived in the base `tauri.conf.json`, every plain
`cargo build -p vance-app` (and CI's `tauri-smoke`) would require the staged
sidecars and a full `.icns`/`.ico` set to exist — which they don't in a normal
dev build.

So the split is:

- **`crates/vance-app/tauri.conf.json`** — base. No `externalBin`, no extra
  icons. `cargo build` and `tauri dev` work with nothing staged. `bundle.active`
  is `false` here so an accidental `tauri build` can't emit a sidecar-less app.
- **`crates/vance-app/tauri.bundle.conf.json`** — release overlay, deep-merged
  only at bundle time via `tauri build --config …`. It flips `active: true`,
  declares the four `externalBin` sidecars, per-platform settings (macOS
  entitlements + dmg layout, Windows NSIS hook, Linux deb deps + AppImage media
  framework), and the store metadata (category, publisher, copyright, license).

`cargo xtask bundle` wires this together: stage sidecars → build the UI → run
`tauri build` from the crate dir with the overlay.

### Sidecars

Vance runs five out-of-process helpers, resolved next to the app binary at
runtime: `vance-decoder`, `vance-encoder`, `vance-audio-decoder`,
`vance-plugin-host` (bundled), and `vance-ofx-host` (feature-gated OpenFX, add
to `SIDECARS`/`externalBin` if you build it). `cargo xtask stage-sidecars`
builds them release and copies them to `crates/vance-app/binaries/<name>-<target-triple>`
(git-ignored) — the layout Tauri's `externalBin` expects. The bundle then places
them in `Vance.app/Contents/MacOS/` (macOS) / next to the exe (Win/Linux), which
is exactly where the runtime locators look.

## The optional AI tools

Three of the four install via **pip into Vance's managed venv** — no hosting
needed, PyPI serves them; only a system Python 3 is required. `sd`
(stable-diffusion) is a native binary + model **download**. Everything lands in
the per-user managed tools dir (`…/Vance/tools/{bin,venv,models}`), resolved
before `PATH`, so tools "just work" with zero setup. See
[adr/0003-ai-architecture.md](adr/0003-ai-architecture.md).

### One primitive, every entry point

The in-app **AI tools** panel, the first-run onboarding step, and every OS
installer all drive the same headless CLI:

```sh
vance-app --install-ai-tools all          # install all four
vance-app --install-ai-tools whisper,sd   # a subset
vance-app --list-ai-tools                 # catalog + install method
vance-app --ai-tools-status               # JSON status (installed/managed/model_ready)
```

These are handled before the GUI starts (`tools::handle_cli`) and exit without
opening a window; a plain launch falls through to the editor.

### "Ask at setup, or add later" — per platform

See [../packaging/README.md](../packaging/README.md) for the files. The managed
tools dir is **per-user**, which decides where each platform prompts:

| Platform | Prompt | Runs as |
|----------|--------|---------|
| Windows (NSIS `.exe`) | installer postinstall hook (`windows/installer.nsh`) | the user (`installMode: currentUser`) |
| macOS `.dmg` (default) | app first-run onboarding | the user |
| macOS `.pkg` (optional) | pkg postinstall (`packaging/macos/postinstall.sh`) | dropped to console user |
| Linux `.deb`/AppImage | app first-run onboarding | the user |

A `.pkg`/`.deb` postinstall runs as **root**, so those platforms defer the
per-user install to first run rather than pulling hundreds of MB into root's
home during a system package operation. Declining anywhere is lossless — the
in-app panel adds the tools later.

### Hosting the `sd` download (the one gated piece)

`--install-ai-tools sd` needs `VANCE_TOOLS_MANIFEST_URL` pointing at a JSON
manifest (schema + sample: [tools-manifest.example.json](tools-manifest.example.json)).
The URLs may point straight at **upstream** (stable-diffusion.cpp GitHub
releases + Hugging Face weights) — the host need not be yours. You must fill in
each file's real `sha256` and confirm the chosen model's license permits the
use. The downloader writes bytes verbatim (no archive extraction), so each URL
must resolve to the final file.

## What's gated (needs external accounts/toolchains, not code)

- **Tauri CLI + per-OS toolchains** for a real `tauri build` (macOS builds
  `.app`/`.dmg`; Windows needs the MSVC target + NSIS; Linux needs webkit2gtk).
  Verified here: `cargo xtask bundle --bundles app` produces a working
  `Vance.app` with all four sidecars embedded and the AI-tools CLI live.
- **Icons** — the repo ships a placeholder `icons/icon.png`. Generate the real
  set from a 1024×1024 master with `cargo tauri icon <master.png>` before a
  public release.
- **Code signing / notarization** — macOS Developer ID + Apple notarization,
  Windows Authenticode. Entitlements are ready
  (`macos/entitlements.plist`, incl. `disable-library-validation` so third-party
  CLAP/LV2/OFX plugins load under the hardened runtime); the certs/secrets are
  the deployment's.
- **Auto-updater** — Tauri updater keys + an update host (see RELEASING.md).
- **`sd` manifest URL + model licensing** — as above.
