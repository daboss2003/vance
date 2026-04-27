# ADR 0005 — UI Viewport Parenting (Tauri webview + native wgpu surface)

- **Status:** Accepted (with constraint; Windows + Linux verification pending)
- **Date:** 2026-04-27
- **Deciders:** Vance core team
- **Supersedes:** —

## Context

PLAN.md commits to a Tauri 2 + Svelte chrome with a **native `wgpu` surface parented inside the webview** for the video viewport. This is the highest single technical risk in the entire plan: if it does not work, the UI stack must be re-architected before any further investment.

The plan reserved Phase −1 — a 2-day spike (`vance-spike-parenting/`) — to buy decisive information. The spike was executed on macOS only; Windows and Linux access is not currently available.

## Decision

**Proceed with the Tauri-host + native-overlay-NSView + CAMetalLayer + wgpu pattern**, with one new architectural constraint and two open verifications.

Concrete pattern (macOS, validated):

1. Tauri 2 hosts the chrome (HTML/Svelte). The HTML contains a placeholder div for the viewport.
2. The frontend reports the placeholder div's bounds (in physical pixels) on every layout change via `invoke('set_viewport_bounds', { x, y, w, h })`. Triggers: `ResizeObserver` on the div, `resize` and `scroll` events.
3. Rust attaches a layer-backed `NSView` as the topmost subview of the Tauri window's content view (sibling of, and positioned **above**, the `WKWebView`). The NSView's backing layer is replaced with a `CAMetalLayer`.
4. wgpu creates its surface via `SurfaceTargetUnsafe::CoreAnimationLayer(layer_ptr)`. A background thread renders frames into that layer.
5. On every IPC bounds update, Rust dispatches a main-thread `setFrame:` on the overlay NSView (libdispatch `dispatch_async_f` to `_dispatch_main_q`).

For Windows and Linux we expect the same shape with platform-equivalent primitives (HWND child window via WebView2 controller; X11 child window or Wayland subsurface via the GTK / wry handle), to be verified as the very first task of Phase 0 Week 1.

## The Constraint (load-bearing)

**Native overlay sits visually above the HTML chrome.** HTML elements that overlap the viewport rectangle are visually obscured by the wgpu surface, even though click events still route correctly to the webview. We discovered this on macOS during the spike and accept it as a permanent constraint of this architecture.

Implications for UI design:

- **Chrome must not visually overlap the viewport area.** Toolbars, sidebars, headers, and status bars surround the viewport but never cross it.
- **Floating UI that needs to appear *over* the viewport** (dropdown menus, tooltips, context menus, modal overlays, drag previews, scrub overlays, transport HUD) must be rendered as **native overlays**, not HTML. Concretely: a second native sublayer (or a transparent native popover NSView/HWND/GTK widget) draws on top of the wgpu surface for these cases. This is a v2 concern; v1 ships without HTML-over-viewport overlays and instructs designers accordingly.
- **The viewport's surrounding rounded-corner / drop-shadow effects must be drawn by the native overlay**, not by HTML. Trying to mask the wgpu surface from HTML is impossible.

## Verification Status

| Platform | Status | Notes |
|---|---|---|
| macOS (Apple Silicon) | ✅ PASS (with constraint) | Window opens, viewport renders an animated rainbow at the placeholder position, resize / sidebar-toggle track correctly, click events on chrome under the rainbow still register. Tested on macOS 26.x, M1, Tauri 2.10, wgpu 29. |
| macOS (Intel) | ⏸️ Not tested | Same code path; expected to work. Verify in Phase 0 if/when an Intel Mac is available. |
| Windows (HWND parenting via WebView2) | ⏸️ Pending | First task of Phase 0 Week 1. |
| Linux X11 | ⏸️ Pending | First task of Phase 0 Week 1. |
| Linux Wayland (GNOME Mutter) | ⏸️ Pending | First task of Phase 0 Week 1. Highest risk: subsurface compositor behavior is compositor-dependent. |
| Linux Wayland (KDE KWin) | ⏸️ Pending | If Mutter passes, KWin almost certainly does too. |

Tests not yet performed even on macOS:
- Drag across monitors at different DPI.
- Native fullscreen enter/exit.
- Window minimize / restore repaint.
- z-order under HiDPI in mixed-monitor setups.

These are deferred to Phase 0 Week 1 alongside Windows/Linux verification.

## Decision Tree (per the original plan)

The plan called out:

| Outcome | Action |
|---|---|
| All four platforms pass cleanly | Proceed as written. |
| Z-order broken on Win/Linux only | Acceptable — adopt constraint. |
| Wayland broken, X11 works | Ship X11 on Linux for v1. |
| Wayland *and* X11 both broken | Re-plan UI seam. |
| macOS broken | Stop and re-think. |

We hit a fifth case the plan did not enumerate: **macOS works but z-order is fixed (overlay always above)**. We treat this as the "acceptable — adopt constraint" branch, generalized to all platforms. The constraint is documented above and folded back into PLAN.md.

## Consequences

**Easier:**
- The biggest architectural risk is now known to be tractable on macOS.
- We can proceed with Phase 0 with high confidence in the UI seam.
- Future native overlays for tooltips/menus/drag previews have a clear pattern (more native sublayers / popovers).

**Harder:**
- Designers cannot freely place floating HTML elements over the viewport. This is a real constraint on the visual design language and must be communicated to anyone working on UI.
- Implementing native floating UI for menus/tooltips over the viewport is more work than the equivalent HTML — deferred to v2 if not needed in v1.
- The IPC must remain low-latency for the viewport to track div bounds without visible lag during chrome layout changes.

**New risks:**
- Windows and Linux verification is unfinished; if either fails, the cost of finding out grows the deeper we go into Phase 0. Mitigation: make verification the literal first task of Phase 0.
- The "native overlay covers HTML" behavior may surprise contributors; document it loudly in `CONTRIBUTING.md` once Vance proper begins.

**Follow-up:**
1. PLAN.md §4 ("UI chrome" / "Viewport") and §5.1 (process model diagram) updated to reflect the constraint.
2. Phase 0 Week 1 has a new explicit task: re-execute the spike test matrix on Windows + Linux X11 + Linux Wayland and update this ADR with the results before any other Phase 0 work proceeds.
3. UI design tokens / Storybook (when introduced) must include a "viewport-safe area" guideline.

## Alternatives Considered

- **Tauri webview-only (render viewport into a `<canvas>` via WebGPU/WebGL2):** Rejected. Performance ceiling for 4K real-time decode + grade is far below native wgpu, and we'd lose direct access to platform encoders / decoders that share the GPU surface.
- **Separate top-level OS window for the viewport** (Resolve-style float-out): Considered as a *fallback* if parenting failed. It would have made multi-monitor workflows nicer but worsened single-monitor UX. We keep the *option* of a "pop out viewport" in v2 as a feature, not a workaround.
- **Drop Tauri, use Iced or Slint for everything:** Considered as the fallback if the spike failed catastrophically. Lower polish for a non-technical audience; not needed since the spike succeeded.
- **Layer behind the WKWebView with transparent web background:** Briefly considered. Would put HTML naturally on top of the viewport. Rejected because (a) WKWebView's backing surface compositing model does not reliably support being made fully transparent for arbitrary content, and (b) it inverts the spike result we already validated. Revisit in v2 if floating-over-viewport HTML becomes a frequent need.

## Notes

- Spike repo: `vance-spike-parenting/` (deleted after this ADR lands; the spike's purpose was to produce this document).
- Key files referenced (in the spike, for posterity):
  - `src-tauri/src/viewport.rs` — overlay NSView, CAMetalLayer attach, wgpu render loop, libdispatch main-thread sync.
  - `src-tauri/src/main.rs` — Tauri host + IPC commands.
  - `dist/index.html` — chrome with `<div id="viewport">` placeholder, ResizeObserver-driven bounds reporting.
- Crates used in the spike: `tauri 2.10`, `wgpu 29`, `objc2 0.6`, `objc2-app-kit 0.3`, `objc2-quartz-core 0.3`, `parking_lot 0.12`, `pollster 0.4`.
- Known caveat in the spike code (intentional, scope-limited): the overlay NSView and CAMetalLayer are leaked via `mem::forget` so they outlive the install function. A real implementation in `vance-viewport` will own them on the app handle and drop them at shutdown.
