# ADR 0001 — Technology Stack Choices

- **Status:** Accepted
- **Date:** 2026-04-27
- **Deciders:** Vance core team
- **Supersedes:** —

## Context

Vance is a multi-year, two-person project to build a cross-platform, professional-grade non-linear video editor. Every load-bearing technology choice must:

- Ship on macOS, Windows, and Linux without per-platform code paths where avoidable.
- Use permissively-licensed open-source code (Apache-2.0 / MIT / BSD compatible).
- Have an active enough maintainer base that we are not the de facto upstream.
- Not require building a new abstraction layer just to be usable from Rust.

PLAN.md §4 enumerates the choices in tabular form. This ADR codifies them with rationale.

## Decision

| Concern | Choice | Why |
|---|---|---|
| Language | Rust (1.95, pinned in `rust-toolchain.toml`) | Memory safety, fearless concurrency, strong cross-platform story for media + GPU. |
| Workspace | Cargo workspace, ~25 crates | Compile-time isolation; parallel builds; clear ownership boundaries. |
| UI chrome | Tauri 2 + SvelteKit + TailwindCSS 4 | Polish needed for non-technical audience; chat-style UI is dramatically easier in HTML; Rust backend keeps us in-language. |
| Viewport | Native overlay (NSView/HWND/GTK) hosting a `wgpu` surface, parented to the Tauri webview | Validated on macOS by ADR 0005. The only path to real-time 4K decode + grade. |
| GPU | wgpu 29 | Cross-platform Metal/Vulkan/D3D12; compute + render. |
| Shading | WGSL primary; SPIR-V via naga where needed | WGSL is wgpu-native. |
| Video I/O | FFmpeg via `ffmpeg-next`, run in sidecar process | Codec coverage no pure-Rust crate matches; sidecar isolates decoder crashes. |
| Audio decode | symphonia preferred, FFmpeg fallback | Pure Rust, fast, safer for common formats. |
| Audio output | cpal | The de facto cross-platform Rust audio I/O. |
| Audio DSP | fundsp built-in; `livi` for LV2; custom CLAP host | LV2 + CLAP are open standards; VST3 license is incompatible. |
| Color management | OpenColorIO v2 via vendored C++ + thin Rust wrapper | Industry standard; ACES support free. |
| Image I/O | `image` + OpenImageIO FFI for DPX/EXR | OIIO covers what `image` does not. |
| Async runtime | Tokio for I/O, Rayon for CPU parallelism | Standard pairing. |
| IPC | `interprocess` + bincode framing + shared memory for frames | Lightweight; no Protobuf overhead. |
| Project serialization | `serde` + RON | Human-readable, diffable, Rust-native. |
| Interchange | OpenTimelineIO import/export adapter | Industry standard for NLE interchange. |
| Math | `glam` for 4×4; `nalgebra` for color matrices | `glam` is faster for common gamedev shapes. |
| Logging | `tracing` + `tracing-subscriber` | Span-based, async-friendly. |
| Errors | `thiserror` for libs, `anyhow` for app code | Convention. |
| Testing | `cargo test`, `insta`, `criterion`, custom golden-frame harness | Covers unit / snapshot / perf / render-correctness. |
| Build automation | `cargo xtask` | No make/just dependency for contributors. |
| Distribution | `cargo-dist` + native installers (MSI, DMG, AppImage) | One source of truth for releases. |
| CI | GitHub Actions, win/mac/linux matrix | Standard. |
| Documentation | `mdbook` for the user manual, `cargo doc` for API | Versioned with code. |

### AI assistant stack

| Concern | Choice | Why |
|---|---|---|
| Default runtime | Hosted APIs (Anthropic, OpenAI, OpenRouter, OpenAI-compatible) via a pluggable provider trait | Most consumer hardware can't run a capable model locally. Cloud is the default; user brings their own key. |
| Optional local runtime | Ollama primary; `llama.cpp` via `llama-cpp-2` for advanced users | Local stays available for power users / privacy-sensitive workflows but is never required. |
| Default recommended model | Anthropic Claude (latest Sonnet tier) | Strong tool-use, low latency, predictable cost. |
| Tool-use protocol | OpenAI-style function calling normalized into a single internal schema | All providers agree on this shape; thin adapter per provider. |

### FFmpeg licensing

Two builds, user picks (see PLAN.md §8.4):

- **LGPL** — bundled in the installer by default. Covers all decoders, AV1/ProRes/DNxHR encode, all hardware encoders.
- **GPL Encoder Pack** — optional one-click download from settings. Adds `libx264`, `libx265`. Whole binary becomes GPL.

## Consequences

**Easier:**
- Every choice has either a thriving Rust ecosystem or a well-known FFI path. No yak-shaving to make a basic decoder work.
- All choices ship a permissive license except FFmpeg, which is contained behind the LGPL/GPL split policy.
- Tauri + Svelte gives us a UX iteration speed an immediate-mode framework cannot match.

**Harder:**
- Five FFI surfaces (FFmpeg, OCIO, OIIO, CLAP, OFX) each carry their own dependency / build complexity. Mitigated by sidecar isolation and vendoring.
- The viewport parenting pattern (ADR 0005) is the single largest piece of unconventional engineering in the stack.
- Tauri webview adds a baseline ~150 MB process footprint; acceptable for a desktop NLE but worth tracking.

**Risks:**
- `wgpu` API churn between major versions. Mitigation: pin per phase, upgrade in dedicated PRs.
- OCIO Rust bindings are immature. Mitigation: vendor C++ via `cxx` crate, write our own thin wrapper.
- VST3 hostability is foreclosed by license. We tell users to use LV2/CLAP versions of their plugins.

## Alternatives Considered

See PLAN.md §4.1 for the full rejected-alternatives list. Key rejections:

- **GStreamer instead of FFmpeg** — codec coverage trails FFmpeg, LGPL+GPL packaging is harder.
- **`egui` for the whole UI** — wrong aesthetic for a non-technical audience; chat-style UI is awkward in immediate mode.
- **Slint** — viable, but commercial-license dual model adds friction.
- **`gpui`** — not a public, supported framework.
- **Pure-Rust codecs only** (`rust-av`, `re_video`) — coverage gap is too large.
- **OTIO as the native project format** — would constrain us to its data model.
- **Local-first AI as the default** — most consumer hardware can't run a capable model.
- **Bundling a model in the installer** — multi-GB installer for a feature most users won't use locally.

## Notes

- ADR 0002 covers the UI stack in more detail (Tauri + Svelte + parented viewport).
- ADR 0003 covers the AI architecture.
- ADR 0005 records the Phase −1 spike result that validated the viewport parenting approach.
