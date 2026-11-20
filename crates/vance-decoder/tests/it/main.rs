//! All integration tests for `vance-decoder`, compiled as ONE test binary.
//!
//! Intentional: one binary instead of one per file. Every integration-test
//! file is otherwise its own executable that statically links this crate's
//! whole dependency tree, which multiplied link time and target-dir size.
//! Add new tests as a module here (`mod my_test;` + `it/my_test.rs`), not as
//! a new top-level `tests/*.rs` file. Run one module with:
//!   cargo test -p vance-decoder --test it <module>::

mod annexb;
mod avi;
mod crash_isolation;
mod decode_smoke;
mod hw_decode;
mod memory_budget;
mod pixel_format_convert;
mod streaming;
mod threading;
mod transport;
