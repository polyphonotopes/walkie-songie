//! Bindings and wrapper for the all-around-keyboard web component.
//!
//! Declarative API:
//! - State in via attributes: `pressed-notes`, `lit-notes`
//! - Indicator children: `data-pitch`, `data-key`, `data-radius`
//! - Events out: `keyclick`, `keyhover`, `keyunhover`

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use dominator::{Dom, clone, html};
use futures_signals::signal::SignalExt as _;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::HtmlElement;
use web_sys::js_sys::Reflect;

use crate::tuning::{PitchClass, Tuning};

use super::app::AppState;

/// State for a single pointer drag operation (existing piece)
#[derive(Clone)]
struct DragState {
    piece_id: String,
    start_pitch: i32,
    start_x: i32,
}

/// State for dragging a new emoji from the picker
#[derive(Clone)]
struct EmojiDragState {
    emoji: String,
}

thread_local! {
    /// Track active drags by pointerId for multitouch support
    static ACTIVE_DRAGS: RefCell<HashMap<i32, DragState>> = RefCell::new(HashMap::new());
    /// Track emoji drags from picker
    static EMOJI_DRAGS: RefCell<HashMap<i32, EmojiDragState>> = RefCell::new(HashMap::new());
}

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

/// Show the delete hole in the center of the keyboard.
fn show_delete_hole() {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };

    // Check if hole already exists
    if document.get_element_by_id("delete-hole").is_some() {
        return;
    }

    let Some(keyboard) = get_keyboard() else {
        return;
    };

    // Create hole element
    let hole: HtmlElement = document.create_element("div").unwrap().unchecked_into();
    hole.set_id("delete-hole");
    hole.set_class_name("delete-hole");
    hole.set_text_content(Some("🕳️"));

    // Append to keyboard
    keyboard.append_child(&hole).ok();
}

/// Hide the delete hole.
fn hide_delete_hole() {
    if let Some(hole) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("delete-hole"))
    {
        hole.remove();
    }
}

/// Check if a point is over the delete hole.
fn is_over_delete_hole(x: i32, y: i32) -> bool {
    let Some(hole) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("delete-hole"))
    else {
        return false;
    };

    let rect = hole.get_bounding_client_rect();
    let px = x as f64;
    let py = y as f64;

    px >= rect.left() && px <= rect.right() && py >= rect.top() && py <= rect.bottom()
}

/// Set an attribute on the keyboard element.
fn set_keyboard_attr(attr: &str, value: &str) {
    if let Some(kb) = get_keyboard() {
        let _ = kb.set_attribute(attr, value);
    }
}

/// Format a slice of note indices as a JSON array string.
fn notes_to_json(notes: &[u8]) -> String {
    format!(
        "[{}]",
        notes
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(",")
    )
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
    if tuning.supports_standard_note_names() {
        // Standard piano: C# D# F# G# A# = indices 1, 3, 6, 8, 10
        vec![1, 3, 6, 8, 10]
    } else {
        // For non-12-TET, no raised keys (pie mode handles this better)
        vec![]
    }
}

/// Unicode clef symbols
const BASS_CLEF: &str = "𝄢";
const TREBLE_CLEF: &str = "𝄞";

/// Sync keyboard state with active pitches from ALL peers.
/// - Toggle pitches show with sunny/manual overlay
/// - Emoji pieces show as emoji indicators on keys
/// - Voice pitches show with wavy overlay and shout emoji
/// - Bass/treble clef indicators show lowest/highest notes
pub fn sync_active_pitches(state: &Arc<AppState>) {
    let room_handle = state.room();
    let room = room_handle.lock_ref();
    let tuning = state.tuning.lock_ref();
    let pc_count = tuning.pitch_class_count() as i32;
    drop(tuning);

    // Toggle pitch classes (manual/ambient toggles)
    let toggle_notes: Vec<u8> = room
        .shared_pitches()
        .iter()
        .filter_map(|pc| u8::try_from(pc.index()).ok())
        .collect();
    sync_toggle_overlays(&toggle_notes);

    // Piece pitch classes (emoji pieces)
    let pieces = room.all_pieces();
    let piece_notes: Vec<u8> = pieces
        .iter()
        .map(|p| p.pitch.rem_euclid(pc_count) as u8)
        .collect();

    // Update piece overlays (piece-dots pattern for key highlight)
    sync_piece_overlays(&piece_notes);

    // Clear pressed notes (pieces are shown as emoji indicators)
    set_pressed_notes(&[]);

    // Voice pitch classes from all peers (wavy overlay with shout emoji)
    let voice_notes: Vec<u8> = room
        .all_voice_pitch_classes()
        .iter()
        .filter_map(|pc| u8::try_from(pc.index()).ok())
        .collect();
    sync_voice_overlays(&voice_notes);

    // Collect all absolute pitches for range indicators
    let mut all_pitches: Vec<i32> = Vec::new();

    // Voice pitches (absolute)
    for (_, (pitch_opt, _)) in room.all_voice_states() {
        if let Some(pitch) = pitch_opt {
            all_pitches.push(pitch);
        }
    }

    // Piece pitches (absolute)
    for piece in room.all_pieces() {
        all_pitches.push(piece.pitch);
    }

    // Update bass/treble clef indicators
    sync_clef_indicators(&all_pitches, pc_count);

    // Clear lit notes (we use overlays now)
    set_lit_notes(&[]);
}

/// Sync bass and treble clef indicators on the keyboard.
/// Shows bass clef at lowest note and treble clef at highest note.
fn sync_clef_indicators(pitches: &[i32], _pc_count: i32) {
    let Some(kb) = get_keyboard() else { return };
    let document = web_sys::window().unwrap().document().unwrap();

    // Helper to get or create an element
    let get_or_create = |selector: &str, class_name: &str| -> web_sys::Element {
        if let Ok(Some(el)) = kb.query_selector(selector) {
            el
        } else {
            let el = document.create_element("div").unwrap();
            el.set_class_name(class_name);
            kb.append_child(&el).ok();
            el
        }
    };

    // Get or create all elements upfront
    let bass_line = get_or_create(
        ".bass-clef-indicator-line",
        "clef-line bass-clef-indicator-line",
    );
    let bass_clef = get_or_create(".bass-clef-indicator", "clef-indicator bass-clef-indicator");
    let treble_line = get_or_create(
        ".treble-clef-indicator-line",
        "clef-line treble-clef-indicator-line",
    );
    let treble_clef = get_or_create(
        ".treble-clef-indicator",
        "clef-indicator treble-clef-indicator",
    );

    if pitches.is_empty() {
        // Hide all by adding hidden class
        bass_line.class_list().add_1("hidden").ok();
        bass_clef.class_list().add_1("hidden").ok();
        treble_line.class_list().add_1("hidden").ok();
        treble_clef.class_list().add_1("hidden").ok();
        return;
    }

    let min_pitch = *pitches.iter().min().unwrap();
    let max_pitch = *pitches.iter().max().unwrap();

    // Update bass clef
    bass_line
        .set_attribute("data-pitch", &min_pitch.to_string())
        .ok();
    bass_line.set_attribute("data-radius", "0.5").ok();
    bass_line.class_list().remove_1("hidden").ok();

    bass_clef
        .set_attribute("data-pitch", &min_pitch.to_string())
        .ok();
    bass_clef.set_attribute("data-radius", "1.1").ok();
    bass_clef.set_text_content(Some(BASS_CLEF));
    bass_clef.class_list().remove_1("hidden").ok();

    // Update treble clef (only if different from bass)
    if max_pitch != min_pitch {
        treble_line
            .set_attribute("data-pitch", &max_pitch.to_string())
            .ok();
        treble_line.set_attribute("data-radius", "0.5").ok();
        treble_line.class_list().remove_1("hidden").ok();

        treble_clef
            .set_attribute("data-pitch", &max_pitch.to_string())
            .ok();
        treble_clef.set_attribute("data-radius", "1.1").ok();
        treble_clef.set_text_content(Some(TREBLE_CLEF));
        treble_clef.class_list().remove_1("hidden").ok();
    } else {
        // Hide treble if same as bass
        treble_line.class_list().add_1("hidden").ok();
        treble_clef.class_list().add_1("hidden").ok();
    }
}

/// Sync toggle overlay elements for toggled/manual pitch classes.
/// Creates/removes overlay elements with data-key-overlay attribute and toggle-lines pattern.
fn sync_toggle_overlays(notes: &[u8]) {
    let Some(kb) = get_keyboard() else { return };

    // Get existing toggle overlay elements
    let existing: web_sys::NodeList = kb.query_selector_all(".toggle-overlay").unwrap();
    let existing_count = existing.length();

    let note_set: std::collections::HashSet<u8> = notes.iter().copied().collect();
    let mut existing_notes: std::collections::HashSet<u8> = std::collections::HashSet::new();

    // Remove overlays for notes no longer active, track existing ones
    for i in 0..existing_count {
        if let Some(node) = existing.get(i) {
            let el: web_sys::Element = node.unchecked_into();
            if let Some(key_str) = el.get_attribute("data-key-overlay") {
                if let Ok(key) = key_str.parse::<u8>() {
                    if note_set.contains(&key) {
                        existing_notes.insert(key);
                    } else {
                        el.remove();
                    }
                }
            }
        }
    }

    // Add overlays for new active notes
    for &note in notes {
        if !existing_notes.contains(&note) {
            let key_index = LEFTMOST_KEY + note as i32;
            let document = web_sys::window().unwrap().document().unwrap();
            let el = document.create_element("div").unwrap();
            el.set_class_name("toggle-overlay");
            el.set_attribute("data-key-overlay", &key_index.to_string())
                .ok();
            el.set_attribute("data-overlay-pattern", "toggle-lines")
                .ok();
            kb.append_child(&el).ok();
        }
    }
}

/// Sync piece overlay elements for piece pitch classes.
/// Creates/removes overlay elements with data-key-overlay attribute and piece-dots pattern.
fn sync_piece_overlays(notes: &[u8]) {
    let Some(kb) = get_keyboard() else { return };
    let document = web_sys::window().unwrap().document().unwrap();

    // Get existing piece overlay elements
    let existing: web_sys::NodeList = kb.query_selector_all(".piece-overlay").unwrap();
    let mut existing_keys: std::collections::HashSet<i32> = std::collections::HashSet::new();

    // Build set of current key indices
    let current_keys: std::collections::HashSet<i32> =
        notes.iter().map(|&n| LEFTMOST_KEY + n as i32).collect();

    // Remove overlays for notes no longer active, track existing ones
    for i in 0..existing.length() {
        if let Some(node) = existing.item(i) {
            let el: web_sys::Element = node.unchecked_into();
            if let Some(key_str) = el.get_attribute("data-key-overlay") {
                if let Ok(key) = key_str.parse::<i32>() {
                    if current_keys.contains(&key) {
                        existing_keys.insert(key);
                    } else {
                        el.remove();
                    }
                }
            }
        }
    }

    // Add overlays for new active notes
    for &note in notes {
        let key_index = LEFTMOST_KEY + note as i32;
        if !existing_keys.contains(&key_index) {
            // Create new overlay element
            let el = document.create_element("div").unwrap();
            el.set_class_name("piece-overlay");
            el.set_attribute("data-key-overlay", &key_index.to_string())
                .ok();
            // Specify piece pattern
            el.set_attribute("data-overlay-pattern", "piece-dots").ok();
            kb.append_child(&el).ok();
        }
    }
}

/// Sync voice overlay elements for voice pitch classes.
/// Creates/removes overlay elements with data-key-overlay attribute, voice-waves pattern, and shout emoji.
fn sync_voice_overlays(notes: &[u8]) {
    let Some(kb) = get_keyboard() else { return };
    let document = web_sys::window().unwrap().document().unwrap();

    // Get existing voice overlay elements
    let existing: web_sys::NodeList = kb.query_selector_all(".voice-overlay").unwrap();
    let mut existing_keys: std::collections::HashSet<i32> = std::collections::HashSet::new();

    // Build set of current key indices
    let current_keys: std::collections::HashSet<i32> =
        notes.iter().map(|&n| LEFTMOST_KEY + n as i32).collect();

    // Remove overlays for notes no longer active, track existing ones
    for i in 0..existing.length() {
        if let Some(node) = existing.item(i) {
            let el: web_sys::Element = node.unchecked_into();
            if let Some(key_str) = el.get_attribute("data-key-overlay") {
                if let Ok(key) = key_str.parse::<i32>() {
                    if current_keys.contains(&key) {
                        existing_keys.insert(key);
                    } else {
                        el.remove();
                    }
                }
            }
        }
    }

    // Also remove voice indicators for notes no longer active
    let existing_indicators: web_sys::NodeList = kb.query_selector_all(".voice-indicator").unwrap();
    for i in 0..existing_indicators.length() {
        if let Some(node) = existing_indicators.item(i) {
            let el: web_sys::Element = node.unchecked_into();
            if let Some(key_str) = el.get_attribute("data-key") {
                if let Ok(key) = key_str.parse::<i32>() {
                    if !current_keys.contains(&key) {
                        el.remove();
                    }
                }
            }
        }
    }

    // Add overlays and indicators for new active notes
    for &note in notes {
        let key_index = LEFTMOST_KEY + note as i32;
        if !existing_keys.contains(&key_index) {
            // Create new overlay element
            let el = document.create_element("div").unwrap();
            el.set_class_name("voice-overlay");
            el.set_attribute("data-key-overlay", &key_index.to_string())
                .ok();
            // Specify voice wave pattern
            el.set_attribute("data-overlay-pattern", "voice-waves").ok();
            kb.append_child(&el).ok();

            // Create shout emoji indicator
            let indicator = document.create_element("div").unwrap();
            indicator.set_class_name("voice-indicator");
            indicator
                .set_attribute("data-key", &key_index.to_string())
                .ok();
            indicator.set_text_content(Some("🗣️"));
            kb.append_child(&indicator).ok();
        }
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
const MAX_DRAG_SEMITONES: i32 = 5;

/// Leftmost key index for the keyboard (C3 = MIDI 36)
const LEFTMOST_KEY: i32 = 36;

/// SVG namespace for creating SVG elements
const SVG_NS: &str = "http://www.w3.org/2000/svg";

/// Create overlay patterns and add them to the keyboard via slot
fn create_overlay_patterns(keyboard: &web_sys::HtmlElement) {
    let document = web_sys::window().unwrap().document().unwrap();

    // Create SVG container for patterns
    let svg = document.create_element_ns(Some(SVG_NS), "svg").unwrap();
    svg.set_attribute("slot", "overlay-pattern").ok();
    svg.set_attribute("style", "display: none").ok();

    // Piece pattern - dots for key highlight when piece is on key
    let pattern2 = document.create_element_ns(Some(SVG_NS), "pattern").unwrap();
    pattern2.set_attribute("id", "piece-dots").ok();
    pattern2
        .set_attribute("patternUnits", "userSpaceOnUse")
        .ok();
    pattern2.set_attribute("width", "10").ok();
    pattern2.set_attribute("height", "10").ok();
    let circle2 = document.create_element_ns(Some(SVG_NS), "circle").unwrap();
    circle2.set_attribute("cx", "5").ok();
    circle2.set_attribute("cy", "5").ok();
    circle2.set_attribute("r", "2").ok();
    circle2.set_attribute("fill", "var(--piece)").ok();
    circle2.set_attribute("fill-opacity", "0.5").ok();
    pattern2.append_child(&circle2).ok();
    svg.append_child(&pattern2).ok();

    // Voice pattern - wavy lines
    let pattern3 = document.create_element_ns(Some(SVG_NS), "pattern").unwrap();
    pattern3.set_attribute("id", "voice-waves").ok();
    pattern3
        .set_attribute("patternUnits", "userSpaceOnUse")
        .ok();
    pattern3.set_attribute("width", "16").ok();
    pattern3.set_attribute("height", "8").ok();
    // Wavy path - sine wave approximation
    let wave = document.create_element_ns(Some(SVG_NS), "path").unwrap();
    wave.set_attribute("d", "M0,4 Q4,0 8,4 T16,4").ok();
    wave.set_attribute("fill", "none").ok();
    wave.set_attribute("stroke", "var(--voice)").ok();
    wave.set_attribute("stroke-width", "2").ok();
    wave.set_attribute("stroke-opacity", "0.6").ok();
    pattern3.append_child(&wave).ok();
    svg.append_child(&pattern3).ok();

    // Toggle pattern - diagonal lines (sunny/manual)
    let pattern4 = document.create_element_ns(Some(SVG_NS), "pattern").unwrap();
    pattern4.set_attribute("id", "toggle-lines").ok();
    pattern4
        .set_attribute("patternUnits", "userSpaceOnUse")
        .ok();
    pattern4.set_attribute("width", "8").ok();
    pattern4.set_attribute("height", "8").ok();
    pattern4
        .set_attribute("patternTransform", "rotate(45)")
        .ok();
    let line = document.create_element_ns(Some(SVG_NS), "line").unwrap();
    line.set_attribute("x1", "0").ok();
    line.set_attribute("y1", "0").ok();
    line.set_attribute("x2", "0").ok();
    line.set_attribute("y2", "8").ok();
    line.set_attribute("stroke", "var(--sunny)").ok();
    line.set_attribute("stroke-width", "3").ok();
    line.set_attribute("stroke-opacity", "0.5").ok();
    pattern4.append_child(&line).ok();
    svg.append_child(&pattern4).ok();

    keyboard.append_child(&svg).ok();
}

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

        // Overlay patterns added in after_inserted (SVG elements need SVG namespace)

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
            // Update style reactively based on confidence - uses voice color (hue 180)
            .attr_signal("style", state.final_confidence.signal().map(|confidence| {
                let size = 12.0 + confidence * 12.0;
                let opacity = 0.6 + confidence * 0.4;
                let glow = (6.0 + confidence * 10.0) as i32;
                // Lightness varies from 65% (low confidence) to 85% (high confidence)
                let lightness = 65.0 + confidence * 20.0;
                format!(
                    "width: {s}px; height: {s}px; opacity: {o:.2}; \
                     background: oklch({l}% 0.16 180); \
                     filter: drop-shadow(0 0 {g}px oklch({l}% 0.16 180));",
                    s = size, o = opacity, g = glow, l = lightness
                )
            }))
        }))

        .after_inserted(clone!(state => move |el| {
            // Add overlay patterns (SVG elements need SVG namespace)
            create_overlay_patterns(&el);

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
    use futures::{StreamExt, future::ready};
    use futures_signals::signal::{SignalExt, from_stream};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    // Track existing piece elements by ID
    let piece_elements: Rc<RefCell<HashMap<String, web_sys::HtmlElement>>> =
        Rc::new(RefCell::new(HashMap::new()));

    // Create signal from room events for pieces
    let (initial_pieces, events) = {
        let room_handle = state.room();
        let room = room_handle.lock_ref();
        let tuning = state.tuning.lock_ref();
        let pitch_count = tuning.pitch_class_count();
        drop(tuning);
        let pieces = room.all_pieces();
        let pieces_locked = room.pieces_locked();
        ((pieces, pitch_count, pieces_locked), room.events())
    };

    let state_for_stream = state.clone();
    let state_stream = events
        .filter(|e| {
            ready(
                e.affects_pieces()
                    || matches!(
                        e,
                        crate::room::RoomEvent::PiecesLockChanged { .. }
                            | crate::room::RoomEvent::FullStateSync { .. }
                    ),
            )
        })
        .map(move |_| {
            let room_handle = state_for_stream.room();
            let room = room_handle.lock_ref();
            let tuning = state_for_stream.tuning.lock_ref();
            let pitch_count = tuning.pitch_class_count();
            drop(tuning);
            let pieces = room.all_pieces();
            let pieces_locked = room.pieces_locked();
            (pieces, pitch_count, pieces_locked)
        });

    let full_stream = futures::stream::once(ready(initial_pieces)).chain(state_stream);
    let pieces_signal =
        from_stream(full_stream).map(|opt| opt.unwrap_or_else(|| (vec![], 12, false)));

    let piece_elements_clone = piece_elements.clone();
    let state_clone = state.clone();
    let keyboard_el_clone = keyboard_el.clone();

    wasm_bindgen_futures::spawn_local(async move {
        // Process each pieces change
        pieces_signal
            .for_each(move |(pieces, pitch_count, pieces_locked)| {
                // Sync pieces_locked to AppState
                if state_clone.pieces_locked.get() != pieces_locked {
                    state_clone.pieces_locked.set(pieces_locked);
                }

                let mut elements = piece_elements_clone.borrow_mut();

                // Build set of current piece IDs
                let current_ids: std::collections::HashSet<String> =
                    pieces.iter().map(|p| p.id.clone()).collect();

                // Remove pieces that no longer exist
                let to_remove: Vec<String> = elements
                    .keys()
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
                        let current_key = el
                            .get_attribute("data-key")
                            .and_then(|s| s.parse::<i32>().ok())
                            .unwrap_or(-1);
                        if current_key != key_index {
                            // Ensure no data-pitch (pieces use data-key for discrete positioning)
                            el.remove_attribute("data-pitch").ok();
                            el.set_attribute("data-key", &key_index.to_string()).ok();
                            el.set_attribute("data-original-pitch", &piece.pitch.to_string())
                                .ok();
                        }
                    } else {
                        // New piece - create element
                        let el =
                            create_piece_element(&piece.id, piece.pitch, key_index, &piece.emoji);
                        keyboard_el_clone.append_child(&el).ok();
                        elements.insert(piece.id.clone(), el);
                    }
                }

                // Return a ready future (for_each needs FnMut -> Future)
                async {}
            })
            .await;
    });
}

/// Create a piece indicator DOM element (raw web_sys version for efficient updates)
fn create_piece_element(
    piece_id: &str,
    original_pitch: i32,
    key_index: i32,
    emoji: &str,
) -> web_sys::HtmlElement {
    let document = web_sys::window().unwrap().document().unwrap();
    let el: web_sys::HtmlElement = document.create_element("div").unwrap().unchecked_into();

    el.set_class_name("piece-indicator");
    el.set_attribute("data-piece-id", piece_id).ok();
    el.set_attribute("data-key", &key_index.to_string()).ok();
    el.set_attribute("data-original-pitch", &original_pitch.to_string())
        .ok();
    el.set_attribute("data-emoji", emoji).ok();
    // Explicitly ensure no data-pitch (pieces use data-key for discrete positioning)
    el.remove_attribute("data-pitch").ok();

    // Display the emoji
    el.set_text_content(Some(emoji));

    // DEBUG: Add MutationObserver to catch any code that sets data-pitch on this piece
    {
        use wasm_bindgen::prelude::*;
        let piece_id_debug = piece_id.to_string();
        let el_debug = el.clone();
        let callback = Closure::<dyn Fn(js_sys::Array)>::new(move |mutations: js_sys::Array| {
            for i in 0..mutations.length() {
                if let Ok(mutation) = mutations.get(i).dyn_into::<web_sys::MutationRecord>() {
                    if let Some(attr) = mutation.attribute_name() {
                        if attr == "data-pitch" {
                            let has_it = el_debug.has_attribute("data-pitch");
                            let val = el_debug.get_attribute("data-pitch").unwrap_or_default();
                            web_sys::console::error_1(
                                &format!(
                                    "[DEBUG] data-pitch MUTATED on piece {}: has={}, val='{}'",
                                    piece_id_debug, has_it, val
                                )
                                .into(),
                            );
                        }
                    }
                }
            }
        });
        let observer = web_sys::MutationObserver::new(callback.as_ref().unchecked_ref()).unwrap();
        let options = web_sys::MutationObserverInit::new();
        options.set_attributes(true);
        options.set_attribute_filter(&js_sys::Array::of1(&JsValue::from_str("data-pitch")));
        observer.observe_with_options(&el, &options).ok();
        callback.forget();
    }

    // Styling handled by CSS .piece-indicator class
    // Only set pointer-events here to ensure it's enabled for drag handling
    el.style().set_property("pointer-events", "auto").ok();

    // Set up drag handler
    setup_piece_drag_handler(&el, piece_id, original_pitch, key_index);

    el
}

/// Set up drag handling for a piece element with multitouch support
fn setup_piece_drag_handler(
    el: &web_sys::HtmlElement,
    piece_id: &str,
    _original_pitch: i32,
    _key_index: i32,
) {
    let piece_id_down = piece_id.to_string();
    let el_down = el.clone();

    // Pointerdown: start drag
    let on_down = Closure::<dyn Fn(web_sys::PointerEvent)>::new(move |e: web_sys::PointerEvent| {
        let pointer_id = e.pointer_id();

        // Read current pitch from element (may have been updated since creation)
        let current_pitch = el_down
            .get_attribute("data-original-pitch")
            .and_then(|s| s.parse::<i32>().ok())
            .unwrap_or(0);

        // Store drag state in thread-local map for multitouch support
        ACTIVE_DRAGS.with(|drags| {
            drags.borrow_mut().insert(
                pointer_id,
                DragState {
                    piece_id: piece_id_down.clone(),
                    start_pitch: current_pitch,
                    start_x: e.client_x(),
                },
            );
        });

        // Store pointerId on element so we can track which pointer is dragging it
        el_down
            .set_attribute("data-pointer-id", &pointer_id.to_string())
            .ok();
        el_down.set_attribute("data-dragging", "true").ok();

        // Capture pointer for reliable tracking even if it leaves the element
        el_down.set_pointer_capture(pointer_id).ok();

        // Switch to fixed positioning for smooth drag
        el_down.style().set_property("position", "fixed").ok();
        el_down.style().set_property("z-index", "1000").ok();
        // touch-action: none to prevent scrolling during drag
        el_down.style().set_property("touch-action", "none").ok();
        // Position at cursor
        let x = e.client_x() as f64 - 16.0;
        let y = e.client_y() as f64 - 16.0;
        el_down
            .style()
            .set_property("left", &format!("{}px", x))
            .ok();
        el_down
            .style()
            .set_property("top", &format!("{}px", y))
            .ok();
        // Remove data-key so it doesn't snap back during drag
        el_down.remove_attribute("data-key").ok();

        // Show delete hole for drag-to-delete
        show_delete_hole();

        e.prevent_default();
        e.stop_propagation();
    });

    el.add_event_listener_with_callback("pointerdown", on_down.as_ref().unchecked_ref())
        .ok();
    on_down.forget();

    // Pointermove: update position during drag (received due to pointer capture)
    let el_move = el.clone();
    let on_move = Closure::<dyn Fn(web_sys::PointerEvent)>::new(move |e: web_sys::PointerEvent| {
        // Only handle if this element is being dragged
        if el_move.get_attribute("data-dragging").is_none() {
            return;
        }

        // Check pointer ID matches
        let expected_id = el_move
            .get_attribute("data-pointer-id")
            .and_then(|s| s.parse::<i32>().ok());
        if expected_id != Some(e.pointer_id()) {
            return;
        }

        // Update position
        let x = e.client_x() as f64 - 16.0;
        let y = e.client_y() as f64 - 16.0;
        el_move
            .style()
            .set_property("left", &format!("{}px", x))
            .ok();
        el_move
            .style()
            .set_property("top", &format!("{}px", y))
            .ok();

        e.prevent_default();
    });

    el.add_event_listener_with_callback("pointermove", on_move.as_ref().unchecked_ref())
        .ok();
    on_move.forget();
}

/// Set up document-level drag handlers for piece dragging.
/// Supports multitouch by tracking drags per pointerId in ACTIVE_DRAGS.
fn setup_document_drag_handlers(state: Arc<AppState>) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Some(document) = window.document() else {
        return;
    };

    // Document-level pointerup handler for dropping pieces (multitouch aware)
    let state_up = state.clone();
    let on_up = Closure::<dyn Fn(web_sys::PointerEvent)>::new(move |e: web_sys::PointerEvent| {
        let pointer_id = e.pointer_id();

        // Look up and remove drag state for this pointer
        let drag_state = ACTIVE_DRAGS.with(|drags| drags.borrow_mut().remove(&pointer_id));

        let Some(drag) = drag_state else { return };

        let piece_id = drag.piece_id;
        let start_pitch = drag.start_pitch;
        let start_x = drag.start_x;

        // Find the piece element
        let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
            return;
        };
        let Some(piece_el) = doc
            .query_selector(&format!("[data-piece-id='{}']", piece_id))
            .ok()
            .flatten()
        else {
            return;
        };
        let Ok(piece_el) = piece_el.dyn_into::<HtmlElement>() else {
            return;
        };

        // Release pointer capture
        piece_el.release_pointer_capture(pointer_id).ok();
        piece_el.remove_attribute("data-pointer-id").ok();

        // Check if dropped on delete hole
        let dropped_on_hole = is_over_delete_hole(e.client_x(), e.client_y());

        // Only remove piece if dropped on hole (not on click)
        if dropped_on_hole && !state_up.pieces_locked.get() {
            state_up.remove_native_piece(&piece_id);
            // Optimistic legacy view; connected modes let the signed store's
            // projection be the sole writer (an owner-gated remove that the
            // store rejects then leaves the piece in place, by construction).
            if !state_up.native_backend {
                state_up.offline_remove_piece(&piece_id);
            }
            // Reset styling before removal triggers UI update
            piece_el.remove_attribute("data-dragging").ok();
            piece_el.style().remove_property("position").ok();
            piece_el.style().remove_property("z-index").ok();
            piece_el.style().remove_property("touch-action").ok();
            piece_el.style().remove_property("left").ok();
            piece_el.style().remove_property("top").ok();
            hide_delete_hole();
            sync_active_pitches(&state_up);
            state_up.sync_midi_toggle_output();
            return;
        }

        // Get drop target using keyboard's getNoteAtPoint
        let tuning = state_up.tuning.lock_ref();
        let pc_count = tuning.pitch_class_count() as i32;
        drop(tuning);

        let end_key = get_key_at_point(e.client_x(), e.client_y());

        // Fallback: use drag vector heuristic
        let drag_dx = e.client_x() - start_x;
        let start_pc = start_pitch.rem_euclid(pc_count);

        let vector_key = if drag_dx.abs() > 20 {
            let estimated_offset = (drag_dx as f32 / 35.0).round() as i32;
            if estimated_offset != 0 && estimated_offset.abs() <= MAX_DRAG_SEMITONES {
                Some((start_pc + estimated_offset).rem_euclid(pc_count))
            } else {
                None
            }
        } else {
            None
        };

        let final_key = end_key.or(vector_key);

        // Check if we have a valid drop target and compute the intended pitch.
        let intended_pitch = if let Some(end_key) = final_key {
            let delta = shortest_delta(start_pc, end_key, pc_count);

            if delta.abs() <= MAX_DRAG_SEMITONES && delta != 0 {
                let target_pitch = start_pitch + delta;
                // Don't allow moving to a key that already has a piece
                if state_up.room().lock_ref().has_piece_at(target_pitch) {
                    start_pitch // Can't move there
                } else {
                    state_up.move_native_piece(&piece_id, target_pitch);
                    // Optimistic legacy view; connected modes let the signed
                    // store's projection be the sole writer.
                    if !state_up.native_backend {
                        state_up.offline_move_piece(&piece_id, target_pitch);
                    }
                    target_pitch
                }
            } else {
                start_pitch // No change
            }
        } else {
            start_pitch // No drop target
        };

        // Position the element. OFFLINE (`!native_backend`) the local view is
        // authoritative, so the intended pitch positions the piece. CONNECTED
        // (`native_backend`) the signed store is the sole writer: read the
        // piece's *projected* position — the owner's key for an owner-gated
        // rejected move (so it snaps back), the new key for an accepted one —
        // never the locally-guessed pitch, so the DOM cannot diverge from the
        // store. Accepted moves then re-arrive as a projection update via
        // `setup_piece_sync`.
        let display_pitch = if state_up.native_backend {
            state_up
                .room()
                .lock_ref()
                .get_piece(&piece_id)
                .map(|piece| piece.pitch)
                .unwrap_or(intended_pitch)
        } else {
            intended_pitch
        };

        // Update the piece position
        let pitch_class = display_pitch.rem_euclid(pc_count);
        let key_index = LEFTMOST_KEY + pitch_class;

        // Reset styling
        piece_el.remove_attribute("data-dragging").ok();
        piece_el.style().remove_property("z-index").ok();
        piece_el.remove_attribute("data-hover-key").ok();

        // Remove data-positioned so keyboard will re-set it after positioning
        piece_el.remove_attribute("data-positioned").ok();

        // Update data-key - this triggers the keyboard's MutationObserver
        piece_el.remove_attribute("data-pitch").ok();
        piece_el
            .set_attribute("data-key", &key_index.to_string())
            .ok();
        piece_el
            .set_attribute("data-original-pitch", &display_pitch.to_string())
            .ok();

        // Defer removing fixed positioning until after MutationObserver runs
        let piece_el_defer = piece_el.clone();
        wasm_bindgen_futures::spawn_local(async move {
            gloo_timers::future::TimeoutFuture::new(0).await;
            piece_el_defer.style().remove_property("position").ok();
            piece_el_defer.style().remove_property("left").ok();
            piece_el_defer.style().remove_property("top").ok();
            piece_el_defer.style().remove_property("touch-action").ok();
            // CSS .piece-indicator class handles background/box-shadow
        });

        hide_delete_hole();
        sync_active_pitches(&state_up);
        state_up.sync_midi_toggle_output();
    });
    let _ = document.add_event_listener_with_callback("pointerup", on_up.as_ref().unchecked_ref());
    on_up.forget();

    // Also handle pointercancel (e.g., system gesture interrupts touch)
    let state_cancel = state.clone();
    let on_cancel =
        Closure::<dyn Fn(web_sys::PointerEvent)>::new(move |e: web_sys::PointerEvent| {
            let pointer_id = e.pointer_id();

            // Look up and remove drag state for this pointer
            let drag_state = ACTIVE_DRAGS.with(|drags| drags.borrow_mut().remove(&pointer_id));

            let Some(drag) = drag_state else { return };

            // Find and reset the piece element
            let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
                return;
            };
            let Some(piece_el) = doc
                .query_selector(&format!("[data-piece-id='{}']", drag.piece_id))
                .ok()
                .flatten()
            else {
                return;
            };
            let Ok(piece_el) = piece_el.dyn_into::<HtmlElement>() else {
                return;
            };

            // Release capture and reset to original position
            piece_el.release_pointer_capture(pointer_id).ok();
            piece_el.remove_attribute("data-pointer-id").ok();
            piece_el.remove_attribute("data-dragging").ok();
            piece_el.remove_attribute("data-positioned").ok();
            piece_el.style().remove_property("position").ok();
            piece_el.style().remove_property("z-index").ok();
            piece_el.style().remove_property("left").ok();
            piece_el.style().remove_property("top").ok();
            piece_el.style().remove_property("touch-action").ok();

            // Restore original key position
            let tuning = state_cancel.tuning.lock_ref();
            let pc_count = tuning.pitch_class_count() as i32;
            drop(tuning);
            let pitch_class = drag.start_pitch.rem_euclid(pc_count);
            let key_index = LEFTMOST_KEY + pitch_class;
            piece_el
                .set_attribute("data-key", &key_index.to_string())
                .ok();

            hide_delete_hole();
        });
    let _ = document
        .add_event_listener_with_callback("pointercancel", on_cancel.as_ref().unchecked_ref());
    on_cancel.forget();
}

/// Set up HTML5 drag-drop handlers for receiving emojis from the picker.
fn setup_keyboard_drop_handlers(state: Arc<AppState>) {
    let Some(kb) = get_keyboard() else { return };

    // Allow drop by preventing default on dragover
    let on_dragover = Closure::<dyn Fn(web_sys::DragEvent)>::new(move |e: web_sys::DragEvent| {
        e.prevent_default();
    });
    let _ = kb.add_event_listener_with_callback("dragover", on_dragover.as_ref().unchecked_ref());
    on_dragover.forget();

    // Handle drop - create new piece at drop location
    let state_drop = state.clone();
    let on_drop = Closure::<dyn Fn(web_sys::DragEvent)>::new(move |e: web_sys::DragEvent| {
        e.prevent_default();

        // Get the dropped emoji
        let Some(dt) = e.data_transfer() else { return };
        let Ok(emoji) = dt.get_data("text/plain") else {
            return;
        };
        if emoji.is_empty() {
            return;
        }

        // Check if pieces are locked
        if state_drop.pieces_locked.get() {
            return;
        }

        // Get the key at the drop location
        let Some(key_index) = get_key_at_point(e.client_x(), e.client_y()) else {
            return;
        };

        // Convert key index to absolute MIDI pitch (key 0 = middle C = 60)
        let pitch = key_index + 60;

        // Don't allow dropping on a key that already has a piece
        if state_drop.room().lock_ref().has_piece_at(pitch) {
            return;
        }

        state_drop.put_native_piece(emoji.clone(), pitch);
        // Optimistic legacy view; the native snapshot replaces it once signed.
        if !state_drop.native_backend {
            state_drop.offline_add_piece(pitch, &emoji);
        }

        // Sync UI
        sync_active_pitches(&state_drop);
        state_drop.sync_midi_toggle_output();
    });
    let _ = kb.add_event_listener_with_callback("drop", on_drop.as_ref().unchecked_ref());
    on_drop.forget();
}

/// Start dragging an emoji from the picker (touch-friendly).
/// Call this from component on pointerdown.
pub fn start_emoji_drag(emoji: String, pointer_id: i32, x: i32, y: i32) {
    EMOJI_DRAGS.with(|drags| {
        drags
            .borrow_mut()
            .insert(pointer_id, EmojiDragState { emoji });
    });

    // Create ghost element
    if let Some(document) = web_sys::window().and_then(|w| w.document()) {
        let ghost: HtmlElement = document.create_element("div").unwrap().unchecked_into();
        ghost.set_id("emoji-drag-ghost");
        ghost.set_class_name("emoji-drag-ghost");
        let emoji_text =
            EMOJI_DRAGS.with(|drags| drags.borrow().get(&pointer_id).map(|d| d.emoji.clone()));
        if let Some(e) = emoji_text {
            ghost.set_text_content(Some(&e));
        }
        ghost
            .style()
            .set_property("left", &format!("{}px", x - 20))
            .ok();
        ghost
            .style()
            .set_property("top", &format!("{}px", y - 20))
            .ok();
        ghost
            .set_attribute("data-pointer-id", &pointer_id.to_string())
            .ok();
        document.body().unwrap().append_child(&ghost).ok();
    }
}

/// Set up document-level handlers for emoji drags from picker.
fn setup_emoji_drag_handlers(state: Arc<AppState>) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Some(document) = window.document() else {
        return;
    };

    // Pointermove - update ghost position
    let on_move = Closure::<dyn Fn(web_sys::PointerEvent)>::new(move |e: web_sys::PointerEvent| {
        let pointer_id = e.pointer_id();

        // Check if we're tracking this emoji drag
        let is_dragging = EMOJI_DRAGS.with(|drags| drags.borrow().contains_key(&pointer_id));
        if !is_dragging {
            return;
        }

        // Prevent browser from scrolling/cancelling the drag
        e.prevent_default();

        // Update ghost position
        if let Some(ghost) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id("emoji-drag-ghost"))
        {
            let ghost: HtmlElement = ghost.unchecked_into();
            ghost
                .style()
                .set_property("left", &format!("{}px", e.client_x() - 20))
                .ok();
            ghost
                .style()
                .set_property("top", &format!("{}px", e.client_y() - 20))
                .ok();
        }
    });
    let _ =
        document.add_event_listener_with_callback("pointermove", on_move.as_ref().unchecked_ref());
    on_move.forget();

    // Pointerup - drop emoji on keyboard
    let state_up = state.clone();
    let on_up = Closure::<dyn Fn(web_sys::PointerEvent)>::new(move |e: web_sys::PointerEvent| {
        let pointer_id = e.pointer_id();

        // Look up and remove emoji drag state
        let drag_state = EMOJI_DRAGS.with(|drags| drags.borrow_mut().remove(&pointer_id));
        let Some(drag) = drag_state else { return };

        // Remove ghost
        if let Some(ghost) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id("emoji-drag-ghost"))
        {
            ghost.remove();
        }

        // Check if pieces are locked
        if state_up.pieces_locked.get() {
            return;
        }

        // Check if over keyboard
        let Some(key_index) = get_key_at_point(e.client_x(), e.client_y()) else {
            return;
        };

        let pitch = key_index + 60;

        // Don't allow dropping on a key that already has a piece
        if state_up.room().lock_ref().has_piece_at(pitch) {
            return;
        }

        state_up.put_native_piece(drag.emoji.clone(), pitch);
        if !state_up.native_backend {
            state_up.offline_add_piece(pitch, &drag.emoji);
        }
        sync_active_pitches(&state_up);
        state_up.sync_midi_toggle_output();
    });
    let _ = document.add_event_listener_with_callback("pointerup", on_up.as_ref().unchecked_ref());
    on_up.forget();

    // Pointercancel - clean up
    let on_cancel =
        Closure::<dyn Fn(web_sys::PointerEvent)>::new(move |e: web_sys::PointerEvent| {
            let pointer_id = e.pointer_id();
            EMOJI_DRAGS.with(|drags| drags.borrow_mut().remove(&pointer_id));

            if let Some(ghost) = web_sys::window()
                .and_then(|w| w.document())
                .and_then(|d| d.get_element_by_id("emoji-drag-ghost"))
            {
                ghost.remove();
            }
        });
    let _ = document
        .add_event_listener_with_callback("pointercancel", on_cancel.as_ref().unchecked_ref());
    on_cancel.forget();
}

/// Get the key (pitch class) at a given screen coordinate using keyboard's getNoteAtPoint
fn get_key_at_point(x: i32, y: i32) -> Option<i32> {
    let keyboard = get_keyboard()?;
    let result = Reflect::get(&keyboard, &JsValue::from_str("getNoteAtPoint")).ok()?;
    let func: js_sys::Function = result.dyn_into().ok()?;
    let result = func
        .call2(&keyboard, &JsValue::from(x), &JsValue::from(y))
        .ok()?;
    if result.is_null() || result.is_undefined() {
        return None;
    }
    let note = Reflect::get(&result, &JsValue::from_str("note")).ok()?;
    note.as_f64().map(|n| n as i32)
}

/// Set up keyboard event listeners.
fn setup_keyboard_events(state: Arc<AppState>) {
    // Set up document-level drag handlers for piece dragging
    // (piece has pointer-events: none during drag, so we handle it at document level)
    setup_document_drag_handlers(state.clone());

    // Set up HTML5 drag-drop handlers for receiving emojis from the picker (desktop)
    setup_keyboard_drop_handlers(state.clone());

    // Set up pointer-based emoji drag handlers (touch-friendly)
    setup_emoji_drag_handlers(state.clone());

    if let Some(kb) = get_keyboard() {
        // Listen for keyclick events (actual user clicks, not hover)
        // Clicks toggle pitch classes AND clear voices at that pitch class
        let state_click = state.clone();
        let on_click = Closure::<dyn Fn(web_sys::Event)>::new(move |event: web_sys::Event| {
            // Skip if voice input is active
            if state_click.voice_active.get() {
                return;
            }

            // Skip if locked
            if state_click.pieces_locked.get() {
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

                        // Derive an absolute, idempotent intent from the
                        // projected presence — never a toggle involution.
                        let voice_cleared = if state_click.native_backend {
                            // Connected: the projection of the authoritative
                            // store is the ONLY writer for pitch presence.
                            // Read the currently-projected presence and
                            // dispatch an absolute intent (on → RemoveDegree,
                            // off → AddDegree). No optimistic local write —
                            // the projection paints the result.
                            let present = state_click.degree_is_active(pc);
                            state_click.set_native_degree(pc, !present);

                            // Voice presence is host-side; clear the local echo.
                            state_click.clear_room_voice_at_pitch_class(pc)
                        } else {
                            // Offline: the local `room` adapter IS the
                            // authoritative state, so it stays the writer.
                            let (active, voice_cleared) = state_click.offline_toggle_pitch(pc);
                            state_click.set_native_degree(pc, active);
                            voice_cleared
                        };

                        if voice_cleared {
                            // Also clear local voice state if it was ours
                            if state_click.voice_pitch.get() == Some(pc) {
                                state_click.voice_pitch.set(None);
                            }
                            state_click.sync_midi_voice_output();
                        }

                        // Re-sync keyboard to reflect new state
                        sync_active_pitches(&state_click);
                        state_click.sync_midi_toggle_output();
                    }
                }
            }
        });

        let _ = kb.add_event_listener_with_callback("keyclick", on_click.as_ref().unchecked_ref());
        on_click.forget();
    }
}
