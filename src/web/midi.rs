//! Web MIDI input/output handling for walkie-songie.
//!
//! Two-channel MIDI model:
//! - Channel 1 (0): Toggle set (collaborative CRDT, pitch classes as notes)
//! - Channel 2 (1): Voice pitches (per-peer, full MIDI notes with octave)
//!
//! Input from MIDI controllers routes to the toggle set.
//! Output sends toggle set changes on ch1 and voice pitches on ch2.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::OnceLock;

use async_channel::{Receiver, Sender};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{MidiAccess, MidiInput, MidiMessageEvent, MidiOutput};

/// MIDI channel for toggle set (pitch classes).
pub const CHANNEL_TOGGLE_SET: u8 = 0;

/// MIDI channel for voice pitches.
pub const CHANNEL_VOICE: u8 = 1;

/// Default velocity for note-on messages.
pub const DEFAULT_VELOCITY: u8 = 100;

/// MIDI message types.
#[derive(Debug, Clone)]
pub enum MidiMessage {
    NoteOn { channel: u8, note: u8, velocity: u8 },
    NoteOff { channel: u8, note: u8 },
    AllNotesOff { channel: u8 },
}

/// Global channel for receiving MIDI input messages.
static MIDI_INPUT_CHANNEL: OnceLock<(Sender<MidiInputEvent>, Receiver<MidiInputEvent>)> =
    OnceLock::new();

fn get_midi_input_channel() -> &'static (Sender<MidiInputEvent>, Receiver<MidiInputEvent>) {
    MIDI_INPUT_CHANNEL.get_or_init(|| async_channel::unbounded())
}

/// MIDI input event (note on/off from controller).
#[derive(Debug, Clone)]
pub struct MidiInputEvent {
    pub note: u8,
    pub velocity: u8,
    pub is_note_on: bool,
}

/// MIDI output state - tracks what notes are currently playing.
pub struct MidiOutputState {
    /// Currently playing notes on toggle set channel.
    toggle_notes: HashSet<u8>,
    /// Currently playing notes on voice channel (per-peer, keyed by peer_id suffix).
    voice_notes: HashSet<u8>,
    /// The active MIDI output device.
    output: Option<MidiOutput>,
    /// Device name for display.
    pub device_name: Option<String>,
}

impl MidiOutputState {
    pub fn new() -> Self {
        Self {
            toggle_notes: HashSet::new(),
            voice_notes: HashSet::new(),
            output: None,
            device_name: None,
        }
    }

    /// Send a MIDI message to the output device.
    fn send(&self, msg: &MidiMessage) {
        if let Some(ref output) = self.output {
            let data = match msg {
                MidiMessage::NoteOn {
                    channel,
                    note,
                    velocity,
                } => vec![0x90 | (channel & 0x0F), *note, *velocity],
                MidiMessage::NoteOff { channel, note } => {
                    vec![0x80 | (channel & 0x0F), *note, 0]
                }
                MidiMessage::AllNotesOff { channel } => {
                    // CC 123 (All Notes Off)
                    vec![0xB0 | (channel & 0x0F), 123, 0]
                }
            };

            let array = js_sys::Uint8Array::from(&data[..]);
            if let Err(e) = output.send(&array) {
                web_sys::console::error_1(&format!("MIDI send error: {:?}", e).into());
            }
        }
    }

    /// Send note-on for toggle set channel.
    pub fn toggle_note_on(&mut self, note: u8) {
        if !self.toggle_notes.contains(&note) {
            self.toggle_notes.insert(note);
            self.send(&MidiMessage::NoteOn {
                channel: CHANNEL_TOGGLE_SET,
                note,
                velocity: DEFAULT_VELOCITY,
            });
        }
    }

    /// Send note-off for toggle set channel.
    pub fn toggle_note_off(&mut self, note: u8) {
        if self.toggle_notes.remove(&note) {
            self.send(&MidiMessage::NoteOff {
                channel: CHANNEL_TOGGLE_SET,
                note,
            });
        }
    }

    /// Sync toggle set notes with current state (send offs for removed, ons for added).
    pub fn sync_toggle_notes(&mut self, current: &HashSet<u8>) {
        // Send note-offs for notes no longer in set
        let to_remove: Vec<u8> = self.toggle_notes.difference(current).copied().collect();
        for note in to_remove {
            self.toggle_note_off(note);
        }

        // Send note-ons for new notes
        let to_add: Vec<u8> = current.difference(&self.toggle_notes).copied().collect();
        for note in to_add {
            self.toggle_note_on(note);
        }
    }

    /// Send note-on for voice channel.
    pub fn voice_note_on(&mut self, note: u8) {
        if !self.voice_notes.contains(&note) {
            // Turn off previous voice note (single voice per peer)
            let old_notes: Vec<u8> = self.voice_notes.iter().copied().collect();
            for old_note in old_notes {
                self.voice_note_off(old_note);
            }

            self.voice_notes.insert(note);
            self.send(&MidiMessage::NoteOn {
                channel: CHANNEL_VOICE,
                note,
                velocity: DEFAULT_VELOCITY,
            });
        }
    }

    /// Send note-off for voice channel.
    pub fn voice_note_off(&mut self, note: u8) {
        if self.voice_notes.remove(&note) {
            self.send(&MidiMessage::NoteOff {
                channel: CHANNEL_VOICE,
                note,
            });
        }
    }

    /// Clear all voice notes (for when stopping singing).
    pub fn clear_voice_notes(&mut self) {
        let notes: Vec<u8> = self.voice_notes.iter().copied().collect();
        for note in notes {
            self.voice_note_off(note);
        }
    }

    /// Send all-notes-off on both channels (for graceful disconnect).
    pub fn all_notes_off(&mut self) {
        self.send(&MidiMessage::AllNotesOff {
            channel: CHANNEL_TOGGLE_SET,
        });
        self.send(&MidiMessage::AllNotesOff {
            channel: CHANNEL_VOICE,
        });
        self.toggle_notes.clear();
        self.voice_notes.clear();
    }

    /// Set the MIDI output device.
    pub fn set_output(&mut self, output: Option<MidiOutput>, name: Option<String>) {
        // Send all-notes-off on old device before switching
        if self.output.is_some() {
            self.all_notes_off();
        }

        self.output = output;
        self.device_name = name;

        // Clear tracking (new device starts fresh)
        self.toggle_notes.clear();
        self.voice_notes.clear();
    }

    /// Check if output is connected.
    pub fn is_connected(&self) -> bool {
        self.output.is_some()
    }
}

impl Default for MidiOutputState {
    fn default() -> Self {
        Self::new()
    }
}

/// A MIDI device descriptor.
#[derive(Debug, Clone)]
pub struct MidiDevice {
    pub id: String,
    pub name: String,
}

/// MIDI manager - handles input/output device connections.
pub struct MidiManager {
    /// MIDI access object from Web MIDI API.
    access: Option<MidiAccess>,
    /// Available input devices.
    pub available_inputs: Vec<MidiDevice>,
    /// Available output devices.
    pub available_outputs: Vec<MidiDevice>,
    /// Currently selected input device ID (None = all inputs).
    pub selected_input: Option<String>,
    /// Currently selected output device ID (None = disabled).
    pub selected_output: Option<String>,
    /// Input device closures (kept alive).
    _input_closures: Vec<Closure<dyn FnMut(MidiMessageEvent)>>,
    /// Statechange closure (kept alive for hot-plug support).
    _statechange_closure: Option<Closure<dyn FnMut(web_sys::Event)>>,
    /// Output state.
    pub output: Rc<RefCell<MidiOutputState>>,
}

impl MidiManager {
    pub fn new() -> Self {
        Self {
            access: None,
            available_inputs: Vec::new(),
            available_outputs: Vec::new(),
            selected_input: None,
            selected_output: None,
            _input_closures: Vec::new(),
            _statechange_closure: None,
            output: Rc::new(RefCell::new(MidiOutputState::new())),
        }
    }

    /// Initialize Web MIDI access (async).
    /// Enumerates devices but doesn't connect to any by default.
    pub async fn init(&mut self) -> Result<(), String> {
        let window = web_sys::window().ok_or("No window")?;
        let navigator = window.navigator();

        // Check if Web MIDI is supported
        let request_midi_access = js_sys::Reflect::get(&navigator, &"requestMIDIAccess".into())
            .map_err(|_| "Web MIDI not supported")?;

        if !request_midi_access.is_function() {
            return Err("Web MIDI not available".to_string());
        }

        // Request MIDI access
        let midi_options = web_sys::MidiOptions::new();
        let promise = navigator
            .request_midi_access_with_options(&midi_options)
            .map_err(|e| format!("MIDI access request failed: {:?}", e))?;

        let midi_access: MidiAccess = JsFuture::from(promise)
            .await
            .map_err(|e| format!("MIDI access denied: {:?}", e))?
            .dyn_into()
            .map_err(|_| "Invalid MIDI access object")?;

        // Enumerate available devices
        self.enumerate_devices(&midi_access)?;

        self.access = Some(midi_access);
        web_sys::console::log_1(
            &format!(
                "Web MIDI initialized: {} inputs, {} outputs available",
                self.available_inputs.len(),
                self.available_outputs.len()
            )
            .into(),
        );

        Ok(())
    }

    /// Initialize with callback for device changes (hot-plug support).
    pub async fn init_with_callback<F>(&mut self, on_devices_changed: Rc<F>) -> Result<(), String>
    where
        F: Fn() + 'static,
    {
        let window = web_sys::window().ok_or("No window")?;
        let navigator = window.navigator();

        // Check if Web MIDI is supported
        let request_midi_access = js_sys::Reflect::get(&navigator, &"requestMIDIAccess".into())
            .map_err(|_| "Web MIDI not supported")?;

        if !request_midi_access.is_function() {
            return Err("Web MIDI not available".to_string());
        }

        // Request MIDI access
        let midi_options = web_sys::MidiOptions::new();
        let promise = navigator
            .request_midi_access_with_options(&midi_options)
            .map_err(|e| format!("MIDI access request failed: {:?}", e))?;

        let midi_access: MidiAccess = JsFuture::from(promise)
            .await
            .map_err(|e| format!("MIDI access denied: {:?}", e))?
            .dyn_into()
            .map_err(|_| "Invalid MIDI access object")?;

        // Enumerate available devices
        self.enumerate_devices(&midi_access)?;

        // Set up statechange listener for hot-plug support
        let callback = on_devices_changed.clone();
        let statechange_closure = Closure::wrap(Box::new(move |_event: web_sys::Event| {
            callback();
        }) as Box<dyn FnMut(web_sys::Event)>);

        midi_access.set_onstatechange(Some(statechange_closure.as_ref().unchecked_ref()));
        self._statechange_closure = Some(statechange_closure);

        self.access = Some(midi_access);

        web_sys::console::log_1(
            &format!(
                "Web MIDI initialized with hot-plug: {} inputs, {} outputs",
                self.available_inputs.len(),
                self.available_outputs.len()
            )
            .into(),
        );

        // Trigger initial callback so UI updates
        on_devices_changed();

        Ok(())
    }

    /// Re-enumerate devices (call after statechange event).
    pub fn refresh_devices(&mut self) {
        if let Some(access) = self.access.clone() {
            if let Err(e) = self.enumerate_devices(&access) {
                web_sys::console::warn_1(&format!("MIDI refresh error: {}", e).into());
            }
        }
    }

    /// Enumerate available MIDI devices without connecting.
    fn enumerate_devices(&mut self, midi_access: &MidiAccess) -> Result<(), String> {
        // Enumerate inputs
        self.available_inputs.clear();
        let inputs = midi_access.inputs();
        if let Some(iter) = js_sys::try_iter(&inputs).ok().flatten() {
            for entry in iter {
                if let Ok(entry) = entry {
                    let array: js_sys::Array = entry.dyn_into().unwrap_or_default();
                    let id = array.get(0).as_string().unwrap_or_default();
                    if let Ok(input) = array.get(1).dyn_into::<MidiInput>() {
                        let name = input.name().unwrap_or_else(|| "Unknown".to_string());
                        self.available_inputs.push(MidiDevice { id, name });
                    }
                }
            }
        }

        // Enumerate outputs
        self.available_outputs.clear();
        let outputs = midi_access.outputs();
        if let Some(iter) = js_sys::try_iter(&outputs).ok().flatten() {
            for entry in iter {
                if let Ok(entry) = entry {
                    let array: js_sys::Array = entry.dyn_into().unwrap_or_default();
                    let id = array.get(0).as_string().unwrap_or_default();
                    if let Ok(output) = array.get(1).dyn_into::<MidiOutput>() {
                        let name = output.name().unwrap_or_else(|| "Unknown".to_string());
                        self.available_outputs.push(MidiDevice { id, name });
                    }
                }
            }
        }

        Ok(())
    }

    /// Select and connect to a specific input device (or None to disconnect).
    pub fn select_input(&mut self, device_id: Option<String>) -> Result<(), String> {
        // Clear existing input connections
        self._input_closures.clear();

        self.selected_input = device_id.clone();

        if let Some(id) = device_id {
            // Clone to avoid borrow conflict with &mut self
            let midi_access = self.access.clone().ok_or("MIDI not initialized")?;
            self.connect_input_by_id(&midi_access, &id)?;
        }

        Ok(())
    }

    /// Select and connect to a specific output device (or None to disconnect).
    pub fn select_output(&mut self, device_id: Option<String>) -> Result<(), String> {
        // Send all notes off on current output before switching
        self.output.borrow_mut().all_notes_off();

        self.selected_output = device_id.clone();

        if let Some(id) = device_id {
            // Clone to avoid borrow conflict with &mut self
            let midi_access = self.access.clone().ok_or("MIDI not initialized")?;
            self.connect_output_by_id(&midi_access, &id)?;
        } else {
            self.output.borrow_mut().set_output(None, None);
        }

        Ok(())
    }

    /// Connect to a specific input device by ID.
    fn connect_input_by_id(
        &mut self,
        midi_access: &MidiAccess,
        device_id: &str,
    ) -> Result<(), String> {
        let inputs = midi_access.inputs();
        let input_iterator = js_sys::try_iter(&inputs)
            .map_err(|_| "Failed to iterate MIDI inputs")?
            .ok_or("No MIDI input iterator")?;

        let (sender, _) = get_midi_input_channel();

        for input_entry in input_iterator {
            let input_entry = input_entry.map_err(|_| "Invalid input entry")?;
            let input_array: js_sys::Array =
                input_entry.dyn_into().map_err(|_| "Invalid input array")?;
            let id = input_array.get(0).as_string().unwrap_or_default();

            if id == device_id {
                let midi_input: MidiInput = input_array
                    .get(1)
                    .dyn_into()
                    .map_err(|_| "Invalid MIDI input")?;
                let name = midi_input.name().unwrap_or_else(|| "Unknown".to_string());
                web_sys::console::log_1(&format!("MIDI input connected: {}", name).into());

                let tx = sender.clone();
                let onmidimessage = Closure::wrap(Box::new(move |event: MidiMessageEvent| {
                    if let Ok(data) = event.data() {
                        if data.len() >= 3 {
                            let status = data[0] & 0xF0;
                            match status {
                                0x90 => {
                                    let _ = tx.try_send(MidiInputEvent {
                                        note: data[1],
                                        velocity: data[2],
                                        is_note_on: data[2] > 0,
                                    });
                                }
                                0x80 => {
                                    let _ = tx.try_send(MidiInputEvent {
                                        note: data[1],
                                        velocity: 0,
                                        is_note_on: false,
                                    });
                                }
                                _ => {}
                            }
                        }
                    }
                })
                    as Box<dyn FnMut(MidiMessageEvent)>);

                midi_input.set_onmidimessage(Some(onmidimessage.as_ref().unchecked_ref()));
                self._input_closures.push(onmidimessage);
                return Ok(());
            }
        }

        Err(format!("Input device not found: {}", device_id))
    }

    /// Connect to a specific output device by ID.
    fn connect_output_by_id(
        &mut self,
        midi_access: &MidiAccess,
        device_id: &str,
    ) -> Result<(), String> {
        let outputs = midi_access.outputs();
        let output_iterator = js_sys::try_iter(&outputs)
            .map_err(|_| "Failed to iterate MIDI outputs")?
            .ok_or("No MIDI output iterator")?;

        for output_entry in output_iterator {
            let output_entry = output_entry.map_err(|_| "Invalid output entry")?;
            let output_array: js_sys::Array = output_entry
                .dyn_into()
                .map_err(|_| "Invalid output array")?;
            let id = output_array.get(0).as_string().unwrap_or_default();

            if id == device_id {
                let midi_output: MidiOutput = output_array
                    .get(1)
                    .dyn_into()
                    .map_err(|_| "Invalid MIDI output")?;
                let name = midi_output.name().unwrap_or_else(|| "Unknown".to_string());
                web_sys::console::log_1(&format!("MIDI output connected: {}", name).into());
                self.output
                    .borrow_mut()
                    .set_output(Some(midi_output), Some(name));
                return Ok(());
            }
        }

        Err(format!("Output device not found: {}", device_id))
    }

    /// Legacy: Enable MIDI output by connecting to the first available device.
    pub fn enable_output(&mut self) -> Result<(), String> {
        if let Some(first) = self.available_outputs.first() {
            self.select_output(Some(first.id.clone()))
        } else {
            Err("No MIDI output devices available".to_string())
        }
    }

    /// Connect to all available MIDI input devices.
    #[allow(dead_code)]
    fn connect_inputs(&mut self, midi_access: &MidiAccess) -> Result<(), String> {
        let inputs = midi_access.inputs();
        let input_iterator = js_sys::try_iter(&inputs)
            .map_err(|_| "Failed to iterate MIDI inputs")?
            .ok_or("No MIDI input iterator")?;

        let (sender, _) = get_midi_input_channel();

        for input_entry in input_iterator {
            let input_entry = input_entry.map_err(|_| "Invalid input entry")?;
            let input_array: js_sys::Array =
                input_entry.dyn_into().map_err(|_| "Invalid input array")?;
            let input_value = input_array.get(1);
            let midi_input: MidiInput = input_value.dyn_into().map_err(|_| "Invalid MIDI input")?;

            let name = midi_input.name().unwrap_or_else(|| "Unknown".to_string());
            web_sys::console::log_1(&format!("MIDI input found: {}", name).into());

            // Clone sender for closure
            let tx = sender.clone();

            // Set up message handler
            let onmidimessage = Closure::wrap(Box::new(move |event: MidiMessageEvent| {
                if let Ok(data) = event.data() {
                    if data.len() >= 3 {
                        let status = data[0] & 0xF0;
                        let _channel = data[0] & 0x0F;

                        match status {
                            0x90 => {
                                // Note On
                                let _ = tx.try_send(MidiInputEvent {
                                    note: data[1],
                                    velocity: data[2],
                                    is_note_on: data[2] > 0,
                                });
                            }
                            0x80 => {
                                // Note Off
                                let _ = tx.try_send(MidiInputEvent {
                                    note: data[1],
                                    velocity: 0,
                                    is_note_on: false,
                                });
                            }
                            _ => {}
                        }
                    }
                }
            }) as Box<dyn FnMut(MidiMessageEvent)>);

            midi_input.set_onmidimessage(Some(onmidimessage.as_ref().unchecked_ref()));
            self._input_closures.push(onmidimessage);
        }

        Ok(())
    }

    /// Connect to the first available MIDI output device.
    #[allow(dead_code)]
    fn connect_first_output(&mut self, midi_access: &MidiAccess) -> Result<(), String> {
        let outputs = midi_access.outputs();
        let output_iterator = js_sys::try_iter(&outputs)
            .map_err(|_| "Failed to iterate MIDI outputs")?
            .ok_or("No MIDI output iterator")?;

        for output_entry in output_iterator {
            let output_entry = output_entry.map_err(|_| "Invalid output entry")?;
            let output_array: js_sys::Array = output_entry
                .dyn_into()
                .map_err(|_| "Invalid output array")?;
            let output_value = output_array.get(1);
            let midi_output: MidiOutput =
                output_value.dyn_into().map_err(|_| "Invalid MIDI output")?;

            let name = midi_output.name().unwrap_or_else(|| "Unknown".to_string());
            web_sys::console::log_1(&format!("MIDI output connected: {}", name).into());

            self.output
                .borrow_mut()
                .set_output(Some(midi_output), Some(name));

            // Only connect to first output
            break;
        }

        Ok(())
    }

    /// Get receiver for MIDI input events.
    pub fn input_receiver() -> &'static Receiver<MidiInputEvent> {
        let (_, receiver) = get_midi_input_channel();
        receiver
    }

    /// Poll for MIDI input events (non-blocking).
    pub fn poll_input() -> Option<MidiInputEvent> {
        let receiver = Self::input_receiver();
        receiver.try_recv().ok()
    }
}

impl Default for MidiManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Initialize MIDI with a callback for device changes (hot-plug support).
pub fn init_midi_with_callback<F>(manager: Rc<RefCell<MidiManager>>, on_devices_changed: F)
where
    F: Fn() + 'static,
{
    let manager_clone = manager.clone();
    let callback = Rc::new(on_devices_changed);

    spawn_local(async move {
        let result = manager_clone
            .borrow_mut()
            .init_with_callback(callback)
            .await;
        if let Err(e) = result {
            web_sys::console::warn_1(&format!("MIDI init warning: {}", e).into());
        }
    });
}

/// Map a standard 12-TET degree into the C4 MIDI octave.
///
/// Arbitrary tunings require MPE/pitch bend in the native backend; silently
/// folding them into twelve notes is forbidden.
pub fn pitch_class_to_midi_note(pitch_class: u16, pitch_count: u16) -> Option<u8> {
    (pitch_count == 12 && pitch_class < 12).then_some(60 + pitch_class as u8)
}
