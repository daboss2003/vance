//! Phase 9.7 — profiling pass.
//!
//! When the env var `VANCE_TRACE_PATH` is set at startup, every
//! `tracing` span emitted by the workspace is written to a Chrome
//! trace JSON file at that path. Drop the file into Chrome's
//! `chrome://tracing` or Perfetto's `ui.perfetto.dev` to inspect
//! flame graphs of decode / render / encode latency.
//!
//! Usage:
//!
//! ```bash
//! VANCE_TRACE_PATH=/tmp/vance.trace.json cargo run -p vance-app
//! ```
//!
//! When the variable is absent, this module is a no-op — the
//! existing stderr fmt subscriber stays in charge.

use tracing_chrome::ChromeLayerBuilder;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Initialise the tracing subscriber. Returns `true` when chrome
/// tracing was enabled (so callers can log a one-line notice).
///
/// MUST be called exactly once at process startup. The chrome flush
/// guard is leaked intentionally so the process exits with the
/// trace file fully written; this is process-lifetime and adds no
/// measurable overhead.
pub fn init() -> bool {
    let chrome_path = std::env::var("VANCE_TRACE_PATH").ok();
    let fmt_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
    if let Some(path) = chrome_path {
        let (chrome_layer, guard) = ChromeLayerBuilder::new()
            .file(path)
            .include_args(true)
            .build();
        Box::leak(Box::new(guard));
        let _ = tracing_subscriber::registry()
            .with(fmt_layer)
            .with(chrome_layer)
            .try_init();
        true
    } else {
        let _ = tracing_subscriber::registry().with(fmt_layer).try_init();
        false
    }
}
