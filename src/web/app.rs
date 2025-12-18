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

use crate::pitch::{DualPitchDetector, PitchDetectorConfig, PitchEvent, PitchSource};
use crate::room::{RoomState, YrsRoomState};
use crate::tuning::{PitchClass, Tuning};

use super::audio::WebAudioInput;
use super::components::{pitch_display, tuning_editor, voice_button};
use super::keyboard::{dim_all, notes_dim, notes_light, pitch_keyboard};

/// How long a confident pitch "lingers" when confidence drops (in milliseconds).
const PITCH_LINGER_MS: u64 = 500;

/// Decay factor for old votes (applied each detection cycle).
/// This gives recency bias - newer confident pitches catch up faster.
const VOTE_DECAY: f64 = 0.95;

/// Extra confidence required to switch to a pitch that's a fifth away (harmonic rejection).
/// Fifths are the most common harmonic confusion (3:2 ratio).
const HARMONIC_REJECTION_BOOST: f64 = 0.25;

/// Application state.
/// Uses Rc for wasm (single-threaded), Arc would work too but Rc is simpler.
pub struct AppState {
    /// Room state with CRDT synchronization.
    pub room: Mutable<YrsRoomState>,
    /// Current tuning system.
    pub tuning: Mutable<Tuning>,
    /// Whether voice input is active.
    pub voice_active: Mutable<bool>,
    /// Current detected pitch from fast detector.
    pub fast_pitch: Mutable<Option<PitchEvent>>,
    /// Current detected pitch from accurate detector.
    pub accurate_pitch: Mutable<Option<PitchEvent>>,
    /// Committed pitch class (from accurate detection on release).
    pub committed_pitch: Mutable<Option<PitchClass>>,
    /// Rolling confidence accumulator per pitch class during voice input.
    /// Maps pitch class index -> accumulated confidence score.
    pub pitch_votes: Rc<RefCell<HashMap<u8, f64>>>,
    /// Currently "locked" pitch (last high-confidence detection).
    pub locked_pitch: Rc<RefCell<Option<PitchClass>>>,
    /// When the locked pitch was last confirmed with high confidence.
    pub locked_at: Rc<RefCell<Option<Instant>>>,
    /// Audio input handler (Rc for sharing with callbacks).
    pub audio: Rc<RefCell<Option<WebAudioInput>>>,
    /// Pitch detector (Rc for sharing with audio callback).
    pub detector: Rc<RefCell<DualPitchDetector>>,
    /// SCL parse error message.
    pub scl_error: Mutable<Option<String>>,
    /// Previously lit note on the keyboard (for detected pitch visualization).
    pub prev_lit_note: Rc<RefCell<Option<u8>>>,
}

impl AppState {
    /// Create a new application state.
    pub fn new(peer_id: String) -> Arc<Self> {
        let room = YrsRoomState::new(peer_id);
        let tuning = Tuning::twelve_tet();
        let detector = DualPitchDetector::new(PitchDetectorConfig::default());

        Arc::new(Self {
            room: Mutable::new(room),
            tuning: Mutable::new(tuning),
            voice_active: Mutable::new(false),
            fast_pitch: Mutable::new(None),
            accurate_pitch: Mutable::new(None),
            committed_pitch: Mutable::new(None),
            pitch_votes: Rc::new(RefCell::new(HashMap::new())),
            locked_pitch: Rc::new(RefCell::new(None)),
            locked_at: Rc::new(RefCell::new(None)),
            audio: Rc::new(RefCell::new(None)),
            detector: Rc::new(RefCell::new(detector)),
            scl_error: Mutable::new(None),
            prev_lit_note: Rc::new(RefCell::new(None)),
        })
    }

    /// Handle pitch detection event.
    pub fn on_pitch_event(self: &Arc<Self>, event: PitchEvent) {
        match event.source {
            PitchSource::Fast => {
                // BCF - ignore, we use SwiftF0/YIN for everything
            }
            PitchSource::McLeod => {
                // YIN fallback - only use if SwiftF0 not available
                self.fast_pitch.set(Some(event.clone()));
            }
            PitchSource::Accurate => {
                // pYIN on native - high accuracy fallback
                self.fast_pitch.set(Some(event.clone()));
                self.accurate_pitch.set(Some(event.clone()));
                if self.voice_active.get() {
                    self.process_pitch_for_locking(&event, 0.4, 1.5);
                }
            }
            PitchSource::SwiftF0 => {
                // SwiftF0 ML model - primary detector, best accuracy
                self.fast_pitch.set(Some(event.clone()));
                self.accurate_pitch.set(Some(event.clone()));
                if self.voice_active.get() {
                    // SwiftF0 is very accurate, use lower threshold
                    self.process_pitch_for_locking(&event, 0.5, 2.0);
                }
            }
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
                // High confidence - lock onto this pitch
                // Update the lock
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

                // Update keyboard lighting for detected pitch
                let note = pc.index();
                let mut prev = self.prev_lit_note.borrow_mut();
                if *prev != Some(note) {
                    if let Some(old) = prev.take() {
                        notes_dim(&[old]);
                    }
                    notes_light(&[note]);
                    *prev = Some(note);
                }
            } else {
                // Low confidence - check if we should linger on previous pitch
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

    /// Update committed_pitch to show the current vote leader.
    fn update_committed_from_votes(self: &Arc<Self>) {
        let votes = self.pitch_votes.borrow();
        if let Some((&pc_idx, _score)) = votes.iter().max_by(|a, b| a.1.partial_cmp(b.1).unwrap()) {
            self.committed_pitch.set(Some(PitchClass::new(pc_idx)));
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
        let detector_rc = self.detector.clone();
        let state = self.clone();

        spawn_local(async move {
            // Take the audio out to avoid holding borrow across await
            let mut audio = {
                let mut audio_ref = audio_rc.borrow_mut();
                audio_ref.take()
            };

            if let Some(ref mut audio_input) = audio {
                let state_for_callback = state.clone();

                let result = audio_input.start(detector_rc, move |event| {
                    state_for_callback.on_pitch_event(event);
                }).await;

                if let Err(e) = result {
                    web_sys::console::error_1(&format!("Failed to start audio: {:?}", e).into());
                    state.voice_active.set(false);
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

        // Stop audio input - use try_borrow_mut in case start is still pending
        if let Ok(mut audio_ref) = self.audio.try_borrow_mut() {
            if let Some(ref mut audio) = *audio_ref {
                audio.stop();
            }
        }

        // Dim the keyboard (detected pitch feedback)
        dim_all();
        *self.prev_lit_note.borrow_mut() = None;

        // Commit the last accurate pitch to the room state
        if let Some(pc) = self.committed_pitch.get() {
            let is_active = self.room.lock_mut().toggle_pitch(pc);
            // Update keyboard pressed state
            let note = pc.index();
            if is_active {
                super::keyboard::keys_press(&[note]);
            } else {
                super::keyboard::keys_release(&[note]);
            }
        }

        self.fast_pitch.set(None);
        self.accurate_pitch.set(None);
    }
}

/// Render the main application.
fn render_app(state: Arc<AppState>) -> Dom {
    html!("div", {
        .class("app")
        .children(&mut [
            // Compact header
            html!("header", {
                .class("header")
                .child(html!("h1", { .text("Walkie Songie") }))
            }),

            // Main content
            html!("main", {
                .class("main")
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

                    // Voice button
                    voice_button(state.clone()),

                    // Active pitches section
                    html!("section", {
                        .class("active-pitches-section")
                        .child(active_pitches_list(state.clone()))
                    }),

                    // Tuning section (collapsible)
                    html!("details", {
                        .class("tuning-section")
                        .children(&mut [
                            html!("summary", { .text("Tuning") }),
                            tuning_editor(state.clone()),
                        ])
                    }),
                ])
            }),
        ])
    })
}

/// List of currently active pitch classes.
fn active_pitches_list(state: Arc<AppState>) -> Dom {
    use dominator::clone;
    use futures_signals::signal::SignalExt;

    html!("div", {
        .class("active-pitches")
        .child_signal(state.room.signal_ref(clone!(state => move |room| {
            let sets = room.all_peer_sets();
            let peer_id = room.local_peer_id();

            let active: Vec<String> = if let Some(set) = sets.get(peer_id) {
                let tuning = state.tuning.lock_ref();
                set.pitch_classes.iter()
                    .map(|pc| tuning.note_name(*pc).to_string())
                    .collect()
            } else {
                vec![]
            };

            if active.is_empty() {
                Some(html!("span", {
                    .class("no-pitches")
                    .text("No active pitches")
                }))
            } else {
                Some(html!("div", {
                    .class("pitch-tags")
                    .children(active.iter().map(|name| {
                        html!("span", {
                            .class("pitch-tag")
                            .text(name)
                        })
                    }).collect::<Vec<_>>())
                }))
            }
        })))
    })
}

/// Run the web application.
#[wasm_bindgen(start)]
pub fn run_app() {
    // Set up panic hook for better error messages
    console_error_panic_hook::set_once();

    // Generate a peer ID
    let peer_id = format!("peer-{}", uuid::Uuid::new_v4());

    // Create application state
    let state = AppState::new(peer_id);

    // Mount the app to the DOM
    dominator::append_dom(&dominator::body(), render_app(state));
}
