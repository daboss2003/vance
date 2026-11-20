//! Vance build automation. Run `cargo xtask <subcommand>`.
//!
//! See PLAN.md §13 for the full list of subcommands and CONTRIBUTING.md for
//! the developer workflow.

use std::env;
use std::path::PathBuf;
use std::process::{exit, Command, ExitStatus};

const HELP: &str = r#"Vance build automation.

USAGE:
    cargo xtask <SUBCOMMAND>

SUBCOMMANDS:
    check       Type-check all Rust crates (no codegen — fast, small on disk).
    build       Build all Rust crates (workspace, all targets).
    test        Run all Rust tests. `cargo xtask test -p <crate>` tests just
                that crate (the everyday loop; run the full suite per milestone).

    check / build / test / clippy / bench are disk-guarded: they refuse to
    start, or stop cargo, when free disk space drops below 5 GB
    (override with VANCE_MIN_FREE_GB). Before starting they also trim
    cargo's incremental caches (oldest first) once those pass 4 GB.
    prune       Delete ALL incremental caches under target/ (safe; the next
                build recompiles each crate once). Run it with no build active.

    fmt         Format all Rust code.
    fmt-check   Check Rust formatting without modifying files.
    clippy      Lint all Rust code (warnings = errors).
    bench       Run benchmarks (criterion).
    dist        Build distribution artifacts (Phase 11).
    stage-sidecars
                Build the release sidecar binaries and copy them, named with
                the target triple, into crates/vance-app/binaries/ so Tauri's
                `externalBin` can bundle them. Pass `--target <triple>` to
                cross-compile; defaults to the host triple.
    bundle      Stage sidecars, then run `cargo tauri build` with the release
                bundle overlay (tauri.bundle.conf.json) to produce installers.
                Extra args (e.g. `--target <triple>`, `--bundles dmg`) pass
                through. Requires the Tauri CLI (`cargo install tauri-cli`).
    ui-dev      Run the Tauri dev shell with hot reload.
    ui-build    Production build of the frontend.
    ui-fmt      Format frontend code (prettier).
    ui-lint     Lint frontend code (eslint + svelte-check).
    app-build   Build the frontend AND rebuild vance-app so the Tauri
                binary embeds the latest UI bundle. ALWAYS use this
                after changing files under ui/.
    app-run     Same as app-build, then launch ./target/debug/vance-app.
    check-deps  Verify the internal crate dependency graph stays a DAG (PLAN.md §6.1).
    verify-grading-schema
                Run the GradingCommand JSON Schema snapshot test (Phase 6.13).
                Fails on any drift between the Rust enum and the committed
                schema/grading_command.schema.json so the AI tool-use spec
                and the engine contract can't silently desync.
    build-plugins
                Build the sample WASM plugins under plugins/ for the
                wasm32-unknown-unknown target (they're excluded from the
                host workspace build).
    help        Print this message.
"#;

fn main() {
    let mut args = env::args().skip(1);
    let cmd = args.next().unwrap_or_else(|| "help".to_string());
    let rest: Vec<String> = args.collect();

    let result = match cmd.as_str() {
        "check" => cargo(&["check", "--workspace", "--all-targets"], &rest),
        "prune" => {
            let freed = prune_incremental(&target_dir(), 0);
            eprintln!(
                "xtask: removed incremental caches, freed {:.1} GB",
                freed as f64 / GIB as f64
            );
            Ok(())
        }
        "build" => cargo(&["build", "--workspace", "--all-targets"], &rest),
        "test" => test(&rest),
        "fmt" => cargo(&["fmt", "--all"], &rest),
        "fmt-check" => cargo(&["fmt", "--all", "--check"], &rest),
        "clippy" => cargo(
            &[
                "clippy",
                "--workspace",
                "--all-targets",
                // Report every crate's lints in one run instead of stopping
                // at the first crate that fails (CI then shows them all).
                "--keep-going",
                "--",
                "-D",
                "warnings",
            ],
            &rest,
        ),
        "bench" => cargo(&["bench", "--workspace"], &rest),
        "dist" => {
            eprintln!("xtask: dist not implemented yet (lands in Phase 11)");
            Ok(())
        }
        "ui-dev" => pnpm(&["dev"], &rest),
        "ui-build" => pnpm(&["build"], &rest),
        "ui-fmt" => pnpm(&["fmt"], &rest),
        "ui-lint" => pnpm(&["lint"], &rest),
        "app-build" => app_build(),
        "app-run" => app_build().and_then(|_| cargo(&["run", "--package", "vance-app"], &rest)),
        "check-deps" => check_deps(),
        "verify-grading-schema" => cargo(
            &[
                "test",
                "--package",
                "vance-grading-cmd",
                "--test",
                "schema_snapshot",
                "--",
                "--exact",
                "schema_matches_committed_snapshot",
            ],
            &rest,
        ),
        "build-plugins" => build_plugins(),
        "stage-sidecars" => stage_sidecars(&rest),
        "bundle" => bundle(&rest),
        "help" | "--help" | "-h" => {
            print!("{HELP}");
            return;
        }
        other => {
            eprintln!("xtask: unknown subcommand: {other}\n\n{HELP}");
            exit(2);
        }
    };

    if let Err(e) = result {
        eprintln!("xtask: {e}");
        exit(1);
    }
}

fn workspace_root() -> PathBuf {
    // xtask binary lives at <root>/target/<profile>/xtask, so two parents up
    // from `CARGO_MANIFEST_DIR` (= <root>/xtask) gives us the workspace root.
    let manifest_dir = env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .expect("CARGO_MANIFEST_DIR not set");
    manifest_dir
        .parent()
        .expect("xtask manifest has no parent")
        .to_path_buf()
}

fn cargo(args: &[&str], extra: &[String]) -> Result<(), String> {
    let mut c = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".to_string()));
    c.current_dir(workspace_root())
        .args(scope_to_packages(args, extra))
        .args(extra);
    run_disk_guarded(c)
}

/// Tests spawn real binaries from `target/debug` — the decoder/encoder/audio
/// sidecars and `vance-ipc`'s `echo-sidecar` example — and `cargo test` does
/// not put them there. Without this step those tests fail or silently skip.
/// Intentional: always the workspace's bins, even for `-p <crate>`, because a
/// crate's tests need sidecars from OTHER crates (vance-app needs
/// vance-decoder). Cargo no-ops when they're already up to date.
fn test(rest: &[String]) -> Result<(), String> {
    cargo(&["build", "--workspace", "--bins", "--examples"], &[])?;
    // Intentional: --no-fail-fast — one failing crate must not hide the
    // results of every crate after it.
    cargo(
        &["test", "--workspace", "--all-targets", "--no-fail-fast"],
        rest,
    )
}

/// `cargo xtask test -p foo` must test only `foo`: drop the default
/// `--workspace` when the caller names packages (before any `--`).
fn scope_to_packages<'a>(args: &[&'a str], extra: &[String]) -> Vec<&'a str> {
    let names_package = extra
        .iter()
        .take_while(|a| a.as_str() != "--")
        .any(|a| a == "-p" || a == "--package" || a.starts_with("--package="));
    args.iter()
        .copied()
        .filter(|a| !(names_package && *a == "--workspace"))
        .collect()
}

const DEFAULT_MIN_FREE_GB: u64 = 5;

const GIB: u64 = 1024 * 1024 * 1024;

/// Above this, the guarded commands trim incremental caches back to half.
/// Cargo keeps one ~300 MB cache per crate per build variant (lib, bin,
/// test, clippy, each feature set) and never deletes stale ones; across a
/// few variants of vance-app they reached 8.6 GB.
const INCREMENTAL_LIMIT: u64 = 4 * GIB;

fn target_dir() -> PathBuf {
    env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("target"))
}

/// One incremental cache: `target/<…>/incremental/<crate>-<hash>`.
struct CacheDir {
    path: PathBuf,
    bytes: u64,
    modified: std::time::SystemTime,
}

/// Which caches to delete so the total drops to at most `limit / 2`,
/// oldest-modified first. Nothing is deleted while the total is within
/// `limit`; `limit == 0` deletes everything.
fn caches_to_prune(caches: &[CacheDir], limit: u64) -> Vec<usize> {
    let total: u64 = caches.iter().map(|c| c.bytes).sum();
    if limit > 0 && total <= limit {
        return Vec::new();
    }
    let target = limit / 2;
    let mut order: Vec<usize> = (0..caches.len()).collect();
    order.sort_by_key(|&i| caches[i].modified);
    let mut remaining = total;
    let mut doomed = Vec::new();
    for i in order {
        if limit > 0 && remaining <= target {
            break;
        }
        remaining -= caches[i].bytes;
        doomed.push(i);
    }
    doomed
}

/// Trim incremental caches under `target` (see [`caches_to_prune`]); returns
/// the bytes freed. Intentional: deleting the OLDEST first means a build
/// running in another terminal, which keeps touching its own cache, is the
/// last thing to lose it, and losing it only costs that crate a recompile.
fn prune_incremental(target: &std::path::Path, limit: u64) -> u64 {
    let mut caches = Vec::new();
    for dir in incremental_dirs(target) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            caches.push(CacheDir {
                bytes: dir_bytes(&path),
                path,
                modified,
            });
        }
    }
    let mut freed = 0;
    for i in caches_to_prune(&caches, limit) {
        if std::fs::remove_dir_all(&caches[i].path).is_ok() {
            freed += caches[i].bytes;
        }
    }
    freed
}

/// `target/<profile>/incremental` and `target/<triple>/<profile>/incremental`.
fn incremental_dirs(target: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(level1) = std::fs::read_dir(target) else {
        return out;
    };
    for a in level1.flatten() {
        let a = a.path();
        if a.join("incremental").is_dir() {
            out.push(a.join("incremental"));
        }
        if let Ok(level2) = std::fs::read_dir(&a) {
            for b in level2.flatten() {
                let inc = b.path().join("incremental");
                if inc.is_dir() {
                    out.push(inc);
                }
            }
        }
    }
    out
}

fn dir_bytes(path: &std::path::Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    std::fs::read_dir(path)
        .map(|entries| entries.flatten().map(|e| dir_bytes(&e.path())).sum())
        .unwrap_or(0)
}

fn min_free_gb() -> u64 {
    env::var("VANCE_MIN_FREE_GB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MIN_FREE_GB)
}

/// Free space (KiB) on the volume holding `path`, via POSIX `df -Pk`.
#[cfg(unix)]
fn free_kib(path: &std::path::Path) -> Option<u64> {
    let out = Command::new("df").arg("-Pk").arg(path).output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    // Line 2, column 4 = "Available" in POSIX (-P) output.
    text.lines().nth(1)?.split_whitespace().nth(3)?.parse().ok()
}

/// Free space (KiB) available to this user on the volume holding `path`.
#[cfg(windows)]
fn free_kib(path: &std::path::Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    // kernel32 is always linked by std on Windows, so this needs no new crate.
    #[link(name = "kernel32")]
    extern "system" {
        fn GetDiskFreeSpaceExW(
            directory: *const u16,
            free_bytes_available_to_caller: *mut u64,
            total_bytes: *mut u64,
            total_free_bytes: *mut u64,
        ) -> i32;
    }
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut available = 0u64;
    // SAFETY: `wide` is NUL-terminated and outlives the call; the two
    // optional out-pointers may be null per the Win32 contract.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(available / 1024)
}

#[cfg(not(any(unix, windows)))]
fn free_kib(_path: &std::path::Path) -> Option<u64> {
    None
}

/// Every descendant of `pid` (children, grandchildren …), deepest last.
#[cfg(unix)]
fn descendants(pid: u32) -> Vec<u32> {
    let mut found = Vec::new();
    let mut frontier = vec![pid];
    while let Some(p) = frontier.pop() {
        let Ok(out) = Command::new("pgrep").args(["-P", &p.to_string()]).output() else {
            continue;
        };
        for child in String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .filter_map(|s| s.parse::<u32>().ok())
        {
            found.push(child);
            frontier.push(child);
        }
    }
    found
}

/// Stop `child` and everything it started: cargo, rustc, linkers, build
/// scripts. Stopping only cargo would let in-flight rustc/linker processes
/// keep writing to the full disk.
fn kill_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // Intentional: signal the process tree explicitly rather than give
        // cargo its own process group — cargo must stay in ours so Ctrl+C
        // in the terminal still reaches the whole build.
        let mut pids = descendants(child.id());
        pids.push(child.id());
        let pids: Vec<String> = pids.iter().map(u32::to_string).collect();
        let _ = Command::new("kill").arg("-TERM").args(&pids).status();
    }
    #[cfg(windows)]
    {
        let pid = child.id().to_string();
        let _ = Command::new("taskkill")
            .args(["/PID", &pid, "/T", "/F"])
            .status();
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = child.kill();
    }
    let _ = child.wait();
}

/// Run cargo, but refuse to start — or stop it mid-build — when the disk
/// is nearly full. A full disk mid-build corrupts the target dir and can
/// destabilise the whole machine, so failing early is the safer default.
fn run_disk_guarded(mut cmd: Command) -> Result<(), String> {
    use std::time::Duration;
    let root = workspace_root();
    let min_gb = min_free_gb();
    let min_kib = min_gb * 1024 * 1024;
    let too_low = |free: u64| {
        format!(
            "only {:.1} GB free on the build volume (minimum {min_gb} GB). \
             Free some space (`cargo xtask prune` first, `cargo clean` if \
             that isn't enough) or set VANCE_MIN_FREE_GB.",
            free as f64 / (1024.0 * 1024.0)
        )
    };
    let freed = prune_incremental(&target_dir(), INCREMENTAL_LIMIT);
    if freed > 0 {
        eprintln!(
            "xtask: trimmed stale incremental caches, freed {:.1} GB",
            freed as f64 / GIB as f64
        );
    }
    match free_kib(&root) {
        Some(free) if free < min_kib => {
            return Err(format!("refusing to start: {}", too_low(free)));
        }
        Some(_) => {}
        None => eprintln!(
            "xtask: warning: can't read free disk space here; \
             the disk guard is OFF for this run"
        ),
    }
    eprintln!("xtask: running {cmd:?}");
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn {cmd:?}: {e}"))?;
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return exit_ok(status, &cmd);
        }
        std::thread::sleep(Duration::from_secs(2));
        let Some(free) = free_kib(&root) else {
            continue;
        };
        if free < min_kib {
            kill_tree(&mut child);
            return Err(format!("stopped the build: {}", too_low(free)));
        }
    }
}

fn exit_ok(status: ExitStatus, cmd: &Command) -> Result<(), String> {
    if !status.success() {
        return Err(format!(
            "command failed (exit {}): {cmd:?}",
            status.code().unwrap_or(-1)
        ));
    }
    Ok(())
}

/// Build every sample WASM plugin under `plugins/` for
/// `wasm32-unknown-unknown`. They're excluded from the host workspace
/// (they only target wasm), so this builds each by manifest path.
fn build_plugins() -> Result<(), String> {
    let root = workspace_root();
    let plugins_dir = root.join("plugins");
    if !plugins_dir.exists() {
        return Err(format!("plugins/ not found at {}", plugins_dir.display()));
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&plugins_dir)
        .map_err(|e| format!("read plugins/: {e}"))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("Cargo.toml").exists())
        .collect();
    dirs.sort();
    if dirs.is_empty() {
        return Err("no plugin crates found under plugins/".into());
    }
    for dir in &dirs {
        let mut c = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".to_string()));
        c.current_dir(&root).args([
            "build",
            "--release",
            "--target",
            "wasm32-unknown-unknown",
            "--manifest-path",
        ]);
        c.arg(dir.join("Cargo.toml"));
        run(c)?;
    }

    // Stage the built plugins next to where the app binary runs, so the
    // co-located `<exe-dir>/plugins` scan root (see Engine::rescan_plugins)
    // discovers the default pack with no manual install. Cover both host
    // profiles so `cargo run`/`build` (debug) and packaged (release) find them.
    let stage_dirs = [
        root.join("target/debug/plugins"),
        root.join("target/release/plugins"),
    ];
    for d in &stage_dirs {
        let _ = std::fs::create_dir_all(d);
    }
    let mut staged = 0usize;
    for dir in &dirs {
        let rel = dir.join("target/wasm32-unknown-unknown/release");
        let Ok(rd) = std::fs::read_dir(&rel) else {
            continue;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().and_then(|x| x.to_str()) != Some("wasm") {
                continue;
            }
            if let Some(name) = p.file_name() {
                for d in &stage_dirs {
                    let _ = std::fs::copy(&p, d.join(name));
                }
                staged += 1;
            }
        }
    }
    eprintln!(
        "xtask: built {} WASM plugin(s); staged {staged} into target/{{debug,release}}/plugins",
        dirs.len()
    );
    Ok(())
}

/// The out-of-process sidecars bundled next to `vance-app`: (cargo package,
/// binary name). The audio crate's package name differs from its binary.
const SIDECARS: &[(&str, &str)] = &[
    ("vance-decoder", "vance-decoder"),
    ("vance-encoder", "vance-encoder"),
    ("vance-audio", "vance-audio-decoder"),
    ("vance-plugin-host", "vance-plugin-host"),
];

/// The host target triple, parsed from `rustc -vV`.
fn host_triple() -> Result<String, String> {
    let out = Command::new("rustc")
        .arg("-vV")
        .output()
        .map_err(|e| format!("running rustc -vV: {e}"))?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .map(|s| s.trim().to_string())
        .ok_or_else(|| "could not parse host triple from `rustc -vV`".to_string())
}

/// Pull an optional target out of the passthrough args, accepting every
/// spelling `cargo`/`tauri` do: `--target <t>`, `--target=<t>`, `-t <t>`,
/// `-t=<t>`. Returning the wrong answer here silently desyncs the staged
/// suffix from the one Tauri later resolves, so match them all.
fn explicit_target(rest: &[String]) -> Option<String> {
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if let Some(t) = a
            .strip_prefix("--target=")
            .or_else(|| a.strip_prefix("-t="))
        {
            return Some(t.to_string());
        }
        if a == "--target" || a == "-t" {
            return it.next().cloned();
        }
    }
    None
}

/// Build the release sidecars and stage them, target-triple-suffixed, under
/// `crates/vance-app/binaries/` so Tauri's `externalBin` can bundle them.
fn stage_sidecars(rest: &[String]) -> Result<(), String> {
    let root = workspace_root();
    let target = explicit_target(rest);
    let triple = match &target {
        Some(t) => t.clone(),
        None => host_triple()?,
    };
    let dest_dir = root.join("crates/vance-app/binaries");
    std::fs::create_dir_all(&dest_dir)
        .map_err(|e| format!("creating {}: {e}", dest_dir.display()))?;

    // `universal-apple-darwin` is a Tauri pseudo-target: cargo can't build it
    // directly, so build both arches and `lipo`-fold each sidecar into a fat
    // binary under the universal suffix Tauri's externalBin expects.
    if triple == "universal-apple-darwin" {
        return stage_universal(&root, &dest_dir);
    }

    let ext = if triple.contains("windows") {
        ".exe"
    } else {
        ""
    };
    // Build every sidecar in one release invocation (add --target only when the
    // caller asked to cross-compile, so a host build stays in target/release).
    let mut args: Vec<String> = vec!["build".into(), "--release".into()];
    for (pkg, _) in SIDECARS {
        args.push("-p".into());
        args.push((*pkg).into());
    }
    if let Some(t) = &target {
        args.push("--target".into());
        args.push(t.clone());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    cargo(&arg_refs, &[])?;

    // Locate the built binaries and copy them with the triple suffix.
    let profile_dir = match &target {
        Some(t) => root.join("target").join(t).join("release"),
        None => root.join("target").join("release"),
    };
    for (_, bin) in SIDECARS {
        let src = profile_dir.join(format!("{bin}{ext}"));
        if !src.exists() {
            return Err(format!(
                "sidecar not found after build: {} (did the build succeed for this target?)",
                src.display()
            ));
        }
        let dst = dest_dir.join(format!("{bin}-{triple}{ext}"));
        copy_executable(&src, &dst)?;
        eprintln!("xtask: staged {}", dst.display());
    }
    eprintln!(
        "xtask: staged {} sidecar(s) for {triple} → {}",
        SIDECARS.len(),
        dest_dir.display()
    );
    Ok(())
}

/// Build both macOS arches and `lipo`-fold each sidecar into a universal binary
/// staged under the `-universal-apple-darwin` suffix.
fn stage_universal(root: &std::path::Path, dest_dir: &std::path::Path) -> Result<(), String> {
    const ARCHES: [&str; 2] = ["x86_64-apple-darwin", "aarch64-apple-darwin"];
    for arch in ARCHES {
        let mut args: Vec<String> = vec!["build".into(), "--release".into()];
        for (pkg, _) in SIDECARS {
            args.push("-p".into());
            args.push((*pkg).into());
        }
        args.push("--target".into());
        args.push(arch.into());
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        cargo(&arg_refs, &[])?;
    }
    for (_, bin) in SIDECARS {
        let inputs: Vec<PathBuf> = ARCHES
            .iter()
            .map(|a| root.join("target").join(a).join("release").join(bin))
            .collect();
        for src in &inputs {
            if !src.exists() {
                return Err(format!("sidecar not found after build: {}", src.display()));
            }
        }
        let dst = dest_dir.join(format!("{bin}-universal-apple-darwin"));
        let mut lipo = Command::new("lipo");
        lipo.arg("-create").arg("-output").arg(&dst).args(&inputs);
        run(lipo)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755))
                .map_err(|e| format!("chmod {}: {e}", dst.display()))?;
        }
        eprintln!("xtask: staged {} (universal)", dst.display());
    }
    eprintln!(
        "xtask: staged {} universal sidecar(s) → {}",
        SIDECARS.len(),
        dest_dir.display()
    );
    Ok(())
}

/// Copy a built binary to `dst` and mark it executable (unix).
fn copy_executable(src: &std::path::Path, dst: &std::path::Path) -> Result<(), String> {
    std::fs::copy(src, dst)
        .map_err(|e| format!("copying {} -> {}: {e}", src.display(), dst.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dst, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod {}: {e}", dst.display()))?;
    }
    Ok(())
}

/// Stage sidecars, then run `cargo tauri build` with the release bundle overlay
/// to produce platform installers. Extra args pass through to `tauri build`.
fn bundle(rest: &[String]) -> Result<(), String> {
    // Fail early with an actionable message if the Tauri CLI isn't installed.
    let has_cli = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["tauri", "--version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !has_cli {
        return Err(
            "the Tauri CLI is required for `bundle`. Install it with \
             `cargo install tauri-cli --version '^2'` (or `npm i -g @tauri-apps/cli`), then retry."
                .to_string(),
        );
    }

    stage_sidecars(rest)?;

    // Build the frontend deterministically here (the overlay disables Tauri's
    // beforeBuildCommand, whose relative path is CWD-sensitive). `frontendDist`
    // in the base config points at ui/build, which this produces.
    pnpm(&["build"], &[])?;

    // Run from the app crate dir so `tauri build` finds tauri.conf.json and the
    // overlay's relative paths (binaries/, macos/, windows/) resolve.
    let app_dir = workspace_root().join("crates/vance-app");
    let mut c = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    c.current_dir(&app_dir)
        .args(["tauri", "build", "--config", "tauri.bundle.conf.json"])
        .args(rest);
    run(c)
}

fn pnpm(args: &[&str], extra: &[String]) -> Result<(), String> {
    let ui_dir = workspace_root().join("ui");
    if !ui_dir.exists() {
        return Err(format!("ui/ does not exist at {}", ui_dir.display()));
    }
    // On Windows pnpm is a corepack/npm `.cmd` shim; std's Command doesn't apply
    // PATHEXT search, so name the shim explicitly. (Rust ≥1.77 auto-escapes args
    // for `.cmd`/`.bat`, so this is safe.)
    let prog = if cfg!(windows) { "pnpm.cmd" } else { "pnpm" };
    let mut c = Command::new(prog);
    c.current_dir(&ui_dir).args(args).args(extra);
    run(c)
}

fn run(mut cmd: Command) -> Result<(), String> {
    eprintln!("xtask: running {cmd:?}");
    let status: ExitStatus = cmd
        .status()
        .map_err(|e| format!("failed to spawn {cmd:?}: {e}"))?;
    exit_ok(status, &cmd)
}

/// Build the frontend AND rebuild vance-app. The Tauri binary embeds the
/// frontend at compile time via `tauri::generate_context!()`; UI rebuilds
/// alone don't take effect until the binary recompiles. A `touch` on
/// tauri.conf.json forces cargo to invalidate the relevant build artifacts.
fn app_build() -> Result<(), String> {
    pnpm(&["build"], &[])?;
    let conf = workspace_root().join("crates/vance-app/tauri.conf.json");
    let now = std::time::SystemTime::now();
    if let Err(e) = std::fs::OpenOptions::new()
        .write(true)
        .open(&conf)
        .and_then(|f| f.set_modified(now))
    {
        // Non-fatal — the conf may already be newer than the build artifact.
        eprintln!("xtask: warning: failed to touch {}: {e}", conf.display());
    }
    cargo(&["build", "--package", "vance-app"], &[])
}

/// PLAN.md §6.1: enforce the internal crate dependency rules.
/// For Phase 0 this is a stub (no internal deps yet). Real checks land as
/// crates start depending on each other.
fn check_deps() -> Result<(), String> {
    let crates_dir = workspace_root().join("crates");
    if !crates_dir.exists() {
        return Err("crates/ not found".to_string());
    }
    let count = std::fs::read_dir(&crates_dir)
        .map_err(|e| format!("read crates/: {e}"))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .count();
    eprintln!("xtask: check-deps: {count} crates discovered (no rules to enforce yet in Phase 0)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn workspace_root_is_a_real_directory() {
        let root = workspace_root();
        assert!(root.is_dir(), "workspace root not a directory: {root:?}");
        assert!(
            root.join("Cargo.toml").is_file(),
            "workspace Cargo.toml missing"
        );
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn naming_a_package_drops_workspace() {
        let base = ["test", "--workspace", "--all-targets"];
        assert_eq!(
            scope_to_packages(&base, &s(&["-p", "vance-timeline"])),
            ["test", "--all-targets"]
        );
        assert_eq!(
            scope_to_packages(&base, &s(&["--package=vance-timeline"])),
            ["test", "--all-targets"]
        );
    }

    #[test]
    fn workspace_kept_without_package_or_after_double_dash() {
        let base = ["test", "--workspace", "--all-targets"];
        assert_eq!(scope_to_packages(&base, &[]), base);
        // `-p` after `--` is a test-binary arg, not a cargo package filter.
        assert_eq!(scope_to_packages(&base, &s(&["--", "-p"])), base);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn free_space_is_readable() {
        let free = free_kib(&workspace_root()).expect("free space should be readable");
        assert!(free > 0);
    }

    #[cfg(unix)]
    #[test]
    fn kill_tree_stops_grandchildren_too() {
        use std::time::{Duration, Instant};
        let alive = |pid: u32| {
            Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        // sh -> sh -> two sleeps. The sleeps are GRANDchildren, like a linker
        // under rustc under cargo; killing only direct children misses them.
        let mut child = Command::new("sh")
            .args(["-c", "sh -c 'sleep 30 & sleep 30 & wait' & wait"])
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut kids = descendants(child.id());
        while kids.len() < 3 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
            kids = descendants(child.id());
        }
        assert!(
            kids.len() >= 3,
            "shells never forked their sleeps: {kids:?}"
        );

        kill_tree(&mut child);

        let deadline = Instant::now() + Duration::from_secs(5);
        while kids.iter().any(|&p| alive(p)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let survivors: Vec<_> = kids.iter().filter(|&&p| alive(p)).collect();
        assert!(survivors.is_empty(), "descendants survived: {survivors:?}");
    }

    fn cache(bytes: u64, age_secs: u64) -> CacheDir {
        CacheDir {
            path: PathBuf::new(),
            bytes,
            modified: std::time::SystemTime::now() - std::time::Duration::from_secs(age_secs),
        }
    }

    #[test]
    fn caches_within_the_limit_are_kept() {
        let caches = [cache(3 * GIB, 30), cache(GIB, 10)];
        assert!(caches_to_prune(&caches, 4 * GIB).is_empty());
    }

    #[test]
    fn over_the_limit_trims_oldest_first_down_to_half() {
        // 6 GiB total, limit 4 GiB → trim to ≤ 2 GiB: drop the two oldest.
        let caches = [
            cache(GIB, 5),
            cache(2 * GIB, 50),
            cache(GIB, 1),
            cache(2 * GIB, 40),
        ];
        let mut doomed = caches_to_prune(&caches, 4 * GIB);
        doomed.sort();
        assert_eq!(doomed, vec![1, 3]);
    }

    #[test]
    fn limit_zero_prunes_everything() {
        let caches = [cache(1, 5), cache(2, 1)];
        assert_eq!(caches_to_prune(&caches, 0).len(), 2);
    }

    #[test]
    fn prune_removes_incremental_dirs_and_reports_bytes() {
        let target = std::env::temp_dir().join(format!("xtask-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&target);
        let host = target.join("debug/incremental/vance_app-abc");
        let cross = target.join("x86_64-unknown-linux-gnu/debug/incremental/vance_ipc-def");
        let keep = target.join("debug/deps");
        for d in [&host, &cross, &keep] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(host.join("a.o"), vec![0u8; 1000]).unwrap();
        std::fs::write(cross.join("b.o"), vec![0u8; 500]).unwrap();
        std::fs::write(keep.join("lib.rlib"), vec![0u8; 10]).unwrap();

        assert_eq!(prune_incremental(&target, 0), 1500);
        assert!(!host.exists() && !cross.exists());
        assert!(keep.join("lib.rlib").exists(), "only incremental caches go");
        std::fs::remove_dir_all(&target).unwrap();
    }
}
