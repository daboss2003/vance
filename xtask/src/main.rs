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
    ui-dev      Run the Tauri dev shell with hot reload.
    ui-build    Production build of the frontend.
    ui-fmt      Format frontend code (prettier).
    ui-lint     Lint frontend code (eslint + svelte-check).
    check-deps  Verify the internal crate dependency graph stays a DAG (PLAN.md §6.1).
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
        "check-deps" => check_deps(),
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

fn pnpm(args: &[&str], extra: &[String]) -> Result<(), String> {
    let ui_dir = workspace_root().join("ui");
    if !ui_dir.exists() {
        return Err(format!("ui/ does not exist at {}", ui_dir.display()));
    }
    let mut c = Command::new("pnpm");
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
