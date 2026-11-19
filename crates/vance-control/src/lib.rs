//! `vance-control` — control surfaces for Vance.
//!
//! A small, pure layer that turns raw input from OSC (network), MIDI, or
//! HID editor keyboards (Speed Editor) into a normalized [`ControlEvent`],
//! then maps events to editor [`ControlAction`]s via a configurable
//! [`Mapping`]. The protocol decoders and mapping are pure + unit-tested;
//! the actual I/O (UDP socket, `midir`, `hidapi`) lives in the app layer,
//! which feeds bytes in and applies the resulting actions.

#![warn(missing_docs)]

pub mod midi;
pub mod osc;
pub mod speededitor;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// A normalized input event from any control surface.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ControlEvent {
    /// A button/key/pad, identified by a surface-local id.
    Button {
        /// Surface-local button id (MIDI note, OSC index, HID keycode).
        id: u32,
        /// Pressed (`true`) vs released.
        pressed: bool,
    },
    /// A continuous fader/knob in `[0, 1]`.
    Fader {
        /// Surface-local control id (MIDI CC, OSC index).
        id: u32,
        /// Normalized value `[0, 1]`.
        value: f32,
    },
    /// A relative jog/scrub wheel, in ticks (sign = direction).
    Jog {
        /// Relative ticks since the last report.
        ticks: i32,
    },
}

/// An editor action a control event maps to. The app layer dispatches
/// these against the engine.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum ControlAction {
    /// Toggle playback.
    PlayPause,
    /// Stop playback.
    Stop,
    /// Step the playhead by N frames (sign = direction).
    JogFrames(i64),
    /// Jump to the sequence start.
    GoToStart,
    /// Jump to the sequence end.
    GoToEnd,
    /// Drop a marker at the playhead.
    AddMarker,
    /// Split the clip under the playhead.
    Split,
    /// Undo / redo the last edit.
    Undo,
    /// Redo.
    Redo,
    /// Set the master bus gain (dB), from a fader.
    SetMasterGainDb(f32),
}

/// What a fader controls.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FaderTarget {
    /// Master bus gain: maps `[0,1]` → `[-60, +6]` dB.
    MasterGain,
}

/// Map a fader value `[0,1]` to master gain dB `[-60, +6]`.
pub fn fader_to_master_db(v: f32) -> f32 {
    -60.0 + v.clamp(0.0, 1.0) * 66.0
}

/// A configurable mapping from control events to editor actions.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Mapping {
    /// Button id → action (fires on PRESS).
    pub buttons: HashMap<u32, ControlAction>,
    /// Fader id → target.
    pub faders: HashMap<u32, FaderTarget>,
    /// Frames stepped per jog tick.
    pub jog_frames: i64,
}

impl Default for Mapping {
    fn default() -> Self {
        Mapping::standard()
    }
}

impl Mapping {
    /// A sensible default surface layout, shared by OSC + MIDI + HID.
    ///
    /// Buttons (MIDI note / OSC `/vance/button/N` / HID keycode):
    /// - `0` PlayPause, `1` Stop, `2` GoToStart, `3` GoToEnd,
    /// - `4` AddMarker, `5` Split, `6` Undo, `7` Redo.
    ///
    /// Fader `0` (MIDI CC / OSC `/vance/fader/0`) → master gain.
    /// Jog → 1 frame per tick.
    pub fn standard() -> Self {
        let mut buttons = HashMap::new();
        buttons.insert(0, ControlAction::PlayPause);
        buttons.insert(1, ControlAction::Stop);
        buttons.insert(2, ControlAction::GoToStart);
        buttons.insert(3, ControlAction::GoToEnd);
        buttons.insert(4, ControlAction::AddMarker);
        buttons.insert(5, ControlAction::Split);
        buttons.insert(6, ControlAction::Undo);
        buttons.insert(7, ControlAction::Redo);
        let mut faders = HashMap::new();
        faders.insert(0, FaderTarget::MasterGain);
        Mapping {
            buttons,
            faders,
            jog_frames: 1,
        }
    }

    /// Map one [`ControlEvent`] to an action (if bound). Button releases
    /// and unbound ids yield `None`.
    pub fn map(&self, ev: &ControlEvent) -> Option<ControlAction> {
        match ev {
            ControlEvent::Button { id, pressed: true } => self.buttons.get(id).copied(),
            ControlEvent::Button { .. } => None,
            ControlEvent::Fader { id, value } => self.faders.get(id).map(|t| match t {
                FaderTarget::MasterGain => {
                    ControlAction::SetMasterGainDb(fader_to_master_db(*value))
                }
            }),
            ControlEvent::Jog { ticks } => {
                Some(ControlAction::JogFrames(*ticks as i64 * self.jog_frames))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_mapping_transport_and_fader() {
        let m = Mapping::standard();
        assert_eq!(
            m.map(&ControlEvent::Button {
                id: 0,
                pressed: true
            }),
            Some(ControlAction::PlayPause)
        );
        // Release doesn't fire.
        assert_eq!(
            m.map(&ControlEvent::Button {
                id: 0,
                pressed: false
            }),
            None
        );
        // Unbound button.
        assert_eq!(
            m.map(&ControlEvent::Button {
                id: 99,
                pressed: true
            }),
            None
        );
        // Fader 0 → master gain, 0.5 → -27 dB.
        match m.map(&ControlEvent::Fader { id: 0, value: 0.5 }) {
            Some(ControlAction::SetMasterGainDb(db)) => assert!((db - (-27.0)).abs() < 1e-4),
            other => panic!("expected master gain, got {other:?}"),
        }
        // Jog → frames.
        assert_eq!(
            m.map(&ControlEvent::Jog { ticks: -3 }),
            Some(ControlAction::JogFrames(-3))
        );
    }

    #[test]
    fn fader_to_db_range() {
        assert!((fader_to_master_db(0.0) - (-60.0)).abs() < 1e-4);
        assert!((fader_to_master_db(1.0) - 6.0).abs() < 1e-4);
        assert_eq!(fader_to_master_db(-1.0), -60.0, "clamped");
    }
}
