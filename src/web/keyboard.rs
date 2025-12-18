//! Bindings and wrapper for the all-around-keyboard web component.
//!
//! New declarative API:
//! - State in via attributes: `pressed-notes`, `lit-notes`
//! - Events out: `keyclick`, `keyhover`, `keyunhover`

use std::sync::Arc;

use dominator::{clone, html, Dom};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::HtmlElement;
use web_sys::js_sys::Reflect;

use crate::room::RoomState;
use crate::tuning::{PitchClass, Tuning};

use super::app::AppState;

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

/// Sync keyboard state with active pitches.
/// - Manual pitches show as "pressed" (red)
/// - Voice pitch shows as "lit" (green)
pub fn sync_active_pitches(state: &Arc<AppState>) {
    let room = state.room.lock_ref();
    let sets = room.all_peer_sets();
    let peer_id = room.local_peer_id();

    // Manual pitches from room -> pressed (red)
    let manual: Vec<u8> = if let Some(set) = sets.get(peer_id) {
        set.pitch_classes.iter().map(|pc| pc.index()).collect()
    } else {
        vec![]
    };
    set_pressed_notes(&manual);

    // Voice pitch -> lit (green)
    if let Some(voice_pc) = state.voice_pitch.get() {
        set_lit_notes(&[voice_pc.index()]);
    } else {
        set_lit_notes(&[]);
    }
}

/// Create the all-around-keyboard component wrapper.
pub fn pitch_keyboard(state: Arc<AppState>) -> Dom {
    html!("div", {
        .class("keyboard-container")
        .child(html!("all-around-keyboard", {
            .attr("notes-in-octave", "12")
            .attr("raised-notes", "[1,3,6,8,10]")
            .attr("octaves", "1")
            .attr("sweep", "360")
            .attr("width", "800")
            .attr("depth", "280")
            .attr("pressed-notes", "[]")
            .attr("lit-notes", "[]")
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
                        } else {
                            // Toggle the pitch in room state (manual pitches)
                            state_click.room.lock_mut().toggle_pitch(pc);
                            // Touch voice_pitch to trigger list update
                            let vp = state_click.voice_pitch.get();
                            state_click.voice_pitch.set(vp);
                        }

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
