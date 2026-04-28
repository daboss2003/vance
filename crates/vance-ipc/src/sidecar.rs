//! Sidecar process supervisor (parent side) and request-loop helper (child side).
//!
//! ## Parent
//!
//! ```ignore
//! use vance_ipc::Sidecar;
//! let mut sc: Sidecar<MyReq, MyResp> = Sidecar::spawn("my-sidecar-binary", &[])?;
//! let resp = sc.request(&MyReq::Ping)?;
//! sc.shutdown()?; // graceful: drops stdin, waits for exit
//! ```
//!
//! ## Child
//!
//! ```ignore
//! use vance_ipc::serve;
//! fn main() -> vance_ipc::Result<()> {
//!     serve::<MyReq, MyResp, _>(|req| match req {
//!         MyReq::Ping => MyResp::Pong,
//!     })
//! }
//! ```

use crate::{read_frame, write_frame, Error, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::ffi::OsStr;
use std::io::{BufReader, BufWriter, Write};
use std::marker::PhantomData;
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};

/// A supervised sidecar process exchanging typed `Req`/`Resp` messages.
///
/// Lifetime: spawned in `spawn`; the child exits when the parent drops the
/// `Sidecar` (Drop kills the child if it hasn't already exited). Use
/// `shutdown()` for a clean stdin-close-and-wait.
pub struct Sidecar<Req, Resp> {
    /// `None` only after `shutdown` has consumed it.
    child: Option<Child>,
    /// `None` only after `shutdown` has dropped it.
    stdin: Option<BufWriter<ChildStdin>>,
    stdout: BufReader<ChildStdout>,
    program: String,
    _phantom: PhantomData<fn(Req) -> Resp>,
}

impl<Req: Serialize, Resp: DeserializeOwned> Sidecar<Req, Resp> {
    /// Spawn a sidecar binary. Inherits the parent's stderr (so child logs
    /// surface in the parent's log stream); pipes stdin and stdout for IPC.
    pub fn spawn<P, A, S>(program: P, args: A) -> Result<Self>
    where
        P: AsRef<Path>,
        A: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Self::spawn_with_env(program, args, std::iter::empty::<(&str, &str)>())
    }

    /// Like [`Sidecar::spawn`] but also sets extra environment variables on
    /// the child process. Used by tests that want to flip feature flags
    /// (e.g. `VANCE_FORCE_SW=1`) without polluting the parent's env.
    pub fn spawn_with_env<P, A, S, E, K, V>(program: P, args: A, env: E) -> Result<Self>
    where
        P: AsRef<Path>,
        A: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
        E: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        let program_str = program.as_ref().display().to_string();
        let mut cmd = Command::new(program.as_ref());
        cmd.args(args)
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = cmd.spawn().map_err(|e| Error::SpawnFailed {
            program: program_str.clone(),
            source: e,
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::SidecarExited("stdin not piped".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::SidecarExited("stdout not piped".into()))?;

        Ok(Self {
            child: Some(child),
            stdin: Some(BufWriter::new(stdin)),
            stdout: BufReader::new(stdout),
            program: program_str,
            _phantom: PhantomData,
        })
    }

    /// Send a request and synchronously block until the sidecar replies.
    ///
    /// If the sidecar has died, returns `Error::SidecarExited`.
    pub fn request(&mut self, req: &Req) -> Result<Resp> {
        match self.try_request(req) {
            Ok(resp) => Ok(resp),
            Err(Error::Io(_)) | Err(Error::UnexpectedEof) => {
                let status = self
                    .child
                    .as_mut()
                    .and_then(|c| c.try_wait().ok().flatten());
                Err(Error::SidecarExited(format!(
                    "{}: {:?}",
                    self.program, status
                )))
            }
            Err(other) => Err(other),
        }
    }

    fn try_request(&mut self, req: &Req) -> Result<Resp> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| Error::SidecarExited("already shut down".into()))?;
        write_frame(stdin, req)?;
        stdin.flush()?;
        read_frame::<_, Resp>(&mut self.stdout)
    }

    /// Has the child exited? Non-blocking.
    pub fn is_alive(&mut self) -> bool {
        match self.child.as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    }

    /// Cleanly shut down: close the child's stdin so its read loop sees EOF
    /// and exits, then wait for the process.
    pub fn shutdown(mut self) -> Result<ExitStatus> {
        // Closing stdin causes the child's read loop to see EOF.
        drop(self.stdin.take());
        let mut child = self
            .child
            .take()
            .expect("Sidecar.child set on construction, taken only here");
        Ok(child.wait()?)
    }

    /// Force-kill the child immediately. Use only if `shutdown` is not viable.
    pub fn kill(&mut self) -> Result<()> {
        if let Some(child) = self.child.as_mut() {
            child.kill()?;
        }
        Ok(())
    }
}

impl<Req, Resp> Drop for Sidecar<Req, Resp> {
    fn drop(&mut self) {
        // Best-effort cleanup if the user didn't call shutdown or kill.
        if let Some(mut child) = self.child.take() {
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

/// Child-side: run a request loop on stdin/stdout until EOF.
///
/// The handler is called once per request; its return value is sent back as
/// the response. Returns `Ok(())` on clean EOF (parent closed stdin) and
/// the appropriate `Err` on framing or I/O failure.
pub fn serve<Req, Resp, F>(mut handler: F) -> Result<()>
where
    Req: DeserializeOwned,
    Resp: Serialize,
    F: FnMut(Req) -> Resp,
{
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut input = BufReader::new(stdin.lock());
    let mut output = BufWriter::new(stdout.lock());

    loop {
        let req: Req = match read_frame(&mut input) {
            Ok(req) => req,
            Err(Error::UnexpectedEof) => return Ok(()), // parent closed stdin
            Err(e) => return Err(e),
        };
        let resp = handler(req);
        write_frame(&mut output, &resp)?;
        output.flush()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    enum EchoReq {
        Ping,
        Echo(String),
    }

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    enum EchoResp {
        Pong,
        Echoed(String),
    }

    /// Find the path to the `echo-sidecar` example binary.
    /// Cargo builds it under `target/<profile>/examples/echo-sidecar`.
    fn echo_sidecar_path() -> std::path::PathBuf {
        // `CARGO_BIN_EXE_<name>` is set for `[[bin]]` targets only;
        // for examples we have to derive it from CARGO_TARGET_DIR / out path.
        // Use `escargot` would be cleaner but adds a heavy dep; for the spike
        // we derive it from the test binary location.
        let mut p = std::env::current_exe().unwrap();
        // current_exe is target/<profile>/deps/<test_binary>
        p.pop(); // deps/
        p.pop(); // <profile>/
        p.push("examples");
        let exe_name = if cfg!(windows) {
            "echo-sidecar.exe"
        } else {
            "echo-sidecar"
        };
        p.push(exe_name);
        p
    }

    #[test]
    fn spawn_round_trip_kill() {
        let path = echo_sidecar_path();
        if !path.exists() {
            // The example binary is built on-demand; cargo test --workspace
            // should build it via the dev-dep on cargo, but if a user runs
            // `cargo test -p vance-ipc` first without building examples
            // they'll see this. Surface a useful hint.
            panic!(
                "echo-sidecar example not built at {}; run `cargo build --example echo-sidecar -p vance-ipc` first",
                path.display()
            );
        }

        let mut sc: Sidecar<EchoReq, EchoResp> =
            Sidecar::spawn(&path, std::iter::empty::<&str>()).unwrap();

        let r = sc.request(&EchoReq::Ping).unwrap();
        assert_eq!(r, EchoResp::Pong);

        let r = sc.request(&EchoReq::Echo("hello".into())).unwrap();
        assert_eq!(r, EchoResp::Echoed("hello".into()));

        // Multiple sequential requests work.
        for i in 0..10 {
            let r = sc.request(&EchoReq::Echo(format!("msg-{i}"))).unwrap();
            assert_eq!(r, EchoResp::Echoed(format!("msg-{i}")));
        }

        let status = sc.shutdown().unwrap();
        assert!(status.success(), "sidecar exited with {status:?}");
    }

    #[test]
    fn detects_dead_sidecar() {
        let path = echo_sidecar_path();
        if !path.exists() {
            panic!(
                "echo-sidecar example not built at {}; run `cargo build --example echo-sidecar -p vance-ipc` first",
                path.display()
            );
        }

        let mut sc: Sidecar<EchoReq, EchoResp> =
            Sidecar::spawn(&path, std::iter::empty::<&str>()).unwrap();
        sc.kill().unwrap();
        // Give the OS a moment to reap.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let r = sc.request(&EchoReq::Ping);
        assert!(matches!(r, Err(Error::SidecarExited(_))), "got {r:?}");
    }
}
