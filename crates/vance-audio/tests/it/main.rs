//! All integration tests for `vance-audio`, compiled as ONE test binary.
//!
//! Intentional: one binary instead of one per file. Every integration-test
//! file is otherwise its own executable that statically links this crate's
//! whole dependency tree, which multiplied link time and target-dir size.
//! Add new tests as a module here (`mod my_test;` + `it/my_test.rs`), not as
//! a new top-level `tests/*.rs` file. Run one module with:
//!   cargo test -p vance-audio --test it <module>::
//!
//! Exception — tests that mutate process-wide environment variables
//! (`std::env::set_var`) must stay in their OWN binary (`tests/lv2_host.rs`):
//! `set_var` racing another thread's env read (any `Command::spawn` copies
//! the environment) is undefined behaviour, and `remote_plugin` spawns
//! sidecars concurrently.

mod remote_plugin;
mod streaming;
