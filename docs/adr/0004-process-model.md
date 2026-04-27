# ADR 0004 — Process Model

- **Status:** Accepted
- **Date:** 2026-04-27
- **Deciders:** Vance core team
- **Supersedes:** —

## Context

A production NLE has to decode untrusted media, host third-party plugins, talk to LLM providers, and render at 60fps. Putting all of that in one process means any crash kills the editor and any plugin can read user files. The other extreme — microservices over the network — is wrong for a desktop app's latency and complexity budget.

PLAN.md §5.1 commits to a small, fixed set of OS processes connected by shared-memory + bincode IPC. This ADR codifies that model and its rules.

## Decision

```
┌───────────────────────────────────────────────────────────────────────┐
│                         vance-app  (main process)                     │
│  ┌─────────────────────────────────────────────────────────────────┐  │
│  │ Tauri webview window  (chrome: panels, browser, chat, scopes)   │  │
│  │  ┌──────────────────────────────────────────────────────────┐   │  │
│  │  │ Native parented winit+wgpu surface  (video viewport)     │   │  │
│  │  └──────────────────────────────────────────────────────────┘   │  │
│  └─────────────────────────────────────────────────────────────────┘  │
│  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌────────────┐                │
│  │  Engine  │ │  Render  │ │  Project │ │ AI Bridge  │                │
│  │ (state)  │ │  (wgpu)  │ │ (RON+VFS)│ │ (tool exec)│                │
│  └──────────┘ └──────────┘ └──────────┘ └─────┬──────┘                │
└────┬──────────────────┬──────────────────────┼───────────────────────┘
     │ IPC              │ IPC                  │ IPC
┌────▼─────────────┐ ┌──▼──────────────────┐ ┌─▼─────────────────────┐
│ vance-decoder    │ │ vance-plugin-host   │ │  vance-ai-runtime     │
│ (FFmpeg sidecar) │ │ (OFX/CLAP/LV2)      │ │  (HTTPS / local)      │
│ N pooled workers │ │ M sandboxed workers │ │  1 worker per chat    │
└──────────────────┘ └─────────────────────┘ └───────────────────────┘
```

### Rules

1. **vance-app owns**: Tauri webview, native viewport surface, engine state, render thread, project I/O, AI Bridge.
2. **Sidecars are isolated.** `vance-decoder`, `vance-plugin-host`, `vance-ai-runtime` are separate OS processes. A crash in any sidecar surfaces as an in-app notice; the editor keeps running.
3. **IPC is bincode framing over `interprocess` sockets** (Unix domain sockets / Windows named pipes), with **shared-memory rings** for per-frame data. No JSON in the hot path.
4. **The AI Bridge submits to the same engine command channel as the UI.** The model has no privileged path. Every AI action is an `EditCommand` like every user action.
5. **Decoder sidecars run unprivileged.** No filesystem write, no network. Read access limited to media paths the engine explicitly opens.
6. **Plugin sandboxes**: WASM plugins (Phase 10) get no FS / no network by default. OFX plugins run in a sandboxed sidecar with seccomp / sandbox-exec / AppContainer.
7. **AI runtime sandbox**: outbound connections allowlisted to configured providers; localhost only when a local backend is selected.
8. **No `Arc<Mutex<Timeline>>`.** The Engine owns the timeline; everyone else gets immutable snapshots over a channel.

### Threads inside vance-app

- **Tauri main thread** — Tauri event loop, IPC routing. Never blocks.
- **Viewport thread** — winit event loop for the native viewport window.
- **Engine thread** — owns mutable timeline, processes commands from UI *and* AI Bridge.
- **Render thread** — owns the wgpu device queue, executes the render graph.
- **Decoder client threads** — IPC clients to the decoder sidecar pool.
- **AI Bridge thread** — Tokio task pool; receives streamed tokens, parses tool calls.
- **Audio thread** — high-priority cpal callback; pulls from a lock-free ring.
- **Background I/O** — Tokio runtime for indexing, thumbnails, autosave, proxy generation.

## Consequences

**Easier:**
- A corrupt MP4, a misbehaving OFX plugin, or a runaway local LLM cannot take down the editor.
- The AI's blast radius equals the Edit Action API, period.
- The headless render daemon (post-v1, `vanced`) reuses the same engine + sidecars.

**Harder:**
- Process boundaries demand an IPC discipline (versioned schemas, framing, error handling). Solved with `vance-ipc`.
- Shared memory for frame data is harder to get right than copying — required for 4K-at-60fps perf.
- Sandbox setup is platform-specific code (seccomp / sandbox-exec / AppContainer) — necessary cost.

**Risks:**
- IPC overhead for tiny per-frame messages: mitigated by shared-memory rings (control plane in bincode, data plane in shmem).
- Sidecar restart storms during fuzz / corrupt input: supervisor uses exponential backoff, surfaces persistent failures to the user.

## Alternatives Considered

- **Single process, threads only**: rejected — no crash isolation; any plugin or codec bug is a crash.
- **Microservices over TCP**: rejected — latency, port management, firewall friction; we're a desktop app.
- **Process-per-clip decoder**: rejected — process startup cost dominates for short clips.
- **WASM-only sandboxing for everything (no separate processes)**: rejected for v1 — FFmpeg in WASM is not a viable path for hardware decode.

## Notes

- Implementation lives in `vance-ipc` (framing + supervisor primitives), `vance-decoder` (sidecar binary + client), `vance-plugin-host` (sidecar), `vance-ai-runtime` (sidecar).
- See ADR 0001 for the broader stack picture.
- See ADR 0003 for the AI Bridge specifics.
- See ADR 0005 for the viewport parenting that ties the native surface to the Tauri webview.
