//! GOAT-4 — mixer buses + sends (Fairlight-class console routing).
//!
//! Phase 7 gave every track gain/pan/mute/solo + an insert chain straight
//! to the master. This adds the routing a real console needs:
//!
//! - **Buses** ([`AudioBus`]): named submixes with their own gain/pan/mute
//!   + insert chain. A track (or another bus) routes its output to a bus;
//!   buses route to the master or to another bus (a DAG — cycles rejected).
//! - **Sends** ([`Send`]): a track/bus taps a COPY of its signal to a bus
//!   at an independent level, pre- or post-fader — the classic reverb/aux
//!   send. Sends never form the main signal path, so they can't cycle.

use serde::{Deserialize, Serialize};

use crate::audio_effect::AudioEffectSpec;
use crate::id::BusId;

/// Where a track or bus routes its MAIN output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BusOutput {
    /// Straight to the sequence master (the default, Phase-7 behaviour).
    #[default]
    Master,
    /// Into another bus (a submix).
    Bus(BusId),
}

/// One aux/reverb-style send: a tap to `target` at `level_db`, taken
/// pre- or post the source's own fader.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Send {
    /// Destination bus.
    pub target: BusId,
    /// Send level in dB (independent of the source's main fader).
    pub level_db: f32,
    /// Tap BEFORE the source's fader/mute (so the send level is constant
    /// even as the channel is faded down). `false` = post-fader.
    #[serde(default)]
    pub pre_fader: bool,
}

impl Send {
    /// A post-fader send to `target` at unity (0 dB).
    pub fn to(target: BusId) -> Self {
        Self {
            target,
            level_db: 0.0,
            pre_fader: false,
        }
    }
}

/// A mixer submix bus.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioBus {
    /// Stable id.
    pub id: BusId,
    /// Display name ("Dialogue", "Music", "Reverb", …).
    pub name: String,
    /// Fader gain in dB. 0.0 = unity.
    #[serde(default)]
    pub gain_db: f32,
    /// Stereo pan, `[-1, +1]`.
    #[serde(default)]
    pub pan: f32,
    /// Muted (excluded from the mix, like a track mute).
    #[serde(default)]
    pub mute: bool,
    /// Insert chain (serial effects on the bus sum).
    #[serde(default)]
    pub inserts: Vec<AudioEffectSpec>,
    /// Where this bus's main output goes (master or another bus).
    #[serde(default)]
    pub output: BusOutput,
    /// Sends from this bus to other buses.
    #[serde(default)]
    pub sends: Vec<Send>,
}

impl AudioBus {
    /// New bus with a fresh id, unity gain, routed to master.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: BusId::new(),
            name: name.into(),
            gain_db: 0.0,
            pan: 0.0,
            mute: false,
            inserts: Vec::new(),
            output: BusOutput::Master,
            sends: Vec::new(),
        }
    }
}

/// Validate that the buses' main-output routing forms a DAG ending at the
/// master — i.e. following `output` from any bus reaches Master without
/// revisiting a bus. Returns the id of a bus that is part of a cycle (or
/// routes to a missing bus), or `None` when the routing is sound.
///
/// Sends are NOT part of this check: they're taps, not the main path, so
/// they cannot create a feedback loop in the summing graph.
pub fn routing_cycle(buses: &[AudioBus]) -> Option<BusId> {
    use std::collections::HashMap;
    let by_id: HashMap<BusId, &AudioBus> = buses.iter().map(|b| (b.id, b)).collect();
    for start in buses {
        // Walk the output chain; if we revisit a bus or one routes to a
        // missing bus, the routing is broken. We return the OFFENDING bus
        // (the one whose output edge is bad), not the missing target.
        let mut seen = std::collections::HashSet::new();
        let mut cur = start.id;
        loop {
            if !seen.insert(cur) {
                return Some(cur); // cycle: cur is revisited
            }
            let Some(bus) = by_id.get(&cur) else {
                return Some(cur); // (only reachable if `start` itself is bogus)
            };
            match bus.output {
                BusOutput::Master => break,
                BusOutput::Bus(next) => {
                    if !by_id.contains_key(&next) {
                        return Some(cur); // `cur` routes to a missing bus
                    }
                    cur = next;
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_bus_defaults_to_master_unity() {
        let b = AudioBus::new("Dialogue");
        assert_eq!(b.name, "Dialogue");
        assert_eq!(b.gain_db, 0.0);
        assert_eq!(b.output, BusOutput::Master);
        assert!(b.sends.is_empty());
    }

    #[test]
    fn linear_chain_to_master_has_no_cycle() {
        let mut dx = AudioBus::new("Dialogue");
        let mut sub = AudioBus::new("Submix");
        // Dialogue → Submix → Master.
        dx.output = BusOutput::Bus(sub.id);
        sub.output = BusOutput::Master;
        assert_eq!(routing_cycle(&[dx, sub]), None);
    }

    #[test]
    fn cycle_is_detected() {
        let mut a = AudioBus::new("A");
        let mut b = AudioBus::new("B");
        a.output = BusOutput::Bus(b.id);
        b.output = BusOutput::Bus(a.id); // A → B → A
        let cycle = routing_cycle(&[a, b]);
        assert!(cycle.is_some(), "A↔B cycle must be detected");
    }

    #[test]
    fn route_to_missing_bus_is_flagged() {
        let mut a = AudioBus::new("A");
        a.output = BusOutput::Bus(BusId::new()); // nonexistent
        let id = a.id;
        assert_eq!(routing_cycle(&[a]), Some(id));
    }

    #[test]
    fn send_round_trips_and_helper() {
        let target = BusId::new();
        let s = Send::to(target);
        assert_eq!(s.target, target);
        assert_eq!(s.level_db, 0.0);
        assert!(!s.pre_fader);
        let j = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<Send>(&j).unwrap(), s);
    }
}
