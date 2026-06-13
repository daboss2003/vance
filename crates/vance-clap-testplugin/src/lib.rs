//! Test fixture: a minimal real CLAP plugin — a stereo gain with ONE
//! automatable parameter (`gain`, 0.0..1.0, default 0.5 = -6 dB). Used to
//! verify `vance-audio`'s CLAP host end-to-end (discovery, processing,
//! AND parameter events) without needing a third-party `.clap` installed.
//! Exports the standard `clap_entry` symbol via `clack_export_entry!`.

use clack_extensions::params::{
    ParamDisplayWriter, ParamInfo, ParamInfoFlags, ParamInfoWriter, PluginAudioProcessorParams,
    PluginMainThreadParams, PluginParams,
};
use clack_plugin::events::spaces::CoreEventSpace;
use clack_plugin::prelude::*;
use std::ffi::CStr;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU32, Ordering};

/// Plugin id the host looks up.
pub const PLUGIN_ID: &str = "vance.test.gain";
/// The single parameter's id.
pub const GAIN_PARAM_ID: u32 = 0;

pub struct VanceTestGain;

/// Shared state: the current gain as f32 bits in an atomic, so the main
/// thread (param queries) and the audio thread (process) can both touch it.
pub struct GainShared {
    gain_bits: AtomicU32,
}
impl GainShared {
    fn gain(&self) -> f32 {
        f32::from_bits(self.gain_bits.load(Ordering::Relaxed))
    }
    fn set_gain(&self, g: f32) {
        self.gain_bits.store(g.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
}
impl<'a> PluginShared<'a> for GainShared {}

/// Main-thread state — just a reference to the shared gain for param I/O.
pub struct GainMainThread<'a> {
    shared: &'a GainShared,
}
impl<'a> PluginMainThread<'a, GainShared> for GainMainThread<'a> {}

impl Plugin for VanceTestGain {
    type AudioProcessor<'a> = GainProcessor<'a>;
    type Shared<'a> = GainShared;
    type MainThread<'a> = GainMainThread<'a>;

    fn declare_extensions(builder: &mut PluginExtensions<Self>, _shared: Option<&GainShared>) {
        builder.register::<PluginParams>();
    }
}

impl DefaultPluginFactory for VanceTestGain {
    fn get_descriptor() -> PluginDescriptor {
        PluginDescriptor::new(PLUGIN_ID, "Vance Test Gain")
    }

    fn new_shared(_host: HostSharedHandle<'_>) -> Result<Self::Shared<'_>, PluginError> {
        Ok(GainShared {
            gain_bits: AtomicU32::new(0.5f32.to_bits()),
        })
    }

    fn new_main_thread<'a>(
        _host: HostMainThreadHandle<'a>,
        shared: &'a Self::Shared<'a>,
    ) -> Result<Self::MainThread<'a>, PluginError> {
        Ok(GainMainThread { shared })
    }
}

pub struct GainProcessor<'a> {
    shared: &'a GainShared,
}

impl<'a> PluginAudioProcessor<'a, GainShared, GainMainThread<'a>> for GainProcessor<'a> {
    fn activate(
        _host: HostAudioProcessorHandle<'a>,
        _main_thread: &mut GainMainThread<'a>,
        shared: &'a GainShared,
        _audio_config: PluginAudioConfiguration,
    ) -> Result<Self, PluginError> {
        Ok(Self { shared })
    }

    fn process(
        &mut self,
        _process: Process,
        mut audio: Audio,
        events: Events,
    ) -> Result<ProcessStatus, PluginError> {
        // Apply any incoming parameter automation before processing.
        for event in events.input {
            if let Some(CoreEventSpace::ParamValue(pv)) = event.as_core_event() {
                if pv.param_id().map(|id| id.get()) == Some(GAIN_PARAM_ID) {
                    self.shared.set_gain(pv.value() as f32);
                }
            }
        }
        let gain = self.shared.gain();
        for mut port_pair in &mut audio {
            let Some(channel_pairs) = port_pair.channels()?.into_f32() else {
                continue;
            };
            for channel_pair in channel_pairs {
                match channel_pair {
                    ChannelPair::InputOnly(_) => {}
                    ChannelPair::OutputOnly(buf) => buf.fill(0.0),
                    ChannelPair::InputOutput(input, output) => {
                        for (i, o) in input.iter().zip(output) {
                            *o = *i * gain;
                        }
                    }
                    ChannelPair::InPlace(buf) => {
                        for s in buf {
                            *s *= gain;
                        }
                    }
                }
            }
        }
        Ok(ProcessStatus::Continue)
    }
}

/// Apply a stream of parameter-value events to the shared gain.
fn apply_param_events(shared: &GainShared, input: &InputEvents) {
    for event in input {
        if let Some(CoreEventSpace::ParamValue(pv)) = event.as_core_event() {
            if pv.param_id().map(|id| id.get()) == Some(GAIN_PARAM_ID) {
                shared.set_gain(pv.value() as f32);
            }
        }
    }
}

impl<'a> PluginMainThreadParams for GainMainThread<'a> {
    fn count(&mut self) -> u32 {
        1
    }

    fn get_info(&mut self, param_index: u32, info: &mut ParamInfoWriter) {
        if param_index != 0 {
            return;
        }
        let Some(id) = ClapId::from_raw(GAIN_PARAM_ID) else {
            return;
        };
        info.set(&ParamInfo {
            id,
            flags: ParamInfoFlags::IS_AUTOMATABLE,
            cookie: Default::default(),
            name: b"Gain",
            module: b"",
            min_value: 0.0,
            max_value: 1.0,
            default_value: 0.5,
        });
    }

    fn get_value(&mut self, param_id: ClapId) -> Option<f64> {
        (param_id.get() == GAIN_PARAM_ID).then(|| self.shared.gain() as f64)
    }

    fn value_to_text(
        &mut self,
        param_id: ClapId,
        value: f64,
        writer: &mut ParamDisplayWriter,
    ) -> core::fmt::Result {
        if param_id.get() != GAIN_PARAM_ID {
            return Err(core::fmt::Error);
        }
        write!(writer, "{value:.3}")
    }

    fn text_to_value(&mut self, param_id: ClapId, text: &CStr) -> Option<f64> {
        if param_id.get() != GAIN_PARAM_ID {
            return None;
        }
        text.to_str().ok()?.trim().parse::<f64>().ok()
    }

    fn flush(&mut self, input: &InputEvents, _output: &mut OutputEvents) {
        apply_param_events(self.shared, input);
    }
}

impl<'a> PluginAudioProcessorParams for GainProcessor<'a> {
    fn flush(&mut self, input: &InputEvents, _output: &mut OutputEvents) {
        apply_param_events(self.shared, input);
    }
}

clack_export_entry!(SinglePluginEntry<VanceTestGain>);
