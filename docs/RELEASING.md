# Releasing Vance

This is the release recipe for Phase 11 (Distribution). It covers what is
fully automatable and, clearly separated, what is **gated on your accounts,
certificates, and hosting** — those are decisions and secrets the project
maintainers must supply; they cannot be embedded in the source.

## Overview

Vance is a Tauri 2 desktop app. Installers are produced by the **Tauri
bundler** (`tauri build`) per platform, orchestrated in CI, with
[`cargo-dist`](https://opensource.axo.dev/cargo-dist/) driving the release
workflow and the auto-updater artifacts.

Wrap the bundler with **`cargo xtask bundle`** — it stages the out-of-process
sidecars (`externalBin`) and applies the release bundle overlay before calling
`tauri build`. A raw `tauri build` alone would emit a sidecar-less app. The
bundling architecture (base vs. overlay config, sidecar staging, and the
optional AI-tools install flow) is documented in [INSTALLER.md](INSTALLER.md).

| Platform | Installer | Produced by |
| --- | --- | --- |
| macOS | `.dmg` (universal: `x86_64` + `aarch64`) | `cargo xtask bundle --target universal-apple-darwin` |
| Windows | `.exe` (NSIS) + updater | `cargo xtask bundle` |
| Linux | `.AppImage` (primary), `.deb`, `.rpm` | `cargo xtask bundle` |

## Buildable now (no external accounts)

1. **Bump the version** in `[workspace.package] version` and the Tauri
   `tauri.conf.json` `version`.
2. **Icons** (once / when branding changes): generate the icon set from a
   1024×1024 master — `cargo tauri icon path/to/master.png` — replacing the
   placeholder `crates/vance-app/icons/`.
3. **Build installers** per platform on that platform's runner (needs the Tauri
   CLI: `cargo install tauri-cli --version '^2'`). `cargo xtask bundle` builds
   the UI, stages the sidecars, and runs `tauri build` with the overlay:
   - macOS: `cargo xtask bundle --target universal-apple-darwin`
   - Windows: `cargo xtask bundle`
   - Linux: `cargo xtask bundle`
4. **Codec pack**: build `vance-plugin-host` / GPL-encoder artifacts as a
   separate, independently-versioned bundle so it can update on its own.
5. **User manual**: `mdbook build docs/manual` → static site to publish.

Unsigned installers work locally and for testing. The steps above require no
secrets. Verified: `cargo xtask bundle --bundles app` produces a working
`Vance.app` with all four sidecars embedded.

### Optional AI tools

The installers wire an "add the optional AI tools?" prompt to the same
`vance-app --install-ai-tools` primitive the in-app panel uses (Windows via the
NSIS hook; macOS/Linux via first-run onboarding). Nothing needs hosting except
the `sd` model download — see [INSTALLER.md](INSTALLER.md) and
[tools-manifest.example.json](tools-manifest.example.json).

## Gated — requires your accounts / certificates / hosting

These cannot be done from the source tree alone. Provide them as CI secrets.

- **macOS notarization & signing** — an **Apple Developer Program** account
  ($99/yr), a *Developer ID Application* certificate, and an app-specific
  password / notarytool credentials. Without these, macOS Gatekeeper warns on
  first launch. Set `APPLE_CERTIFICATE`, `APPLE_SIGNING_IDENTITY`,
  `APPLE_ID`, `APPLE_TEAM_ID`, `APPLE_PASSWORD` in CI.
- **Windows code signing** — an **Authenticode code-signing certificate**
  (OV or EV, from a CA; annual cost). Without it, SmartScreen warns. Set the
  cert + password as CI secrets and point the Tauri Windows signing config at
  them.
- **Auto-update host** — a place to serve the update manifest + artifacts
  (GitHub Releases, S3/R2, or your own host) and a Tauri updater **public
  key** in `tauri.conf.json` with the matching **private key** as a CI secret
  (`TAURI_SIGNING_PRIVATE_KEY`). Channels: `stable` and `nightly`.
- **Crash-report upload** (optional) — Vance always writes crash logs
  locally (`…/Vance/crashes/`). Uploading them (only when the user opts in)
  needs a **Sentry** DSN or a self-hosted endpoint. Wire the DSN as a build
  env var; the local reporter already captures the panic.
- **Telemetry endpoint** (optional) — opt-in, aggregate, no PII. Needs an
  ingestion endpoint you host; the opt-in flag already exists in settings.
- **AI-tools `sd` download** (optional) — a `VANCE_TOOLS_MANIFEST_URL` serving
  the manifest, plus a chosen stable-diffusion model whose license permits the
  use. URLs may point upstream (GitHub/Hugging Face); the pip-based tools
  (whisper/demucs/backgroundremover) need nothing hosted. See INSTALLER.md.

## cargo-dist

Run `cargo dist init` once to generate the release workflow, then keep the
generated `[workspace.metadata.dist]` in `Cargo.toml` under review. Because
the OS bundles come from the Tauri bundler, cargo-dist is used for the release
orchestration + updater artifacts rather than to build the `.dmg`/`.msi`
itself. (Left uncommitted here intentionally: an unvalidated config is worse
than none — generate it on a machine with `cargo-dist` installed.)

## Acceptance (per PLAN Phase 11)

- A non-developer installs on Win/Mac/Linux and opens a sample project in
  under 5 minutes.
- macOS build is signed + notarized (no Gatekeeper warning).
- The crash reporter captures a panic and writes a report.
