//! Bindings and wrapper for the all-around-keyboard web component.
//!
//! Declarative API:
//! - State in via attributes: `pressed-notes`, `lit-notes`
//! - Indicator children: `data-pitch`, `data-key`, `data-radius`
//! - Events out: `keyclick`, `keyhover`, `keyunhover`

use std::sync::Arc;

use dominator::{clone, html, Dom};
use futures_signals::signal::SignalExt;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::HtmlElement;
use web_sys::js_sys::Reflect;

use crate::room::RoomState;
use crate::tuning::{PitchClass, Tuning};

use super::app::AppState;

/// Reference frequency for A4 (standard tuning).
const A4_HZ: f64 = 440.0;

/// MIDI note number for A4.
const A4_MIDI: f64 = 69.0;

/// Get the keyboard element from the DOM.
fn get_keyboard() -> Option<HtmlElement> {
    web_sys::window()?
        .document()?
        .query_selector("all-around-keyboard")
        .ok()?
        .map(|el| el.unchecked_into())
}

/// Set an attribute on the keyboard element.
fn set_keyboard_attr(attr: &str, value: &str) {
    if let Some(kb) = get_keyboard() {
        let _ = kb.set_attribute(attr, value);
    }
}

/// Format a slice of note indices as a JSON array string.
fn notes_to_json(notes: &[u8]) -> String {
    format!("[{}]", notes.iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(","))
}

/// Set which notes are pressed (active pitches).
pub fn set_pressed_notes(notes: &[u8]) {
    set_keyboard_attr("pressed-notes", &notes_to_json(notes));
}

/// Set which notes are lit (detected pitch during singing).
pub fn set_lit_notes(notes: &[u8]) {
    set_keyboard_attr("lit-notes", &notes_to_json(notes));
}

/// Update keyboard to match tuning.
pub fn update_tuning(tuning: &Tuning) {
    let count = tuning.pitch_class_count();
    set_keyboard_attr("notes-in-octave", &count.to_string());

    // Compute raised notes pattern
    let raised = compute_raised_notes(tuning);
    set_keyboard_attr("raised-notes", &notes_to_json(&raised));

    // Use pie mode for non-12-TET (no raised keys looks better as pie)
    if raised.is_empty() {
        set_keyboard_attr("pie", "true");
    } else {
        set_keyboard_attr("pie", "false");
    }
}

/// Compute which notes should be "raised" (black keys) for a tuning.
/// For 12-TET, uses standard piano layout.
/// For other tunings, uses pie mode (no raised keys).
pub fn compute_raised_notes(tuning: &Tuning) -> Vec<u8> {
    let count = tuning.pitch_class_count();

    if count == 12 {
        // Standard piano: C# D# F# G# A# = indices 1, 3, 6, 8, 10
        vec![1, 3, 6, 8, 10]
    } else {
        // For non-12-TET, no raised keys (pie mode handles this better)
        vec![]
    }
}

/// Sync keyboard state with active pitches from ALL peers.
/// - Combined pitches from all peers show as "pressed" (red)
/// - Local voice pitch shows as "lit" (green)
pub fn sync_active_pitches(state: &Arc<AppState>) {
    let room = state.room.lock_ref();

    // Get combined pitch result from all peers (uses combination method)
    let room_result = room.compute_room_result();

    // All pitches from all peers -> pressed (red)
    let combined: Vec<u8> = room_result.pitch_classes.iter().map(|pc| pc.index()).collect();
    set_pressed_notes(&combined);

    // Local voice pitch from CRDT -> lit (green)
    let (_, local_voice_pc) = room.local_voice();
    if let Some(pc) = local_voice_pc {
        set_lit_notes(&[pc.index()]);
    } else {
        set_lit_notes(&[]);
    }
}

/// Convert Hz to a continuous pitch position (0.0 to N where N is pitch count).
fn hz_to_pitch_position(hz: f64, pitch_count: usize) -> f64 {
    // Convert Hz to MIDI note number (continuous)
    let midi = A4_MIDI + 12.0 * (hz / A4_HZ).log2();

    // For 12-TET, MIDI note 0 is C, so we can use midi % 12 directly
    // For other tunings, we need to scale
    let scale_factor = pitch_count as f64 / 12.0;
    let position = (midi * scale_factor) % pitch_count as f64;

    // Ensure positive
    if position < 0.0 {
        position + pitch_count as f64
    } else {
        position
    }
}

/// Create the all-around-keyboard component with indicator child.
pub fn pitch_keyboard(state: Arc<AppState>) -> Dom {
    html!("all-around-keyboard", {
        .class("keyboard")
        .attr("notes-in-octave", "12")
        .attr("raised-notes", "[1,3,6,8,10]")
        .attr("octaves", "1")
        .attr("sweep", "360")
        .attr("width", "800")
        .attr("depth", "280")
        .attr("pressed-notes", "[]")
        .attr("lit-notes", "[]")

        // Pitch indicator child - stable element, attributes updated reactively
        .child(html!("div", {
            .class("pitch-indicator")
            // Update data-radius reactively based on confidence (0.4 to 0.8)
            .attr_signal("data-radius", state.final_confidence.signal().map(|confidence| {
                let radius = 0.4 + confidence as f64 * 0.4;
                format!("{:.2}", radius)
            }))
            // Update data-pitch reactively
            .attr_signal("data-pitch", state.continuous_hz.signal().map(clone!(state => move |hz_opt| {
                let gate_open = state.gate_open.get();
                if !gate_open || hz_opt.is_none() {
                    return "0".to_string();
                }
                let hz = hz_opt.unwrap();
                let tuning = state.tuning.lock_ref();
                let pitch_count = tuning.pitch_class_count();
                drop(tuning);
                let pitch_position = hz_to_pitch_position(hz, pitch_count);
                format!("{:.2}", pitch_position)
            })))
            // Update visibility reactively - hide when voice button released OR gate closed
            .class_signal("hidden", state.continuous_hz.signal().map(clone!(state => move |_| {
                !state.voice_active.get() || !state.gate_open.get()
            })))
            // Update style reactively based on confidence
            .attr_signal("style", state.final_confidence.signal().map(|confidence| {
                let size = 12.0 + confidence * 12.0;
                let opacity = 0.6 + confidence * 0.4;
                let glow = (6.0 + confidence * 10.0) as i32;
                let brightness = (120.0 + confidence * 135.0) as u8;
                format!(
                    "width: {s}px; height: {s}px; opacity: {o:.2}; \
                     background: rgb({b}, 255, {b}); \
                     filter: drop-shadow(0 0 {g}px rgb({b}, 255, {b}));",
                    s = size, o = opacity, g = glow, b = brightness
                )
            }))
        }))

        .after_inserted(clone!(state => move |_| {
            setup_keyboard_events(state.clone());
            // Initial sync with tuning
            let tuning = state.tuning.lock_ref();
            update_tuning(&tuning);
            drop(tuning);
            // Initial sync of active pitches
            sync_active_pitches(&state);
        }))
    })
}

/// Set up keyboard event listeners.
fn setup_keyboard_events(state: Arc<AppState>) {
    if let Some(kb) = get_keyboard() {
        // Listen for keyclick events (actual user clicks, not hover)
        let state_click = state.clone();
        let on_click = Closure::<dyn Fn(web_sys::Event)>::new(move |event: web_sys::Event| {
            // Skip if voice input is active
            if state_click.voice_active.get() {
                return;
            }

            // Get note from event detail
            if let Ok(detail) = Reflect::get(&event, &JsValue::from_str("detail")) {
                if let Ok(note_val) = Reflect::get(&detail, &JsValue::from_str("note")) {
                    if let Some(note) = note_val.as_f64() {
                        let tuning = state_click.tuning.lock_ref();
                        let count = tuning.pitch_class_count() as u8;
                        let note = (note as u8) % count;
                        let pc = PitchClass::new(note);
                        drop(tuning);

                        // Check if this is the voice pitch
                        let is_voice_pitch = state_click.voice_pitch.get() == Some(pc);

                        if is_voice_pitch {
                            // Clicking voice pitch clears it
                            state_click.voice_pitch.set(None);
                            state_click.room.lock_mut().set_voice_pitchclass(None);
                            state_click.sync_midi_voice_output();
                        } else {
                            // Toggle the pitch in room state (manual pitches)
                            state_click.room.lock_mut().toggle_pitch(pc);
                            // Sync MIDI toggle output
                            state_click.sync_midi_toggle_output();
                        }

                        // Increment room_version to trigger UI updates
                        state_click.room_version.set(state_click.room_version.get() + 1);
                        // Re-sync keyboard to reflect new state
                        sync_active_pitches(&state_click);
                    }
                }
            }
        });

        let _ = kb.add_event_listener_with_callback(
            "keyclick",
            on_click.as_ref().unchecked_ref(),
        );
        on_click.forget();
    }
}
