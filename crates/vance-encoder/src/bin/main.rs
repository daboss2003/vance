//! `vance-encoder` sidecar binary.
//!
//! Spawns the `ffmpeg` CLI binary as a child process and pipes raw
//! pixel data + audio samples through it. Going via the CLI keeps the
//! sidecar small and codec-agnostic — every preset ffmpeg supports is
//! reachable by varying argv. Trade-off: requires `ffmpeg` on PATH.
//!
//! Audio is buffered to a side WAV file during the session and muxed
//! into the final container at `Finish`. This avoids needing multiple
//! stdin pipes (the CLI only takes one stdin) at the cost of one extra
//! mux pass per export.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};
use vance_encoder::protocol::{
    AudioCodec, AudioConfig, ColorPrimaries, ColorSignaling, EncoderError, EncoderPreset,
    EncoderRequest, EncoderResponse, HdrMetadata, MatrixCoefficients, SignalRange,
    TransferCharacteristics, VideoConfig, PROTOCOL_VERSION,
};
use vance_ipc::BlobReader;

fn main() -> vance_ipc::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    tracing::info!("encoder sidecar starting");
    sweep_stale_temp_files(&std::env::temp_dir());

    let mut state: State = State::default();
    vance_ipc::serve_with_blobs::<EncoderRequest, EncoderResponse, _>(|req, blobs| {
        state.handle(req, blobs).into()
    })
}

#[derive(Default)]
struct State {
    open: Option<OpenSession>,
    /// Set when a pushed blob couldn't be read. The request stream is then out
    /// of sync, so nothing that follows can be trusted to be a real request.
    stream_error: Option<String>,
    /// Sessions opened so far: part of each session's temp file names.
    sessions: u32,
}

/// One export. It is atomic: the container is written to a hidden partial
/// file next to the destination ([`partial_path`]) and renamed over it only
/// when `Finish` succeeds, so a failed or abandoned export never touches a
/// file already at the destination. The partial file and the temp files (the
/// side video and WAV of an export with audio) are deleted when the session
/// drops: after `Finish`, on every error, and when it is abandoned (a new
/// `Open`, `Shutdown`, the parent closing the pipe). A killed sidecar leaves
/// them to the startup sweep ([`sweep_stale_temp_files`]) and, for the
/// partial file, the next export to the same directory
/// ([`sweep_stale_partials`]).
struct OpenSession {
    destination: PathBuf,
    video: Option<VideoPipe>,
    audio: Option<AudioPipe>,
    /// Declared after `video`: its ffmpeg is stopped before this goes.
    partial: TempFile,
}

/// Part of every partial file's name:
/// `.<stem>.vance-partial-<host>-<pid>[.<ext>]`.
const PARTIAL_TAG: &str = ".vance-partial-";

/// What a finished export that couldn't replace its destination is kept as:
/// `<stem> (not replaced)[.<ext>]` next to it.
const NOT_REPLACED: &str = " (not replaced";

/// The longest file name (in bytes) the names made here may have: the limit
/// of every common filesystem (255 bytes, or 255 UTF-16 units, which a
/// UTF-8 name never has more of than bytes).
const MAX_NAME_BYTES: usize = 255;

/// A partial file older than this may be swept (with its pid gone): one
/// being written changes all the time.
const PARTIAL_MIN_AGE: Duration = Duration::from_secs(10 * 60);

/// `name` split as its user sees it: the stem and the extension after the
/// last dot. A name that starts with its only dot (`.mp4`) is all extension;
/// an empty extension (`name.`) is none.
fn split_name(name: &str) -> (&str, Option<&str>) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !ext.is_empty() => (stem, Some(ext)),
        _ => (name, None),
    }
}

/// FNV-1a: a short, stable fingerprint (no hasher is stable across Rust
/// releases).
fn fnv1a(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c_9dc5, |h, &b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    })
}

/// `prefix + stem + suffix + .ext`, with the stem shortened (and a hash of
/// it added, so different long stems stay apart) when the whole would be
/// longer than [`MAX_NAME_BYTES`].
fn fitted_name(prefix: &str, stem: &str, suffix: &str, ext: Option<&str>) -> String {
    let ext = ext.map(|e| format!(".{e}")).unwrap_or_default();
    let fixed = prefix.len() + suffix.len() + ext.len();
    if fixed + stem.len() <= MAX_NAME_BYTES {
        return format!("{prefix}{stem}{suffix}{ext}");
    }
    let hash = format!("~{:08x}", fnv1a(stem.as_bytes()));
    let mut keep = MAX_NAME_BYTES
        .saturating_sub(fixed + hash.len())
        .min(stem.len());
    while !stem.is_char_boundary(keep) {
        keep -= 1;
    }
    format!("{prefix}{}{hash}{suffix}{ext}", &stem[..keep])
}

/// This machine's id in partial file names: a hash of its host name, so a
/// sweep never deletes another machine's partial file (two machines
/// exporting into one network folder; pids mean nothing across hosts).
/// `None` when the host name can't be found: nothing is swept then.
fn host_tag() -> Option<&'static str> {
    static TAG: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    TAG.get_or_init(|| host_name().map(|name| format!("{:08x}", fnv1a(name.as_bytes()))))
        .as_deref()
}

fn host_name() -> Option<String> {
    let name = if cfg!(windows) {
        std::env::var("COMPUTERNAME").ok()
    } else if cfg!(target_os = "linux") {
        std::fs::read_to_string("/proc/sys/kernel/hostname").ok()
    } else {
        // `output` keeps it off this sidecar's IPC stdin / stdout.
        vance_ipc::process::output(&mut Command::new("hostname"))
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty())
}

/// The file name of `destination` (lossy for a name that isn't UTF-8: the
/// names made from it only need to be recognisable).
fn destination_name(destination: &Path) -> Result<String, String> {
    destination
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| format!("export path {} has no file name", destination.display()))
}

/// Where an export to `destination` is written until it is complete: a
/// hidden file in the same directory, so the final rename stays on one
/// filesystem, with the same extension, which tells ffmpeg the container.
fn partial_path(destination: &Path) -> Result<PathBuf, String> {
    let name = destination_name(destination)?;
    let (stem, ext) = split_name(&name);
    let host = host_tag().unwrap_or("unknown");
    let suffix = format!("{PARTIAL_TAG}{host}-{}", std::process::id());
    Ok(destination.with_file_name(fitted_name(".", stem, &suffix, ext)))
}

/// The host tag and pid in a partial file name
/// `.<stem>.vance-partial-<host>-<pid>[.<ext>]`.
fn partial_file_owner(name: &str) -> Option<(&str, u32)> {
    if !name.starts_with('.') {
        return None;
    }
    let rest = &name[name.rfind(PARTIAL_TAG)? + PARTIAL_TAG.len()..];
    let (host, rest) = rest.split_once('-')?;
    let pid = rest.split_once('.').map_or(rest, |(pid, _)| pid);
    if host.is_empty() || pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((host, pid.parse().ok()?))
}

/// The name a finished export that couldn't replace `destination` is
/// kept under, `<stem> (not replaced)[.<ext>]` (then `(not replaced 2)`
/// ...): visible (no leading dot, so never a
/// partial file's name either), `export` for a destination that is only
/// an extension.
fn kept_name(destination: &Path, n: u32) -> Result<String, String> {
    let name = destination_name(destination)?;
    let (stem, ext) = split_name(&name);
    let stem = match stem.trim_start_matches('.') {
        "" => "export",
        visible => visible,
    };
    let suffix = if n > 1 {
        format!("{NOT_REPLACED} {n})")
    } else {
        format!("{NOT_REPLACED})")
    };
    Ok(fitted_name("", stem, &suffix, ext))
}

/// Delete the partial files of exports into `dir` made on this machine by
/// encoder sidecars that are no longer running, and not written to for
/// [`PARTIAL_MIN_AGE`], before this sidecar exports there. Only the
/// directories this sidecar exports to are swept: nothing records where a
/// dead one was writing.
fn sweep_stale_partials(dir: &Path) {
    let Some(host) = host_tag() else {
        return;
    };
    let ours = |name: &str| {
        partial_file_owner(name)
            .filter(|&(h, _)| h == host)
            .map(|(_, pid)| pid)
    };
    sweep_dead(dir, ours, Some(PARTIAL_MIN_AGE), "partial export");
}

/// An export about to start: fail now, not after the whole encode, when
/// its finished file couldn't be moved to `destination`. A folder there
/// can't be replaced; on Windows neither can a file another program holds
/// open without delete sharing (opening it for deletion asks exactly that).
/// Elsewhere the partial file's creation, which follows, checks that the
/// directory takes new files.
fn check_destination(destination: &Path) -> Result<(), String> {
    let Ok(meta) = std::fs::symlink_metadata(destination) else {
        return Ok(());
    };
    if meta.is_dir() {
        return Err(format!(
            "{} is a folder; choose a file name",
            destination.display()
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const DELETE: u32 = 0x0001_0000;
        const SHARE_READ_WRITE_DELETE: u32 = 0x1 | 0x2 | 0x4;
        if let Err(e) = std::fs::OpenOptions::new()
            .access_mode(DELETE)
            .share_mode(SHARE_READ_WRITE_DELETE)
            .open(destination)
        {
            return Err(format!(
                "{} can't be replaced (is it open in another program?): {e}",
                destination.display()
            ));
        }
    }
    Ok(())
}

/// A file this sidecar created in the temp dir, deleted on drop.
struct TempFile(PathBuf);

impl TempFile {
    fn path(&self) -> &Path {
        &self.0
    }

    /// Keep the file after all: dropping this no longer deletes it.
    fn keep(&mut self) -> PathBuf {
        std::mem::take(&mut self.0)
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.0.as_os_str().is_empty() {
            return;
        }
        match std::fs::remove_file(&self.0) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("could not delete {}: {e}", self.0.display()),
        }
    }
}

/// The temp file `vance-enc-<pid>-<session>-<what>` (see
/// [`sweep_stale_temp_files`]).
fn temp_file(dir: &Path, session: u32, what: &str) -> TempFile {
    let pid = std::process::id();
    TempFile(dir.join(format!("{TEMP_PREFIX}{pid}-{session}-{what}")))
}

const TEMP_PREFIX: &str = "vance-enc-";

struct VideoPipe {
    child: Child,
    /// `None` once closed (end of input for ffmpeg).
    stdin: Option<ChildStdin>,
    /// The child has been waited for.
    exited: bool,
    /// ffmpeg encoder name, for error messages.
    encoder: &'static str,
    width: u32,
    height: u32,
    /// Bytes per pixel of the raw frames the parent pushes (4 for 8-bit
    /// RGBA/BGRA, 8 for the 10-bit `rgba64le` HDR wire format).
    bytes_per_pixel: usize,
    out_path: PathBuf,
    /// `out_path`, when it is a temp file (an export with audio, muxed at
    /// `Finish`) rather than the export itself.
    _temp: Option<TempFile>,
    frames: u64,
}

impl VideoPipe {
    /// Close ffmpeg's input and wait for it to finish the file.
    fn finish(&mut self) -> Result<(), String> {
        drop(self.stdin.take());
        let status = self.child.wait().map_err(|e| format!("wait ffmpeg: {e}"))?;
        self.exited = true;
        if !status.success() {
            return Err(format!(
                "ffmpeg video ({}) exited with {status:?}",
                self.encoder
            ));
        }
        Ok(())
    }
}

impl Drop for VideoPipe {
    /// An abandoned encode: stop ffmpeg before its temp file goes (fields
    /// drop after this), so nothing writes it after it is deleted (and
    /// Windows can delete it at all).
    fn drop(&mut self) {
        if !self.exited {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

struct AudioPipe {
    /// Declared before `temp`: closed before the file is deleted.
    file: std::fs::File,
    temp: TempFile,
    sample_rate: u32,
    channels: u8,
    codec: AudioCodec,
    samples_written: u64,
    bytes_written: u64,
}

/// Delete `vance-enc-<pid>-*` files in `dir` left by an encoder sidecar that
/// is no longer running (killed mid-export, or crashed). Best effort: a file
/// whose process may still be alive, or that can't be read, is left alone.
fn sweep_stale_temp_files(dir: &Path) {
    sweep_dead(dir, temp_file_pid, None, "encoder temp");
}

/// Delete the files in `dir` whose name `pid_of` gives the pid of a process
/// that is no longer running, and (with `min_age`) that weren't modified for
/// that long. `what` is for the log.
fn sweep_dead(
    dir: &Path,
    pid_of: impl Fn(&str) -> Option<u32>,
    min_age: Option<Duration>,
    what: &str,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let me = std::process::id();
    let mut alive = BTreeMap::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(&pid_of) else {
            continue;
        };
        if pid == me || !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        if let Some(min_age) = min_age {
            // A file whose age can't be told counts as recent.
            let age = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok());
            if age.is_none_or(|age| age < min_age) {
                continue;
            }
        }
        if *alive
            .entry(pid)
            .or_insert_with(|| process_may_be_alive(pid))
        {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => tracing::info!("deleted stale {what} file {}", entry.path().display()),
            Err(e) => tracing::warn!("could not delete {}: {e}", entry.path().display()),
        }
    }
}

/// A finished export that couldn't replace `destination`: keep it under a
/// visible name ([`kept_name`]) next to it, else as a copy in the temp or
/// home folder, and return where it is now. Only if all of that fails does
/// it stay where it is (and is no longer deleted by this session).
fn keep_finished_export(partial: &mut TempFile, destination: &Path) -> PathBuf {
    // Next to the destination, by rename.
    if let Some(kept) = free_kept_name(destination, destination.parent()) {
        match std::fs::rename(partial.path(), &kept) {
            Ok(()) => return kept,
            Err(e) => tracing::warn!("could not keep the export as {}: {e}", kept.display()),
        }
    }
    // The folder takes no new names (it went read-only): a copy where one
    // can be written. Intentional: never left under its partial name, which
    // a later sweep of that folder would delete.
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
    for dir in [Some(std::env::temp_dir()), home.map(PathBuf::from)]
        .into_iter()
        .flatten()
    {
        let Some(kept) = free_kept_name(destination, Some(&dir)) else {
            continue;
        };
        match std::fs::copy(partial.path(), &kept) {
            // The partial (an extra copy now) goes with the session, if it can.
            Ok(_) => return kept,
            Err(e) => {
                let _ = std::fs::remove_file(&kept);
                tracing::warn!("could not keep the export as {}: {e}", kept.display());
            }
        }
    }
    // Nowhere else to put it: it stays where it is.
    partial.keep()
}

/// The first [`kept_name`] for `destination` not taken in `dir` (the
/// destination's own folder when `None`).
fn free_kept_name(destination: &Path, dir: Option<&Path>) -> Option<PathBuf> {
    let dir = dir.unwrap_or(Path::new("."));
    (1..=100)
        .map_while(|n| kept_name(destination, n).ok())
        .map(|name| dir.join(name))
        .find(|path| std::fs::symlink_metadata(path).is_err())
}

/// Windows: start a console program (`tasklist`) without flashing a
/// console window, as a windowless sidecar's children otherwise do.
fn no_console_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

/// The pid in a temp file name `vance-enc-<pid>-…`.
fn temp_file_pid(name: &str) -> Option<u32> {
    let (pid, _) = name.strip_prefix(TEMP_PREFIX)?.split_once('-')?;
    pid.bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| pid.parse().ok())?
}

/// Whether process `pid` may be running. `true` whenever that can't be
/// told (the check fails to run): a live encoder's files must never go.
fn process_may_be_alive(pid: u32) -> bool {
    let pid = pid.to_string();
    if cfg!(target_os = "linux") {
        return Path::new("/proc").join(&pid).exists();
    }
    let mut cmd = if cfg!(windows) {
        let mut cmd = Command::new("tasklist");
        cmd.args(["/FO", "CSV", "/NH", "/FI"])
            .arg(format!("PID eq {pid}"));
        cmd
    } else {
        let mut cmd = Command::new("ps");
        cmd.args(["-o", "pid=", "-p", &pid]);
        cmd
    };
    no_console_window(&mut cmd);
    // stdin / stdout carry this sidecar's IPC frames; `output` keeps the
    // check off them.
    match vance_ipc::process::output(&mut cmd) {
        // `ps` exits non-zero for no such process; `tasklist` prints an
        // "INFO: no tasks" line instead of the process's CSV row.
        Ok(out) if cfg!(windows) => {
            String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
        }
        Ok(out) => out.status.success(),
        Err(_) => true,
    }
}

impl State {
    fn handle(&mut self, req: EncoderRequest, blobs: &mut BlobReader<'_>) -> EncoderResponse {
        if let Some(reason) = &self.stream_error {
            // Intentional: refuse every request, even Hello or Shutdown. After
            // a failed blob read, "requests" may be leftover pixel bytes that
            // happen to decode, e.g. as an Open with a garbage path.
            return EncoderResponse::EncodeFailed(EncoderError::new(format!(
                "encoder request stream is out of sync after an earlier error: {reason}"
            )));
        }
        match req {
            EncoderRequest::Hello { .. } => EncoderResponse::Hello {
                protocol_version: PROTOCOL_VERSION,
                ffmpeg_version: probe_ffmpeg_version(),
                available_presets: available_presets(),
            },
            EncoderRequest::Open { path, video, audio } => match self.open(path, video, audio) {
                Ok(()) => EncoderResponse::Opened,
                Err(e) => EncoderResponse::OpenFailed(EncoderError::new(e)),
            },
            EncoderRequest::PushVideoFrame { pts: _, len } => {
                self.push_blob(blobs, len, Self::push_video)
            }
            EncoderRequest::PushAudioFrame { pts: _, len } => {
                self.push_blob(blobs, len, Self::push_audio)
            }
            EncoderRequest::Finish => match self.finish() {
                Ok((v, a)) => EncoderResponse::Finished {
                    video_frames: v,
                    audio_samples: a,
                },
                Err(e) => EncoderResponse::EncodeFailed(EncoderError::new(e)),
            },
            EncoderRequest::Shutdown => {
                tracing::info!("encoder sidecar shutting down");
                // Before the reply: the parent may kill us once it has it.
                self.open = None;
                EncoderResponse::Goodbye
            }
        }
    }

    fn open(
        &mut self,
        path: String,
        video: Option<VideoConfig>,
        audio: Option<AudioConfig>,
    ) -> Result<(), String> {
        if self.open.take().is_some() {
            // A session the parent never finished (an aborted export): it
            // is dropped, which stops its ffmpeg and deletes its temp files.
            tracing::warn!("Open replaces an unfinished export");
        }
        if video.is_none() && audio.is_none() {
            return Err("Open requires at least one of video or audio config".into());
        }
        if audio.as_ref().is_some_and(|a| a.channels == 0) {
            return Err("audio config has no channels".into());
        }

        let destination = PathBuf::from(path);
        check_destination(&destination)?;
        let partial = TempFile(partial_path(&destination)?);
        match partial.path().parent() {
            Some(dir) if !dir.as_os_str().is_empty() => sweep_stale_partials(dir),
            _ => sweep_stale_partials(Path::new(".")),
        }
        // Created now, so a folder that takes no new files (or a name too
        // long for it) fails the export here, not at its end. ffmpeg
        // overwrites it.
        std::fs::File::create(partial.path()).map_err(|e| {
            format!(
                "can't write an export next to {}: {e}",
                destination.display()
            )
        })?;
        let temp_dir = std::env::temp_dir();
        self.sessions = self.sessions.wrapping_add(1);
        let session = self.sessions;

        // Video goes either straight into the partial container (when
        // there's no audio) or to a temp file we mux at Finish. On any
        // error below, what was already set up drops and cleans up.
        let video_only = audio.is_none();
        let v_pipe = video
            .map(|cfg| {
                let temp = (!video_only).then(|| temp_file(&temp_dir, session, "video.mov"));
                let video_out = temp
                    .as_ref()
                    .map_or_else(|| partial.path().to_path_buf(), |t| t.path().to_path_buf());
                spawn_video_ffmpeg(&cfg, &video_out, temp)
            })
            .transpose()?;

        let a_pipe = audio
            .map(|cfg| open_wav_writer(&cfg, temp_file(&temp_dir, session, "audio.wav")))
            .transpose()?;

        self.open = Some(OpenSession {
            destination,
            video: v_pipe,
            audio: a_pipe,
            partial,
        });
        Ok(())
    }

    /// Reads the `len`-byte blob a push request announced and hands it to
    /// `push`, which checks its length against the open session.
    fn push_blob(
        &mut self,
        blobs: &mut BlobReader<'_>,
        len: u64,
        push: fn(&mut Self, &[u8]) -> Result<(), String>,
    ) -> EncoderResponse {
        // Intentional: the blob is read before anything is validated, even
        // when the push will be refused (wrong size, nothing open), so the
        // next request starts where the parent wrote it. `read_blob` bounds
        // the allocation at `MAX_BLOB_BYTES`.
        let bytes = match blobs.read_blob(len) {
            Ok(bytes) => bytes,
            Err(e) => {
                let reason = format!("reading a {len}-byte push: {e}");
                tracing::error!("{reason}");
                self.stream_error = Some(reason.clone());
                return EncoderResponse::EncodeFailed(EncoderError::new(reason));
            }
        };
        match push(self, &bytes) {
            Ok(()) => EncoderResponse::FramePushed,
            Err(e) => EncoderResponse::EncodeFailed(EncoderError::new(e)),
        }
    }

    fn push_video(&mut self, pixels: &[u8]) -> Result<(), String> {
        let s = self.open.as_mut().ok_or("encoder not open")?;
        let v = s.video.as_mut().ok_or("no video stream configured")?;
        let expected = (v.width as usize) * (v.height as usize) * v.bytes_per_pixel;
        if pixels.len() != expected {
            return Err(format!(
                "video frame size mismatch: got {} bytes, expected {} ({}x{} at {} bytes per pixel)",
                pixels.len(),
                expected,
                v.width,
                v.height,
                v.bytes_per_pixel
            ));
        }
        v.stdin
            .as_mut()
            .ok_or("ffmpeg input already closed")?
            .write_all(pixels)
            .map_err(|e| format!("write to ffmpeg stdin: {e}"))?;
        v.frames += 1;
        Ok(())
    }

    /// `bytes`: interleaved little-endian f32 samples.
    fn push_audio(&mut self, bytes: &[u8]) -> Result<(), String> {
        let s = self.open.as_mut().ok_or("encoder not open")?;
        let a = s.audio.as_mut().ok_or("no audio stream configured")?;
        let frame_bytes = 4 * usize::from(a.channels);
        if !bytes.len().is_multiple_of(frame_bytes) {
            return Err(format!(
                "audio push of {} bytes is not a whole number of {}-channel f32 sample frames",
                bytes.len(),
                a.channels
            ));
        }
        let mut pcm = Vec::with_capacity(bytes.len() / 4 * 3);
        for chunk in bytes.chunks_exact(4) {
            let sample = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            let clamped = sample.clamp(-1.0, 1.0);
            let v = (clamped * 8_388_607.0) as i32; // 2^23 - 1
            pcm.extend_from_slice(&v.to_le_bytes()[0..3]);
        }
        a.file
            .write_all(&pcm)
            .map_err(|e| format!("wav write: {e}"))?;
        a.bytes_written += pcm.len() as u64;
        a.samples_written += (bytes.len() / frame_bytes) as u64;
        Ok(())
    }

    /// Finish the open export: complete its partial file and rename it over
    /// the destination. The session's temp and partial files go when it
    /// drops at the end of this, whether it succeeded or not.
    fn finish(&mut self) -> Result<(u64, u64), String> {
        let mut s = self.open.take().ok_or("encoder not open")?;

        // 1. Close the video ffmpeg child and wait.
        let video_frames = match s.video.as_mut() {
            Some(v) => {
                v.finish()?;
                v.frames
            }
            None => 0,
        };

        // 2. Finalise WAV header.
        let audio_samples = match &s.audio {
            Some(a) => {
                finalize_wav_header(a)?;
                a.samples_written
            }
            None => 0,
        };

        // 3. Mux. Re-encode audio to the requested codec at this step
        // so the final container carries the user-chosen format
        // (AAC / FLAC / PCM) instead of the side WAV's PCM_S24.
        let partial = s.partial.path();
        match (&s.video, &s.audio) {
            (Some(v), Some(a)) => run_mux(&v.out_path, a.temp.path(), partial, a.codec)?,
            (None, Some(a)) => run_audio_only_export(a.temp.path(), partial, a.codec)?,
            // The video went straight into the partial file.
            (Some(_), None) => {}
            (None, None) => return Err("nothing was encoded".into()),
        }

        // 4. Into place, replacing any earlier file there: one rename on one
        // filesystem. On Windows std uses `MoveFileExW` with
        // `MOVEFILE_REPLACE_EXISTING` (then a POSIX-semantics rename if that
        // is refused): it replaces the old file too, but fails while another
        // program holds it open without delete sharing. The old file then
        // stays, and so does the finished export, under a visible name.
        if let Err(e) = std::fs::rename(partial, &s.destination) {
            let kept = keep_finished_export(&mut s.partial, &s.destination);
            return Err(format!(
                "could not replace {}: {e}. The finished export is kept as {}",
                s.destination.display(),
                kept.display()
            ));
        }

        Ok((video_frames, audio_samples))
    }
}

fn spawn_video_ffmpeg(
    cfg: &VideoConfig,
    out_path: &Path,
    temp: Option<TempFile>,
) -> Result<VideoPipe, String> {
    // Resolved before spawning so "no usable H.264 / HEVC encoder" fails Open
    // with a clear message instead of a broken pipe on the first frame.
    let choice = video_encoder_choice(cfg, selected_encoder)?;

    let mut child = Command::new("ffmpeg")
        .args(video_ffmpeg_args(cfg, &choice, out_path))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("failed to spawn ffmpeg (is it on PATH?): {e}"))?;
    let stdin = child.stdin.take();
    // Built before checking stdin, so a failure still stops the child.
    let pipe = VideoPipe {
        child,
        exited: false,
        stdin,
        encoder: choice.name,
        width: cfg.width,
        height: cfg.height,
        bytes_per_pixel: cfg.pixel_format.bytes_per_pixel(),
        out_path: out_path.to_path_buf(),
        _temp: temp,
        frames: 0,
    };
    if pipe.stdin.is_none() {
        return Err("ffmpeg stdin unavailable".into());
    }
    Ok(pipe)
}

/// The colour signaling of every SDR export: Rec.709 primaries, transfer and
/// matrix, limited range.
const SDR_SIGNALING: ColorSignaling = ColorSignaling {
    primaries: ColorPrimaries::Bt709,
    transfer: TransferCharacteristics::Bt709,
    matrix: MatrixCoefficients::Bt709,
    range: SignalRange::Limited,
};

/// How an export's RGB frames become the encoder's YUV, and the flags that
/// tag the stream with it.
struct ColorConversion {
    /// The scale filter doing the RGB → YUV conversion, with its matrix and
    /// range set: swscale's own default is BT.601, while players (and our
    /// decoder) read untagged HD video as BT.709, so exports and proxies
    /// played back with shifted colours.
    filter: String,
    /// `-color_primaries … -color_trc … -colorspace … -color_range …`, so
    /// the bitstream and the container say what the filter did.
    tags: Vec<String>,
}

impl ColorConversion {
    /// Rec.709 limited range for SDR; the HDR metadata's signaling (Rec.2020
    /// NCL, PQ / HLG) for HDR.
    fn of(hdr: Option<&HdrMetadata>) -> Self {
        let signaling = hdr.map_or(SDR_SIGNALING, |h| h.signaling);
        // The scale filter's names for the matrices.
        let matrix = match signaling.matrix {
            MatrixCoefficients::Bt709 => "bt709",
            MatrixCoefficients::Bt2020Ncl => "bt2020",
        };
        let (primaries, transfer, space, range) = (
            signaling.primaries.ffmpeg(),
            signaling.transfer.ffmpeg(),
            signaling.matrix.ffmpeg(),
            signaling.range.ffmpeg(),
        );
        // Intentional: the frames are tagged too (`setparams`), not only the
        // encoder. FFmpeg 8's CLI takes an encoder's colour properties from
        // its frames, and `scale` sets only matrix and range there: ProRes
        // came out with its primaries and transfer "unknown".
        ColorConversion {
            filter: format!(
                "scale=out_color_matrix={matrix}:out_range={range},setparams=\
                 color_primaries={primaries}:color_trc={transfer}:colorspace={space}:range={range}"
            ),
            tags: signaling.ffmpeg_args(),
        }
    }

    /// `-vf <conversion>[,<filters>]`: the conversion first, so an encoder's
    /// own filters (VAAPI's upload) get YUV.
    fn filter_args(&self, filters: &[String]) -> [String; 2] {
        let mut chain = vec![self.filter.clone()];
        chain.extend(filters.iter().cloned());
        ["-vf".to_string(), chain.join(",")]
    }
}

/// Full ffmpeg argv for a video encode: raw frames on stdin, encoded into
/// `out_path`. Hardware device setup (`pre_input`) has to precede `-i`.
fn video_ffmpeg_args(
    cfg: &VideoConfig,
    choice: &VideoEncoderChoice,
    out_path: &Path,
) -> Vec<OsString> {
    let size = format!("{}x{}", cfg.width, cfg.height);
    let frame_rate = format!("{}/{}", cfg.frame_rate_num, cfg.frame_rate_den);
    let mut args: Vec<OsString> = ["-hide_banner", "-loglevel", "warning", "-y"]
        .into_iter()
        .map(OsString::from)
        .collect();
    args.extend(choice.pre_input.iter().map(OsString::from));
    for a in [
        "-f",
        "rawvideo",
        "-pix_fmt",
        cfg.pixel_format.ffmpeg_rawvideo(),
        "-s",
        size.as_str(),
        "-framerate",
        frame_rate.as_str(),
        "-i",
        "pipe:0",
    ] {
        args.push(a.into());
    }
    let color = ColorConversion::of(cfg.hdr.as_ref());
    args.extend(color.filter_args(&choice.filters).map(OsString::from));
    args.extend(choice.codec_args.iter().map(OsString::from));
    args.extend(color.tags.iter().map(OsString::from));
    args.push(out_path.into());
    args
}

/// The encoder for `cfg`. HDR10 overrides the preset with a software HEVC
/// Main10 path so the mastering metadata + PQ signaling are emitted reliably
/// (hardware encoders don't accept x265-params); otherwise the preset decides.
/// `select` picks an H.264 / HEVC encoder that works at `cfg`'s frame format.
fn video_encoder_choice(
    cfg: &VideoConfig,
    select: impl FnOnce(VideoCodec, FrameFormat) -> Result<Candidate, String>,
) -> Result<VideoEncoderChoice, String> {
    match &cfg.hdr {
        // ProRes carries HDR itself: 10-bit, tagged with the signaling
        // (ColorConversion).
        Some(hdr) if !cfg.preset.is_prores() => Ok(VideoEncoderChoice {
            name: "libx265",
            pre_input: Vec::new(),
            filters: Vec::new(),
            codec_args: hdr_encode_args(hdr, &cfg.preset),
        }),
        _ => preset_encoder_choice(&cfg.preset, |codec| select(codec, FrameFormat::of(cfg))),
    }
}

/// FFmpeg args for an HDR10 encode: software HEVC Main10 (`libx265` +
/// `yuv420p10le`) with the full HDR10 x265 params (color signaling +
/// mastering display + content light + `hdr10=1`). Replaces the preset
/// entirely so no incompatible option (e.g. a ProRes profile) leaks in. The
/// stream-level signaling flags come with the conversion
/// ([`ColorConversion`]), as for SDR.
fn hdr_encode_args(hdr: &HdrMetadata, preset: &EncoderPreset) -> Vec<String> {
    let v = |s: &str| s.to_string();
    let crf = preset_crf(preset).unwrap_or(18);
    vec![
        v("-c:v"),
        v("libx265"),
        v("-pix_fmt"),
        v("yuv420p10le"),
        v("-profile:v"),
        v("main10"),
        v("-x265-params"),
        hdr.x265_params(crf),
    ]
}

/// Extract a CRF from a quality-based preset (H.264 / HEVC / AV1), else None.
fn preset_crf(preset: &EncoderPreset) -> Option<u8> {
    match preset {
        EncoderPreset::H264 { crf } | EncoderPreset::Hevc { crf } | EncoderPreset::Av1 { crf } => {
            Some(*crf)
        }
        _ => None,
    }
}

/// The encoder for a non-HDR preset. H.264 / HEVC have no single encoder that
/// works everywhere, so they go through `select`; the rest are fixed.
fn preset_encoder_choice(
    preset: &EncoderPreset,
    select: impl FnOnce(VideoCodec) -> Result<Candidate, String>,
) -> Result<VideoEncoderChoice, String> {
    let fixed = |name: &'static str, rest: &[&str]| -> Result<VideoEncoderChoice, String> {
        let mut codec_args = vec!["-c:v".to_string(), name.to_string()];
        codec_args.extend(rest.iter().map(|s| s.to_string()));
        Ok(VideoEncoderChoice {
            name,
            pre_input: Vec::new(),
            filters: Vec::new(),
            codec_args,
        })
    };
    match preset {
        EncoderPreset::ProRes422HQ => {
            fixed("prores_ks", &["-profile:v", "3", "-pix_fmt", "yuv422p10le"])
        }
        EncoderPreset::ProRes422 => {
            fixed("prores_ks", &["-profile:v", "2", "-pix_fmt", "yuv422p10le"])
        }
        EncoderPreset::ProRes422Proxy => {
            fixed("prores_ks", &["-profile:v", "0", "-pix_fmt", "yuv422p10le"])
        }
        EncoderPreset::ProRes4444 => fixed(
            "prores_ks",
            &["-profile:v", "4", "-pix_fmt", "yuva444p10le"],
        ),
        EncoderPreset::DnxhrHq => {
            fixed("dnxhd", &["-profile:v", "dnxhr_hq", "-pix_fmt", "yuv422p"])
        }
        EncoderPreset::DnxhrSq => {
            fixed("dnxhd", &["-profile:v", "dnxhr_sq", "-pix_fmt", "yuv422p"])
        }
        EncoderPreset::DnxhrLb => {
            fixed("dnxhd", &["-profile:v", "dnxhr_lb", "-pix_fmt", "yuv422p"])
        }
        EncoderPreset::H264 { crf } => Ok(select(VideoCodec::H264)?.choice(*crf)),
        EncoderPreset::Hevc { crf } => Ok(select(VideoCodec::Hevc)?.choice(*crf)),
        EncoderPreset::Av1 { crf } => fixed(
            "libsvtav1",
            &["-crf", &crf.to_string(), "-pix_fmt", "yuv420p"],
        ),
    }
}

// ---------------------------------------------------------------------
// Runtime H.264 / HEVC encoder selection
// ---------------------------------------------------------------------

/// The encoder one export runs: `pre_input` goes before `-i` (hardware
/// device setup), `filters` after the colour conversion in its `-vf` chain
/// (see [`ColorConversion`]), `codec_args` after that (codec, quality,
/// pix_fmt).
#[derive(Clone, Debug, PartialEq, Eq)]
struct VideoEncoderChoice {
    name: &'static str,
    pre_input: Vec<String>,
    filters: Vec<String>,
    codec_args: Vec<String>,
}

/// Codecs whose encoder is picked at runtime from a candidate list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum VideoCodec {
    H264,
    Hevc,
}

impl VideoCodec {
    fn label(self) -> &'static str {
        match self {
            VideoCodec::H264 => "H.264",
            VideoCodec::Hevc => "HEVC",
        }
    }

    fn software_encoder(self) -> &'static str {
        match self {
            VideoCodec::H264 => "libx264",
            VideoCodec::Hevc => "libx265",
        }
    }
}

/// The frame size and rate an export encodes at. Hardware encoders have size
/// limits (an M1's h264_videotoolbox refuses frames wider than 4096), so a
/// candidate is probed, and its pick cached, at the export's own format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct FrameFormat {
    width: u32,
    height: u32,
    frame_rate_num: u32,
    frame_rate_den: u32,
}

impl FrameFormat {
    fn of(cfg: &VideoConfig) -> Self {
        FrameFormat {
            width: cfg.width,
            height: cfg.height,
            frame_rate_num: cfg.frame_rate_num,
            frame_rate_den: cfg.frame_rate_den,
        }
    }
}

impl fmt::Display for FrameFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}x{} at {}/{} fps",
            self.width, self.height, self.frame_rate_num, self.frame_rate_den
        )
    }
}

/// How an encoder takes its quality and pixel-format options.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EncoderFamily {
    VideoToolbox,
    Nvenc,
    Qsv,
    Vaapi,
    Amf,
    MediaFoundation,
    /// libx264 / libx265.
    X26x,
    OpenH264,
}

/// One encoder ffmpeg might provide for a codec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Candidate {
    codec: VideoCodec,
    name: &'static str,
    family: EncoderFamily,
}

/// DRM render node the VAAPI candidates open (the first GPU).
const VAAPI_DEVICE: &str = "/dev/dri/renderD128";

/// Encoders to try for `codec` on `os` (a `std::env::consts::OS` value), in
/// order: the platform's hardware encoders, then software.
fn candidates_for(os: &str, codec: VideoCodec) -> Vec<Candidate> {
    use EncoderFamily as F;
    let hardware: &[(&'static str, EncoderFamily)] = match (os, codec) {
        ("macos", VideoCodec::H264) => &[("h264_videotoolbox", F::VideoToolbox)],
        ("macos", VideoCodec::Hevc) => &[("hevc_videotoolbox", F::VideoToolbox)],
        ("linux", VideoCodec::H264) => &[
            ("h264_nvenc", F::Nvenc),
            ("h264_qsv", F::Qsv),
            ("h264_vaapi", F::Vaapi),
        ],
        ("linux", VideoCodec::Hevc) => &[
            ("hevc_nvenc", F::Nvenc),
            ("hevc_qsv", F::Qsv),
            ("hevc_vaapi", F::Vaapi),
        ],
        ("windows", VideoCodec::H264) => &[
            ("h264_nvenc", F::Nvenc),
            ("h264_qsv", F::Qsv),
            ("h264_amf", F::Amf),
            ("h264_mf", F::MediaFoundation),
        ],
        ("windows", VideoCodec::Hevc) => &[
            ("hevc_nvenc", F::Nvenc),
            ("hevc_qsv", F::Qsv),
            ("hevc_amf", F::Amf),
            ("hevc_mf", F::MediaFoundation),
        ],
        _ => &[],
    };
    // Intentional: libx264 / libx265 are GPL. Vance doesn't bundle or link
    // them; it runs the user's installed `ffmpeg` as a separate process, so
    // they're used only when that ffmpeg build already provides them.
    let software: &[(&'static str, EncoderFamily)] = match codec {
        VideoCodec::H264 => &[("libx264", F::X26x), ("libopenh264", F::OpenH264)],
        VideoCodec::Hevc => &[("libx265", F::X26x)],
    };
    hardware
        .iter()
        .chain(software)
        .map(|&(name, family)| Candidate {
            codec,
            name,
            family,
        })
        .collect()
}

impl Candidate {
    /// ffmpeg args for this encoder at a preset CRF (0 best – 51 worst).
    /// Every family outputs 8-bit 4:2:0 (`yuv420p`, or `nv12` where that's
    /// the encoder's native input).
    fn choice(self, crf: u8) -> VideoEncoderChoice {
        use EncoderFamily as F;
        let crf_arg = crf.to_string();
        // QP-style knobs treat 0 as "unset / automatic", so CRF 0 maps to 1.
        let qp = crf.clamp(1, 51).to_string();
        // VideoToolbox and Media Foundation take a 1..=100 quality (higher is
        // better) instead of a CRF.
        let quality = (100 - i32::from(crf) * 2).clamp(1, 100).to_string();
        let bitrate = format!("{}k", openh264_bitrate_kbps(crf));
        let mut pre_input: Vec<&str> = Vec::new();
        let mut filters: Vec<&str> = Vec::new();
        let rest: Vec<&str> = match self.family {
            F::VideoToolbox => vec!["-q:v", &quality, "-pix_fmt", "yuv420p"],
            F::Nvenc => vec!["-rc", "vbr", "-cq", &qp, "-b:v", "0", "-pix_fmt", "yuv420p"],
            F::Qsv => vec!["-global_quality", &qp, "-pix_fmt", "nv12"],
            F::Vaapi => {
                // VAAPI encodes GPU surfaces: open the device before `-i`, then
                // convert + upload each frame. No `-pix_fmt` after the upload.
                pre_input.extend(["-vaapi_device", VAAPI_DEVICE]);
                filters.extend(["format=nv12", "hwupload"]);
                vec!["-qp", &qp]
            }
            F::Amf => {
                let mut a = vec!["-rc", "cqp", "-qp_i", &qp, "-qp_p", &qp];
                // hevc_amf has no B-frame QP option.
                if self.codec == VideoCodec::H264 {
                    a.extend(["-qp_b", &qp]);
                }
                a.extend(["-pix_fmt", "nv12"]);
                a
            }
            F::MediaFoundation => vec![
                "-rate_control",
                "quality",
                "-quality",
                &quality,
                "-pix_fmt",
                "nv12",
            ],
            F::X26x => vec!["-preset", "medium", "-crf", &crf_arg, "-pix_fmt", "yuv420p"],
            F::OpenH264 => vec!["-b:v", &bitrate, "-pix_fmt", "yuv420p"],
        };
        let mut codec_args = vec!["-c:v".to_string(), self.name.to_string()];
        codec_args.extend(rest.into_iter().map(String::from));
        VideoEncoderChoice {
            name: self.name,
            pre_input: pre_input.into_iter().map(String::from).collect(),
            filters: filters.into_iter().map(String::from).collect(),
            codec_args,
        }
    }
}

/// libopenh264 has no CRF / constant-quality mode, so the CRF becomes a
/// target bitrate: 8 Mb/s at CRF 23 (a sensible 1080p rate), doubling every 6
/// CRF steps toward higher quality, as with x264's CRF scale.
fn openh264_bitrate_kbps(crf: u8) -> u32 {
    let kbps = 8000.0 * 2f64.powf((23.0 - f64::from(crf.min(51))) / 6.0);
    (kbps.round() as u32).clamp(500, 50_000)
}

/// The first candidate that this ffmpeg lists (`available`) AND that passes
/// `probe` (a real test encode at `format`): an encoder can be compiled into
/// ffmpeg yet unusable on this machine (no GPU, driver or device node) or at
/// this frame size.
fn select_encoder(
    codec: VideoCodec,
    format: FrameFormat,
    candidates: &[Candidate],
    available: &HashSet<String>,
    mut probe: impl FnMut(&Candidate, FrameFormat) -> bool,
) -> Result<Candidate, String> {
    let mut tried = Vec::with_capacity(candidates.len());
    for c in candidates {
        if !available.contains(c.name) {
            tried.push(format!("{} (not in this ffmpeg build)", c.name));
        } else if probe(c, format) {
            return Ok(*c);
        } else {
            tried.push(format!("{} (test encode failed)", c.name));
        }
    }
    Err(format!(
        "No working {} encoder found for {format} (tried: {}). Install an ffmpeg build with \
         {} or a supported hardware encoder.",
        codec.label(),
        tried.join(", "),
        codec.software_encoder()
    ))
}

/// CRF the probe encodes at. Any in-range value works: the probe checks that
/// the encoder opens and encodes a frame, not its quality.
const PROBE_CRF: u8 = 23;
/// A wedged hardware driver must not hang the export's Open forever. This is
/// the limit for frames up to 4K UHD; see [`probe_timeout`].
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Encoder picks per (codec, frame format). A pick is reused only for the
/// format it was probed at: hardware that handles 1080p can refuse 8K.
struct SelectionCache(Mutex<BTreeMap<(VideoCodec, FrameFormat), Result<Candidate, String>>>);

impl SelectionCache {
    const fn new() -> Self {
        SelectionCache(Mutex::new(BTreeMap::new()))
    }

    /// The cached pick for `codec` at `format`, running `select` on a miss.
    /// Failures are cached too: probing again would fail the same way.
    fn get_or_select(
        &self,
        codec: VideoCodec,
        format: FrameFormat,
        select: impl FnOnce() -> Result<Candidate, String>,
    ) -> Result<Candidate, String> {
        // Intentional: the lock is held across `select` (seconds of probe
        // encodes). The sidecar handles one request at a time, and a second
        // caller for the same format should wait for the pick, not re-probe.
        let mut picks = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        picks.entry((codec, format)).or_insert_with(select).clone()
    }
}

/// The encoder this sidecar uses for `codec` at `format`, chosen at most once
/// per process and format (listing + probe encodes cost real time) and logged
/// once.
fn selected_encoder(codec: VideoCodec, format: FrameFormat) -> Result<Candidate, String> {
    static PICKS: SelectionCache = SelectionCache::new();
    PICKS.get_or_select(codec, format, || {
        let picked = list_ffmpeg_encoders().and_then(|available| {
            let candidates = candidates_for(std::env::consts::OS, codec);
            select_encoder(codec, format, &candidates, &available, probe_encoder)
        });
        match &picked {
            Ok(c) => eprintln!(
                "[encoder] {} encoder for {format}: {}",
                codec.label(),
                c.name
            ),
            Err(e) => eprintln!("[encoder] {e}"),
        }
        picked
    })
}

/// Encoder names compiled into the `ffmpeg` on PATH.
fn list_ffmpeg_encoders() -> Result<HashSet<String>, String> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-encoders"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("failed to run ffmpeg (is it on PATH?): {e}"))?;
    if !out.status.success() {
        return Err(format!("`ffmpeg -encoders` exited with {}", out.status));
    }
    Ok(parse_encoder_list(&String::from_utf8_lossy(&out.stdout)))
}

/// Parse `ffmpeg -encoders`: a flags legend, a ` ------` rule, then one
/// ` V....D name  description` row per encoder.
fn parse_encoder_list(listing: &str) -> HashSet<String> {
    listing
        .lines()
        .skip_while(|l| !l.trim_start().starts_with("---"))
        .skip(1)
        .filter_map(|l| l.split_whitespace().nth(1))
        .map(str::to_owned)
        .collect()
}

fn probe_encoder(candidate: &Candidate, format: FrameFormat) -> bool {
    let args = probe_args(&candidate.choice(PROBE_CRF), format);
    match run_probe(&args, probe_timeout(format)) {
        Ok(()) => true,
        Err(reason) => {
            eprintln!(
                "[encoder] {} unusable at {format}: {reason}",
                candidate.name
            );
            false
        }
    }
}

/// A one-frame test encode with exactly the filters, codec args, colour tags,
/// frame size and rate a real (SDR: selection never runs for HDR) export
/// would use.
fn probe_args(choice: &VideoEncoderChoice, format: FrameFormat) -> Vec<String> {
    let s = |a: &[&str]| a.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let source = format!(
        "color=c=black:s={}x{}:r={}/{}",
        format.width, format.height, format.frame_rate_num, format.frame_rate_den
    );
    let color = ColorConversion::of(None);
    let mut args = s(&["-hide_banner", "-nostdin", "-loglevel", "error"]);
    args.extend(choice.pre_input.iter().cloned());
    args.extend(s(&["-f", "lavfi", "-i", &source, "-frames:v", "1"]));
    args.extend(color.filter_args(&choice.filters));
    args.extend(choice.codec_args.iter().cloned());
    args.extend(color.tags);
    args.extend(s(&["-f", "null", "-"]));
    args
}

/// [`PROBE_TIMEOUT`] scaled by frame area above 4K UHD. A software probe's
/// time grows with the frame (on an M1, one libx265 frame takes ~1.7 s at 8K
/// and ~7 s at 16K), so a fixed limit would fail a slower CPU's legitimate
/// probe and refuse a big export for being big.
fn probe_timeout(format: FrameFormat) -> Duration {
    const UHD_AREA: u128 = 3840 * 2160;
    let area = u128::from(format.width) * u128::from(format.height);
    let ms = PROBE_TIMEOUT.as_millis() * area / UHD_AREA;
    Duration::from_millis(u64::try_from(ms).unwrap_or(u64::MAX)).max(PROBE_TIMEOUT)
}

fn run_probe(args: &[String], timeout: Duration) -> Result<(), String> {
    // stdin / stdout carry this sidecar's IPC frames; the probe must not touch them.
    let mut child = Command::new("ffmpeg")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn ffmpeg: {e}"))?;
    let mut stderr = child.stderr.take();
    // Drained on a thread so a chatty failure can't fill the pipe and stall
    // the child while we poll it.
    let drain = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(pipe) = stderr.as_mut() {
            let _ = pipe.read_to_string(&mut text);
        }
        text
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(format!(
                    "test encode timed out after {}s",
                    timeout.as_secs()
                ));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(format!("waiting for test encode: {e}"));
            }
        }
    };
    let stderr = drain.join().unwrap_or_default();
    let status = status?;
    if status.success() {
        return Ok(());
    }
    let detail = stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no error output");
    Err(format!("test encode exited with {status}: {detail}"))
}

fn open_wav_writer(cfg: &AudioConfig, temp: TempFile) -> Result<AudioPipe, String> {
    use std::fs::OpenOptions;
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(temp.path())
        .map_err(|e| format!("open wav: {e}"))?;
    let mut a = AudioPipe {
        file,
        temp,
        sample_rate: cfg.sample_rate,
        channels: cfg.channels,
        codec: cfg.codec,
        samples_written: 0,
        bytes_written: 0,
    };
    write_wav_header_placeholder(&mut a)?;
    Ok(a)
}

fn write_wav_header_placeholder(a: &mut AudioPipe) -> Result<(), String> {
    let mut hdr = Vec::with_capacity(44);
    hdr.extend_from_slice(b"RIFF");
    hdr.extend_from_slice(&0u32.to_le_bytes());
    hdr.extend_from_slice(b"WAVE");
    hdr.extend_from_slice(b"fmt ");
    hdr.extend_from_slice(&16u32.to_le_bytes());
    hdr.extend_from_slice(&1u16.to_le_bytes()); // PCM
    hdr.extend_from_slice(&(a.channels as u16).to_le_bytes());
    hdr.extend_from_slice(&a.sample_rate.to_le_bytes());
    let byte_rate = a.sample_rate * a.channels as u32 * 3;
    hdr.extend_from_slice(&byte_rate.to_le_bytes());
    let block_align = a.channels as u16 * 3;
    hdr.extend_from_slice(&block_align.to_le_bytes());
    hdr.extend_from_slice(&24u16.to_le_bytes()); // bits per sample
    hdr.extend_from_slice(b"data");
    hdr.extend_from_slice(&0u32.to_le_bytes());
    a.file
        .write_all(&hdr)
        .map_err(|e| format!("wav header: {e}"))
}

fn finalize_wav_header(a: &AudioPipe) -> Result<(), String> {
    use std::io::{Seek, SeekFrom};
    let data_size = a.bytes_written as u32;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(a.temp.path())
        .map_err(|e| format!("open wav for patch: {e}"))?;
    let riff_size = 36 + data_size;
    file.seek(SeekFrom::Start(4))
        .map_err(|e| format!("seek riff: {e}"))?;
    file.write_all(&riff_size.to_le_bytes())
        .map_err(|e| format!("patch riff: {e}"))?;
    file.seek(SeekFrom::Start(40))
        .map_err(|e| format!("seek data: {e}"))?;
    file.write_all(&data_size.to_le_bytes())
        .map_err(|e| format!("patch data: {e}"))?;
    Ok(())
}

fn audio_codec_args(codec: AudioCodec) -> Vec<String> {
    match codec {
        AudioCodec::PcmS16 => vec!["-c:a".into(), "pcm_s16le".into()],
        AudioCodec::PcmS24 => vec!["-c:a".into(), "pcm_s24le".into()],
        AudioCodec::Aac => vec![
            "-c:a".into(),
            "aac".into(),
            // Use a constant-quality VBR mode so output bitrate
            // matches source loudness rather than being padded with
            // silence at a fixed CBR.
            "-vbr".into(),
            "4".into(),
        ],
        AudioCodec::Flac => vec![
            "-c:a".into(),
            "flac".into(),
            "-compression_level".into(),
            "5".into(),
        ],
    }
}

fn run_mux(
    video_path: &Path,
    audio_path: &Path,
    final_path: &Path,
    audio_codec: AudioCodec,
) -> Result<(), String> {
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-hide_banner")
        .arg("-loglevel")
        .arg("warning")
        .arg("-y")
        .arg("-i")
        .arg(video_path)
        .arg("-i")
        .arg(audio_path)
        .arg("-c:v")
        .arg("copy");
    for a in audio_codec_args(audio_codec) {
        cmd.arg(a);
    }
    cmd.arg("-map")
        .arg("0:v:0")
        .arg("-map")
        .arg("1:a:0")
        .arg(final_path);
    let status = cmd.status().map_err(|e| format!("mux spawn: {e}"))?;
    if !status.success() {
        return Err(format!("ffmpeg mux exited with {status:?}"));
    }
    Ok(())
}

fn run_audio_only_export(
    audio_path: &Path,
    final_path: &Path,
    audio_codec: AudioCodec,
) -> Result<(), String> {
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-hide_banner")
        .arg("-loglevel")
        .arg("warning")
        .arg("-y")
        .arg("-i")
        .arg(audio_path);
    for a in audio_codec_args(audio_codec) {
        cmd.arg(a);
    }
    cmd.arg(final_path);
    let status = cmd
        .status()
        .map_err(|e| format!("audio export spawn: {e}"))?;
    if !status.success() {
        return Err(format!("ffmpeg audio export exited with {status:?}"));
    }
    Ok(())
}

fn probe_ffmpeg_version() -> String {
    match Command::new("ffmpeg").arg("-version").output() {
        Ok(out) => {
            let s = String::from_utf8_lossy(&out.stdout);
            s.lines()
                .next()
                .unwrap_or("ffmpeg present, version unknown")
                .to_string()
        }
        Err(_) => "ffmpeg NOT FOUND on PATH".to_string(),
    }
}

fn available_presets() -> Vec<EncoderPreset> {
    vec![
        EncoderPreset::ProRes422HQ,
        EncoderPreset::ProRes422,
        EncoderPreset::ProRes422Proxy,
        EncoderPreset::ProRes4444,
        EncoderPreset::DnxhrHq,
        EncoderPreset::DnxhrSq,
        EncoderPreset::DnxhrLb,
        EncoderPreset::H264 { crf: 23 },
        EncoderPreset::Hevc { crf: 28 },
        EncoderPreset::Av1 { crf: 30 },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use vance_encoder::protocol::VideoPixelFormat;
    use VideoCodec::{Hevc, H264};

    fn listed(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// `width`×`height` at 30 fps.
    fn at(width: u32, height: u32) -> FrameFormat {
        FrameFormat {
            width,
            height,
            frame_rate_num: 30,
            frame_rate_den: 1,
        }
    }

    fn config(preset: EncoderPreset) -> VideoConfig {
        VideoConfig {
            width: 320,
            height: 240,
            frame_rate_num: 24,
            frame_rate_den: 1,
            pixel_format: VideoPixelFormat::Rgba8,
            preset,
            time_base_num: 1,
            time_base_den: 24,
            hdr: None,
        }
    }

    fn candidate(os: &str, codec: VideoCodec, name: &str) -> Candidate {
        candidates_for(os, codec)
            .into_iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("{name} is not a {os} {codec:?} candidate"))
    }

    fn argv(cfg: &VideoConfig, choice: &VideoEncoderChoice) -> Vec<String> {
        video_ffmpeg_args(cfg, choice, Path::new("out.mp4"))
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect()
    }

    #[test]
    fn listed_hardware_that_fails_its_probe_falls_back_to_libx264() {
        let candidates = candidates_for("linux", H264);
        let available = listed(&["h264_vaapi", "libx264", "libopenh264"]);
        let mut probed = Vec::new();
        let picked = select_encoder(H264, at(1920, 1080), &candidates, &available, |c, _| {
            probed.push(c.name);
            c.name != "h264_vaapi"
        })
        .unwrap();
        assert_eq!(picked.name, "libx264");
        // Unlisted encoders are never probed; probing stops at the first success.
        assert_eq!(probed, ["h264_vaapi", "libx264"]);
    }

    #[test]
    fn working_hardware_encoder_wins_over_software() {
        let candidates = candidates_for("linux", H264);
        // NVENC isn't in this build, so the first listed hardware encoder wins.
        let available = listed(&["h264_vaapi", "h264_qsv", "libx264"]);
        let picked =
            select_encoder(H264, at(1920, 1080), &candidates, &available, |_, _| true).unwrap();
        assert_eq!(picked.name, "h264_qsv");
    }

    #[test]
    fn probe_runs_at_the_export_size_so_8k_falls_back_while_1080p_keeps_hardware() {
        let candidates = candidates_for("macos", H264);
        let available = listed(&["h264_videotoolbox", "libx264", "libopenh264"]);
        let (hd, uhd8k) = (at(1920, 1080), at(7680, 4320));
        let mut probed = Vec::new();
        let mut pick = |format| {
            select_encoder(H264, format, &candidates, &available, |c, seen| {
                probed.push((c.name, seen));
                // Like an M1's h264_videotoolbox: nothing wider than 4096.
                c.family != EncoderFamily::VideoToolbox || seen.width <= 4096
            })
            .map(|c| c.name)
        };
        assert_eq!(pick(hd), Ok("h264_videotoolbox"));
        assert_eq!(pick(uhd8k), Ok("libx264"));
        assert_eq!(
            probed,
            [
                ("h264_videotoolbox", hd),
                ("h264_videotoolbox", uhd8k),
                ("libx264", uhd8k)
            ]
        );
    }

    #[test]
    fn video_encoder_choice_selects_at_the_configs_size_and_rate() {
        let mut cfg = config(EncoderPreset::Hevc { crf: 28 });
        (cfg.width, cfg.height) = (7680, 4320);
        (cfg.frame_rate_num, cfg.frame_rate_den) = (30_000, 1001);
        let choice = video_encoder_choice(&cfg, |codec, format| {
            assert_eq!(codec, Hevc);
            assert_eq!(
                format,
                FrameFormat {
                    width: 7680,
                    height: 4320,
                    frame_rate_num: 30_000,
                    frame_rate_den: 1001
                }
            );
            Ok(candidate("macos", Hevc, "libx265"))
        })
        .unwrap();
        assert_eq!(choice.name, "libx265");
    }

    #[test]
    fn selection_cache_never_reuses_a_pick_made_for_another_format() {
        let cache = SelectionCache::new();
        let vt = candidate("macos", H264, "h264_videotoolbox");
        let x264 = candidate("macos", H264, "libx264");
        let hd = at(1920, 1080);
        let cached = || -> Result<Candidate, String> { panic!("re-selected a cached format") };

        assert_eq!(cache.get_or_select(H264, hd, || Ok(vt)), Ok(vt));
        assert_eq!(cache.get_or_select(H264, hd, cached), Ok(vt));
        // Each of these differs from `hd` in one field, so each gets its own
        // selection instead of the 1080p pick.
        for other in [
            at(7680, 4320),
            at(1920, 4320),
            at(7680, 1080),
            FrameFormat {
                frame_rate_num: 240,
                ..hd
            },
            FrameFormat {
                frame_rate_num: 30_000,
                frame_rate_den: 1001,
                ..hd
            },
        ] {
            assert_eq!(cache.get_or_select(H264, other, || Ok(x264)), Ok(x264));
            assert_eq!(cache.get_or_select(H264, other, cached), Ok(x264));
        }
        assert_eq!(cache.get_or_select(H264, hd, cached), Ok(vt));

        // The codec is part of the key, and a failed selection is cached too.
        let none = Err("No working HEVC encoder found".to_string());
        assert_eq!(cache.get_or_select(Hevc, hd, || none.clone()), none);
        assert_eq!(cache.get_or_select(Hevc, hd, cached), none);
        assert_eq!(cache.get_or_select(H264, hd, cached), Ok(vt));
    }

    #[test]
    fn probe_encodes_one_frame_at_the_export_size_and_rate() {
        let choice = candidate("macos", H264, "h264_videotoolbox").choice(PROBE_CRF);
        let format = FrameFormat {
            width: 7680,
            height: 4320,
            frame_rate_num: 30_000,
            frame_rate_den: 1001,
        };
        assert_eq!(
            probe_args(&choice, format),
            [
                "-hide_banner",
                "-nostdin",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=7680x4320:r=30000/1001",
                "-frames:v",
                "1",
                "-vf",
                "scale=out_color_matrix=bt709:out_range=tv,setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709:range=tv",
                "-c:v",
                "h264_videotoolbox",
                "-q:v",
                "54",
                "-pix_fmt",
                "yuv420p",
                "-color_primaries",
                "bt709",
                "-color_trc",
                "bt709",
                "-colorspace",
                "bt709",
                "-color_range",
                "tv",
                "-f",
                "null",
                "-",
            ]
        );

        // The probe's size and rate are the export command line's `-s` / `-framerate`.
        let cfg = config(EncoderPreset::H264 { crf: 23 });
        let export = argv(&cfg, &choice);
        let value_of = |flag: &str| &export[export.iter().position(|a| a == flag).unwrap() + 1];
        let source = format!(
            "color=c=black:s={}:r={}",
            value_of("-s"),
            value_of("-framerate")
        );
        assert_eq!(source, "color=c=black:s=320x240:r=24/1");
        assert!(probe_args(&choice, FrameFormat::of(&cfg)).contains(&source));
    }

    #[test]
    fn probe_timeout_grows_with_frame_area() {
        let secs = |w, h| probe_timeout(at(w, h)).as_secs();
        assert_eq!(secs(256, 256), 15);
        assert_eq!(secs(1920, 1080), 15);
        assert_eq!(secs(3840, 2160), 15);
        assert_eq!(secs(4096, 2160), 16);
        assert_eq!(secs(7680, 4320), 60);
        assert_eq!(secs(8192, 4320), 64);
        assert_eq!(secs(15_360, 8640), 240);
        assert_eq!(secs(0, 0), 15);
        assert!(secs(u32::MAX, u32::MAX) >= 240);
    }

    #[test]
    fn no_working_encoder_is_a_clear_error_naming_what_was_tried() {
        let candidates = candidates_for("linux", H264);
        let err = select_encoder(
            H264,
            at(7680, 4320),
            &candidates,
            &listed(&["h264_vaapi"]),
            |_, _| false,
        )
        .unwrap_err();
        assert!(
            err.starts_with("No working H.264 encoder found for 7680x4320 at 30/1 fps (tried: "),
            "{err}"
        );
        assert!(err.contains("h264_vaapi (test encode failed)"), "{err}");
        for name in ["h264_nvenc", "h264_qsv", "libx264", "libopenh264"] {
            assert!(
                err.contains(&format!("{name} (not in this ffmpeg build)")),
                "{err}"
            );
        }
        assert!(
            err.ends_with("Install an ffmpeg build with libx264 or a supported hardware encoder."),
            "{err}"
        );

        let err = select_encoder(
            Hevc,
            at(1920, 1080),
            &candidates_for("linux", Hevc),
            &listed(&[]),
            |_, _| true,
        )
        .unwrap_err();
        assert!(
            err.starts_with("No working HEVC encoder found for 1920x1080 at 30/1 fps"),
            "{err}"
        );
        assert!(
            err.contains("Install an ffmpeg build with libx265"),
            "{err}"
        );
    }

    #[test]
    fn selection_error_surfaces_through_the_video_encoder_path() {
        let err = video_encoder_choice(&config(EncoderPreset::Hevc { crf: 28 }), |codec, _| {
            assert_eq!(codec, Hevc);
            Err("No working HEVC encoder found (tried: libx265)".to_string())
        })
        .unwrap_err();
        assert!(err.starts_with("No working HEVC encoder found"), "{err}");
    }

    #[test]
    fn vaapi_opens_its_device_before_the_input_and_uploads_frames() {
        let cfg = config(EncoderPreset::H264 { crf: 23 });
        let vaapi = candidate("linux", H264, "h264_vaapi");
        let choice = video_encoder_choice(&cfg, |_, _| Ok(vaapi)).unwrap();
        for args in [
            argv(&cfg, &choice),
            probe_args(&choice, FrameFormat::of(&cfg)),
        ] {
            let pos = |a: &str| {
                args.iter()
                    .position(|x| x == a)
                    .unwrap_or_else(|| panic!("`{a}` missing from {args:?}"))
            };
            let input = pos("-i");
            let device = pos("-vaapi_device");
            assert!(device < input, "{args:?}");
            assert_eq!(args[device + 1], VAAPI_DEVICE);
            let vf = pos("-vf");
            assert!(vf > input, "{args:?}");
            // Converted to BT.709 YUV before the upload.
            assert_eq!(
                args[vf + 1],
                "scale=out_color_matrix=bt709:out_range=tv,setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709:range=tv,format=nv12,hwupload"
            );
            assert_eq!(args[pos("-c:v") + 1], "h264_vaapi");
            assert_eq!(args[pos("-qp") + 1], "23");
            // Frames are VAAPI surfaces after hwupload: no software output pix_fmt.
            assert!(!args[input..].iter().any(|a| a == "-pix_fmt"), "{args:?}");
        }
        assert_eq!(argv(&cfg, &choice).last().unwrap(), "out.mp4");
    }

    #[test]
    fn videotoolbox_args_match_the_previous_hard_coded_ones() {
        for (codec, name) in [(H264, "h264_videotoolbox"), (Hevc, "hevc_videotoolbox")] {
            let vt = candidates_for("macos", codec)[0];
            assert_eq!(vt.name, name);
            for (crf, q) in [
                (0, "100"),
                (18, "64"),
                (23, "54"),
                (28, "44"),
                (51, "1"),
                (255, "1"),
            ] {
                let choice = vt.choice(crf);
                assert!(choice.pre_input.is_empty());
                assert_eq!(
                    choice.codec_args,
                    ["-c:v", name, "-q:v", q, "-pix_fmt", "yuv420p"]
                );
            }
        }
        // And the whole command line keeps its previous layout, plus the
        // BT.709 conversion and its tags.
        let cfg = config(EncoderPreset::H264 { crf: 23 });
        let choice =
            video_encoder_choice(&cfg, |_, _| Ok(candidates_for("macos", H264)[0])).unwrap();
        assert_eq!(
            argv(&cfg, &choice),
            [
                "-hide_banner",
                "-loglevel",
                "warning",
                "-y",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
                "-s",
                "320x240",
                "-framerate",
                "24/1",
                "-i",
                "pipe:0",
                "-vf",
                "scale=out_color_matrix=bt709:out_range=tv,setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709:range=tv",
                "-c:v",
                "h264_videotoolbox",
                "-q:v",
                "54",
                "-pix_fmt",
                "yuv420p",
                "-color_primaries",
                "bt709",
                "-color_trc",
                "bt709",
                "-colorspace",
                "bt709",
                "-color_range",
                "tv",
                "out.mp4",
            ]
        );
    }

    #[test]
    fn temp_file_names_carry_the_pid() {
        assert_eq!(temp_file_pid("vance-enc-123-4-video.mov"), Some(123));
        assert_eq!(temp_file_pid("vance-enc-123-audio.wav"), Some(123));
        for name in [
            "vance-enc-select-123-320x240.mp4",
            "vance-enc--video.mov",
            "vance-enc-12a-video.mov",
            "vance-preset-123-x.mov",
            "vance-enc-123",
        ] {
            assert_eq!(temp_file_pid(name), None, "{name}");
        }
        let t = temp_file(Path::new("tmp"), 7, "video.mov");
        assert_eq!(
            temp_file_pid(&t.path().file_name().unwrap().to_string_lossy()),
            Some(std::process::id())
        );
    }

    /// The startup sweep deletes the files of an encoder that is gone, and
    /// leaves a running one's (this process's own, here) and anything else.
    #[test]
    fn the_sweep_deletes_only_dead_encoders_files() {
        let dir = std::env::temp_dir().join(format!("vance-enc-sweep-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A process that has exited: its pid is free.
        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("--list")
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let dead = child.id();
        child.wait().unwrap();
        assert!(!process_may_be_alive(dead), "pid {dead} still alive");
        assert!(process_may_be_alive(std::process::id()));
        let me = std::process::id();
        let names = [
            format!("vance-enc-{dead}-1-video.mov"),
            format!("vance-enc-{dead}-1-audio.wav"),
            format!("vance-enc-{me}-1-video.mov"),
            format!("vance-preset-{dead}-x.mov"),
        ];
        for name in &names {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        // A directory with an encoder's name is not a temp file.
        std::fs::create_dir_all(dir.join(format!("vance-enc-{dead}-2-dir"))).unwrap();
        sweep_stale_temp_files(&dir);
        let left: Vec<bool> = names.iter().map(|n| dir.join(n).exists()).collect();
        let kept_dir = dir.join(format!("vance-enc-{dead}-2-dir")).exists();

        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(left, [false, false, true, true], "{names:?}");
        assert!(kept_dir);
    }

    /// A partial export is swept only when it was made on this machine, by
    /// a process that is gone, and wasn't written to for 10 minutes: another
    /// machine's export into the same (network) folder, or one written a
    /// moment ago, stays.
    #[test]
    fn the_partial_sweep_takes_only_this_hosts_old_dead_files() {
        let dir =
            std::env::temp_dir().join(format!("vance-enc-partial-sweep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("--list")
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let dead = child.id();
        child.wait().unwrap();
        let (me, host) = (
            std::process::id(),
            host_tag().expect("this machine's host tag"),
        );
        let other = format!("{:08x}", fnv1a(b"another machine"));
        let old = std::time::SystemTime::now() - Duration::from_secs(11 * 60);
        let cases = [
            (format!(".out.vance-partial-{host}-{dead}.mov"), old, false),
            (
                format!(".new.vance-partial-{host}-{dead}.mov"),
                std::time::SystemTime::now(),
                true,
            ),
            (format!(".out.vance-partial-{other}-{dead}.mov"), old, true),
            (format!(".mine.vance-partial-{host}-{me}.mov"), old, true),
            (format!(".pidonly.vance-partial-{dead}.mov"), old, true),
            ("out.mov".to_string(), old, true),
        ];
        for (name, modified, _) in &cases {
            let file = std::fs::File::create(dir.join(name)).unwrap();
            file.set_modified(*modified).unwrap();
        }
        sweep_stale_partials(&dir);
        let left: Vec<(String, bool)> = cases
            .iter()
            .map(|(n, ..)| (n.clone(), dir.join(n).exists()))
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        let want: Vec<(String, bool)> = cases
            .iter()
            .map(|(n, _, keep)| (n.clone(), *keep))
            .collect();
        assert_eq!(left, want);
    }

    /// An export's partial file: hidden, next to the destination, with its
    /// real extension (ffmpeg picks the container by it) even for a name
    /// that is only an extension, and its host and pid readable back.
    #[test]
    fn partial_files_sit_hidden_next_to_the_destination() {
        let (pid, host) = (std::process::id(), host_tag().unwrap());
        let tag = format!("vance-partial-{host}-{pid}");
        for (destination, partial) in [
            ("/exports/out.mov", format!("/exports/.out.{tag}.mov")),
            ("/exports/a.b.mp4", format!("/exports/.a.b.{tag}.mp4")),
            ("/exports/plain", format!("/exports/.plain.{tag}")),
            ("/exports/.mp4", format!("/exports/..{tag}.mp4")),
            (
                "/exports/.hidden.mov",
                format!("/exports/..hidden.{tag}.mov"),
            ),
            ("relative.mkv", format!(".relative.{tag}.mkv")),
        ] {
            let got = partial_path(Path::new(destination)).unwrap();
            assert_eq!(got, PathBuf::from(&partial), "{destination}");
            let name = got.file_name().unwrap().to_str().unwrap();
            assert_eq!(partial_file_owner(name), Some((host, pid)), "{name}");
        }
        for destination in ["/", ".."] {
            assert!(
                partial_path(Path::new(destination)).is_err(),
                "{destination}"
            );
        }
        for name in [
            ".out.vance-partial-h-.mov",
            ".out.vance-partial-h-12x.mov",
            ".out.vance-partial--12.mov",
            ".out.vance-partial-12.mov",
            "out.vance-partial-h-12.mov",
            ".out.mov",
        ] {
            assert_eq!(partial_file_owner(name), None, "{name}");
        }
        assert_eq!(
            partial_file_owner(".x.vance-partial-a-5.y.vance-partial-b-7.mov"),
            Some(("b", 7))
        );
    }

    /// Names near the 255-byte limit: the made names shorten the stem (on a
    /// character boundary, with a hash of it so long stems stay apart) and
    /// keep the extension whole.
    #[test]
    fn made_names_fit_the_file_name_limit() {
        let (pid, host) = (std::process::id(), host_tag().unwrap());
        for stem in [
            "a".repeat(250),
            "é".repeat(125),
            format!("x{}", "ü".repeat(124)),
        ] {
            let destination = PathBuf::from(format!("/exports/{stem}.mov"));
            for made in [
                partial_path(&destination).unwrap(),
                destination.with_file_name(kept_name(&destination, 3).unwrap()),
            ] {
                let name = made.file_name().unwrap().to_str().unwrap().to_string();
                assert!(name.len() <= MAX_NAME_BYTES, "{} bytes: {name}", name.len());
                assert!(name.ends_with(".mov"), "{name}");
                assert!(
                    name.contains(&format!("~{:08x}", fnv1a(stem.as_bytes()))),
                    "{name}"
                );
            }
            let partial = partial_path(&destination).unwrap();
            let name = partial.file_name().unwrap().to_str().unwrap();
            assert_eq!(partial_file_owner(name), Some((host, pid)));
        }
        // Two long stems that differ only at the end stay apart.
        let a = partial_path(Path::new(&format!("/e/{}a.mov", "s".repeat(240)))).unwrap();
        let b = partial_path(Path::new(&format!("/e/{}b.mov", "s".repeat(240)))).unwrap();
        assert_ne!(a, b);
        // Short names are left alone.
        assert_eq!(
            kept_name(Path::new("/e/out.mov"), 1).unwrap(),
            "out (not replaced).mov"
        );
        assert_eq!(
            kept_name(Path::new("/e/.mp4"), 2).unwrap(),
            "export (not replaced 2).mp4"
        );
    }

    /// Every export converts RGB to YUV with an explicit matrix and range,
    /// and tags the stream with them: BT.709 limited for SDR, the HDR
    /// metadata's BT.2020 NCL for HDR. Fixed presets and HDR alike.
    #[test]
    fn every_export_converts_with_its_tagged_matrix() {
        let value_of = |args: &[String], flag: &str| -> String {
            let i = args.iter().position(|a| a == flag);
            i.map(|i| args[i + 1].clone())
                .unwrap_or_else(|| panic!("no {flag} in {args:?}"))
        };
        let mut configs: Vec<VideoConfig> = [
            EncoderPreset::ProRes422HQ,
            EncoderPreset::ProRes4444,
            EncoderPreset::DnxhrLb,
            EncoderPreset::Av1 { crf: 30 },
            EncoderPreset::H264 { crf: 23 },
        ]
        .into_iter()
        .map(config)
        .collect();
        let mut hdr = config(EncoderPreset::Hevc { crf: 20 });
        hdr.hdr = Some(HdrMetadata::hdr10(1000.0, 0.0001, 1000, 400));
        configs.push(hdr);
        for cfg in configs {
            let choice =
                video_encoder_choice(&cfg, |_, _| Ok(candidates_for("macos", H264)[0])).unwrap();
            let args = argv(&cfg, &choice);
            let what = format!("{:?} hdr={}", cfg.preset, cfg.hdr.is_some());
            let (matrix, tags) = if cfg.hdr.is_some() {
                ("bt2020", ["bt2020", "smpte2084", "bt2020nc", "tv"])
            } else {
                ("bt709", ["bt709", "bt709", "bt709", "tv"])
            };
            let [primaries, transfer, space, range] = tags;
            assert_eq!(
                value_of(&args, "-vf"),
                format!(
                    "scale=out_color_matrix={matrix}:out_range=tv,setparams=color_primaries=\
                     {primaries}:color_trc={transfer}:colorspace={space}:range={range}"
                ),
                "{what}"
            );
            for (flag, want) in [
                "-color_primaries",
                "-color_trc",
                "-colorspace",
                "-color_range",
            ]
            .into_iter()
            .zip(tags)
            {
                assert_eq!(value_of(&args, flag), want, "{what}: {flag}");
                // Once: HDR's flags aren't repeated by its codec args.
                assert_eq!(
                    args.iter().filter(|a| *a == flag).count(),
                    1,
                    "{what}: {flag}"
                );
            }
        }
    }

    #[test]
    fn candidate_order_per_platform() {
        let names = |os: &str, codec| -> Vec<&str> {
            candidates_for(os, codec).iter().map(|c| c.name).collect()
        };
        assert_eq!(
            names("macos", H264),
            ["h264_videotoolbox", "libx264", "libopenh264"]
        );
        assert_eq!(
            names("linux", H264),
            [
                "h264_nvenc",
                "h264_qsv",
                "h264_vaapi",
                "libx264",
                "libopenh264"
            ]
        );
        assert_eq!(
            names("windows", H264),
            [
                "h264_nvenc",
                "h264_qsv",
                "h264_amf",
                "h264_mf",
                "libx264",
                "libopenh264"
            ]
        );
        assert_eq!(names("macos", Hevc), ["hevc_videotoolbox", "libx265"]);
        assert_eq!(
            names("linux", Hevc),
            ["hevc_nvenc", "hevc_qsv", "hevc_vaapi", "libx265"]
        );
        assert_eq!(
            names("windows", Hevc),
            ["hevc_nvenc", "hevc_qsv", "hevc_amf", "hevc_mf", "libx265"]
        );
        assert_eq!(names("freebsd", H264), ["libx264", "libopenh264"]);
        assert_eq!(names("freebsd", Hevc), ["libx265"]);
    }

    #[test]
    fn crf_maps_onto_each_encoders_quality_controls() {
        let args = |os, codec, name, crf| candidate(os, codec, name).choice(crf).codec_args;
        assert_eq!(
            args("linux", H264, "h264_nvenc", 23),
            [
                "-c:v",
                "h264_nvenc",
                "-rc",
                "vbr",
                "-cq",
                "23",
                "-b:v",
                "0",
                "-pix_fmt",
                "yuv420p"
            ]
        );
        assert_eq!(
            args("linux", Hevc, "hevc_qsv", 28),
            [
                "-c:v",
                "hevc_qsv",
                "-global_quality",
                "28",
                "-pix_fmt",
                "nv12"
            ]
        );
        assert_eq!(
            args("windows", H264, "h264_amf", 23),
            [
                "-c:v", "h264_amf", "-rc", "cqp", "-qp_i", "23", "-qp_p", "23", "-qp_b", "23",
                "-pix_fmt", "nv12",
            ]
        );
        assert_eq!(
            args("windows", Hevc, "hevc_amf", 23),
            ["-c:v", "hevc_amf", "-rc", "cqp", "-qp_i", "23", "-qp_p", "23", "-pix_fmt", "nv12"]
        );
        assert_eq!(
            args("windows", H264, "h264_mf", 23),
            [
                "-c:v",
                "h264_mf",
                "-rate_control",
                "quality",
                "-quality",
                "54",
                "-pix_fmt",
                "nv12"
            ]
        );
        assert_eq!(
            args("linux", H264, "libx264", 23),
            ["-c:v", "libx264", "-preset", "medium", "-crf", "23", "-pix_fmt", "yuv420p"]
        );
        assert_eq!(
            args("linux", Hevc, "libx265", 28),
            ["-c:v", "libx265", "-preset", "medium", "-crf", "28", "-pix_fmt", "yuv420p"]
        );
        assert_eq!(
            args("linux", H264, "libopenh264", 23),
            [
                "-c:v",
                "libopenh264",
                "-b:v",
                "8000k",
                "-pix_fmt",
                "yuv420p"
            ]
        );
        // CRF 0 must not land on the "0 = unset / automatic" value of QP knobs.
        assert_eq!(args("linux", H264, "h264_nvenc", 0)[5], "1");
        assert_eq!(args("linux", H264, "h264_qsv", 0)[3], "1");
        assert_eq!(args("linux", H264, "h264_vaapi", 0).last().unwrap(), "1");
        // openh264's bitrate stays bounded at the CRF extremes.
        assert_eq!(openh264_bitrate_kbps(0), 50_000);
        assert_eq!(openh264_bitrate_kbps(51), 500);
        assert_eq!(openh264_bitrate_kbps(255), 500);
    }

    #[test]
    fn every_candidate_outputs_8_bit_420() {
        for os in ["macos", "linux", "windows", "freebsd"] {
            for codec in [H264, Hevc] {
                for c in candidates_for(os, codec) {
                    let choice = c.choice(23);
                    let args = &choice.codec_args;
                    assert_eq!(args[..2], ["-c:v", c.name]);
                    let has = |k: &str, v: &str| args.windows(2).any(|w| w[0] == k && w[1] == v);
                    assert!(
                        has("-pix_fmt", "yuv420p")
                            || has("-pix_fmt", "nv12")
                            || choice.filters == ["format=nv12", "hwupload"],
                        "{}: {choice:?}",
                        c.name
                    );
                }
            }
        }
    }

    #[test]
    fn fixed_presets_keep_their_args_and_never_run_selection() {
        let cases: [(EncoderPreset, &[&str]); 8] = [
            (
                EncoderPreset::ProRes422Proxy,
                &[
                    "-c:v",
                    "prores_ks",
                    "-profile:v",
                    "0",
                    "-pix_fmt",
                    "yuv422p10le",
                ],
            ),
            (
                EncoderPreset::ProRes422HQ,
                &[
                    "-c:v",
                    "prores_ks",
                    "-profile:v",
                    "3",
                    "-pix_fmt",
                    "yuv422p10le",
                ],
            ),
            (
                EncoderPreset::ProRes422,
                &[
                    "-c:v",
                    "prores_ks",
                    "-profile:v",
                    "2",
                    "-pix_fmt",
                    "yuv422p10le",
                ],
            ),
            (
                EncoderPreset::ProRes4444,
                &[
                    "-c:v",
                    "prores_ks",
                    "-profile:v",
                    "4",
                    "-pix_fmt",
                    "yuva444p10le",
                ],
            ),
            (
                EncoderPreset::DnxhrHq,
                &[
                    "-c:v",
                    "dnxhd",
                    "-profile:v",
                    "dnxhr_hq",
                    "-pix_fmt",
                    "yuv422p",
                ],
            ),
            (
                EncoderPreset::DnxhrSq,
                &[
                    "-c:v",
                    "dnxhd",
                    "-profile:v",
                    "dnxhr_sq",
                    "-pix_fmt",
                    "yuv422p",
                ],
            ),
            (
                EncoderPreset::DnxhrLb,
                &[
                    "-c:v",
                    "dnxhd",
                    "-profile:v",
                    "dnxhr_lb",
                    "-pix_fmt",
                    "yuv422p",
                ],
            ),
            (
                EncoderPreset::Av1 { crf: 30 },
                &["-c:v", "libsvtav1", "-crf", "30", "-pix_fmt", "yuv420p"],
            ),
        ];
        for (preset, expected) in cases {
            let choice = video_encoder_choice(&config(preset.clone()), |_, _| {
                panic!("{preset:?} must not run encoder selection")
            })
            .unwrap();
            assert!(choice.pre_input.is_empty());
            assert_eq!(choice.codec_args, expected);
        }
    }

    #[test]
    fn hdr_uses_software_hevc_main10_without_selection_unless_prores() {
        for preset in [
            EncoderPreset::H264 { crf: 20 },
            EncoderPreset::Hevc { crf: 20 },
            EncoderPreset::DnxhrHq,
            EncoderPreset::Av1 { crf: 30 },
        ] {
            let mut cfg = config(preset);
            cfg.hdr = Some(HdrMetadata::hdr10(1000.0, 0.0001, 1000, 400));
            let choice =
                video_encoder_choice(&cfg, |_, _| panic!("HDR must not run encoder selection"))
                    .unwrap();
            assert!(choice.pre_input.is_empty());
            assert_eq!(
                choice.codec_args,
                hdr_encode_args(cfg.hdr.as_ref().unwrap(), &cfg.preset)
            );
            assert_eq!(
                choice.codec_args[..4],
                ["-c:v", "libx265", "-pix_fmt", "yuv420p10le"]
            );
        }
    }

    /// HDR with a ProRes preset stays that ProRes, 10-bit, converted with
    /// BT.2020 NCL and tagged with the HDR signaling (PQ, or HLG).
    #[test]
    fn hdr_prores_stays_prores_tagged_with_its_signaling() {
        let mut hlg = HdrMetadata::hdr10(1000.0, 0.0001, 1000, 400);
        hlg.signaling.transfer = TransferCharacteristics::Hlg;
        for (preset, profile, pix_fmt) in [
            (EncoderPreset::ProRes422Proxy, "0", "yuv422p10le"),
            (EncoderPreset::ProRes422, "2", "yuv422p10le"),
            (EncoderPreset::ProRes422HQ, "3", "yuv422p10le"),
            (EncoderPreset::ProRes4444, "4", "yuva444p10le"),
        ] {
            for (hdr, transfer) in [
                (HdrMetadata::hdr10(1000.0, 0.0001, 1000, 400), "smpte2084"),
                (hlg, "arib-std-b67"),
            ] {
                let mut cfg = config(preset.clone());
                cfg.pixel_format = VideoPixelFormat::Rgba64Le;
                cfg.hdr = Some(hdr);
                let choice = video_encoder_choice(&cfg, |_, _| panic!("no selection")).unwrap();
                assert_eq!(
                    choice.codec_args,
                    [
                        "-c:v",
                        "prores_ks",
                        "-profile:v",
                        profile,
                        "-pix_fmt",
                        pix_fmt
                    ],
                    "{preset:?}"
                );
                let args = argv(&cfg, &choice);
                let joined = args.join(" ");
                assert!(
                    joined.contains(&format!(
                        "-vf scale=out_color_matrix=bt2020:out_range=tv,setparams=\
                         color_primaries=bt2020:color_trc={transfer}:colorspace=bt2020nc:range=tv"
                    )),
                    "{preset:?}: {joined}"
                );
                assert!(
                    joined.contains(&format!(
                        "-color_primaries bt2020 -color_trc {transfer} -colorspace bt2020nc \
                         -color_range tv"
                    )),
                    "{preset:?}: {joined}"
                );
                assert!(!joined.contains("x265"), "{preset:?}: {joined}");
            }
        }
    }

    #[test]
    fn parses_the_ffmpeg_encoder_listing() {
        let listing = "Encoders:\n V..... = Video\n A..... = Audio\n .....D = Supports direct \
                       rendering method 1\n ------\n V....D libx264              libx264 \
                       H.264 / AVC (codec h264)\n V....D h264_videotoolbox    VideoToolbox \
                       H.264 Encoder (codec h264)\n A....D aac                  AAC (Advanced \
                       Audio Coding)\n";
        assert_eq!(
            parse_encoder_list(listing),
            listed(&["libx264", "h264_videotoolbox", "aac"])
        );
    }

    fn ffmpeg_on_path() -> bool {
        Command::new("ffmpeg").arg("-version").output().is_ok()
    }

    /// Opens `cfg` the way Open does (encoder selection included), pushes
    /// `frames` frames and returns the encoder used + the output size in bytes.
    fn real_encode(cfg: &VideoConfig, frames: u8) -> (&'static str, u64) {
        let out = std::env::temp_dir().join(format!(
            "vance-enc-select-{}-{}x{}.mp4",
            std::process::id(),
            cfg.width,
            cfg.height
        ));
        let _ = std::fs::remove_file(&out);
        // The pipe owns the output as its temp file: deleted when it drops.
        let temp = Some(TempFile(out.clone()));
        let mut pipe = spawn_video_ffmpeg(cfg, &out, temp).expect("spawn ffmpeg");
        let pixels = cfg.width as usize * cfg.height as usize;
        let mut frame = vec![0u8; pixels * cfg.pixel_format.bytes_per_pixel()];
        let mut written = Ok(());
        for f in 0..frames {
            // Bands of flat color: some content, and cheap to fill at 8K.
            for (i, band) in frame.chunks_mut(4096).enumerate() {
                band.fill((i as u8) ^ f.wrapping_mul(37));
            }
            written = pipe.stdin.as_mut().expect("stdin").write_all(&frame);
            if written.is_err() {
                break;
            }
        }
        let finished = pipe.finish();
        let len = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        let encoder = pipe.encoder;
        drop(pipe);
        assert!(!out.exists(), "the pipe's temp file outlived it");
        assert!(
            written.is_ok() && finished.is_ok(),
            "{encoder} encode at {} failed: write {written:?}, {finished:?}",
            FrameFormat::of(cfg)
        );
        assert!(len > 1_000, "{encoder} wrote only {len} bytes");
        (encoder, len)
    }

    /// Real ffmpeg on this machine: selection must find a working H.264
    /// encoder, and a short encode through the export's own argv must succeed.
    #[test]
    fn real_ffmpeg_selects_a_working_h264_encoder_and_encodes() {
        if !ffmpeg_on_path() {
            eprintln!("ffmpeg not on PATH — skipping real encoder selection test");
            return;
        }
        let cfg = config(EncoderPreset::H264 { crf: 23 });
        let format = FrameFormat::of(&cfg);
        let picked = selected_encoder(H264, format).expect("a working H.264 encoder");
        assert!(candidates_for(std::env::consts::OS, H264).contains(&picked));
        // Cached for the process: a second call returns the same pick.
        assert_eq!(selected_encoder(H264, format), Ok(picked));
        assert_eq!(real_encode(&cfg, 8).0, picked.name);
    }

    /// Real ffmpeg at 8K, which many hardware H.264 encoders refuse (an M1's
    /// h264_videotoolbox passes a small probe, then can't open at 8K): the
    /// export must get an encoder that works at its own size.
    #[test]
    fn real_ffmpeg_8k_h264_export_gets_an_encoder_that_handles_8k() {
        if !ffmpeg_on_path() {
            eprintln!("ffmpeg not on PATH — skipping real 8K encoder selection test");
            return;
        }
        let mut cfg = config(EncoderPreset::H264 { crf: 23 });
        (cfg.width, cfg.height) = (7680, 4320);
        let (encoder, len) = real_encode(&cfg, 2);
        eprintln!("8K H.264 export used {encoder} ({len} bytes)");
    }
}
