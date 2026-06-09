//! `vance-plugin-host` — sidecar binary that hosts sandboxed audio
//! plugins for the parent `vance-app` process.
//!
//! See `src/main.rs` for the entry point. The lib target keeps the
//! crate doc-buildable and houses the runtime backends.

#![warn(missing_docs)]

pub mod runtime;
