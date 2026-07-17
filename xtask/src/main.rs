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
    build       Build all Rust crates (workspace, all targets).
    test        Run all Rust tests.
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
        "build" => cargo(&["build", "--workspace", "--all-targets"], &rest),
        "test" => cargo(&["test", "--workspace", "--all-targets"], &rest),
        "fmt" => cargo(&["fmt", "--all"], &rest),
        "fmt-check" => cargo(&["fmt", "--all", "--check"], &rest),
        "clippy" => cargo(
            &[
                "clippy",
                "--workspace",
                "--all-targets",
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
    c.current_dir(workspace_root()).args(args).args(extra);
    run(c)
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
    eprintln!(
        "xtask: built {} sample WASM plugin(s) for wasm32-unknown-unknown",
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
        if let Some(t) = a.strip_prefix("--target=").or_else(|| a.strip_prefix("-t=")) {
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

    let ext = if triple.contains("windows") { ".exe" } else { "" };
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
    if !status.success() {
        return Err(format!(
            "command failed (exit {}): {cmd:?}",
            status.code().unwrap_or(-1)
        ));
    }
    Ok(())
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
}
