//! Main web application entry point.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use dominator::{html, Dom};
use futures_signals::signal::Mutable;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;
use web_time::Instant;

use futures::StreamExt;

use crate::pitch::{PitchDetectorConfig, PitchEvent, SwiftF0Detector};
use crate::room::{RoomEvent, RoomState, YrsRoomState};
use crate::tuning::{PitchClass, Tuning};

use crate::words::generate_room_name;

use super::audio::WebAudioInput;
use super::components::{clear_button, emoji_picker, lock_button, pitch_display, room_header_button, room_overlay, tuning_editor, voice_button};
use super::keyboard::{pitch_keyboard, sync_active_pitches};
use super::midi::{init_midi, MidiManager, pitch_class_to_midi_note, midi_note_to_pitch_class};
use super::libp2p_sync::start_libp2p_room_sync;
use super::voice_conditioner::{VoiceConditioner, ConditionerOutput};

/// How long a confident pitch "lingers" when confidence drops (in milliseconds).
const PITCH_LINGER_MS: u64 = 500;

/// Decay factor for old votes (applied each detection cycle).
/// This gives recency bias - newer confident pitches catch up faster.
const VOTE_DECAY: f64 = 0.95;

/// Extra confidence required to switch to a pitch that's a fifth away (harmonic rejection).
/// Fifths are the most common harmonic confusion (3:2 ratio).
const HARMONIC_REJECTION_BOOST: f64 = 0.25;

/// How long a pitch must stay above threshold before committing (milliseconds).
const STABILITY_DURATION_MS: u64 = 100;

/// Application state.
/// Uses Rc for wasm (single-threaded), Arc would work too but Rc is simpler.
pub struct AppState {
    /// Room state with CRDT synchronization (manual/clicked pitches).
    pub room: Mutable<YrsRoomState>,
    /// Current tuning system.
    pub tuning: Mutable<Tuning>,
    /// Whether voice input is active.
    pub voice_active: Mutable<bool>,
    /// Current detected pitch from SwiftF0.
    pub current_pitch: Mutable<Option<PitchEvent>>,
    /// Committed pitch class (from detection on release) - shown during singing.
    pub committed_pitch: Mutable<Option<PitchClass>>,
    /// Voice-committed pitch (single slot, separate from manual pitches).
    pub voice_pitch: Mutable<Option<PitchClass>>,
    /// Rolling confidence accumulator per pitch class during voice input.
    pub pitch_votes: Rc<RefCell<HashMap<u8, f64>>>,
    /// Currently "locked" pitch (last high-confidence detection).
    pub locked_pitch: Rc<RefCell<Option<PitchClass>>>,
    /// When the locked pitch was last confirmed with high confidence.
    pub locked_at: Rc<RefCell<Option<Instant>>>,
    /// Audio input handler.
    pub audio: Rc<RefCell<Option<WebAudioInput>>>,
    /// SwiftF0 ML pitch detector (async via JS bridge).
    pub swiftf0: Rc<RefCell<SwiftF0Detector>>,
    /// Whether SwiftF0 is ready for inference.
    pub swiftf0_ready: Mutable<bool>,
    /// SCL parse error message.
    pub scl_error: Mutable<Option<String>>,
    /// Previously lit note on the keyboard (for detected pitch visualization).
    pub prev_lit_note: Rc<RefCell<Option<u8>>>,
    /// Buffer for SwiftF0 samples (accumulates between async calls).
    pub swiftf0_buffer: Rc<RefCell<Vec<f32>>>,
    /// Voice conditioner for noise gating and AGC.
    pub conditioner: Rc<RefCell<VoiceConditioner>>,
    /// Continuous detected pitch in Hz (for dot indicator, not quantized).
    pub continuous_hz: Mutable<Option<f64>>,
    /// Final confidence after conditioner modifier (for dot indicator).
    pub final_confidence: Mutable<f64>,
    /// Whether the voice conditioner gate is open.
    pub gate_open: Mutable<bool>,
    /// Pitch being tracked for stability (must stay above threshold for duration).
    pub stable_pitch: Rc<RefCell<Option<PitchClass>>>,
    /// When the stable pitch first went above threshold.
    pub stable_since: Rc<RefCell<Option<Instant>>>,
    /// Current room name (wholesome words format).
    pub room_name: Mutable<String>,
    /// Whether the room overlay is visible.
    pub room_overlay_visible: Mutable<bool>,
    /// Text input for joining a room by name.
    pub room_input: Mutable<String>,
    /// Our iroh endpoint ID for P2P connections (set when sync starts).
    pub iroh_peer_id: Mutable<Option<String>>,
    /// MIDI manager for input/output.
    pub midi: Rc<RefCell<MidiManager>>,
    /// Selected MIDI input device ID (None = disabled).
    pub midi_input_id: Mutable<Option<String>>,
    /// Selected MIDI output device ID (None = disabled).
    pub midi_output_id: Mutable<Option<String>>,
    /// Room state version - incremented on every change (local or remote).
    /// UI components subscribe to this to know when to refresh.
    pub room_version: Mutable<u64>,
    /// Whether pieces are locked (can't drag to move or delete via hole).
    pub pieces_locked: Mutable<bool>,
    /// Index of currently selected emoji in the picker (for prev/next navigation).
    pub selected_emoji_idx: Mutable<usize>,
}

impl AppState {
    /// Create a new application state.
    pub fn new(peer_id: String) -> Arc<Self> {
        let room = YrsRoomState::new(peer_id);
        let tuning = Tuning::twelve_tet();
        let config = PitchDetectorConfig::default();
        let swiftf0 = SwiftF0Detector::new(config.sample_rate);
        let conditioner = VoiceConditioner::new(config.sample_rate as f32);

        Arc::new(Self {
            room: Mutable::new(room),
            tuning: Mutable::new(tuning),
            voice_active: Mutable::new(false),
            current_pitch: Mutable::new(None),
            committed_pitch: Mutable::new(None),
            voice_pitch: Mutable::new(None),
            pitch_votes: Rc::new(RefCell::new(HashMap::new())),
            locked_pitch: Rc::new(RefCell::new(None)),
            locked_at: Rc::new(RefCell::new(None)),
            audio: Rc::new(RefCell::new(None)),
            swiftf0: Rc::new(RefCell::new(swiftf0)),
            swiftf0_ready: Mutable::new(false),
            scl_error: Mutable::new(None),
            prev_lit_note: Rc::new(RefCell::new(None)),
            swiftf0_buffer: Rc::new(RefCell::new(Vec::with_capacity(8192))),
            conditioner: Rc::new(RefCell::new(conditioner)),
            continuous_hz: Mutable::new(None),
            final_confidence: Mutable::new(0.0),
            gate_open: Mutable::new(false),
            stable_pitch: Rc::new(RefCell::new(None)),
            stable_since: Rc::new(RefCell::new(None)),
            room_name: Mutable::new(String::new()), // Will be set in run_app
            room_overlay_visible: Mutable::new(false),
            room_input: Mutable::new(String::new()),
            iroh_peer_id: Mutable::new(None), // Set when P2P sync starts
            midi: init_midi(),
            midi_input_id: Mutable::new(None),
            midi_output_id: Mutable::new(None),
            room_version: Mutable::new(0),
            pieces_locked: Mutable::new(false), // Pieces can be dragged/deleted
            selected_emoji_idx: Mutable::new(0),
        })
    }

    /// Handle pitch detection event from SwiftF0, applying conditioner modifier.
    pub fn on_pitch_event(self: &Arc<Self>, event: PitchEvent, conditioner_output: &ConditionerOutput) {
        // Apply confidence modifier from conditioner
        let modified_confidence = event.confidence * conditioner_output.confidence_modifier as f64;
        let modified_event = PitchEvent {
            hz: event.hz,
            confidence: modified_confidence,
        };

        self.current_pitch.set(Some(modified_event.clone()));

        // Update continuous Hz for dot indicator
        self.continuous_hz.set(event.hz);
        self.final_confidence.set(modified_confidence);
        self.gate_open.set(conditioner_output.gate_open);

        // Calibrate reference level if we have a confident pitch
        if event.hz.is_some() && event.confidence > 0.6 {
            self.conditioner.borrow_mut().calibrate_reference(
                conditioner_output.rms_db,
                event.confidence,
            );
        }

        if self.voice_active.get() && conditioner_output.gate_open {
            self.process_pitch_for_locking(&modified_event, 0.5, 2.0);
        }
    }

    /// Process a pitch event with locking/lingering logic.
    /// - confidence_threshold: minimum confidence to lock onto a pitch
    /// - vote_weight: how much to weight this source's votes
    fn process_pitch_for_locking(
        self: &Arc<Self>,
        event: &PitchEvent,
        confidence_threshold: f64,
        vote_weight: f64,
    ) {
        let now = Instant::now();

        // Apply decay to all existing votes (recency bias)
        {
            let mut votes = self.pitch_votes.borrow_mut();
            for score in votes.values_mut() {
                *score *= VOTE_DECAY;
            }
        }

        if let Some(hz) = event.hz {
            // Quantize the detected pitch
            let tuning = self.tuning.lock_ref();
            let result = tuning.quantize(hz);
            let pc = result.pitch_class;
            let pitch_count = tuning.pitch_class_count() as i16;
            drop(tuning);

            // Check if this pitch is a fifth away from locked pitch (harmonic rejection)
            let effective_threshold = if let Some(locked) = *self.locked_pitch.borrow() {
                let diff = (pc.index() as i16 - locked.index() as i16).abs();
                // In 12-TET: fifth = 7 semitones, fourth = 5 semitones
                // Generalize: fifth ≈ 7/12 of pitch count, fourth ≈ 5/12
                let fifth_interval = (pitch_count * 7 / 12) as i16;
                let fourth_interval = (pitch_count * 5 / 12) as i16;

                if diff == fifth_interval || diff == fourth_interval ||
                   diff == pitch_count - fifth_interval || diff == pitch_count - fourth_interval {
                    // Likely harmonic confusion - require higher confidence
                    confidence_threshold + HARMONIC_REJECTION_BOOST
                } else {
                    confidence_threshold
                }
            } else {
                confidence_threshold
            };

            if event.confidence >= effective_threshold {
                // Above threshold - check stability before committing
                let mut stable_pitch = self.stable_pitch.borrow_mut();
                let mut stable_since = self.stable_since.borrow_mut();

                let is_stable = if *stable_pitch == Some(pc) {
                    // Same pitch as we've been tracking
                    if let Some(since) = *stable_since {
                        let elapsed_ms = now.duration_since(since).as_millis() as u64;
                        elapsed_ms >= STABILITY_DURATION_MS
                    } else {
                        false
                    }
                } else {
                    // Different pitch - start tracking this one
                    *stable_pitch = Some(pc);
                    *stable_since = Some(now);
                    false
                };

                drop(stable_pitch);
                drop(stable_since);

                if is_stable {
                    // Pitch has been stable long enough - commit it
                    *self.locked_pitch.borrow_mut() = Some(pc);
                    *self.locked_at.borrow_mut() = Some(now);

                    // Accumulate vote (with decay already applied, new votes catch up fast)
                    {
                        let mut votes = self.pitch_votes.borrow_mut();
                        let entry = votes.entry(pc.index()).or_insert(0.0);
                        *entry += event.confidence * vote_weight;
                    }

                    // Display shows current locked pitch directly
                    self.committed_pitch.set(Some(pc));

                    // Set voice_pitch during singing (shows as lit/green on keyboard)
                    self.voice_pitch.set(Some(pc));

                    // Sync to CRDT for P2P
                    self.room.lock_mut().set_voice_pitchclass(Some(pc));

                    // Increment room_version to trigger UI updates
                    self.room_version.set(self.room_version.get() + 1);

                    // Sync keyboard (voice pitch -> lit/green, manual -> pressed/red)
                    sync_active_pitches(self);

                    // Update MIDI voice output in real-time
                    self.sync_midi_voice_output();
                }
                // If not stable yet, just wait (indicator still shows the pitch)
            } else {
                // Low confidence - reset stability tracking and check linger
                *self.stable_pitch.borrow_mut() = None;
                *self.stable_since.borrow_mut() = None;
                self.maybe_linger(now);
            }
        } else {
            // No pitch detected - check if we should linger
            self.maybe_linger(now);
        }
    }

    /// If we have a recent locked pitch within the linger window, keep showing it.
    fn maybe_linger(self: &Arc<Self>, now: Instant) {
        let locked_at = self.locked_at.borrow();
        if let Some(lock_time) = *locked_at {
            let elapsed_ms = now.duration_since(lock_time).as_millis() as u64;
            if elapsed_ms <= PITCH_LINGER_MS {
                // Still within linger window - keep showing locked pitch
                if let Some(pc) = *self.locked_pitch.borrow() {
                    self.committed_pitch.set(Some(pc));
                }
            }
            // If outside linger window, don't update committed_pitch
            // (it will stay on vote leader)
        }
    }

    /// Start voice input.
    pub fn start_voice(self: &Arc<Self>) {
        if self.voice_active.get() {
            return;
        }

        self.voice_active.set(true);
        self.committed_pitch.set(None);

        // Clear accumulated votes and lock state from previous session
        self.pitch_votes.borrow_mut().clear();
        *self.locked_pitch.borrow_mut() = None;
        *self.locked_at.borrow_mut() = None;
        *self.stable_pitch.borrow_mut() = None;
        *self.stable_since.borrow_mut() = None;
        self.swiftf0_buffer.borrow_mut().clear();

        // Reset voice conditioner for new session
        self.conditioner.borrow_mut().reset();
        self.continuous_hz.set(None);
        self.final_confidence.set(0.0);
        self.gate_open.set(false);

        // Ensure active pitches are shown as lit during voice input
        super::keyboard::sync_active_pitches(self);

        // Create audio input if needed (scope the borrow)
        {
            let mut audio_ref = self.audio.borrow_mut();
            if audio_ref.is_none() {
                match WebAudioInput::new() {
                    Ok(audio) => *audio_ref = Some(audio),
                    Err(e) => {
                        web_sys::console::error_1(&format!("Failed to create audio: {:?}", e).into());
                        self.voice_active.set(false);
                        return;
                    }
                }
            }
        } // Borrow released here

        // Clone what we need for the async block
        let audio_rc = self.audio.clone();
        let swiftf0_buffer = self.swiftf0_buffer.clone();
        let state = self.clone();

        spawn_local(async move {
            // Take the audio out to avoid holding borrow across await
            let mut audio = {
                let mut audio_ref = audio_rc.borrow_mut();
                audio_ref.take()
            };

            if let Some(ref mut audio_input) = audio {
                let state_for_swiftf0 = state.clone();

                // Start audio capture - samples go to swiftf0_buffer
                let result = audio_input.start(swiftf0_buffer.clone()).await;

                if let Err(e) = result {
                    web_sys::console::error_1(&format!("Failed to start audio: {:?}", e).into());
                    state.voice_active.set(false);
                } else {
                    // Start SwiftF0 processing loop
                    spawn_local(async move {
                        loop {
                            if !state_for_swiftf0.voice_active.get() {
                                break;
                            }
                            state_for_swiftf0.process_swiftf0().await;
                            // Small delay to batch samples (~50ms)
                            let promise = js_sys::Promise::new(&mut |resolve, _| {
                                let window = web_sys::window().unwrap();
                                let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                                    &resolve,
                                    50,
                                );
                            });
                            let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
                        }
                    });
                }

                // Put the audio back
                let mut audio_ref = audio_rc.borrow_mut();
                *audio_ref = audio;
            }
        });
    }

    /// Stop voice input and commit the detected pitch.
    pub fn stop_voice(self: &Arc<Self>) {
        if !self.voice_active.get() {
            return;
        }

        self.voice_active.set(false);

        // Clear indicator state immediately
        self.gate_open.set(false);
        self.continuous_hz.set(None);

        // Stop audio input - use try_borrow_mut in case start is still pending
        if let Ok(mut audio_ref) = self.audio.try_borrow_mut() {
            if let Some(ref mut audio) = *audio_ref {
                audio.stop();
            }
        }

        // Clear prev_lit tracking
        *self.prev_lit_note.borrow_mut() = None;

        // Commit the detected pitch to the voice_pitch slot (single, replaces previous)
        self.voice_pitch.set(self.committed_pitch.get());

        // Re-sync keyboard (voice pitch -> lit/green, manual -> pressed/red)
        sync_active_pitches(self);

        // Update MIDI voice output
        self.sync_midi_voice_output();

        self.current_pitch.set(None);
    }

    /// Set the selected MIDI input device.
    pub fn set_midi_input(self: &Arc<Self>, device_id: Option<String>) {
        if let Ok(mut midi) = self.midi.try_borrow_mut() {
            if let Err(e) = midi.select_input(device_id.clone()) {
                web_sys::console::warn_1(&format!("Failed to select MIDI input: {}", e).into());
                return;
            }
        }
        self.midi_input_id.set(device_id);
    }

    /// Set the selected MIDI output device.
    pub fn set_midi_output(self: &Arc<Self>, device_id: Option<String>) {
        if let Ok(mut midi) = self.midi.try_borrow_mut() {
            if let Err(e) = midi.select_output(device_id.clone()) {
                web_sys::console::warn_1(&format!("Failed to select MIDI output: {}", e).into());
                return;
            }
        }
        self.midi_output_id.set(device_id.clone());

        // If output was enabled, sync current state
        if device_id.is_some() {
            self.sync_midi_toggle_output();
            self.sync_midi_voice_output();
        }
    }

    /// Check if MIDI output is enabled.
    pub fn midi_output_enabled(&self) -> bool {
        self.midi_output_id.get_cloned().is_some()
    }

    /// Set the room name and update the URL hash.
    pub fn set_room_name(&self, name: String) {
        set_url_hash(&name);
        self.room_name.set(name.clone());
        self.room_input.set(name);
    }

    /// Poll MIDI input and route note events to toggle set.
    pub fn poll_midi_input(self: &Arc<Self>) {
        let pitch_count = self.tuning.lock_ref().pitch_class_count() as u8;

        while let Some(event) = MidiManager::poll_input() {
            // Convert MIDI note to pitch class
            let pc = midi_note_to_pitch_class(event.note, pitch_count);
            let pitch_class = PitchClass::new(pc);

            // Route to toggle set: note-on adds, note-off removes
            {
                let mut room = self.room.lock_mut();
                if event.is_note_on {
                    room.add_pitch(pitch_class);
                } else {
                    room.remove_pitch(pitch_class);
                }
            }

            // Sync MIDI output and keyboard display
            self.sync_midi_toggle_output();
            sync_active_pitches(self);
        }
    }

    /// Update MIDI output for toggle set changes.
    /// Call this whenever the toggle set or pieces change.
    pub fn sync_midi_toggle_output(self: &Arc<Self>) {
        // Skip if MIDI output is disabled
        if !self.midi_output_enabled() {
            return;
        }

        let room = self.room.lock_ref();
        let local_peer = room.local_peer_id();
        let pitch_count = self.tuning.lock_ref().pitch_class_count() as u8;

        // Get current pitch classes from local peer's set
        let mut current_notes: std::collections::HashSet<u8> =
            if let Some(set) = room.all_peer_sets().get(local_peer) {
                set.pitch_classes
                    .iter()
                    .map(|pc| pitch_class_to_midi_note(pc.index(), pitch_count))
                    .collect()
            } else {
                std::collections::HashSet::new()
            };

        // Add piece absolute pitches as MIDI notes
        for piece in room.all_pieces() {
            // Pieces have absolute pitch, use directly as MIDI note (clamped to valid range)
            let midi_note = piece.pitch.clamp(0, 127) as u8;
            current_notes.insert(midi_note);
        }

        drop(room);

        // Sync with MIDI output
        if let Ok(midi) = self.midi.try_borrow() {
            midi.output.borrow_mut().sync_toggle_notes(&current_notes);
        }
    }

    /// Update MIDI output for voice pitch changes.
    /// Call this whenever the voice pitch changes.
    pub fn sync_midi_voice_output(self: &Arc<Self>) {
        // Skip if MIDI output is disabled
        if !self.midi_output_enabled() {
            return;
        }

        let voice_pitch = self.voice_pitch.get();
        let pitch_count = self.tuning.lock_ref().pitch_class_count() as u8;

        if let Ok(midi) = self.midi.try_borrow() {
            let mut output = midi.output.borrow_mut();

            if let Some(pc) = voice_pitch {
                // Convert pitch class to MIDI note (with octave info)
                // For voice, we use the actual detected pitch if available
                // Otherwise fall back to middle octave
                let note = pitch_class_to_midi_note(pc.index(), pitch_count);
                output.voice_note_on(note);
            } else {
                // No voice pitch - clear voice notes
                output.clear_voice_notes();
            }
        }
    }

    /// Initialize the SwiftF0 ML model (call once on startup).
    pub async fn init_swiftf0(self: &Arc<Self>) {
        web_sys::console::log_1(&"Initializing SwiftF0 ML model...".into());
        let result = {
            let mut swiftf0 = self.swiftf0.borrow_mut();
            swiftf0.init().await
        };
        match result {
            Ok(()) => {
                self.swiftf0_ready.set(true);
                web_sys::console::log_1(&"SwiftF0 ML model ready".into());
            }
            Err(e) => {
                web_sys::console::error_1(&format!("SwiftF0 init failed: {}", e).into());
            }
        }
    }

    /// Process accumulated samples through SwiftF0 (async).
    /// Called from audio callback via spawn_local.
    pub async fn process_swiftf0(self: &Arc<Self>) {
        if !self.swiftf0_ready.get() || !self.voice_active.get() {
            return;
        }

        // Take samples from the buffer
        let samples: Vec<f32> = {
            let mut buffer = self.swiftf0_buffer.borrow_mut();
            if buffer.len() < 1024 {
                return; // Not enough samples
            }
            std::mem::take(&mut *buffer)
        };

        // Run voice conditioner
        let conditioner_output = {
            let mut conditioner = self.conditioner.borrow_mut();
            conditioner.decay_reference(); // Slow decay each frame
            conditioner.process(&samples)
        };

        // Update gate state for UI
        self.gate_open.set(conditioner_output.gate_open);

        // Only run pitch detection if gate is open
        if !conditioner_output.gate_open {
            // Gate closed - clear continuous pitch display
            self.continuous_hz.set(None);
            self.final_confidence.set(0.0);
            return;
        }

        // Run async inference on conditioned samples
        let result = {
            let mut swiftf0 = self.swiftf0.borrow_mut();
            swiftf0.detect(&conditioner_output.samples).await
        };

        // Emit pitch event if we got a result
        if let Some((hz, confidence)) = result {
            let event = PitchEvent {
                hz: Some(hz),
                confidence,
            };
            self.on_pitch_event(event, &conditioner_output);
        } else {
            // No pitch detected even though gate is open
            self.continuous_hz.set(None);
            self.final_confidence.set(0.0);
        }
    }
}

/// Render the main application.
fn render_app(state: Arc<AppState>) -> Dom {
    html!("div", {
        .class("app")
        .children(&mut [
            // Header: title + room button + room controls (sticky)
            html!("header", {
                .class("header")
                .children(&mut [
                    // Left: title + room
                    html!("div", {
                        .class("header-left")
                        .children(&mut [
                            html!("span", { .class("title").text("Walkie Songie") }),
                            room_header_button(state.clone()),
                        ])
                    }),
                    // Right: room controls
                    html!("div", {
                        .class("header-right")
                        .children(&mut [
                            lock_button(state.clone()),
                            clear_button(state.clone()),
                        ])
                    }),
                ])
            }),

            // Room overlay (hidden by default)
            room_overlay(state.clone()),

            // Scroll-snap pages container
            html!("div", {
                .class("pages")
                .attr("id", "pages")
                .after_inserted(|el| {
                    // Track scroll to update page dots
                    let on_scroll = wasm_bindgen::closure::Closure::<dyn Fn()>::new(move || {
                        let Some(window) = web_sys::window() else { return };
                        let Some(document) = window.document() else { return };
                        let Some(pages) = document.get_element_by_id("pages") else { return };

                        let scroll_top = pages.scroll_top() as f64;
                        let page_height = pages.client_height() as f64;
                        let current_page = if page_height > 0.0 {
                            (scroll_top / page_height).round() as usize
                        } else {
                            0
                        };

                        // Update dots
                        let dots = document.query_selector_all(".page-dot").ok();
                        if let Some(dots) = dots {
                            for i in 0..dots.length() {
                                if let Some(dot) = dots.get(i) {
                                    let dot: web_sys::Element = dot.unchecked_into();
                                    if i as usize == current_page {
                                        dot.class_list().add_1("active").ok();
                                    } else {
                                        dot.class_list().remove_1("active").ok();
                                    }
                                }
                            }
                        }
                    });
                    el.add_event_listener_with_callback("scroll", on_scroll.as_ref().unchecked_ref()).ok();
                    on_scroll.forget();
                })
                .children(&mut [
                    // Page 1: Keyboard
                    html!("div", {
                        .class("page")
                        .class("keyboard-page")
                        .children(&mut [
                            // Keyboard with pitch info overlay
                            html!("div", {
                                .class("keyboard-section")
                                .children(&mut [
                                    pitch_keyboard(state.clone()),
                                    // Overlay pitch info in center
                                    html!("div", {
                                        .class("keyboard-overlay")
                                        .child(pitch_display(state.clone()))
                                    }),
                                ])
                            }),

                            // Sing button + emoji picker row
                            html!("div", {
                                .class("button-row")
                                .children(&mut [
                                    voice_button(state.clone()),
                                    emoji_picker(state.clone()),
                                ])
                            }),
                        ])
                    }),

                    // Page 2: Graph & Info
                    html!("div", {
                        .class("page")
                        .class("graph-page")
                        .children(&mut [
                            // Active pitches section
                            html!("section", {
                                .class("active-pitches-section")
                                .child(active_pitches_list(state.clone()))
                            }),

                            // Graph placeholder (will be polyphonotope web component)
                            html!("div", {
                                .class("graph-container")
                                .child(html!("span", {
                                    .class("graph-placeholder")
                                    .text("Polyphonotope graph")
                                }))
                            }),
                        ])
                    }),

                    // Page 3: Tuning (hidden for now)
                    html!("div", {
                        .class("page")
                        .class("tuning-page")
                        .style("display", "none")
                        .children(&mut [
                            html!("h2", { .class("page-title").text("Tuning") }),
                            tuning_editor(state.clone()),
                        ])
                    }),
                ])
            }),

            // Page indicator dots
            html!("div", {
                .class("page-dots")
                .children(&mut [
                    html!("div", { .class("page-dot").class("active") }),
                    html!("div", { .class("page-dot") }),
                ])
            }),
        ])
    })
}

/// List of currently active pitch classes.
fn active_pitches_list(state: Arc<AppState>) -> Dom {
    use dominator::clone;
    use futures_signals::signal::SignalExt;

    // React to room_version changes (triggers on both local and remote CRDT updates)
    html!("div", {
        .class("active-pitches")
        .child_signal(state.room_version.signal().map(clone!(state => move |_version| {
            let room = state.room.lock_ref();
            let tuning = state.tuning.lock_ref();
            let pc_count = tuning.pitch_class_count() as i32;

            // Get combined pitches from ALL peers via CRDT
            let room_result = room.compute_room_result();

            // Get local voice pitch from CRDT (shows as green)
            let (_, local_voice_pc) = room.local_voice();

            // Build list of active pitches: (name, is_voice, is_piece, sort_key)
            let mut active: Vec<(String, bool, bool, i32)> = Vec::new();

            // Add all pitches from room result (toggle mode pitches)
            for pc in &room_result.pitch_classes {
                let is_voice = local_voice_pc == Some(*pc);
                active.push((tuning.note_name(*pc).to_string(), is_voice, false, pc.index() as i32));
            }

            // Add pieces with octave info
            for piece in room.all_pieces() {
                let pc_idx = piece.pitch.rem_euclid(pc_count) as u8;
                let pc = crate::tuning::PitchClass::new(pc_idx);
                let octave = piece.octave();
                let name = format!("{}{}", tuning.note_name(pc), octave);
                active.push((name, false, true, piece.pitch));
            }

            // Sort by sort key for consistent ordering
            active.sort_by_key(|(_, _, _, key)| *key);

            if active.is_empty() {
                Some(html!("span", {
                    .class("no-pitches")
                    .text("No active pitches")
                }))
            } else {
                Some(html!("div", {
                    .class("pitch-tags")
                    .children(active.iter().map(|(name, is_voice, is_piece, _)| {
                        html!("span", {
                            .class("pitch-tag")
                            .apply_if(*is_voice, |d| d.class("voice-pitch"))
                            .apply_if(*is_piece, |d| d.class("piece-pitch"))
                            .text(name)
                        })
                    }).collect::<Vec<_>>())
                }))
            }
        })))
    })
}

/// Get room topic from URL hash or query param, or generate a new one.
/// Returns the full topic string which may include @peer-id for bootstrapping.
fn get_or_generate_room_name() -> String {
    if let Some(window) = web_sys::window() {
        // First, try hash: #room-name or #room-name@peer-id
        if let Ok(hash) = window.location().hash() {
            let full = hash.trim_start_matches('#');
            if !full.is_empty() {
                // Split on @ to separate room name from optional peer ID
                let room_part = full.split('@').next().unwrap_or(full);
                if crate::words::is_valid_room_name(room_part) {
                    // Return the FULL string including @peer-id if present
                    return full.to_string();
                }
            }
        }

        // Fallback: try query param ?room=name
        if let Ok(search) = window.location().search() {
            if search.starts_with("?room=") {
                let name = search.trim_start_matches("?room=");
                if crate::words::is_valid_room_name(name) {
                    return name.to_string();
                }
            }
            if let Some(pos) = search.find("room=") {
                let rest = &search[pos + 5..];
                let name = rest.split('&').next().unwrap_or("");
                if crate::words::is_valid_room_name(name) {
                    return name.to_string();
                }
            }
        }
    }
    // Generate a new room name
    generate_room_name()
}

/// Update the URL hash to reflect the current room.
fn set_url_hash(room_name: &str) {
    if let Some(window) = web_sys::window() {
        let _ = window.location().set_hash(room_name);
    }
}

/// Run the web application.
#[wasm_bindgen(start)]
pub fn run_app() {
    // Set up panic hook for better error messages
    console_error_panic_hook::set_once();

    // Set up tracing to output to browser console (INFO level only to reduce noise)
    tracing_wasm::set_as_global_default_with_config(
        tracing_wasm::WASMLayerConfigBuilder::new()
            .set_max_level(tracing::Level::INFO)
            .build()
    );

    // Initialize app asynchronously (to load peer ID from IndexedDB)
    spawn_local(async {
        init_app().await;
    });
}

/// Initialize the application (async to support IndexedDB).
async fn init_app() {
    // Load or generate peer ID from IndexedDB
    let peer_id = super::storage::get_or_create_peer_id().await;

    // Create application state
    let state = AppState::new(peer_id);

    // Initialize room name (from URL or generate new) and sync to hash
    // room_topic may include @peer-id for bootstrapping
    let room_topic = get_or_generate_room_name();
    // Extract just the room name for display/state (strip @peer-id if present)
    let room_name = room_topic.split('@').next().unwrap_or(&room_topic).to_string();
    state.set_room_name(room_name.clone());

    // Load saved room state from IndexedDB (before P2P sync)
    if let Some(saved_state) = super::storage::get_room_state(&room_name).await {
        web_sys::console::log_1(&format!("Loading saved room state ({} bytes)", saved_state.len()).into());
        if let Err(e) = state.room.lock_mut().load_state(&saved_state) {
            web_sys::console::warn_1(&format!("Failed to load saved state: {}", e).into());
        } else {
            // Trigger UI update after loading state
            state.room_version.set(state.room_version.get() + 1);
        }
    }

    // Start P2P sync for the room
    // Pass full room_topic (including @peer-id if present) to signaller
    let room_for_sync = state.room.clone();
    let iroh_peer_id = state.iroh_peer_id.clone();
    let room_version = state.room_version.clone();

    // Start libp2p sync (WebRTC + gossipsub via circuit relay)
    start_libp2p_room_sync(
        room_for_sync,
        room_topic,
        iroh_peer_id,
        room_version,
    );

    // Initialize SwiftF0 ML model in background
    let state_for_init = state.clone();
    spawn_local(async move {
        state_for_init.init_swiftf0().await;
    });

    // Start MIDI input polling loop
    let state_for_midi = state.clone();
    spawn_local(async move {
        loop {
            state_for_midi.poll_midi_input();

            // Poll at ~60Hz
            let promise = js_sys::Promise::new(&mut |resolve, _| {
                let window = web_sys::window().unwrap();
                let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                    &resolve,
                    16,
                );
            });
            let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
        }
    });

    // Subscribe to room events for UI updates and state persistence
    let state_for_events = state.clone();
    let room_name_for_events = state.room_name.get_cloned();
    spawn_local(async move {
        // Get event stream from room
        let events = state_for_events.room.lock_ref().events();

        // Process each event
        events.for_each(|event| {
            // Handle the event
            handle_room_event(&state_for_events, &event, &room_name_for_events);
            async {}
        }).await;
    });

    // Subscribe to room_version changes to update UI and persist state
    use futures_signals::signal::SignalExt;
    let state_for_version = state.clone();
    let room_name_for_save = state.room_name.get_cloned();
    spawn_local(async move {
        let mut last_save_version = 0u64;
        state_for_version.room_version.signal()
            .for_each(|version| {
                // Refresh keyboard display when room state changes
                sync_active_pitches(&state_for_version);

                // Save state to IndexedDB (debounced - only if version changed)
                if version > last_save_version {
                    last_save_version = version;
                    let state_bytes = state_for_version.room.lock_ref().encode_state_as_update();
                    let room_name = room_name_for_save.clone();
                    spawn_local(async move {
                        if let Err(e) = super::storage::set_room_state(&room_name, &state_bytes).await {
                            web_sys::console::warn_1(&format!("Failed to save room state: {}", e).into());
                        }
                    });
                }

                async {}
            })
            .await;
    });

    // Mount the app to the DOM
    dominator::append_dom(&dominator::body(), render_app(state));
}

/// Handle a room event - update UI, MIDI, and persist state.
fn handle_room_event(state: &Arc<AppState>, event: &RoomEvent, room_name: &str) {
    // Log event for debugging
    web_sys::console::log_1(&format!("[RoomEvent] {:?}", event).into());

    // Increment room_version for backward compatibility with signal-based reactivity
    state.room_version.set(state.room_version.get() + 1);

    // Handle pitch-affecting events
    if event.affects_pitches() {
        sync_active_pitches(state);
        state.sync_midi_toggle_output();
    }

    // Handle voice events
    if event.affects_voice() {
        sync_active_pitches(state);
        state.sync_midi_voice_output();
    }

    // Handle piece lock changes
    if let RoomEvent::PiecesLockChanged { locked } = event {
        state.pieces_locked.set(*locked);
    }

    // Handle full state sync (initial load or reconnect)
    if let RoomEvent::FullStateSync { pieces_locked, .. } = event {
        state.pieces_locked.set(*pieces_locked);
        sync_active_pitches(state);
        state.sync_midi_toggle_output();
        state.sync_midi_voice_output();
    }

    // Persist state to IndexedDB on any change
    let state_bytes = state.room.lock_ref().encode_state_as_update();
    let room_name = room_name.to_string();
    spawn_local(async move {
        if let Err(e) = super::storage::set_room_state(&room_name, &state_bytes).await {
            web_sys::console::warn_1(&format!("Failed to save room state: {}", e).into());
        }
    });
}
