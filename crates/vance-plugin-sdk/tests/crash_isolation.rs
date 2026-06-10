//! Phase 10.2 — crash isolation + load-time validation.
//!
//! Uses hand-written WAT modules (parsed via the `wat` crate) so
//! these run without the wasm32 guest toolchain. Each module
//! implements just enough of the ABI to reach the failure mode
//! under test, proving the host turns guest faults into `Err`
//! values rather than host panics/hangs.

#![cfg(feature = "host")]

use vance_plugin_sdk::host::{PluginError, WasmPlugin};

/// `WasmPlugin` isn't `Debug` (it owns a wasmtime `Store`), so
/// `Result::unwrap_err` doesn't apply. This helper extracts the
/// error or panics.
fn expect_load_err(bytes: &[u8]) -> PluginError {
    match WasmPlugin::load(bytes) {
        Ok(_) => panic!("expected load to fail"),
        Err(e) => e,
    }
}

/// A minimal valid manifest stored in a data segment at offset 1024.
/// `params:[]`, kind Effect. Length is computed by the helper below.
const MANIFEST: &str =
    r#"PluginManifest(id:"t",name:"T",version:"1",author:"a",kind:Effect,params:[])"#;

/// Build a WAT module that implements the full ABI but runs `body`
/// inside `vance_process`. `body` is raw WAT instructions; it should
/// leave an i32 result on the stack (the process return code) unless
/// it traps.
fn wat_plugin(process_body: &str) -> Vec<u8> {
    // Byte length is what the host reads back; the `"` escaping below
    // is only for the WAT source, not the stored bytes.
    let len = MANIFEST.len();
    let escaped = MANIFEST.replace('"', "\\\"");
    let wat = format!(
        r#"(module
  (memory (export "memory") 8)
  (global $bump (mut i32) (i32.const 65536))
  (data (i32.const 1024) "{manifest}")
  (func (export "vance_abi_version") (result i32) (i32.const 1))
  (func (export "vance_alloc") (param $len i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $bump))
    (global.set $bump (i32.add (global.get $bump) (local.get $len)))
    (local.get $p))
  (func (export "vance_dealloc") (param i32) (param i32))
  (func (export "vance_manifest") (result i64)
    (i64.or
      (i64.shl (i64.const 1024) (i64.const 32))
      (i64.const {len})))
  (func (export "vance_process")
        (param i32) (param i32) (param i32) (param i32) (param i32) (result i32)
    {body}))
"#,
        manifest = escaped,
        len = len,
        body = process_body,
    );
    wat::parse_str(&wat).expect("valid wat")
}

#[test]
fn manifest_loads_from_hand_written_wat() {
    // Sanity: the harness module itself loads + parses its manifest.
    let bytes = wat_plugin("(i32.const 0)");
    let plugin = WasmPlugin::load(&bytes).expect("load");
    assert_eq!(plugin.manifest().id, "t");
}

#[test]
fn guest_trap_becomes_err_not_panic() {
    // `unreachable` traps immediately.
    let bytes = wat_plugin("unreachable");
    let mut plugin = WasmPlugin::load(&bytes).expect("load");
    let mut frame = vec![0u8; 2 * 2 * 4];
    let err = plugin.process(&mut frame, 2, 2, &[]).unwrap_err();
    assert!(matches!(err, PluginError::Trap(_)), "got {err:?}");
    // Host still usable: a second call also returns Err, no panic.
    let err2 = plugin.process(&mut frame, 2, 2, &[]).unwrap_err();
    assert!(matches!(err2, PluginError::Trap(_)));
}

#[test]
fn nonzero_return_code_is_err() {
    // Return 7 (≠ 0) → host surfaces a Trap error carrying the code.
    let bytes = wat_plugin("(i32.const 7)");
    let mut plugin = WasmPlugin::load(&bytes).expect("load");
    let mut frame = vec![0u8; 2 * 2 * 4];
    let err = plugin.process(&mut frame, 2, 2, &[]).unwrap_err();
    assert!(matches!(err, PluginError::Trap(msg) if msg.contains('7')));
}

#[test]
fn infinite_loop_is_bounded_by_fuel() {
    // `loop ... br 0` spins forever; fuel must cut it off and surface
    // a Trap rather than hanging the test runner.
    let bytes = wat_plugin("(loop $l (br $l)) (i32.const 0)");
    let mut plugin = WasmPlugin::load(&bytes).expect("load");
    let mut frame = vec![0u8; 2 * 2 * 4];
    let err = plugin.process(&mut frame, 2, 2, &[]).unwrap_err();
    assert!(matches!(err, PluginError::Trap(_)), "expected fuel trap, got {err:?}");
}

#[test]
fn missing_memory_export_is_rejected() {
    let wat = r#"(module
        (func (export "vance_abi_version") (result i32) (i32.const 1)))"#;
    let bytes = wat::parse_str(wat).unwrap();
    let err = expect_load_err(&bytes);
    assert!(matches!(err, PluginError::MissingExport(_)), "got {err:?}");
}

#[test]
fn abi_version_mismatch_is_rejected() {
    let len = MANIFEST.len();
    let escaped = MANIFEST.replace('"', "\\\"");
    let wat = format!(
        r#"(module
        (memory (export "memory") 2)
        (data (i32.const 1024) "{escaped}")
        (func (export "vance_abi_version") (result i32) (i32.const 99))
        (func (export "vance_alloc") (param i32) (result i32) (i32.const 65536))
        (func (export "vance_dealloc") (param i32) (param i32))
        (func (export "vance_manifest") (result i64)
            (i64.or (i64.shl (i64.const 1024) (i64.const 32)) (i64.const {len}))))"#,
    );
    let bytes = wat::parse_str(&wat).unwrap();
    let err = expect_load_err(&bytes);
    assert!(matches!(err, PluginError::AbiMismatch { plugin: 99, .. }), "got {err:?}");
}

#[test]
fn garbage_bytes_fail_to_load() {
    let err = expect_load_err(b"not a wasm module");
    assert!(matches!(err, PluginError::Load(_)));
}

/// Review finding #4 — a guest that returns a huge manifest length
/// must be rejected at load, NOT allocate gigabytes on the host heap.
#[test]
fn oversized_manifest_length_is_rejected_without_huge_alloc() {
    // vance_manifest returns (ptr=1024, len=0x7FFF_FFFF ≈ 2 GiB). A
    // pre-fix host would `vec![0u8; 2GiB]`; the fixed host rejects it
    // up front. The data segment is tiny — we never actually read it.
    let wat = r#"(module
        (memory (export "memory") 2)
        (data (i32.const 1024) "x")
        (func (export "vance_abi_version") (result i32) (i32.const 1))
        (func (export "vance_alloc") (param i32) (result i32) (i32.const 65536))
        (func (export "vance_dealloc") (param i32) (param i32))
        (func (export "vance_manifest") (result i64)
            (i64.or (i64.shl (i64.const 1024) (i64.const 32)) (i64.const 0x7fffffff))))"#;
    let bytes = wat::parse_str(wat).unwrap();
    let err = expect_load_err(&bytes);
    // Surfaces as a Manifest error (length cap exceeded), fast.
    assert!(matches!(err, PluginError::Manifest(_)), "got {err:?}");
}

/// Review finding #6 — an effect call whose buffer exceeds the frame
/// cap is rejected as FrameSize, never marshalled through a truncated
/// i32 length.
#[test]
fn oversized_frame_is_rejected() {
    let bytes = wat_plugin("(i32.const 0)");
    let mut plugin = WasmPlugin::load(&bytes).expect("load");
    // Claim a resolution whose byte length overflows i32 (46341² * 4 >
    // i32::MAX). The buffer length won't actually match, but the cap
    // check fires first / alongside the size mismatch.
    let w = 46_341u32;
    let h = 46_341u32;
    // Don't allocate the real buffer (that'd be 8 GiB) — pass a small
    // slice; the w*h*4 mismatch + cap both trigger FrameSize.
    let mut tiny = vec![0u8; 16];
    let err = plugin.process(&mut tiny, w, h, &[]).unwrap_err();
    assert!(matches!(err, PluginError::FrameSize { .. }), "got {err:?}");
}
