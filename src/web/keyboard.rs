//! Bindings and wrapper for the all-around-keyboard web component.
//!
//! Declarative API:
//! - State in via attributes: `pressed-notes`, `lit-notes`
//! - Indicator children: `data-pitch`, `data-key`, `data-radius`
//! - Events out: `keyclick`, `keyhover`, `keyunhover`

use std::sync::Arc;

use dominator::{clone, html, Dom};
use futures_signals::signal::SignalExt as _;
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
/// - Combined pitches from all peers + pieces show as "pressed" (red)
/// - Local voice pitch shows as "lit" (green)
pub fn sync_active_pitches(state: &Arc<AppState>) {
    let room = state.room.lock_ref();
    let tuning = state.tuning.lock_ref();
    let pc_count = tuning.pitch_class_count() as i32;
    drop(tuning);

    // Get combined pitch result from all peers (uses combination method)
    let room_result = room.compute_room_result();

    // Start with pitches from peers
    let mut combined: Vec<u8> = room_result.pitch_classes.iter().map(|pc| pc.index()).collect();

    // Add piece pitch classes
    let pieces = room.all_pieces();
    for piece in &pieces {
        let pc = piece.pitch.rem_euclid(pc_count) as u8;
        if !combined.contains(&pc) {
            combined.push(pc);
        }
    }

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

/// Maximum allowed drag distance in semitones before snapping back.
const MAX_DRAG_SEMITONES: i32 = 4;

/// Leftmost key index for the keyboard (C3 = MIDI 36)
const LEFTMOST_KEY: i32 = 36;

/// Calculate the shortest delta between two pitch classes around the circle.
fn shortest_delta(from_pc: i32, to_pc: i32, pc_count: i32) -> i32 {
    let mut delta = to_pc - from_pc;
    if delta > pc_count / 2 {
        delta -= pc_count;
    } else if delta < -pc_count / 2 {
        delta += pc_count;
    }
    delta
}

/// Create the all-around-keyboard component with voice indicator and pieces as slotted children.
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

        .after_inserted(clone!(state => move |el| {
            setup_keyboard_events(state.clone());
            // Initial sync with tuning
            let tuning = state.tuning.lock_ref();
            update_tuning(&tuning);
            drop(tuning);
            // Initial sync of active pitches
            sync_active_pitches(&state);
            // Set up efficient piece management
            setup_piece_sync(state.clone(), el);
        }))
    })
}

/// Set up efficient piece synchronization that only updates changed pieces.
fn setup_piece_sync(state: Arc<AppState>, keyboard_el: web_sys::HtmlElement) {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    // Track existing piece elements by ID
    let piece_elements: Rc<RefCell<HashMap<String, web_sys::HtmlElement>>> = Rc::new(RefCell::new(HashMap::new()));

    // Spawn a future that watches room_version and syncs pieces
    let piece_elements_clone = piece_elements.clone();
    let state_clone = state.clone();
    let keyboard_el_clone = keyboard_el.clone();

    wasm_bindgen_futures::spawn_local(async move {
        let signal = state_clone.room_version.signal();

        // Process each room_version change
        futures_signals::signal::SignalExt::for_each(signal, move |_version| {
            // Get current pieces from room
            let (pieces, pitch_count, piece_mode) = {
                let tuning = state_clone.tuning.lock_ref();
                let pitch_count = tuning.pitch_class_count();
                drop(tuning);
                let room = state_clone.room.lock_ref();
                let pieces = room.all_pieces();
                let piece_mode = state_clone.piece_mode.get();
                (pieces, pitch_count, piece_mode)
            };

            let mut elements = piece_elements_clone.borrow_mut();

            if !piece_mode {
                // Not in piece mode - remove all piece elements
                for (_, el) in elements.drain() {
                    el.remove();
                }
            } else {
                // Build set of current piece IDs
                let current_ids: std::collections::HashSet<String> = pieces.iter().map(|p| p.id.clone()).collect();

                // Remove pieces that no longer exist
                let to_remove: Vec<String> = elements.keys()
                    .filter(|id| !current_ids.contains(*id))
                    .cloned()
                    .collect();
                for id in to_remove {
                    if let Some(el) = elements.remove(&id) {
                        el.remove();
                    }
                }

                // Add or update pieces
                for piece in pieces {
                    let pitch_class = piece.pitch.rem_euclid(pitch_count as i32);
                    let key_index = LEFTMOST_KEY + pitch_class;

                    if let Some(el) = elements.get(&piece.id) {
                        // Piece exists - just update data-key if pitch changed
                        let current_key = el.get_attribute("data-key")
                            .and_then(|s| s.parse::<i32>().ok())
                            .unwrap_or(-1);
                        if current_key != key_index {
                            el.set_attribute("data-key", &key_index.to_string()).ok();
                            el.set_attribute("data-original-pitch", &piece.pitch.to_string()).ok();
                        }
                    } else {
                        // New piece - create element
                        let el = create_piece_element(&piece.id, piece.pitch, key_index);
                        keyboard_el_clone.append_child(&el).ok();
                        elements.insert(piece.id.clone(), el);
                    }
                }
            }

            // Return a ready future (for_each needs FnMut -> Future)
            async {}
        }).await;
    });
}

/// Create a piece indicator DOM element (raw web_sys version for efficient updates)
fn create_piece_element(piece_id: &str, original_pitch: i32, key_index: i32) -> web_sys::HtmlElement {
    let document = web_sys::window().unwrap().document().unwrap();
    let el: web_sys::HtmlElement = document.create_element("div").unwrap().unchecked_into();

    el.set_class_name("piece-indicator");
    el.set_attribute("data-piece-id", piece_id).ok();
    el.set_attribute("data-key", &key_index.to_string()).ok();
    el.set_attribute("data-original-pitch", &original_pitch.to_string()).ok();

    // Inline styles
    let style = el.style();
    style.set_property("width", "32px").ok();
    style.set_property("height", "32px").ok();
    style.set_property("background", "#8b5cf6").ok();
    style.set_property("border-radius", "50%").ok();
    style.set_property("cursor", "grab").ok();
    style.set_property("user-select", "none").ok();
    style.set_property("box-shadow", "0 0 12px rgba(139, 92, 246, 0.6), 0 2px 8px rgba(0, 0, 0, 0.3)").ok();
    style.set_property("pointer-events", "auto").ok();

    // Set up drag handler
    setup_piece_drag_handler(&el, piece_id, original_pitch, key_index);

    el
}

/// Set up drag handling for a piece element
fn setup_piece_drag_handler(el: &web_sys::HtmlElement, piece_id: &str, original_pitch: i32, _key_index: i32) {
    let piece_id = piece_id.to_string();
    let el_clone = el.clone();

    let on_down = Closure::<dyn Fn(web_sys::PointerEvent)>::new(move |e: web_sys::PointerEvent| {
        // Store drag info on document body for document-level handlers
        if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
            let body = doc.body().unwrap();
            body.set_attribute("data-dragging-piece", &piece_id).ok();
            body.set_attribute("data-drag-start-pitch", &original_pitch.to_string()).ok();
        }

        el_clone.set_attribute("data-dragging", "true").ok();
        // Switch to fixed positioning for smooth drag
        el_clone.style().set_property("position", "fixed").ok();
        el_clone.style().set_property("z-index", "1000").ok();
        // pointer-events: none so events pass through to keyboard
        el_clone.style().set_property("pointer-events", "none").ok();
        // Position at cursor
        let x = e.x() as f64 - 16.0;
        let y = e.y() as f64 - 16.0;
        el_clone.style().set_property("left", &format!("{}px", x)).ok();
        el_clone.style().set_property("top", &format!("{}px", y)).ok();
        // Remove data-key so it doesn't snap back during drag
        el_clone.remove_attribute("data-key").ok();
    });

    el.add_event_listener_with_callback("pointerdown", on_down.as_ref().unchecked_ref()).ok();
    on_down.forget();
}

/// Store the currently hovered key note (used during piece drag)
static HOVERED_KEY_NOTE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// Store the last key that received a pointerup (for drop detection)
static POINTERUP_KEY_NOTE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// Get the currently hovered key note (pitch class 0-11, or -1 if none)
pub fn get_hovered_key_note() -> Option<i32> {
    let val = HOVERED_KEY_NOTE.load(std::sync::atomic::Ordering::Relaxed);
    if val >= 0 { Some(val) } else { None }
}

/// Get and clear the last pointerup key note (pitch class 0-11, or -1 if none)
pub fn take_pointerup_key_note() -> Option<i32> {
    let val = POINTERUP_KEY_NOTE.swap(-1, std::sync::atomic::Ordering::Relaxed);
    if val >= 0 { Some(val) } else { None }
}

/// Set up document-level drag handlers for piece dragging.
/// This is needed because the piece has pointer-events: none during drag,
/// so we handle movement and drop at the document level.
fn setup_document_drag_handlers(state: Arc<AppState>) {
    let Some(window) = web_sys::window() else { return };
    let Some(document) = window.document() else { return };

    // Document-level pointermove handler for dragging pieces
    let state_move = state.clone();
    let on_move = Closure::<dyn Fn(web_sys::PointerEvent)>::new(move |e: web_sys::PointerEvent| {
        let Some(body) = web_sys::window().and_then(|w| w.document()).and_then(|d| d.body()) else { return };

        // Check if we're dragging a piece
        let Some(piece_id) = body.get_attribute("data-dragging-piece") else { return };

        // Find the piece element
        let Some(doc) = web_sys::window().and_then(|w| w.document()) else { return };
        let Some(piece_el) = doc.query_selector(&format!("[data-piece-id='{}']", piece_id)).ok().flatten() else { return };
        let Ok(piece_el) = piece_el.dyn_into::<HtmlElement>() else { return };

        // Move piece to follow cursor
        let x = e.x() as f64 - 16.0;
        let y = e.y() as f64 - 16.0;
        piece_el.style().set_property("left", &format!("{}px", x)).ok();
        piece_el.style().set_property("top", &format!("{}px", y)).ok();

        // Update hover key tracking for visual feedback
        if let Some(current_pc) = get_hovered_key_note() {
            piece_el.set_attribute("data-hover-key", &current_pc.to_string()).ok();

            // Visual feedback: change color if out of range
            if let Some(start_str) = body.get_attribute("data-drag-start-pitch") {
                if let Ok(start_pitch) = start_str.parse::<i32>() {
                    let tuning = state_move.tuning.lock_ref();
                    let pc_count = tuning.pitch_class_count() as i32;
                    drop(tuning);

                    let start_pc = start_pitch.rem_euclid(pc_count);
                    let delta = shortest_delta(start_pc, current_pc, pc_count);

                    if delta.abs() > MAX_DRAG_SEMITONES {
                        // Out of range - show warning color (dimmed)
                        piece_el.style().set_property("background", "#666").ok();
                        piece_el.style().set_property("box-shadow", "0 0 8px rgba(102, 102, 102, 0.5)").ok();
                    } else {
                        // In range - show normal purple
                        piece_el.style().set_property("background", "#8b5cf6").ok();
                        piece_el.style().set_property("box-shadow", "0 0 12px rgba(139, 92, 246, 0.6), 0 2px 8px rgba(0, 0, 0, 0.3)").ok();
                    }
                }
            }
        }
    });
    let _ = document.add_event_listener_with_callback("pointermove", on_move.as_ref().unchecked_ref());
    on_move.forget();

    // Document-level pointerup handler for dropping pieces
    let state_up = state.clone();
    let on_up = Closure::<dyn Fn(web_sys::PointerEvent)>::new(move |_e: web_sys::PointerEvent| {
        let Some(body) = web_sys::window().and_then(|w| w.document()).and_then(|d| d.body()) else { return };

        // Check if we're dragging a piece
        let Some(piece_id) = body.get_attribute("data-dragging-piece") else { return };
        let start_pitch = body.get_attribute("data-drag-start-pitch")
            .and_then(|s| s.parse::<i32>().ok())
            .unwrap_or(0);

        // Clear drag state from body
        body.remove_attribute("data-dragging-piece").ok();
        body.remove_attribute("data-drag-start-pitch").ok();

        // Find the piece element
        let Some(doc) = web_sys::window().and_then(|w| w.document()) else { return };
        let Some(piece_el) = doc.query_selector(&format!("[data-piece-id='{}']", piece_id)).ok().flatten() else { return };
        let Ok(piece_el) = piece_el.dyn_into::<HtmlElement>() else { return };

        // Get hover key from piece element (set during move)
        let hover_key = piece_el.get_attribute("data-hover-key")
            .and_then(|s| s.parse::<i32>().ok());

        // Check for keypointerup event (fires when pointer up happens over a key)
        let pointerup_key = take_pointerup_key_note();

        // Also check the global hovered key from keyhover events
        let global_hover = get_hovered_key_note();

        // Use best available drop target: pointerup > global_hover > hover_key
        let end_key = pointerup_key
            .or(global_hover)
            .or(hover_key);

        let tuning = state_up.tuning.lock_ref();
        let pc_count = tuning.pitch_class_count() as i32;
        drop(tuning);

        // Check if we have a valid drop target and compute new pitch
        let new_pitch = if let Some(end_key) = end_key {
            let start_pc = start_pitch.rem_euclid(pc_count);
            let delta = shortest_delta(start_pc, end_key, pc_count);

            if delta.abs() <= MAX_DRAG_SEMITONES && delta != 0 {
                // Valid move - update CRDT
                let new_pitch = start_pitch + delta;
                state_up.room.lock_mut().move_piece(&piece_id, new_pitch);
                new_pitch
            } else {
                start_pitch // No change
            }
        } else {
            start_pitch // No drop target
        };

        // Set data-key FIRST to avoid flash - compute from new pitch
        let pitch_class = new_pitch.rem_euclid(pc_count);
        let key_index = LEFTMOST_KEY + pitch_class;
        piece_el.set_attribute("data-key", &key_index.to_string()).ok();

        // Then reset element styling
        piece_el.remove_attribute("data-dragging").ok();
        piece_el.style().remove_property("position").ok();
        piece_el.style().remove_property("z-index").ok();
        piece_el.style().remove_property("pointer-events").ok();
        piece_el.style().remove_property("left").ok();
        piece_el.style().remove_property("top").ok();
        piece_el.style().set_property("background", "#8b5cf6").ok();
        piece_el.style().set_property("box-shadow", "0 0 12px rgba(139, 92, 246, 0.6), 0 2px 8px rgba(0, 0, 0, 0.3)").ok();
        piece_el.remove_attribute("data-hover-key").ok();

        state_up.room_version.set(state_up.room_version.get() + 1);
        sync_active_pitches(&state_up);
        // Sync MIDI output for piece changes
        state_up.sync_midi_toggle_output();
    });
    let _ = document.add_event_listener_with_callback("pointerup", on_up.as_ref().unchecked_ref());
    on_up.forget();
}

/// Set up keyboard event listeners.
fn setup_keyboard_events(state: Arc<AppState>) {
    // Set up document-level drag handlers for piece dragging
    // (piece has pointer-events: none during drag, so we handle it at document level)
    setup_document_drag_handlers(state.clone());

    if let Some(kb) = get_keyboard() {
        // Listen for keyhover events to track hovered key (used during drag)
        let on_hover = Closure::<dyn Fn(web_sys::Event)>::new(move |event: web_sys::Event| {
            if let Ok(detail) = Reflect::get(&event, &JsValue::from_str("detail")) {
                if let Ok(note_val) = Reflect::get(&detail, &JsValue::from_str("note")) {
                    if let Some(note) = note_val.as_f64() {
                        HOVERED_KEY_NOTE.store(note as i32, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }
        });
        let _ = kb.add_event_listener_with_callback("keyhover", on_hover.as_ref().unchecked_ref());
        on_hover.forget();

        // Listen for keyunhover to clear hovered key
        let on_unhover = Closure::<dyn Fn(web_sys::Event)>::new(move |_event: web_sys::Event| {
            HOVERED_KEY_NOTE.store(-1, std::sync::atomic::Ordering::Relaxed);
        });
        let _ = kb.add_event_listener_with_callback("keyunhover", on_unhover.as_ref().unchecked_ref());
        on_unhover.forget();

        // Listen for keypointerup to detect drop targets
        let on_pointerup = Closure::<dyn Fn(web_sys::Event)>::new(move |event: web_sys::Event| {
            if let Ok(detail) = Reflect::get(&event, &JsValue::from_str("detail")) {
                if let Ok(note_val) = Reflect::get(&detail, &JsValue::from_str("note")) {
                    if let Some(note) = note_val.as_f64() {
                        POINTERUP_KEY_NOTE.store(note as i32, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }
        });
        let _ = kb.add_event_listener_with_callback("keypointerup", on_pointerup.as_ref().unchecked_ref());
        on_pointerup.forget();

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

                        // Handle piece mode vs toggle mode
                        if state_click.piece_mode.get() {
                            // Piece mode: add a piece at this pitch class (default octave 4 = middle C range)
                            let absolute_pitch = 60 + note as i32; // MIDI note 60 = C4
                            state_click.room.lock_mut().add_piece(absolute_pitch);
                            // Sync MIDI output for piece changes
                            state_click.sync_midi_toggle_output();
                        } else {
                            // Toggle mode: existing behavior
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
