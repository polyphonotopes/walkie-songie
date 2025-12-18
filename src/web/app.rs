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

use crate::pitch::{PitchDetectorConfig, PitchEvent, SwiftF0Detector};
use crate::room::{RoomState, YrsRoomState};
use crate::tuning::{PitchClass, Tuning};

use super::audio::WebAudioInput;
use super::components::{clear_button, pitch_display, tuning_editor, voice_button};
use super::keyboard::{pitch_keyboard, sync_active_pitches};

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
}

impl AppState {
    /// Create a new application state.
    pub fn new(peer_id: String) -> Arc<Self> {
        let room = YrsRoomState::new(peer_id);
        let tuning = Tuning::twelve_tet();
        let config = PitchDetectorConfig::default();
        let swiftf0 = SwiftF0Detector::new(config.sample_rate);

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
        })
    }

    /// Handle pitch detection event from SwiftF0.
    pub fn on_pitch_event(self: &Arc<Self>, event: PitchEvent) {
        self.current_pitch.set(Some(event.clone()));
        if self.voice_active.get() {
            self.process_pitch_for_locking(&event, 0.5, 2.0);
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

                // Set voice_pitch during singing (shows as lit/green on keyboard)
                self.voice_pitch.set(Some(pc));

                // Sync keyboard (voice pitch -> lit/green, manual -> pressed/red)
                sync_active_pitches(self);
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
        self.swiftf0_buffer.borrow_mut().clear();

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

        self.current_pitch.set(None);
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

        // Run async inference
        let result = {
            let mut swiftf0 = self.swiftf0.borrow_mut();
            swiftf0.detect(&samples).await
        };

        // Emit pitch event if we got a result
        if let Some((hz, confidence)) = result {
            let event = PitchEvent {
                hz: Some(hz),
                confidence,
            };
            self.on_pitch_event(event);
        }
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

                    // Voice and clear buttons
                    html!("div", {
                        .class("button-row")
                        .children(&mut [
                            voice_button(state.clone()),
                            clear_button(state.clone()),
                        ])
                    }),

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

    // React to voice_pitch changes
    html!("div", {
        .class("active-pitches")
        .child_signal(state.voice_pitch.signal().map(clone!(state => move |voice_pitch| {
            let room = state.room.lock_ref();
            let sets = room.all_peer_sets();
            let peer_id = room.local_peer_id();
            let tuning = state.tuning.lock_ref();

            // Collect manual pitches (red), sorted by pitch index
            let mut manual: Vec<_> = if let Some(set) = sets.get(peer_id) {
                set.pitch_classes.iter().copied().collect()
            } else {
                vec![]
            };
            manual.sort_by_key(|pc| pc.index());

            // Build list: voice pitch (green) + manual pitches (red), sorted
            let mut active: Vec<(String, bool, u8)> = Vec::new();
            if let Some(vpc) = voice_pitch {
                active.push((tuning.note_name(vpc).to_string(), true, vpc.index()));
            }
            for pc in manual {
                active.push((tuning.note_name(pc).to_string(), false, pc.index()));
            }
            // Sort by pitch index for consistent ordering
            active.sort_by_key(|(_, _, idx)| *idx);

            if active.is_empty() {
                Some(html!("span", {
                    .class("no-pitches")
                    .text("No active pitches")
                }))
            } else {
                Some(html!("div", {
                    .class("pitch-tags")
                    .children(active.iter().map(|(name, is_voice, _idx)| {
                        html!("span", {
                            .class("pitch-tag")
                            .apply_if(*is_voice, |d| d.class("voice-pitch"))
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

    // Initialize SwiftF0 ML model in background
    let state_for_init = state.clone();
    spawn_local(async move {
        state_for_init.init_swiftf0().await;
    });

    // Mount the app to the DOM
    dominator::append_dom(&dominator::body(), render_app(state));
}
