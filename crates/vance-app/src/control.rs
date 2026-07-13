//! GOAT-10 — control surfaces: OSC (UDP) + MIDI listeners that feed a
//! bounded action queue the UI polls and dispatches through the existing,
//! thread-correct IPC commands (play/pause, seek, master gain, undo…).
//!
//! Applying actions on a background thread would race the GPU/playback
//! main-thread invariants, so the listeners only DECODE + MAP here and
//! enqueue [`ControlAction`]s; the frontend drains them via
//! `poll_control_actions` and calls the right command. OSC needs no
//! hardware (any phone OSC app over WiFi); MIDI binds real controllers.
//! The Blackmagic Speed Editor HID decoder lives in `vance-control`; its
//! device I/O + auth handshake are the gated hardware piece.

use std::collections::VecDeque;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use vance_control::{osc, ControlAction, Mapping};

/// Cap so a UI that stops polling can't grow the queue without bound.
const QUEUE_CAP: usize = 256;

/// Discrete-action queue + the coalesced latest continuous (fader) value.
/// Shared with the listener threads.
#[derive(Default)]
struct Sink {
    /// One-shot actions (buttons, jog). Never evicted by a fader flood.
    queue: Mutex<VecDeque<ControlAction>>,
    /// Latest continuous value (master gain); only the newest matters, so
    /// it's coalesced (overwrite) rather than queued — a network fader
    /// flood can't evict the user's discrete transport/undo presses.
    latest_fader: Mutex<Option<ControlAction>>,
}

impl Sink {
    fn push(&self, action: ControlAction) {
        if matches!(action, ControlAction::SetMasterGainDb(_)) {
            if let Ok(mut f) = self.latest_fader.lock() {
                *f = Some(action);
            }
        } else if let Ok(mut q) = self.queue.lock() {
            if q.len() >= QUEUE_CAP {
                q.pop_front();
            }
            q.push_back(action);
        }
    }

    fn drain(&self) -> Vec<ControlAction> {
        let mut out: Vec<ControlAction> = self
            .queue
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default();
        if let Some(f) = self.latest_fader.lock().ok().and_then(|mut l| l.take()) {
            out.push(f); // apply the newest fader value last
        }
        out
    }
}

/// Shared, UI-pollable control-surface hub. Managed as Tauri state.
pub struct ControlHub {
    sink: Arc<Sink>,
    mapping: Arc<Mapping>,
    osc_stop: Option<Arc<AtomicBool>>,
    osc_thread: Option<std::thread::JoinHandle<()>>,
    osc_port: Option<u16>,
    // Kept alive to hold the MIDI callback open; dropping it disconnects.
    midi_conn: Option<midir::MidiInputConnection<()>>,
    midi_name: Option<String>,
}

impl Default for ControlHub {
    fn default() -> Self {
        ControlHub {
            sink: Arc::new(Sink::default()),
            mapping: Arc::new(Mapping::standard()),
            osc_stop: None,
            osc_thread: None,
            osc_port: None,
            midi_conn: None,
            midi_name: None,
        }
    }
}

impl ControlHub {
    /// Drain all queued actions (the UI polls this on a timer).
    pub fn drain(&self) -> Vec<ControlAction> {
        self.sink.drain()
    }

    /// Whether OSC / MIDI are currently listening, + their identifiers.
    pub fn status(&self) -> (bool, Option<u16>, bool, Option<String>) {
        (
            self.osc_stop.is_some(),
            self.osc_port,
            self.midi_conn.is_some(),
            self.midi_name.clone(),
        )
    }

    /// Start the OSC UDP listener on `port` (rebinds if already running —
    /// the prior listener is JOINED first so the socket is released before
    /// the new bind, avoiding EADDRINUSE).
    pub fn start_osc(&mut self, port: u16) -> std::io::Result<()> {
        self.stop_osc();
        let socket = UdpSocket::bind(("0.0.0.0", port))?;
        // Short timeout so `stop_osc`'s join returns promptly.
        socket.set_read_timeout(Some(Duration::from_millis(50)))?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let sink = self.sink.clone();
        let mapping = self.mapping.clone();
        let handle = std::thread::Builder::new()
            .name("vance.osc".into())
            .spawn(move || {
                let mut buf = [0u8; 2048];
                while !stop_thread.load(Ordering::Acquire) {
                    if let Ok((n, _)) = socket.recv_from(&mut buf) {
                        if let Some(msg) = osc::decode_message(&buf[..n]) {
                            if let Some(ev) = osc::to_control_event(&msg) {
                                if let Some(action) = mapping.map(&ev) {
                                    sink.push(action);
                                }
                            }
                        }
                    }
                    // Timeout / error → re-check the stop flag and continue.
                }
            })?;
        self.osc_stop = Some(stop);
        self.osc_thread = Some(handle);
        self.osc_port = Some(port);
        Ok(())
    }

    /// Stop the OSC listener + JOIN its thread so the socket is released
    /// (idempotent). Blocks up to the ~50ms read timeout.
    pub fn stop_osc(&mut self) {
        if let Some(stop) = self.osc_stop.take() {
            stop.store(true, Ordering::Release);
        }
        if let Some(handle) = self.osc_thread.take() {
            let _ = handle.join();
        }
        self.osc_port = None;
    }

    /// Available MIDI input port names.
    pub fn midi_ports() -> Vec<String> {
        match midir::MidiInput::new("vance-midi-scan") {
            Ok(input) => input
                .ports()
                .iter()
                .map(|p| input.port_name(p).unwrap_or_else(|_| "?".into()))
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Open a MIDI input port by index and route it through the mapping.
    pub fn start_midi(&mut self, port_index: usize) -> Result<(), String> {
        self.stop_midi();
        let input = midir::MidiInput::new("vance-midi").map_err(|e| e.to_string())?;
        let ports = input.ports();
        let port = ports.get(port_index).ok_or("no such MIDI port")?;
        let name = input.port_name(port).unwrap_or_else(|_| "MIDI".into());
        let sink = self.sink.clone();
        let mapping = self.mapping.clone();
        let conn = input
            .connect(
                port,
                "vance-midi-in",
                move |_stamp, bytes, _| {
                    if let Some(ev) = vance_control::midi::decode(bytes) {
                        if let Some(action) = mapping.map(&ev) {
                            sink.push(action);
                        }
                    }
                },
                (),
            )
            .map_err(|e| e.to_string())?;
        self.midi_conn = Some(conn);
        self.midi_name = Some(name);
        Ok(())
    }

    /// Close the MIDI connection (idempotent).
    pub fn stop_midi(&mut self) {
        self.midi_conn = None;
        self.midi_name = None;
    }
}

impl Drop for ControlHub {
    fn drop(&mut self) {
        self.stop_osc();
        self.stop_midi();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fader_flood_does_not_evict_discrete_actions() {
        let sink = Sink::default();
        // A user's one-shot press...
        sink.push(ControlAction::PlayPause);
        // ...then a flood of fader values far exceeding the queue cap.
        for i in 0..(QUEUE_CAP as i64 * 4) {
            sink.push(ControlAction::SetMasterGainDb(-60.0 + (i % 60) as f32));
        }
        let out = sink.drain();
        // The PlayPause survived (faders coalesced, didn't evict it).
        assert!(
            out.contains(&ControlAction::PlayPause),
            "discrete action must not be evicted by a fader flood"
        );
        // Exactly ONE fader action, and it's the LATEST value.
        let faders: Vec<_> = out
            .iter()
            .filter(|a| matches!(a, ControlAction::SetMasterGainDb(_)))
            .collect();
        assert_eq!(faders.len(), 1, "faders coalesce to one");
        let last = -60.0 + ((QUEUE_CAP as i64 * 4 - 1) % 60) as f32;
        assert_eq!(
            faders[0],
            &ControlAction::SetMasterGainDb(last),
            "keeps the newest fader value"
        );
        // Draining again yields nothing.
        assert!(sink.drain().is_empty());
    }
}
