//! Starting child processes without leaking pipes between them.
//!
//! On macOS, `std::process::Command` creates a child's stdio pipes with
//! `pipe()` and only then marks them close-on-exec (there is no `pipe2`). If
//! another thread starts a process in that window, its child inherits the
//! pipe and keeps it open for its whole life. For a sidecar that means a
//! crash isn't seen as end-of-file until that unrelated process exits, so a
//! request can hang. (Linux creates the pipes close-on-exec atomically.)
//!
//! Every child the app starts goes through [`spawn`], [`output`] or
//! [`status`], which hold one process-wide lock while the process is being
//! started (never while waiting for it), so no two starts overlap.

use std::io;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::Mutex;

static SPAWN_LOCK: Mutex<()> = Mutex::new(());

/// `cmd.spawn()`, serialized with every other child start in this process.
pub fn spawn(cmd: &mut Command) -> io::Result<Child> {
    // Intentional: a poisoned lock only means another start panicked; the
    // lock guards no data, so carry on rather than failing every later start.
    let _start = SPAWN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    #[allow(clippy::disallowed_methods)] // the one sanctioned call site
    cmd.spawn()
}

/// Like `cmd.output()` with std's default stdio: stdout and stderr captured,
/// stdin null. Overrides any stdio set on `cmd`; a caller that configures its
/// own stdio must use [`spawn`] + `Child::wait_with_output` instead. Starts
/// under the lock, then waits without holding it.
pub fn output(cmd: &mut Command) -> io::Result<Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    spawn(cmd)?.wait_with_output()
}

/// `cmd.status()`: stdio as configured on `cmd` (std's default is to inherit).
/// Starts under the lock, then waits without holding it.
pub fn status(cmd: &mut Command) -> io::Result<ExitStatus> {
    spawn(cmd)?.wait()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_captures_stdout_and_stderr_like_std() {
        let out = output(Command::new("sh").args(["-c", "printf out; printf err >&2"])).unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, b"out");
        assert_eq!(out.stderr, b"err");
    }

    #[test]
    fn output_gives_the_child_a_closed_stdin() {
        // `cat` exits at once on a null stdin instead of blocking on ours.
        let out = output(&mut Command::new("cat")).unwrap();
        assert!(out.status.success());
        assert!(out.stdout.is_empty());
    }

    #[test]
    fn status_reports_exit_code() {
        let st = status(Command::new("sh").args(["-c", "exit 3"])).unwrap();
        assert_eq!(st.code(), Some(3));
    }

    #[test]
    fn lock_is_not_held_while_a_child_runs() {
        let mut slow = spawn(Command::new("sh").args(["-c", "sleep 2"])).unwrap();
        let t = std::time::Instant::now();
        let st = status(Command::new("sh").args(["-c", "exit 0"])).unwrap();
        assert!(st.success());
        assert!(t.elapsed() < std::time::Duration::from_secs(1));
        let _ = slow.kill();
        let _ = slow.wait();
    }
}
