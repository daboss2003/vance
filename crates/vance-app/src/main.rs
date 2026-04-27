//! Vance — Tauri 2 host process.
//!
//! Phase 0 scope: open a Tauri window that loads the SvelteKit chrome and
//! responds to a single `backend_version` command. Wgpu viewport, decoder
//! sidecar, AI bridge, etc. land in later phases per PLAN.md §7.

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

#[tauri::command]
fn backend_version() -> String {
    format!(
        "vance-app {} (rust {})",
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_RUST_VERSION"),
    )
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![backend_version])
        .run(tauri::generate_context!())
        .expect("tauri failed to start");
}
