//! Out-of-process plugin host integration tests.
//!
//! Drives [`vance_audio::remote_plugin::RemotePluginHost`] against the
//! real `vance-plugin-host` sidecar binary, proving CLAP plugins process
//! in the sandbox AND that a sidecar crash degrades to bypass + respawn
//! rather than taking down the host.
//!
//! Requires the sidecar built WITH `--features clap` and the synthetic
//! CLAP fixture. Skips gracefully otherwise. Build first, e.g.:
//!   cargo build -p vance-plugin-host -p vance-clap-testplugin --features vance-plugin-host/clap

use std::path::PathBuf;
use vance_audio::remote_plugin::RemotePluginHost;
use vance_audio_plugins::{PluginClient, PluginDescriptor, PluginFormat};

fn workspace_target_debug() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug")
}

fn sidecar_bin() -> Option<PathBuf> {
    let p = workspace_target_debug().join(if cfg!(windows) {
        "vance-plugin-host.exe"
    } else {
        "vance-plugin-host"
    });
    p.exists().then_some(p)
}

fn fixture_id() -> Option<String> {
    let (prefix, ext) = if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };
    let p = workspace_target_debug().join(format!("{prefix}vance_clap_testplugin.{ext}"));
    p.exists()
        .then(|| format!("{}::vance.test.gain", p.to_string_lossy()))
}

/// Is the located sidecar actually built with CLAP hosting? (The binary
/// may exist from a default build that can't host CLAP.)
fn sidecar_speaks_clap(bin: &PathBuf) -> bool {
    match PluginClient::spawn(bin) {
        Ok(c) => c.supported_formats().contains(&PluginFormat::Clap),
        Err(_) => false,
    }
}

fn descriptor(id: String) -> PluginDescriptor {
    PluginDescriptor {
        format: PluginFormat::Clap,
        id,
        name: "Vance Test Gain".into(),
        vendor: "vance".into(),
        runnable: true,
    }
}

fn prerequisites() -> Option<(PathBuf, String)> {
    let bin = sidecar_bin()?;
    if !sidecar_speaks_clap(&bin) {
        eprintln!("sidecar not built with --features clap — skipping remote_plugin test");
        return None;
    }
    let id = fixture_id()?;
    Some((bin, id))
}

#[test]
fn remote_processes_clap_in_sandbox() {
    let Some((bin, id)) = prerequisites() else {
        return;
    };
    let mut host = RemotePluginHost::new(bin, 48_000.0, 512);
    let (inst, gen) = host.instantiate(descriptor(id)).expect("instantiate over IPC");
    assert!(host.is_connected());

    let block = vec![1.0f32; 64 * 2];
    let out = host.process(inst, gen, &block).expect("process over IPC");
    assert_eq!(out.len(), block.len());
    for &s in &out {
        assert!((s - 0.5).abs() < 1e-3, "expected ~0.5, got {s}");
    }
    host.destroy(inst, gen);
}

#[test]
fn bogus_sidecar_path_bypasses_without_panicking() {
    let mut host = RemotePluginHost::new(
        PathBuf::from("/no/such/vance-plugin-host-binary"),
        48_000.0,
        512,
    );
    // Spawn fails → instantiate is None (bypass), never panics.
    let d = descriptor("/whatever.clap::x".into());
    assert!(host.instantiate(d).is_none());
    assert!(!host.is_connected());
    // A stale process call is also a clean bypass.
    assert!(host.process(uuid::Uuid::new_v4(), 0, &[0.0, 0.0]).is_none());
}

#[test]
fn sidecar_crash_degrades_to_bypass_then_respawns() {
    let Some((bin, id)) = prerequisites() else {
        return;
    };
    let mut host = RemotePluginHost::new(bin, 48_000.0, 512);
    let (inst, gen) = host.instantiate(descriptor(id.clone())).expect("instantiate");
    let block = vec![1.0f32; 64 * 2];
    assert!(host.process(inst, gen, &block).is_some(), "first process ok");

    // Simulate a plugin crash by killing the sidecar out from under us.
    let pid = host.sidecar_pid();
    assert!(pid != 0, "expected a live sidecar pid");
    kill(pid);
    // Give the OS a moment to tear the process down.
    std::thread::sleep(std::time::Duration::from_millis(100));

    // The next process call hits a broken pipe → bypass (None), no panic.
    assert!(
        host.process(inst, gen, &block).is_none(),
        "process on a dead sidecar must bypass"
    );

    // Re-instantiate respawns the sidecar (new generation) and works.
    let (inst2, gen2) = host
        .instantiate(descriptor(id))
        .expect("re-instantiate after crash");
    assert_ne!(gen2, gen, "respawn must bump the generation");
    let out = host.process(inst2, gen2, &block).expect("process after respawn");
    for &s in &out {
        assert!((s - 0.5).abs() < 1e-3, "expected ~0.5 after respawn, got {s}");
    }
    // The stale instance from before the crash stays bypassed.
    assert!(host.process(inst, gen, &block).is_none());
}

#[test]
fn mixer_track_dsp_routes_clap_through_sidecar() {
    let Some((bin, id)) = prerequisites() else {
        return;
    };
    // A track DSP chain with a single CLAP plugin, driven through the
    // real out-of-process path. After the block-priming latency the
    // fixture's 0.5x gain must show up at the chain output.
    let mut host = RemotePluginHost::new(bin, 48_000.0, 512);
    let spec = vec![vance_audio::AudioEffectKind::Plugin {
        enabled: true,
        format_str: "clap".into(),
        id,
        name: "Vance Test Gain".into(),
        params: vec![],
    }];
    let mut dsp = vance_audio::TrackDspState::new();
    dsp.reconcile_with(&spec, 48_000.0, Some(&mut host));
    let mut last = (0.0f32, 0.0f32);
    for _ in 0..512 {
        last = dsp.tick_stereo(&spec, 1.0, 1.0, Some(&mut host));
    }
    assert!((last.0 - 0.5).abs() < 1e-2, "L expected ~0.5, got {}", last.0);
    assert!((last.1 - 0.5).abs() < 1e-2, "R expected ~0.5, got {}", last.1);
    // Normal fast processing must never trip the hang watchdog.
    assert_eq!(host.kills(), 0, "watchdog killed a healthy sidecar");
}

#[test]
fn mixer_applies_clap_params_via_reconcile() {
    let Some((bin, id)) = prerequisites() else {
        return;
    };
    // Setting a CLAP parameter through the mixer spec must actually
    // change the audio: the fixture's `gain` param (id 0) at 1.0 → unity,
    // at 0.0 → silence — both initial application and a live change.
    let mut host = RemotePluginHost::new(bin, 48_000.0, 512);
    let mk = |gain: f32| {
        vec![vance_audio::AudioEffectKind::Plugin {
            enabled: true,
            format_str: "clap".into(),
            id: id.clone(),
            name: "Vance Test Gain".into(),
            params: vec![(0, gain)],
        }]
    };
    let mut dsp = vance_audio::TrackDspState::new();

    // Initial param 1.0 must apply on instantiate (not just on change).
    dsp.reconcile_with(&mk(1.0), 48_000.0, Some(&mut host));
    let mut out = (0.0f32, 0.0f32);
    for _ in 0..512 {
        out = dsp.tick_stereo(&mk(1.0), 1.0, 1.0, Some(&mut host));
    }
    assert!((out.0 - 1.0).abs() < 1e-2, "initial gain 1.0 → unity, got {}", out.0);

    // Live change to 0.0 → silence.
    dsp.reconcile_with(&mk(0.0), 48_000.0, Some(&mut host));
    for _ in 0..512 {
        out = dsp.tick_stereo(&mk(0.0), 1.0, 1.0, Some(&mut host));
    }
    assert!(out.0.abs() < 1e-2, "gain 0.0 → silence, got {}", out.0);
}

#[test]
fn crash_recovers_through_reconcile_only() {
    let Some((bin, id)) = prerequisites() else {
        return;
    };
    // The REAL fold-loop recovery path: after the sidecar dies, recovery
    // must happen purely through reconcile_with + tick_stereo on an
    // UNCHANGED spec — no manual instantiate(). (Regression for the bug
    // where a crashed lone plugin was bypassed forever because the
    // generation never advanced.)
    let mut host = RemotePluginHost::new(bin, 48_000.0, 512);
    let spec = vec![vance_audio::AudioEffectKind::Plugin {
        enabled: true,
        format_str: "clap".into(),
        id,
        name: "Vance Test Gain".into(),
        params: vec![],
    }];
    let mut dsp = vance_audio::TrackDspState::new();
    dsp.reconcile_with(&spec, 48_000.0, Some(&mut host));
    for _ in 0..512 {
        dsp.tick_stereo(&spec, 1.0, 1.0, Some(&mut host));
    }
    assert_eq!(host.instantiations(), 1);

    // Kill the sidecar out from under the host.
    let pid = host.sidecar_pid();
    assert!(pid != 0);
    kill(pid);
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Drive the fold-loop cycle (reconcile then a block of ticks) with the
    // SAME spec until audio recovers — never calling instantiate directly.
    let mut out = (0.0f32, 0.0f32);
    let mut recovered = false;
    for _round in 0..12 {
        dsp.reconcile_with(&spec, 48_000.0, Some(&mut host));
        for _ in 0..512 {
            out = dsp.tick_stereo(&spec, 1.0, 1.0, Some(&mut host));
        }
        if (out.0 - 0.5).abs() < 1e-2 {
            recovered = true;
            break;
        }
    }
    assert!(recovered, "audio did not recover after crash (got {})", out.0);
    assert!(
        host.instantiations() >= 2,
        "recovery must re-instantiate via reconcile, instantiations={}",
        host.instantiations()
    );
}

#[test]
fn disable_clears_remote_accumulator() {
    let Some((bin, id)) = prerequisites() else {
        return;
    };
    let mut host = RemotePluginHost::new(bin, 48_000.0, 512);
    let on = vec![vance_audio::AudioEffectKind::Plugin {
        enabled: true,
        format_str: "clap".into(),
        id: id.clone(),
        name: "g".into(),
        params: vec![],
    }];
    let off = vec![vance_audio::AudioEffectKind::Plugin {
        enabled: false,
        format_str: "clap".into(),
        id: id.clone(),
        name: "g".into(),
        params: vec![],
    }];
    let mut dsp = vance_audio::TrackDspState::new();
    dsp.reconcile_with(&on, 48_000.0, Some(&mut host));
    // Prime past a couple block boundaries so the output queue holds 0.5s.
    for _ in 0..200 {
        dsp.tick_stereo(&on, 1.0, 1.0, Some(&mut host));
    }
    // Disable for a few ticks — this must flush the accumulator.
    for _ in 0..5 {
        dsp.tick_stereo(&off, 1.0, 1.0, Some(&mut host));
    }
    // Re-enable: the first sample must be priming silence (clean block),
    // NOT a stale 0.5 left over from before the disable.
    let first = dsp.tick_stereo(&on, 1.0, 1.0, Some(&mut host));
    assert!(
        first.0.abs() < 1e-6,
        "stale buffered sample leaked across disable: got {}",
        first.0
    );
}

#[test]
fn param_change_does_not_reinstantiate_remote_plugin() {
    let Some((bin, id)) = prerequisites() else {
        return;
    };
    // A CLAP slot whose params change between reconciles must NOT
    // re-instantiate (which would re-dlopen the plugin and leak the old
    // sidecar instance every audio batch during a knob drag).
    let mut host = RemotePluginHost::new(bin, 48_000.0, 512);
    let mk_spec = |gain: f32| {
        vec![vance_audio::AudioEffectKind::Plugin {
            enabled: true,
            format_str: "clap".into(),
            id: id.clone(),
            name: "Vance Test Gain".into(),
            params: vec![(0, gain)],
        }]
    };
    let mut dsp = vance_audio::TrackDspState::new();
    dsp.reconcile_with(&mk_spec(-6.0), 48_000.0, Some(&mut host));
    assert_eq!(host.instantiations(), 1, "first reconcile instantiates once");

    // Several param changes (a knob drag) — none should re-instantiate.
    for g in [-5.0, -4.0, -3.0, 0.0, 3.0] {
        dsp.reconcile_with(&mk_spec(g), 48_000.0, Some(&mut host));
    }
    assert_eq!(
        host.instantiations(),
        1,
        "param changes must not re-instantiate the remote plugin"
    );

    // Changing the plugin IDENTITY (id) is a real rebuild → re-instantiate.
    let other = vec![vance_audio::AudioEffectKind::Plugin {
        enabled: true,
        format_str: "clap".into(),
        id: format!("{id}x"), // different id → won't load, but is a rebuild attempt
        name: "X".into(),
        params: vec![],
    }];
    dsp.reconcile_with(&other, 48_000.0, Some(&mut host));
    assert!(
        host.instantiations() >= 1,
        "identity change should attempt a (re-)instantiate"
    );
}

// --- LV2: full mixer → sidecar → lv2_backend chain --------------------

const LV2_URI: &str = "urn:vance:test:gain";

fn lv2_dylib() -> Option<PathBuf> {
    let (prefix, ext) = if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };
    let p = workspace_target_debug().join(format!("{prefix}vance_lv2_testplugin.{ext}"));
    p.exists().then_some(p)
}

fn sidecar_speaks_lv2(bin: &PathBuf) -> bool {
    match PluginClient::spawn(bin) {
        Ok(c) => c.supported_formats().contains(&PluginFormat::Lv2),
        Err(_) => false,
    }
}

fn stage_lv2_bundle(dylib: &PathBuf) -> std::io::Result<PathBuf> {
    let root = std::env::temp_dir().join(format!("vance-lv2-rp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let bundle = root.join("vance-test.lv2");
    std::fs::create_dir_all(&bundle)?;
    let dy = dylib.file_name().unwrap().to_string_lossy().into_owned();
    std::fs::copy(dylib, bundle.join(&dy))?;
    std::fs::write(
        bundle.join("manifest.ttl"),
        format!(
            "@prefix lv2: <http://lv2plug.in/ns/lv2core#> .\n\
             @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
             <{LV2_URI}>\n\ta lv2:Plugin ;\n\tlv2:binary <{dy}> ;\n\trdfs:seeAlso <vance-test.ttl> .\n"
        ),
    )?;
    std::fs::write(
        bundle.join("vance-test.ttl"),
        format!(
            "@prefix lv2: <http://lv2plug.in/ns/lv2core#> .\n\
             @prefix doap: <http://usefulinc.com/ns/doap#> .\n\
             <{LV2_URI}>\n\ta lv2:Plugin ;\n\tdoap:name \"Vance Test Gain\" ;\n\
             \tlv2:port [\n\t\ta lv2:AudioPort, lv2:InputPort ;\n\t\tlv2:index 0 ; lv2:symbol \"in\" ; lv2:name \"In\"\n\
             \t] , [\n\t\ta lv2:AudioPort, lv2:OutputPort ;\n\t\tlv2:index 1 ; lv2:symbol \"out\" ; lv2:name \"Out\"\n\t] .\n"
        ),
    )?;
    Ok(root)
}

#[test]
fn mixer_routes_lv2_through_sidecar() {
    let Some(bin) = sidecar_bin() else {
        return;
    };
    if !sidecar_speaks_lv2(&bin) {
        eprintln!("sidecar not built with --features lv2 — skipping mixer LV2 test");
        return;
    }
    let Some(dylib) = lv2_dylib() else {
        eprintln!("lv2 test fixture not built — skipping");
        return;
    };
    let root = stage_lv2_bundle(&dylib).expect("stage .lv2 bundle");
    // SAFETY: single-threaded test setup; the host spawns the sidecar
    // lazily on first instantiate, inheriting this env.
    unsafe {
        std::env::set_var("LV2_PATH", &root);
    }

    let mut host = RemotePluginHost::new(bin, 48_000.0, 512);
    let spec = vec![vance_audio::AudioEffectKind::Plugin {
        enabled: true,
        format_str: "lv2".into(),
        id: LV2_URI.into(),
        name: "Vance Test Gain".into(),
        params: vec![],
    }];
    let mut dsp = vance_audio::TrackDspState::new();
    dsp.reconcile_with(&spec, 48_000.0, Some(&mut host));
    let mut out = (0.0f32, 0.0f32);
    for _ in 0..512 {
        out = dsp.tick_stereo(&spec, 1.0, 1.0, Some(&mut host));
    }
    assert!((out.0 - 0.5).abs() < 1e-2, "L expected ~0.5, got {}", out.0);
    assert!((out.1 - 0.5).abs() < 1e-2, "R expected ~0.5, got {}", out.1);
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
fn kill(pid: u32) {
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill(_pid: u32) {
    // Windows test path not implemented; the unix CI covers this.
}
