//! All integration tests for `vance-plugin-host`, compiled as ONE test binary.
//!
//! Intentional: one binary instead of one per file. Every integration-test
//! file is otherwise its own executable that statically links this crate's
//! whole dependency tree, which multiplied link time and target-dir size.
//! Add new tests as a module here (`mod my_test;` + `it/my_test.rs`), not as
//! a new top-level `tests/*.rs` file. Run one module with:
//!   cargo test -p vance-plugin-host --test it <module>::
//!
//! Exception — tests that mutate process-wide environment variables
//! (`std::env::set_var`) must stay in their OWN binary
//! (`tests/clap_discovery_e2e.rs`): `set_var` racing another thread's env
//! read (any `Command::spawn` copies the environment) is undefined behaviour,
//! and modules in this binary run concurrently and spawn sidecars.

mod clap_processing_e2e;
mod lv2_processing_e2e;
