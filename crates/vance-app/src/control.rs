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

/// Shared, UI-pollable control-surface hub. Managed as Tauri state.
pub struct ControlHub {
    queue: Arc<Mutex<VecDeque<ControlAction>>>,
    mapping: Arc<Mapping>,
    osc_stop: Option<Arc<AtomicBool>>,
    osc_port: Option<u16>,
    // Kept alive to hold the MIDI callback open; dropping it disconnects.
    midi_conn: Option<midir::MidiInputConnection<()>>,
    midi_name: Option<String>,
}

impl Default for ControlHub {
    fn default() -> Self {
        ControlHub {
            queue: Arc::new(Mutex::new(VecDeque::new())),
            mapping: Arc::new(Mapping::standard()),
            osc_stop: None,
            osc_port: None,
            midi_conn: None,
            midi_name: None,
        }
    }
}

fn push(queue: &Mutex<VecDeque<ControlAction>>, action: ControlAction) {
    if let Ok(mut q) = queue.lock() {
        if q.len() >= QUEUE_CAP {
            q.pop_front();
        }
        q.push_back(action);
    }
}

impl ControlHub {
    /// Drain all queued actions (the UI polls this on a timer).
    pub fn drain(&self) -> Vec<ControlAction> {
        match self.queue.lock() {
            Ok(mut q) => q.drain(..).collect(),
            Err(_) => Vec::new(),
        }
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

    /// Start the OSC UDP listener on `port` (rebinds if already running).
    pub fn start_osc(&mut self, port: u16) -> std::io::Result<()> {
        self.stop_osc();
        let socket = UdpSocket::bind(("0.0.0.0", port))?;
        socket.set_read_timeout(Some(Duration::from_millis(200)))?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let queue = self.queue.clone();
        let mapping = self.mapping.clone();
        std::thread::Builder::new()
            .name("vance.osc".into())
            .spawn(move || {
                let mut buf = [0u8; 2048];
                while !stop_thread.load(Ordering::Acquire) {
                    match socket.recv_from(&mut buf) {
                        Ok((n, _)) => {
                            if let Some(msg) = osc::decode_message(&buf[..n]) {
                                if let Some(ev) = osc::to_control_event(&msg) {
                                    if let Some(action) = mapping.map(&ev) {
                                        push(&queue, action);
                                    }
                                }
                            }
                        }
                        // Timeout → re-check the stop flag; other errors → keep going.
                        Err(_) => {}
                    }
                }
            })?;
        self.osc_stop = Some(stop);
        self.osc_port = Some(port);
        Ok(())
    }

    /// Stop the OSC listener (idempotent).
    pub fn stop_osc(&mut self) {
        if let Some(stop) = self.osc_stop.take() {
            stop.store(true, Ordering::Release);
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
        let queue = self.queue.clone();
        let mapping = self.mapping.clone();
        let conn = input
            .connect(
                port,
                "vance-midi-in",
                move |_stamp, bytes, _| {
                    if let Some(ev) = vance_control::midi::decode(bytes) {
                        if let Some(action) = mapping.map(&ev) {
                            push(&queue, action);
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
