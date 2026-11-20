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
//!
//! ## Bulk bytes
//!
//! A message can announce a raw blob that follows it on the same stream
//! (see [`crate::write_blob`]). Parent side: [`Sidecar::send`] +
//! [`Sidecar::send_blob`], or [`Sidecar::recv`] + [`Sidecar::recv_blob`].
//! Child side: [`serve_with_blobs`], whose handler reads request blobs via
//! [`BlobReader`] and attaches a reply blob with [`Reply::blob`].

use crate::{read_blob, read_frame, write_blob, write_frame, Error, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::ffi::OsStr;
use std::io::{BufReader, BufWriter, Read, Write};
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
    /// Equivalent to [`Sidecar::send`] followed by [`Sidecar::recv`].
    ///
    /// If the sidecar has died, returns `Error::SidecarExited`.
    pub fn request(&mut self, req: &Req) -> Result<Resp> {
        self.send(req)?;
        self.recv()
    }

    /// Send one request and flush, without waiting for the reply.
    ///
    /// If the sidecar has died, returns `Error::SidecarExited`.
    pub fn send(&mut self, req: &Req) -> Result<()> {
        let result = self.stdin().and_then(|stdin| {
            write_frame(stdin, req)?;
            Ok(stdin.flush()?)
        });
        result.map_err(|e| self.exited(e))
    }

    /// Block until the sidecar's next reply arrives.
    ///
    /// If the sidecar has died, returns `Error::SidecarExited`.
    pub fn recv(&mut self) -> Result<Resp> {
        let result = read_frame::<_, Resp>(&mut self.stdout);
        result.map_err(|e| self.exited(e))
    }

    /// Read the raw blob that follows a reply announcing `expected_len`
    /// bytes (see [`crate::read_blob`]).
    ///
    /// If the sidecar has died, returns `Error::SidecarExited`.
    pub fn recv_blob(&mut self, expected_len: u64) -> Result<Vec<u8>> {
        let result = read_blob(&mut self.stdout, expected_len);
        result.map_err(|e| self.exited(e))
    }

    /// Send a raw blob after a request that announced it, and flush.
    ///
    /// If the sidecar has died, returns `Error::SidecarExited`.
    pub fn send_blob(&mut self, bytes: &[u8]) -> Result<()> {
        let result = self.stdin().and_then(|stdin| {
            write_blob(stdin, bytes)?;
            Ok(stdin.flush()?)
        });
        result.map_err(|e| self.exited(e))
    }

    fn stdin(&mut self) -> Result<&mut BufWriter<ChildStdin>> {
        self.stdin
            .as_mut()
            .ok_or_else(|| Error::SidecarExited("already shut down".into()))
    }

    /// A broken pipe or EOF means the child is gone: report it as
    /// `SidecarExited` with its exit status (if it has been reaped yet).
    fn exited(&mut self, e: Error) -> Error {
        match e {
            Error::Io(_) | Error::UnexpectedEof => {
                let status = self
                    .child
                    .as_mut()
                    .and_then(|c| c.try_wait().ok().flatten());
                Error::SidecarExited(format!("{}: {:?}", self.program, status))
            }
            other => other,
        }
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

    /// The OS PID of the running child (if it hasn't already been
    /// shut down). Use this when you need to signal the process from
    /// a different thread than the one that owns the `Sidecar` —
    /// `kill` requires `&mut self`, but signalling via PID does not.
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(|c| c.id())
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

/// A child-side reply for [`serve_with_blobs`]: the message, plus the raw
/// blob that follows it when the message announces one.
#[derive(Debug)]
pub struct Reply<Resp> {
    /// The response message.
    pub msg: Resp,
    /// Raw bytes written right after `msg` (see [`crate::write_blob`]).
    pub blob: Option<Vec<u8>>,
}

impl<Resp> From<Resp> for Reply<Resp> {
    /// A reply with no blob.
    fn from(msg: Resp) -> Self {
        Self { msg, blob: None }
    }
}

/// Lets a [`serve_with_blobs`] handler read the raw blob that follows the
/// request it is handling.
pub struct BlobReader<'a> {
    input: &'a mut dyn Read,
}

impl BlobReader<'_> {
    /// Read the blob that follows the current request, which announced
    /// `expected_len` bytes (see [`crate::read_blob`]).
    pub fn read_blob(&mut self, expected_len: u64) -> Result<Vec<u8>> {
        read_blob(&mut self.input, expected_len)
    }
}

/// Child-side: like [`serve`], but requests and replies may carry raw blobs.
///
/// The handler reads any blob its request announced through the
/// [`BlobReader`]; a [`Reply`] with `blob: Some(..)` writes that blob right
/// after the response message. Returns `Ok(())` on clean EOF.
pub fn serve_with_blobs<Req, Resp, F>(handler: F) -> Result<()>
where
    Req: DeserializeOwned,
    Resp: Serialize,
    F: FnMut(Req, &mut BlobReader<'_>) -> Reply<Resp>,
{
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve_blobs_on(
        BufReader::new(stdin.lock()),
        BufWriter::new(stdout.lock()),
        handler,
    )
}

fn serve_blobs_on<Req, Resp, F>(
    mut input: impl Read,
    mut output: impl Write,
    mut handler: F,
) -> Result<()>
where
    Req: DeserializeOwned,
    Resp: Serialize,
    F: FnMut(Req, &mut BlobReader<'_>) -> Reply<Resp>,
{
    loop {
        let req: Req = match read_frame(&mut input) {
            Ok(req) => req,
            Err(Error::UnexpectedEof) => return Ok(()), // parent closed stdin
            Err(e) => return Err(e),
        };
        let reply = handler(req, &mut BlobReader { input: &mut input });
        write_frame(&mut output, &reply.msg)?;
        if let Some(blob) = &reply.blob {
            write_blob(&mut output, blob)?;
        }
        output.flush()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    // Must match `examples/echo_sidecar.rs` variant for variant (bincode
    // encodes the variant index).
    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    enum EchoReq {
        Ping,
        Echo(String),
        EchoBlob { len: u64 },
        MakeBlob { len: u64 },
    }

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    enum EchoResp {
        Pong,
        Echoed(String),
        Blob { len: u64 },
        Failed(String),
    }

    /// What `MakeBlob` fills its blob with (mirrors the example).
    fn pattern(len: u64) -> Vec<u8> {
        (0..len).map(|i| (i % 253) as u8).collect()
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

    /// Spawn the echo sidecar. The example binary is built on demand
    /// (`cargo xtask test` builds examples first; a bare `cargo test -p
    /// vance-ipc` may not have), hence the hint.
    ///
    /// Intentional: spawns in this module are serialized. On macOS std
    /// creates a child's pipes with `pipe()` and sets CLOEXEC afterwards, so
    /// a sidecar spawned by a concurrent test in between inherits the pipe
    /// and keeps it open; a write to a killed child then still succeeds
    /// (seen ~2 in 1000 runs).
    fn spawn_echo(args: &[&str]) -> Sidecar<EchoReq, EchoResp> {
        static SPAWN: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _serialized = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
        let path = echo_sidecar_path();
        assert!(
            path.exists(),
            "echo-sidecar example not built at {}; run `cargo build --example echo-sidecar -p vance-ipc` first",
            path.display()
        );
        Sidecar::spawn(&path, args).unwrap()
    }

    #[test]
    fn spawn_round_trip_kill() {
        let mut sc = spawn_echo(&[]);

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
        let mut sc = spawn_echo(&[]);
        sc.kill().unwrap();
        // Give the OS a moment to reap.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let r = sc.request(&EchoReq::Ping);
        assert!(matches!(r, Err(Error::SidecarExited(_))), "got {r:?}");
    }

    #[test]
    fn blobs_cross_the_process_boundary_both_ways() {
        let mut sc = spawn_echo(&["--blobs"]);

        // Plain request/response still works against a blob server.
        assert_eq!(sc.request(&EchoReq::Ping).unwrap(), EchoResp::Pong);

        // Parent → child → parent. Far bigger than a pipe buffer, so both
        // sides must stream it rather than fit it in one write.
        let big: Vec<u8> = (0..9_000_011u32).map(|i| (i * 7 % 256) as u8).collect();
        sc.send(&EchoReq::EchoBlob {
            len: big.len() as u64,
        })
        .unwrap();
        sc.send_blob(&big).unwrap();
        assert_eq!(
            sc.recv().unwrap(),
            EchoResp::Blob {
                len: big.len() as u64
            }
        );
        assert!(sc.recv_blob(big.len() as u64).unwrap() == big);

        // Child → parent, several in a row, interleaved with plain messages.
        for len in [0u64, 1, 65_537, 4_000_000] {
            assert_eq!(
                sc.request(&EchoReq::MakeBlob { len }).unwrap(),
                EchoResp::Blob { len }
            );
            assert!(sc.recv_blob(len).unwrap() == pattern(len), "len {len}");
            assert_eq!(
                sc.request(&EchoReq::Echo(format!("after {len}"))).unwrap(),
                EchoResp::Echoed(format!("after {len}"))
            );
        }

        assert!(sc.shutdown().unwrap().success());
    }

    #[test]
    fn blob_from_a_dead_sidecar_is_sidecar_exited() {
        let mut sc = spawn_echo(&["--blobs"]);
        sc.kill().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let r = sc.recv_blob(10);
        assert!(matches!(r, Err(Error::SidecarExited(_))), "got {r:?}");
        let r = sc.send_blob(&[1, 2, 3]);
        assert!(matches!(r, Err(Error::SidecarExited(_))), "got {r:?}");
    }

    #[test]
    fn serve_loop_reads_request_blobs_and_writes_reply_blobs() {
        let mut input = Vec::new();
        write_frame(&mut input, &EchoReq::EchoBlob { len: 3 }).unwrap();
        write_blob(&mut input, &[1, 2, 3]).unwrap();
        write_frame(&mut input, &EchoReq::Ping).unwrap();
        let mut output = Vec::new();
        serve_blobs_on(
            std::io::Cursor::new(input),
            &mut output,
            |req: EchoReq, blobs| match req {
                EchoReq::EchoBlob { len } => Reply {
                    msg: EchoResp::Blob { len },
                    blob: Some(blobs.read_blob(len).unwrap()),
                },
                EchoReq::Ping => EchoResp::Pong.into(),
                other => EchoResp::Failed(format!("{other:?}")).into(),
            },
        )
        .unwrap();

        let mut r = std::io::Cursor::new(output);
        let m: EchoResp = read_frame(&mut r).unwrap();
        assert_eq!(m, EchoResp::Blob { len: 3 });
        assert_eq!(read_blob(&mut r, 3).unwrap(), [1, 2, 3]);
        let m: EchoResp = read_frame(&mut r).unwrap();
        assert_eq!(m, EchoResp::Pong);
        assert_eq!(
            r.position() as usize,
            r.get_ref().len(),
            "nothing extra written"
        );
    }
}
