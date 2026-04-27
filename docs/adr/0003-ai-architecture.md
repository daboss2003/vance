# ADR 0003 — AI Assistant Architecture

- **Status:** Accepted
- **Date:** 2026-04-27
- **Deciders:** Vance core team
- **Supersedes:** —

## Context

PLAN.md commits to a first-class AI assistant with two modes:

- **Ask** — answer questions about the app, the project, editing concepts (read-only).
- **Act** — execute edits via a tool-use API.

The architecture must:

- Be **safe** by default (no arbitrary execution; no silent privileged actions).
- Be **undoable** (every AI action is reversible).
- Be **auditable** (the user can see what the AI did and why).
- Be **provider-agnostic** (Anthropic, OpenAI, OpenRouter, OpenAI-compatible, Ollama, llama.cpp).
- Be **cloud-first** but never cloud-required (consumer hardware can't run a capable model locally).
- Be **enhancement, not dependency** (the editor must be fully usable with the assistant disabled).

## Decision

### The Edit Action API

A versioned, schema-defined set of operations the AI is allowed to perform. This is the **only** boundary between the model and the engine.

Each action declares:

- `name` (e.g., `trim_clip`, `apply_lut`, `query_sequence_summary`)
- `description` (model-facing, included in the tool schema)
- `params` (JSON schema, validated before execution)
- `permission_class` — `read` | `edit` | `destructive` | `export`
- `idempotent` (bool)

Actions are **never** "execute arbitrary Rust", "run a shell command", "read/write arbitrary files", or "evaluate code". If a future capability needs filesystem access, it gets its own typed action with a permission class.

### Permission classes & confirmation policy

| Class | Default behavior |
|---|---|
| `read` | Always allowed, no confirmation. |
| `edit` | Allowed by default; user can flip to "ask first" globally or per-conversation. |
| `destructive` | Always confirmed (covers irreversible-from-UI operations like deleting source media; clip deletion is *not* destructive because it is undoable). |
| `export` | Always confirmed; user picks the destination, the assistant cannot. |

A session-level audit log records every AI-initiated action. Cmd/Ctrl+Z after an AI turn reverts all of that turn's actions as one operation.

### Provider abstraction

```rust
trait LlmProvider {
    async fn stream(&self, req: ChatRequest) -> Stream<Token>;
}
```

Implementations (priority order): `AnthropicProvider`, `OpenAiProvider`, `OpenRouterProvider`, `OpenAiCompatibleProvider`, `OllamaProvider`, `LlamaCppProvider`. Tool-use shape is normalized internally; per-provider adapters translate.

### Process model

- `vance-ai-runtime` is a **sidecar process** that owns either an HTTPS client (cloud providers) or a local backend (Ollama / llama.cpp). Streams tokens back to the AI Bridge over IPC.
- The **AI Bridge** in the main process routes tool calls from the runtime into engine commands, and routes engine state snapshots back as context.
- The Bridge submits to the **same command channel** the UI uses. The AI is a peer, not a privileged caller.

### Context discipline

- The assistant gets a **summary** snapshot, not the whole project: counts, selection, playhead, the last N edits, sequence structure at clip-level resolution, bin names.
- For deeper queries the model uses `query_*` tools (e.g., `query_clip_details(clip_id)`), keeping context windows small and cost predictable.
- A token / cost meter is visible in the chat UI.

### Security

- API keys stored in OS keychain (`keyring` crate). Never in project files, logs, or telemetry.
- All project-derived strings (clip names, file paths, captions) are wrapped in delimited blocks in the prompt. The system prompt instructs the model to treat such content as data, not instructions.
- Tool calls referencing project-derived identifiers are validated against current state — the model cannot, for example, "delete clip 'foo'" if no clip with that ID exists.
- The AI runtime sidecar's network egress is allowlisted to configured provider endpoints (sandboxed: seccomp on Linux, sandbox-exec on macOS, AppContainer on Windows). Local backends are restricted to localhost.

## Consequences

**Easier:**
- Adding an AI capability = adding a new typed Edit Action + (maybe) a new provider adapter. No engine changes.
- Switching providers is hot-swappable; conversations continue.
- Auditing what the AI did is trivial (the audit log is structured).
- Undoing an AI mistake is a single keystroke.

**Harder:**
- Every AI capability requires defining a typed action up front. We can never just "let the model run code."
- Cost surprises are a real risk — mitigated by token meters, per-conversation cost display, and confirmation gates on high-cost operations.

**Risks:**
- The AI hallucinates parameters that pass schema validation but are semantically wrong. Mitigation: per-action confirmation defaults for `edit`-class actions until the user opts into auto-approve.
- Prompt injection via media metadata (filenames, embedded captions). Mitigation: strict delimiter discipline + identifier re-validation.
- Provider API drift breaking adapters. Mitigation: provider trait isolates each adapter; integration tests per provider.

## Alternatives Considered

- **Local-first AI as the default**: Rejected. Hardware reality. Local stays as opt-in for power users.
- **A single hardcoded provider (no abstraction)**: Rejected. Forces a bet on one company's pricing/policies; users want choice.
- **Free-form code execution by the model** (e.g., model writes Python that drives the engine): Rejected. Catastrophic blast radius; impossible to audit; no clean undo.
- **Bundling a model in the installer**: Rejected. Multi-GB installer for a feature most users won't use locally.
- **A managed Vance-hosted relay as the *only* cloud option**: Rejected for v1. May be offered alongside BYO-key in the future.

## Notes

- See ADR 0001 for the broader stack picture.
- The Edit Action API schema is implemented in `vance-ai` (Phase A1) with semver-stable public API.
