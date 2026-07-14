//! Phase 11 — local crash reporter.
//!
//! Installs a panic hook that appends a structured crash report to a local
//! file in the app config dir (`…/Vance/crashes/`) — always local, never
//! uploaded (any future upload is gated on `crash_reports_opt_in`). This
//! turns an otherwise-silent panic into something a user can find and attach
//! to a bug report. The default hook still runs (so the backtrace prints).

use std::io::Write;

/// Install the crash-reporting panic hook. Call once, early in `main`.
pub fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        write_report(info);
        // Preserve normal behavior (print panic + backtrace to stderr).
        default_hook(info);
    }));
}

/// The crash-log directory (`…/Vance/crashes`), created on demand.
pub fn crash_dir() -> std::io::Result<std::path::PathBuf> {
    let dir = crate::settings::config_dir()?.join("crashes");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn write_report(info: &std::panic::PanicHookInfo<'_>) {
    // Best-effort: a failure here must never mask the original panic.
    let Ok(dir) = crash_dir() else { return };
    let path = dir.join("last-crash.log");

    let location = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_else(|| "unknown".to_string());
    let message = payload_str(info.payload());
    let backtrace = std::backtrace::Backtrace::force_capture();
    let thread = std::thread::current();
    let thread_name = thread.name().unwrap_or("unnamed").to_string();

    let report = format!(
        "==== Vance crash ====\n\
         version: {}\n\
         os: {} {}\n\
         thread: {thread_name}\n\
         location: {location}\n\
         message: {message}\n\
         backtrace:\n{backtrace}\n\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
    );

    // Overwrite last-crash.log with the newest, and also append to a rolling
    // history so a crash-loop leaves a trail.
    let _ = std::fs::write(&path, report.as_bytes());
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("history.log"))
    {
        let _ = f.write_all(report.as_bytes());
    }
}

/// Extract a human string from a panic payload (`&str` or `String`).
fn payload_str(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_str_reads_str_and_string() {
        let a: Box<dyn std::any::Any + Send> = Box::new("boom");
        assert_eq!(payload_str(a.as_ref()), "boom");
        let b: Box<dyn std::any::Any + Send> = Box::new(String::from("kaboom"));
        assert_eq!(payload_str(b.as_ref()), "kaboom");
        let c: Box<dyn std::any::Any + Send> = Box::new(42u32);
        assert_eq!(payload_str(c.as_ref()), "<non-string panic payload>");
    }
}
