//! Phase 7.11 — CLAP audio-plugin backend (feature `clap`).
//!
//! Hosts a third-party `.clap` plugin in-process via `clack-host`
//! (pure-Rust + dlopen) and adapts it to the fold chain's
//! [`crate::audio_plugin::AudioPlugin`] block interface. The plugin
//! instance + its audio processor have no lifetime params (the
//! instance refcounts the loaded entry), so both live in one struct —
//! no self-referential gymnastics.
//!
//! v1 scope: stereo 32-bit float, fixed block size, first plugin in
//! the bundle, no parameter events yet (the host passes audio + empty
//! event streams). Parameter automation is a follow-up.

use crate::audio_plugin::AudioPlugin;
use clack_host::events::io::{InputEvents, OutputEvents};
use clack_host::prelude::*;

/// Minimal no-op host. Audio plugins only need the three request
/// callbacks (we don't act on them — offline/realtime pull model).
struct VanceHostShared;
impl<'a> SharedHandler<'a> for VanceHostShared {
    fn request_restart(&self) {}
    fn request_process(&self) {}
    fn request_callback(&self) {}
}

struct VanceHost;
impl HostHandlers for VanceHost {
    type Shared<'a> = VanceHostShared;
    type MainThread<'a> = ();
    type AudioProcessor<'a> = ();
}

/// A loaded CLAP plugin wrapped as a block-processing [`AudioPlugin`].
pub struct ClapPlugin {
    // Field order matters for Drop: the processor must drop before the
    // instance. `processor` is listed first.
    processor: Option<StartedPluginAudioProcessor<VanceHost>>,
    instance: PluginInstance<VanceHost>,
    in_ports: AudioPorts,
    out_ports: AudioPorts,
    in_l: Vec<f32>,
    in_r: Vec<f32>,
    event_out: EventBuffer,
    block: usize,
}

impl AudioPlugin for ClapPlugin {
    fn process_block(&mut self, in_l: &[f32], in_r: &[f32], out_l: &mut [f32], out_r: &mut [f32]) {
        let n = in_l.len().min(self.block);
        let Some(proc) = self.processor.as_mut() else {
            // No active processor → bypass.
            out_l[..n].copy_from_slice(&in_l[..n]);
            out_r[..n].copy_from_slice(&in_r[..n]);
            return;
        };
        // Copy the input into the plugin's (mutable) input buffers.
        self.in_l[..n].copy_from_slice(&in_l[..n]);
        self.in_r[..n].copy_from_slice(&in_r[..n]);

        let input_audio = self.in_ports.with_input_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only(
                [
                    InputChannel::variable(&mut self.in_l[..n]),
                    InputChannel::variable(&mut self.in_r[..n]),
                ]
                .into_iter(),
            ),
        }]);
        let mut output_audio = self.out_ports.with_output_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(
                [&mut out_l[..n], &mut out_r[..n]].into_iter(),
            ),
        }]);

        let input_events = InputEvents::empty();
        let mut output_events = OutputEvents::from_buffer(&mut self.event_out);
        // `proc.process` runs untrusted third-party native code. Contain
        // a *Rust* panic from it (e.g. a clack-host assertion or a plugin
        // that unwinds) so one bad block degrades to bypass instead of
        // taking down the audio thread. NOTE: this only helps when built
        // with `panic = "unwind"`; under the workspace's release
        // `panic = "abort"` it is a no-op, and a hard segfault from the
        // plugin is uncatchable either way — full crash isolation of
        // plugin *processing* needs out-of-process hosting (tracked
        // separately). Discovery already loads bundles out-of-process.
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            proc.process(
                &input_audio,
                &mut output_audio,
                &input_events,
                &mut output_events,
                None,
                None,
            )
        }));
        if matches!(res, Ok(Ok(_))) {
            // Plugin succeeded — `out_l`/`out_r` hold its output.
        } else {
            // Process error OR a caught panic → pass audio through
            // unchanged so the slot is transparent.
            out_l[..n].copy_from_slice(&in_l[..n]);
            out_r[..n].copy_from_slice(&in_r[..n]);
        }
        self.event_out.clear();
    }
}

/// Split a plugin id into `(bundle path, optional clap plugin id)`.
///
/// The id is either a bare bundle path or the composite
/// `"<bundle-path>::<clap-plugin-id>"` emitted by discovery. Both the
/// bundle path AND the clap plugin id may themselves contain `::` (a
/// path with `::` is unusual but legal; a clap id with `::` is unusual
/// but not forbidden), so a naive `rsplit_once`/`split_once` mis-splits
/// either one. We resolve the ambiguity against the filesystem:
///
/// 1. If the whole id is itself an existing path, it is a bare bundle —
///    no specific plugin requested. (A real bundle whose name contains
///    `::` wins over any coincidental sub-split.)
/// 2. Otherwise scan `::` boundaries left-to-right and take the FIRST one
///    whose left side is an existing path; the remainder (which may
///    contain further `::`) is the clap plugin id.
/// 3. If no boundary yields an existing path, treat the whole id as a
///    bundle path (load will simply fail downstream and the slot is
///    bypassed).
fn split_clap_id(id: &str) -> (&str, Option<&str>) {
    use std::path::Path;
    if Path::new(id).exists() {
        return (id, None);
    }
    let mut from = 0;
    while let Some(rel) = id[from..].find("::") {
        let pos = from + rel;
        let left = &id[..pos];
        if Path::new(left).exists() {
            return (left, Some(&id[pos + 2..]));
        }
        from = pos + 2;
    }
    (id, None)
}

/// Instantiate a CLAP plugin. `id` is either a bare `.clap` bundle path
/// (the first plugin in the bundle is used) or a composite
/// `"<bundle-path>::<clap-plugin-id>"` produced by
/// [`crate::clap_host`]'s discovery counterpart in `vance-plugin-host`,
/// which selects a specific plugin from a multi-plugin bundle. Returns
/// `None` on any failure so the caller bypasses the chain slot.
pub fn instantiate(
    id: &str,
    _params: &[(u32, f32)],
    sample_rate: f32,
    block: usize,
) -> Option<Box<dyn AudioPlugin>> {
    let (bundle_path, wanted_id) = split_clap_id(id);

    let entry = unsafe { PluginEntry::load(bundle_path) }.ok()?;
    let factory = entry.get_plugin_factory()?;
    // Pick the requested plugin from the bundle, or the first one when
    // no specific id was requested.
    let plugin_id = match wanted_id {
        Some(want) => factory
            .plugin_descriptors()
            .find(|d| d.id().and_then(|c| c.to_str().ok()) == Some(want))
            .and_then(|d| d.id())?
            .to_owned(),
        None => factory
            .plugin_descriptors()
            .next()
            .and_then(|d| d.id())?
            .to_owned(),
    };
    let host_info = HostInfo::new("Vance", "Vance", "https://vance.app", "1.0").ok()?;

    let mut instance = PluginInstance::<VanceHost>::new(
        |_| VanceHostShared,
        |_| (),
        &entry,
        &plugin_id,
        &host_info,
    )
    .ok()?;

    let config = PluginAudioConfiguration {
        sample_rate: sample_rate as f64,
        min_frames_count: block as u32,
        max_frames_count: block as u32,
    };
    let stopped = instance.activate(|_, _| (), config).ok()?;
    let started = stopped.start_processing().ok()?;

    Some(Box::new(ClapPlugin {
        processor: Some(started),
        instance,
        in_ports: AudioPorts::with_capacity(2, 1),
        out_ports: AudioPorts::with_capacity(2, 1),
        in_l: vec![0.0; block],
        in_r: vec![0.0; block],
        event_out: EventBuffer::new(),
        block,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture_path() -> Option<PathBuf> {
        let (prefix, ext) = if cfg!(target_os = "windows") {
            ("", "dll")
        } else if cfg!(target_os = "macos") {
            ("lib", "dylib")
        } else {
            ("lib", "so")
        };
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/debug")
            .join(format!("{prefix}vance_clap_testplugin.{ext}"));
        p.exists().then_some(p)
    }

    #[test]
    fn loads_and_processes_real_clap_plugin() {
        let Some(path) = fixture_path() else {
            eprintln!("clap test fixture not built — skipping");
            return;
        };
        let block = 64usize;
        let mut plugin = instantiate(&path.to_string_lossy(), &[], 48_000.0, block)
            .expect("instantiate CLAP fixture");
        // The fixture applies a fixed 0.5x gain. Feed 1.0, expect ~0.5.
        let inl = vec![1.0f32; block];
        let inr = vec![1.0f32; block];
        let mut outl = vec![0.0f32; block];
        let mut outr = vec![0.0f32; block];
        plugin.process_block(&inl, &inr, &mut outl, &mut outr);
        for (&l, &r) in outl.iter().zip(outr.iter()) {
            assert!((l - 0.5).abs() < 1e-3, "L expected ~0.5, got {l}");
            assert!((r - 0.5).abs() < 1e-3, "R expected ~0.5, got {r}");
        }
    }

    #[test]
    fn loads_specific_plugin_by_composite_id() {
        // `"<path>::<clap-plugin-id>"` is what discovery emits. The
        // fixture's plugin id is `vance.test.gain`.
        let Some(path) = fixture_path() else {
            eprintln!("clap test fixture not built — skipping");
            return;
        };
        let block = 64usize;
        let composite = format!("{}::vance.test.gain", path.to_string_lossy());
        let mut plugin = instantiate(&composite, &[], 48_000.0, block)
            .expect("instantiate CLAP fixture via composite id");
        let inl = vec![1.0f32; block];
        let mut outl = vec![0.0f32; block];
        let mut outr = vec![0.0f32; block];
        plugin.process_block(&inl, &inl, &mut outl, &mut outr);
        assert!((outl[0] - 0.5).abs() < 1e-3, "got {}", outl[0]);

        // A composite id naming a plugin the bundle does NOT expose must
        // fail cleanly (caller bypasses the slot).
        let wrong = format!("{}::com.nope.missing", path.to_string_lossy());
        assert!(instantiate(&wrong, &[], 48_000.0, block).is_none());
    }

    #[test]
    fn missing_clap_bundle_is_none() {
        assert!(instantiate("/nonexistent/x.clap", &[], 48_000.0, 64).is_none());
    }

    #[test]
    fn split_clap_id_handles_paths_and_pids_with_colons() {
        use std::path::PathBuf;
        // Build real fixtures so the exists()-based resolver has ground
        // truth. One bundle has a NAME containing "::"; we also form a
        // composite whose clap-id contains "::".
        let base = std::env::temp_dir().join(format!("vance-splitid-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let weird_bundle = base.join("Od::dName.clap");
        std::fs::write(&weird_bundle, b"x").unwrap();
        let weird_str = weird_bundle.to_string_lossy().into_owned();

        // 1. Whole id is an existing bundle whose name contains "::" →
        //    treated as a bare path, NOT split.
        assert_eq!(split_clap_id(&weird_str), (weird_str.as_str(), None));

        // 2. Composite over that same weird bundle, plus a clap id that
        //    itself contains "::". The FIRST boundary whose left side is
        //    a real path must win, and the remainder (with its "::")
        //    stays intact as the plugin id.
        let composite = format!("{weird_str}::com::weird::plug");
        let (path, pid) = split_clap_id(&composite);
        assert_eq!(path, weird_str.as_str(), "path must be the real bundle");
        assert_eq!(pid, Some("com::weird::plug"), "pid must keep its colons");

        // 3. A simple existing bundle + simple pid.
        let simple = base.join("Synth.clap");
        std::fs::write(&simple, b"x").unwrap();
        let s = simple.to_string_lossy().into_owned();
        assert_eq!(
            split_clap_id(&format!("{s}::com.v.synth")),
            (s.as_str(), Some("com.v.synth"))
        );

        // 4. Nonexistent path → whole thing is the (doomed) bundle path.
        let ghost = "/no/such/Bundle.clap::com.x";
        assert_eq!(split_clap_id(ghost), (ghost, None));
        let _ = PathBuf::from(&base);
        let _ = std::fs::remove_dir_all(&base);
    }
}
