//! Test fixture: a minimal real CLAP plugin — a fixed 0.5× (-6 dB)
//! stereo gain. Used to verify `vance-audio`'s CLAP host end-to-end
//! without needing a third-party `.clap` installed. Exports the
//! standard `clap_entry` symbol via `clack_export_entry!`; the host
//! dlopen()s the resulting cdylib directly.

use clack_plugin::prelude::*;

/// Plugin id the host looks up.
pub const PLUGIN_ID: &str = "vance.test.gain";

pub struct VanceTestGain;

impl Plugin for VanceTestGain {
    type AudioProcessor<'a> = GainProcessor;
    type Shared<'a> = ();
    type MainThread<'a> = ();
}

impl DefaultPluginFactory for VanceTestGain {
    fn get_descriptor() -> PluginDescriptor {
        PluginDescriptor::new(PLUGIN_ID, "Vance Test Gain")
    }

    fn new_shared(_host: HostSharedHandle<'_>) -> Result<Self::Shared<'_>, PluginError> {
        Ok(())
    }

    fn new_main_thread<'a>(
        _host: HostMainThreadHandle<'a>,
        _shared: &'a Self::Shared<'a>,
    ) -> Result<Self::MainThread<'a>, PluginError> {
        Ok(())
    }
}

pub struct GainProcessor;

impl<'a> PluginAudioProcessor<'a, (), ()> for GainProcessor {
    fn activate(
        _host: HostAudioProcessorHandle<'a>,
        _main_thread: &mut (),
        _shared: &'a (),
        _audio_config: PluginAudioConfiguration,
    ) -> Result<Self, PluginError> {
        Ok(Self)
    }

    fn process(
        &mut self,
        _process: Process,
        mut audio: Audio,
        _events: Events,
    ) -> Result<ProcessStatus, PluginError> {
        const GAIN: f32 = 0.5; // -6.02 dB
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
                            *o = *i * GAIN;
                        }
                    }
                    ChannelPair::InPlace(buf) => {
                        for s in buf {
                            *s *= GAIN;
                        }
                    }
                }
            }
        }
        Ok(ProcessStatus::Continue)
    }
}

clack_export_entry!(SinglePluginEntry<VanceTestGain>);
