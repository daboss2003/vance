//! `vance-plugin-sdk` — WASM effect-plugin SDK + host runtime.
//!
//! Two audiences share this crate:
//! - **Plugin authors** compile to `wasm32-unknown-unknown` with the
//!   `guest` feature and use [`guest`] helpers to export the ABI.
//! - **The host** (`vance-app`) enables the `host` feature to get the
//!   `wasmtime`-backed [`host::WasmPlugin`] loader + frame processor.
//!
//! Both share the [`abi`] contract + [`manifest`] types, which build
//! on any target with no extra features.
//!
//! See [`abi`] for the function-level contract a plugin must satisfy
//! and [`manifest`] for the RON schema.

#![warn(missing_docs)]

pub mod abi;
pub mod manifest;

#[cfg(feature = "guest")]
pub mod guest;

#[cfg(feature = "host")]
pub mod host;

#[cfg(feature = "host")]
pub mod discovery;

pub use manifest::{ManifestError, ParamSpec, PluginKind, PluginManifest};
