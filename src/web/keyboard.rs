//! Bindings and wrapper for the all-around-keyboard web component.

use std::sync::Arc;

use dominator::{clone, html, Dom};
use web_sys::js_sys::{Array, Reflect};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::HtmlElement;

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

/// Call a method on the keyboard element with an array of indices.
fn call_keyboard_method(method: &str, indices: &[u8]) {
    if let Some(kb) = get_keyboard() {
        let array = Array::new();
        for &idx in indices {
            array.push(&JsValue::from(idx));
        }
        if let Ok(func) = Reflect::get(&kb, &JsValue::from_str(method)) {
            if let Some(func) = func.dyn_ref::<web_sys::js_sys::Function>() {
                let _ = func.call1(&kb, &array);
            }
        }
    }
}

/// Set an attribute on the keyboard element.
fn set_keyboard_attr(attr: &str, value: &str) {
    if let Some(kb) = get_keyboard() {
        let _ = kb.set_attribute(attr, value);
    }
}

/// Press keys on the keyboard (active/toggled state).
pub fn keys_press(indices: &[u8]) {
    call_keyboard_method("keysPress", indices);
}

/// Release keys on the keyboard.
pub fn keys_release(indices: &[u8]) {
    call_keyboard_method("keysRelease", indices);
}

/// Light keys on the keyboard (detected pitch state).
pub fn notes_light(notes: &[u8]) {
    call_keyboard_method("notesLight", notes);
}

/// Dim keys on the keyboard.
pub fn notes_dim(notes: &[u8]) {
    call_keyboard_method("notesDim", notes);
}


/// Dim all lit keys.
pub fn dim_all() {
    if let Some(kb) = get_keyboard() {
        if let Ok(func) = Reflect::get(&kb, &JsValue::from_str("dimAll")) {
            if let Some(func) = func.dyn_ref::<web_sys::js_sys::Function>() {
                let _ = func.call0(&kb);
            }
        }
    }
}

/// Sync keyboard pressed state with active pitches in room.
pub fn sync_active_pitches(state: &Arc<AppState>) {
    let room = state.room.lock_ref();
    let sets = room.all_peer_sets();
    let peer_id = room.local_peer_id();

    if let Some(set) = sets.get(peer_id) {
        let active: Vec<u8> = set.pitch_classes.iter().map(|pc| pc.index()).collect();
        if !active.is_empty() {
            keys_press(&active);
        }
    }
}

/// Update keyboard to match tuning.
pub fn update_tuning(tuning: &Tuning) {
    let count = tuning.pitch_class_count();
    set_keyboard_attr("notes-in-octave", &count.to_string());

    // Compute raised notes pattern
    let raised = compute_raised_notes(tuning);
    let raised_json = format!("[{}]", raised.iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(","));
    set_keyboard_attr("raised-notes", &raised_json);

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
        // Could implement MOS pattern detection here in the future
        vec![]
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
            // Match UI accent colors
            .attr("pressed-fill", "#e94560")
            .attr("lit-fill", "#4ade80")
        }))
        // Set up event listener for keypress events
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
        // Listen for keypress events to toggle pitch classes
        let state_press = state.clone();
        let on_keypress = Closure::<dyn Fn(web_sys::Event)>::new(move |event: web_sys::Event| {
            // Get the key index from the event
            if let Ok(index) = Reflect::get(&event, &JsValue::from_str("index")) {
                if let Some(idx) = index.as_f64() {
                    let tuning = state_press.tuning.lock_ref();
                    let count = tuning.pitch_class_count() as u8;
                    let note = (idx as u8) % count;
                    let pc = PitchClass::new(note);
                    drop(tuning);

                    // Toggle the pitch in room state
                    let is_active = state_press.room.lock_mut().toggle_pitch(pc);

                    // Update keyboard visual state
                    if is_active {
                        keys_press(&[note]);
                    } else {
                        keys_release(&[note]);
                    }
                }
            }
        });

        let _ = kb.add_event_listener_with_callback(
            "keypress",
            on_keypress.as_ref().unchecked_ref(),
        );
        on_keypress.forget(); // Leak the closure to keep it alive
    }
}
