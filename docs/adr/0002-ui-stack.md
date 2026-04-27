# ADR 0002 — UI Stack: Tauri + SvelteKit + Parented Native Viewport

- **Status:** Accepted (with constraint per ADR 0005)
- **Date:** 2026-04-27
- **Deciders:** Vance core team
- **Supersedes:** —

## Context

Vance targets semi-technical creators (YouTubers, indie filmmakers, marketing producers) — people accustomed to Premiere, Final Cut, CapCut. They expect:

- A polished, modern UI.
- A first-class chat panel for the AI assistant.
- Real-time playback of 4K video at 60fps with effects and color.

These are partially conflicting requirements. The first two favor a web-stack UI. The third demands native GPU rendering. The plan resolves this with a **hybrid**: Tauri webview for chrome, native `wgpu` surface for the viewport.

## Decision

- **Chrome**: Tauri 2 hosts the application. The frontend is a SvelteKit SPA built statically (`@sveltejs/adapter-static`), styled with TailwindCSS 4. The frontend talks to Rust via Tauri's `invoke` and event channels.
- **Viewport**: A native overlay (NSView on macOS, HWND child on Windows, X11 child window or Wayland subsurface on Linux) is parented to the Tauri webview's content area. The overlay hosts a `wgpu` surface (CAMetalLayer on macOS; equivalent on other platforms). Rust owns the overlay; the frontend reports the placeholder div's bounds via IPC and Rust resyncs the overlay's frame on every layout change.
- **Custom widgets that need GPU performance** (timeline canvas, color scopes) render to `<canvas>` via WebGL2/WebGPU inside the webview, not DOM. Avoids the "DOM with 1000 clips" failure mode.
- **Floating UI over the viewport** (menus, tooltips, drag previews) is rendered as additional native overlays, not HTML — see ADR 0005 for the constraint.

## The constraint

The native overlay sits **visually above** the HTML chrome inside its rectangle. Click events still route to the webview correctly, but HTML cannot visually appear over the viewport. This is a permanent property of the architecture; treat it as a design rule:

- Chrome panels surround the viewport — they never overlap it.
- Anything that *needs* to appear visually over the viewport must be drawn natively. Patterns for this land in Phase 4 / Phase 5.

## Consequences

**Easier:**
- Designers can iterate on chrome with hot reload and the full web ecosystem.
- The AI chat panel and color page UI are dramatically easier to build in HTML/CSS/Svelte than in any Rust-native framework.
- Cross-platform polish for free: HTML/CSS render identically across Mac/Win/Linux.
- The Rust ↔ frontend boundary is small and well-defined (Tauri commands).

**Harder:**
- The viewport parenting code is platform-specific Rust (per OS). Maintained in `vance-viewport`.
- Floating UI over the viewport requires native overlay code — more work than the equivalent HTML.
- The Tauri runtime adds ~150 MB to the process footprint; acceptable for a desktop NLE.
- Designers must internalize the "no chrome over the viewport" rule. Documented in `CONTRIBUTING.md` and any future design system.

**Risks:**
- Wayland subsurface behavior is compositor-dependent; verification in Phase 0 Week 1.
- WebView2 update channel on Windows: users may have older versions; we require the runtime to be present (Tauri handles bootstrap).

## Alternatives Considered

- **`egui` for everything**: Rejected. Immediate-mode aesthetic is wrong for our audience; chat panel is awkward.
- **Slint**: Rejected. Dual-license commercial friction; smaller ecosystem.
- **Pure-web (WebGPU canvas) viewport**: Rejected. Performance ceiling far below native; loses access to platform-specific decoders sharing the GPU surface.
- **Separate top-level OS window for the viewport** (Resolve-style float-out): Considered as fallback; we may *offer* it as a "pop-out viewport" feature in v2 but not as v1's primary architecture.

## Notes

- See ADR 0005 for the spike that validated this on macOS and the test matrix that remains for Windows/Linux.
- See ADR 0001 for the broader stack picture.
