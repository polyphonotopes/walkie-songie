//! Main web application entry point.

use std::sync::Arc;

use dominator::{html, Dom};
use futures_signals::signal::Mutable;
use wasm_bindgen::prelude::*;

use crate::pitch::{DualPitchDetector, PitchDetectorConfig, PitchEvent, PitchSource};
use crate::room::{RoomState, YrsRoomState};
use crate::tuning::{PitchClass, Tuning};

use super::audio::WebAudioInput;
use super::components::{pitch_display, pitch_grid, voice_button};

/// Application state.
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
    /// Audio input handler.
    pub audio: Mutable<Option<WebAudioInput>>,
    /// Pitch detector.
    pub detector: Mutable<DualPitchDetector>,
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
            audio: Mutable::new(None),
            detector: Mutable::new(detector),
        })
    }

    /// Handle pitch detection event.
    pub fn on_pitch_event(self: &Arc<Self>, event: PitchEvent) {
        match event.source {
            PitchSource::Fast => {
                self.fast_pitch.set(Some(event));
            }
            PitchSource::Accurate => {
                self.accurate_pitch.set(Some(event.clone()));
                // If voice is active and we have a confident pitch, prepare for commit
                if self.voice_active.get() {
                    if let Some(hz) = event.hz {
                        if event.confidence > 0.8 {
                            let tuning = self.tuning.lock_ref();
                            let result = tuning.quantize(hz);
                            self.committed_pitch.set(Some(result.pitch_class));
                        }
                    }
                }
            }
        }
    }

    /// Start voice input.
    pub fn start_voice(self: &Arc<Self>) {
        self.voice_active.set(true);
        self.committed_pitch.set(None);
        // Audio input will be started by the audio module
    }

    /// Stop voice input and commit the detected pitch.
    pub fn stop_voice(self: &Arc<Self>) {
        self.voice_active.set(false);

        // Commit the last accurate pitch to the room state
        if let Some(pc) = self.committed_pitch.get() {
            self.room.lock_mut().toggle_pitch(pc);
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
            // Header
            html!("header", {
                .class("header")
                .children(&mut [
                    html!("h1", {
                        .text("Walkie Songie")
                    }),
                    html!("p", {
                        .class("subtitle")
                        .text("Collaborative pitch space")
                    }),
                ])
            }),

            // Main content
            html!("main", {
                .class("main")
                .children(&mut [
                    // Voice input section
                    html!("section", {
                        .class("voice-section")
                        .children(&mut [
                            html!("h2", {
                                .text("Voice Input")
                            }),
                            voice_button(state.clone()),
                            pitch_display(state.clone()),
                        ])
                    }),

                    // Pitch grid section
                    html!("section", {
                        .class("pitch-section")
                        .children(&mut [
                            html!("h2", {
                                .text("Pitch Classes")
                            }),
                            pitch_grid(state.clone()),
                        ])
                    }),
                ])
            }),
        ])
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
