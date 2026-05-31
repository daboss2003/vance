//! `vance-ai-runtime` — stub for the AI assistant runtime.
//!
//! Full implementation lands in Phase A; this crate currently only
//! contains documentation for how the assistant will consume the
//! Phase 6.13 typed grading commands.
//!
//! # Grading-command integration contract (Phase 6.13 → A3)
//!
//! The engine exposes a typed grading-command surface in
//! [`vance_grading_cmd::GradingCommand`]. The schema is the single
//! source of truth — there is NO hand-mirrored Anthropic tool spec to
//! drift from it.
//!
//! ## Discovery
//!
//! 1. Call the Tauri command `grading_command_schema` (Rust:
//!    `vance_grading_cmd::grading_command_schema()`) once at
//!    prompt-build time. It returns a serde-JSON `Value` containing a
//!    full `schemars` `RootSchema`.
//! 2. Inject that schema directly into the Anthropic Messages API's
//!    `tools[].input_schema` field — the schema is already valid JSON
//!    Schema 2019-09, which Anthropic tool-use accepts natively.
//!
//! ## Submission
//!
//! When the model returns a `tool_use` block whose `name` is
//! `apply_grading_command` / `apply_grading_commands`, forward the
//! `input` JSON straight to the corresponding Tauri command. The
//! discriminator field is `"op"` (snake_case variant), e.g.:
//!
//! ```jsonc
//! {
//!   "op": "set_qualifier",
//!   "track_id": "00000000-0000-0000-0000-000000000001",
//!   "clip_id":  "00000000-0000-0000-0000-000000000002",
//!   "effect_id":"00000000-0000-0000-0000-000000000003",
//!   "enabled": true,
//!   "hue": { "center": 0.0, "range": 0.2 }
//! }
//! ```
//!
//! Batches (one AI turn ↔ one undo step) go to `apply_grading_commands`
//! with an optional `label` shown in the user's undo history.
//!
//! ## Outcomes
//!
//! Each command returns a [`vance_grading_cmd::GradingCommandOutcome`]
//! discriminated by `"kind"`. The two non-trivial variants:
//!
//! * `node_added` — carries the new `effect_id` (UUID). Use it as
//!   `effect_id` for any follow-up commands targeting the same node in
//!   the SAME batch.
//! * `tracked` — carries `frames_tracked`, `mean_score`, and the
//!   `start_clip_seconds` / `end_clip_seconds` range that the tracker
//!   overwrote.
//!
//! ## Schema stability
//!
//! Drift between the Rust enum and the committed snapshot is rejected
//! by the `vance-grading-cmd` snapshot test, surfaced as the
//! `cargo xtask verify-grading-schema` CI gate. Any change to the
//! command surface is intentional and must be reflected in the AI
//! prompt by re-fetching the schema at the next prompt-build.

#[doc(hidden)]
pub fn _stub() {}
