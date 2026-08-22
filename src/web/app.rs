//! Main web application entry point.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use dominator::{Dom, html};
use futures_signals::signal::{Mutable, MutableLockMut, ReadOnlyMutable};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;
use web_time::Instant;

use futures::{StreamExt, future::ready};
use futures_signals::signal::from_stream;

use crate::client::{
    AppEvent, AppEventEnvelope, AppSnapshot, ClientCommand, DiscoverySource, MidiPortSnapshot,
    PeerPath,
};
use crate::pitch::{PitchDetectorConfig, PitchEvent, SwiftF0Detector};
use crate::room::{Piece, RoomEvent, RoomProjection};
use crate::tuning::{PitchClass, TunedDegree, TunedPeriodicPitch, Tuning, TuningDefinition};

use crate::words::generate_room_name;

use super::audio::WebAudioInput;
use super::components::{
    clear_button, emoji_picker, info_panel, lock_button, midi_settings, pitch_display,
    room_header_button, room_overlay, tuning_editor, voice_button,
};
use super::keyboard::{pitch_keyboard, sync_active_pitches};
use super::midi::{MidiInputEvent, MidiManager, pitch_class_to_midi_note};
use super::voice_conditioner::{ConditionerOutput, VoiceConditioner};

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
    /// True when a capability-native Replica host is authoritative — either the Tauri
    /// runtime or the in-page browser iroh host. In these modes the Yrs state
    /// below is only a rendering adapter.
    pub native_backend: bool,
    /// True when that host is the in-page browser iroh host
    /// (`web::browser_host`): commands stay in this tab and MIDI stays Web MIDI.
    pub browser_host: bool,
    /// Last ordered snapshot received from the native runtime.
    pub native_snapshot: Mutable<Option<AppSnapshot>>,
    /// Highest accepted native event sequence.
    pub native_sequence: Mutable<u64>,
    /// Native MIDI ports rendered by the existing settings component.
    pub native_midi_inputs: Mutable<Vec<MidiPortSnapshot>>,
    pub native_midi_outputs: Mutable<Vec<MidiPortSnapshot>>,
    /// Human-readable Iroh path/discovery state for the room overlay.
    pub native_status: Mutable<String>,
    /// Current shareable native room ticket.
    pub room_ticket: Mutable<Option<String>>,
    /// Stable non-zero presence session for the current press/hold gesture.
    voice_session: Rc<RefCell<u64>>,
    /// Last preview sent to Tauri, used to coalesce detector-rate updates while
    /// still refreshing the 1.5-second signed lease.
    last_native_voice: Rc<RefCell<Option<(Instant, TunedPeriodicPitch)>>>,
    /// Ephemeral UI/MIDI projection of the admitted Replica view.
    ///
    /// PRIVATE: the projection (`project_native_snapshot`) is the sole
    /// effective writer. Sibling UI modules (`keyboard`, `components`, …) see
    /// it only through the read-only handle [`AppState::room`]; the vestigial
    /// offline writers below reach it via [`AppState::room_mut`], which never
    /// escapes this module. This makes the single-writer invariant a compile
    /// error to violate, not a convention.
    room: Mutable<RoomProjection>,
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
    pub pitch_votes: Rc<RefCell<HashMap<u16, f64>>>,
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
    /// Version counter for MIDI devices (increments when devices change).
    pub midi_devices_version: Mutable<u32>,
    /// Whether pieces are locked (can't drag to move or delete via hole).
    pub pieces_locked: Mutable<bool>,
    /// Index of currently selected emoji in the picker (for prev/next navigation).
    pub selected_emoji_idx: Mutable<usize>,
    /// Whether graph uses 3D layout (true) or 2D (false).
    pub graph_3d_mode: Mutable<bool>,
    /// Current hop level for graph visualization (1-6).
    pub graph_hop_level: Mutable<u32>,
}

impl AppState {
    /// Create a new application state.
    pub fn new(peer_id: String) -> Arc<Self> {
        let room = RoomProjection::new(peer_id);
        let tuning = Tuning::twelve_tet();
        let config = PitchDetectorConfig::default();
        let swiftf0 = SwiftF0Detector::new(config.sample_rate);
        let conditioner = VoiceConditioner::new(config.sample_rate as f32);
        let voice_session = (js_sys::Date::now() as u64).max(1);

        let tauri = super::native_bridge::is_available();
        // Without Tauri, the browser-net build hosts the signed room in-page.
        let browser_host = !tauri && cfg!(feature = "browser-net");

        Arc::new(Self {
            native_backend: tauri || browser_host,
            browser_host,
            native_snapshot: Mutable::new(None),
            native_sequence: Mutable::new(0),
            native_midi_inputs: Mutable::new(Vec::new()),
            native_midi_outputs: Mutable::new(Vec::new()),
            native_status: Mutable::new(
                if browser_host {
                    "Waiting for peers via the relay…"
                } else {
                    "Searching this room with mDNS…"
                }
                .to_owned(),
            ),
            room_ticket: Mutable::new(None),
            voice_session: Rc::new(RefCell::new(voice_session)),
            last_native_voice: Rc::new(RefCell::new(None)),
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
            midi: Rc::new(RefCell::new(MidiManager::new())),
            midi_input_id: Mutable::new(None),
            midi_output_id: Mutable::new(None),
            midi_devices_version: Mutable::new(0),
            pieces_locked: Mutable::new(false), // Pieces can be dragged/deleted
            selected_emoji_idx: Mutable::new(0),
            graph_3d_mode: Mutable::new(true), // 3D by default
            graph_hop_level: Mutable::new(1),  // Start with 1-hop
        })
    }

    /// True only inside the Tauri webview (the host is out-of-process).
    pub fn tauri_backend(&self) -> bool {
        self.native_backend && !self.browser_host
    }

    /// Read-only view of the render adapter for sibling UI modules.
    ///
    /// This is the ONLY way `keyboard`, `components`, `graph`, … reach room
    /// state. `ReadOnlyMutable` exposes `lock_ref`/`signal_cloned`/`get_cloned`
    /// but no `lock_mut`, so a component structurally cannot write unbacked
    /// state — the projection is the sole writer, enforced by the type system.
    pub fn room(&self) -> ReadOnlyMutable<RoomProjection> {
        self.room.read_only()
    }

    /// Private mutable access to the render adapter. Module-scoped so only the
    /// projection and the vestigial offline writers below can lock it for
    /// mutation; no write handle escapes to sibling UI modules.
    fn room_mut(&self) -> MutableLockMut<'_, RoomProjection> {
        self.room.lock_mut()
    }

    /// Offline-only: toggle a pitch class in the authoritative local adapter,
    /// also dropping any local voice echo at that class. Returns
    /// `(now_active, voice_cleared)`. In connected modes the store is
    /// authoritative and this is never called (see the keyclick handler).
    pub fn offline_toggle_pitch(&self, pitch_class: PitchClass) -> (bool, bool) {
        let mut room = self.room_mut();
        let active = if room.contains_pitch(pitch_class) {
            room.remove_pitch(pitch_class);
            false
        } else {
            room.add_pitch(pitch_class);
            true
        };
        let voice_cleared = room.clear_voice_at_pitch_class(pitch_class);
        (active, voice_cleared)
    }

    /// Drop any local voice echo painted at `pitch_class`. Returns whether a
    /// voice was cleared. In connected modes host presence is authoritative;
    /// this only clears the transient local echo the keyclick paints.
    pub fn clear_room_voice_at_pitch_class(&self, pitch_class: PitchClass) -> bool {
        self.room_mut().clear_voice_at_pitch_class(pitch_class)
    }

    /// Offline-only: add a piece to the authoritative local adapter.
    pub fn offline_add_piece(&self, pitch: i32, emoji: &str) {
        self.room_mut().add_piece(pitch, emoji);
    }

    /// Offline-only: remove a piece from the authoritative local adapter.
    pub fn offline_remove_piece(&self, piece_id: &str) {
        self.room_mut().remove_piece(piece_id);
    }

    /// Offline-only: move a piece in the authoritative local adapter.
    pub fn offline_move_piece(&self, piece_id: &str, new_pitch: i32) {
        self.room_mut().move_piece(piece_id, new_pitch);
    }

    /// Offline-only: set the pieces-locked flag on the local adapter.
    pub fn offline_set_pieces_locked(&self, locked: bool) {
        self.room_mut().set_pieces_locked(locked);
    }

    /// Offline-only: wipe pitches, voice, and pieces from the local adapter.
    pub fn offline_clear_musical_state(&self) {
        let mut room = self.room_mut();
        room.clear_pitches();
        room.clear_voice();
        room.clear_pieces();
    }

    /// Offline-only: apply an SCL tuning definition to the local adapter.
    pub fn offline_set_tuning_scl(&self, scl: &str) {
        self.room_mut().set_tuning_scl(scl);
    }

    fn dispatch_native(&self, command: ClientCommand) {
        if !self.native_backend {
            return;
        }
        let status = self.native_status.clone();
        let on_error = move |error: String| {
            status.set(format!("⚠ {error}"));
        };
        #[cfg(feature = "browser-net")]
        if self.browser_host {
            super::replica_host::dispatch(command, on_error);
            return;
        }
        super::native_bridge::dispatch(command, on_error);
    }

    fn current_native_pitch(&self, pitch_class: PitchClass) -> Option<TunedDegree> {
        TunedDegree::new(&self.tuning.lock_ref(), pitch_class.index()).ok()
    }

    fn native_periodic_pitch(&self, absolute_pitch: i32) -> Option<TunedPeriodicPitch> {
        let tuning = self.tuning.lock_ref();
        let degree_count = i32::try_from(tuning.pitch_class_count()).ok()?;
        let absolute_degree = absolute_pitch.checked_sub(60)?;
        let degree = u16::try_from(absolute_degree.rem_euclid(degree_count)).ok()?;
        let period = absolute_degree.div_euclid(degree_count);
        TunedPeriodicPitch::new(&tuning, degree, period).ok()
    }

    pub fn set_native_degree(&self, pitch_class: PitchClass, active: bool) {
        let Some(pitch) = self.current_native_pitch(pitch_class) else {
            return;
        };
        self.dispatch_native(if active {
            ClientCommand::AddDegree { pitch }
        } else {
            ClientCommand::RemoveDegree { pitch }
        });
    }

    /// Presence of a pitch class in the projected (authoritative) snapshot —
    /// the exact set `apply_room_view` computed and the projection paints.
    /// The tap handler reads this to derive an absolute, idempotent intent
    /// (`AddDegree`/`RemoveDegree`) instead of a store-blind toggle involution.
    pub fn degree_is_active(&self, pitch_class: PitchClass) -> bool {
        let Some(pitch) = self.current_native_pitch(pitch_class) else {
            return false;
        };
        self.native_snapshot
            .lock_ref()
            .as_ref()
            .is_some_and(|snapshot| snapshot.active_degrees.contains(&pitch))
    }

    pub fn put_native_piece(&self, emoji: String, absolute_pitch: i32) {
        if let Some(pitch) = self.native_periodic_pitch(absolute_pitch) {
            self.dispatch_native(ClientCommand::PutPiece { emoji, pitch });
        }
    }

    pub fn move_native_piece(&self, piece_id: &str, absolute_pitch: i32) {
        let Some(piece) = parse_piece_id(piece_id) else {
            return;
        };
        if let Some(pitch) = self.native_periodic_pitch(absolute_pitch) {
            self.dispatch_native(ClientCommand::MovePiece { piece, pitch });
        }
    }

    pub fn remove_native_piece(&self, piece_id: &str) {
        if let Some(piece) = parse_piece_id(piece_id) {
            self.dispatch_native(ClientCommand::RemovePiece { piece });
        }
    }

    pub fn set_native_pieces_locked(&self, locked: bool) {
        self.dispatch_native(ClientCommand::SetRoomConfig {
            pieces_locked: Some(locked),
            available_emojis: None,
        });
    }

    pub fn clear_native_musical_state(&self) {
        let Some(snapshot) = self.native_snapshot.get_cloned() else {
            return;
        };
        for pitch in snapshot.active_degrees {
            self.dispatch_native(ClientCommand::RemoveDegree { pitch });
        }
        for piece in snapshot.pieces {
            self.dispatch_native(ClientCommand::RemovePiece { piece: piece.id });
        }
        self.send_native_voice(None);
    }

    pub fn set_native_tuning(&self, scl: String) {
        match TuningDefinition::new(scl, None) {
            Ok(definition) => self.dispatch_native(ClientCommand::SetTuning { definition }),
            Err(error) => self.scl_error.set(Some(error.to_string())),
        }
    }

    fn send_native_voice(&self, pitch: Option<TunedPeriodicPitch>) {
        if let Some(pitch) = pitch {
            let now = Instant::now();
            if let Some((last_at, last_pitch)) = *self.last_native_voice.borrow() {
                let elapsed = now.duration_since(last_at).as_millis() as u64;
                if (last_pitch == pitch && elapsed < 750) || elapsed < 50 {
                    return;
                }
            }
            *self.last_native_voice.borrow_mut() = Some((now, pitch));
        } else {
            *self.last_native_voice.borrow_mut() = None;
        }
        self.dispatch_native(ClientCommand::SetVoicePreview {
            session: *self.voice_session.borrow(),
            pitch,
        });
    }

    /// Apply one strictly ordered runtime event, then re-project the complete
    /// snapshot into the legacy UI model.
    pub fn apply_native_event(self: &Arc<Self>, envelope: AppEventEnvelope) {
        if envelope.sequence <= self.native_sequence.get() {
            return;
        }
        self.native_sequence.set(envelope.sequence);

        {
            let mut current = self.native_snapshot.lock_mut();
            match envelope.event {
                AppEvent::Snapshot { snapshot } => *current = Some(*snapshot),
                event => {
                    let Some(snapshot) = current.as_mut() else {
                        web_sys::console::warn_1(
                            &"native delta arrived before the initial snapshot".into(),
                        );
                        return;
                    };
                    apply_native_delta(snapshot, event);
                }
            }
        }
        self.project_native_snapshot();
    }

    fn project_native_snapshot(self: &Arc<Self>) {
        let Some(snapshot) = self.native_snapshot.get_cloned() else {
            return;
        };

        if let Some(room_name) = snapshot.room_name.clone() {
            self.room_name.set(room_name.clone());
            self.room_input.set(room_name);
        }
        self.room_ticket.set(snapshot.room_ticket.clone());

        if let Some(definition) = snapshot.tuning.as_ref() {
            match definition.validate("room tuning") {
                Ok(tuning) => {
                    self.tuning.set(tuning.clone());
                    self.scl_error.set(None);
                    super::keyboard::update_tuning(&tuning);
                }
                Err(error) => self.scl_error.set(Some(error.to_string())),
            }
        }

        // MIDI ports live in the snapshot only when the host owns MIDI (Tauri).
        // The in-page browser host reports `native_midi: false` and Web MIDI
        // keeps managing these fields — overwriting them with the snapshot's
        // empty lists would wipe the user's device selection on every event.
        if snapshot.capabilities.native_midi {
            self.native_midi_inputs.set(snapshot.midi_inputs.clone());
            self.native_midi_outputs.set(snapshot.midi_outputs.clone());
            self.midi_input_id.set(
                snapshot
                    .midi_inputs
                    .iter()
                    .find(|port| port.selected)
                    .map(|port| port.id.clone()),
            );
            self.midi_output_id.set(
                snapshot
                    .midi_outputs
                    .iter()
                    .find(|port| port.selected)
                    .map(|port| port.id.clone()),
            );
            self.midi_devices_version
                .set(self.midi_devices_version.get().wrapping_add(1));
        }

        let connected = snapshot
            .peers
            .iter()
            .find(|peer| matches!(peer.path, PeerPath::Direct | PeerPath::Relayed));
        self.iroh_peer_id
            .set(connected.map(|peer| peer.endpoint_id.clone()));
        self.native_status.set(native_status(&snapshot));

        let tuning = self.tuning.lock_ref();
        let degree_count = i64::try_from(tuning.pitch_class_count()).unwrap_or(1);
        let pitches: Vec<_> = snapshot
            .active_degrees
            .iter()
            .filter(|pitch| pitch.tuning_id == tuning.id())
            .map(|pitch| PitchClass::from(pitch.degree))
            .collect();
        let pieces: Vec<_> = snapshot
            .pieces
            .iter()
            .filter(|piece| piece.pitch.tuning_id == tuning.id())
            .filter_map(|piece| {
                let pitch = i64::from(piece.pitch.pitch.period())
                    .checked_mul(degree_count)?
                    .checked_add(i64::from(piece.pitch.pitch.degree().index()))?
                    .checked_add(60)?;
                Some(Piece {
                    id: piece.id.to_hex(),
                    pitch: i32::try_from(pitch).ok()?,
                    emoji: piece.emoji.clone(),
                })
            })
            .collect();
        let voices: Vec<_> = snapshot
            .voices
            .iter()
            .filter_map(|voice| {
                let pitch = voice.pitch?;
                if pitch.tuning_id != tuning.id() {
                    return None;
                }
                let absolute = i64::from(pitch.pitch.period())
                    .checked_mul(degree_count)?
                    .checked_add(i64::from(pitch.pitch.degree().index()))?
                    .checked_add(60)?;
                Some((
                    voice.author.to_hex(),
                    i32::try_from(absolute).ok(),
                    Some(PitchClass::from(pitch.pitch.degree())),
                ))
            })
            .collect();
        drop(tuning);

        self.pieces_locked.set(snapshot.pieces_locked);
        self.room.lock_mut().replace_replica_projection(
            &pitches,
            &pieces,
            &voices,
            snapshot.pieces_locked,
            snapshot.available_emojis.as_deref(),
        );
        sync_active_pitches(self);
    }

    /// Handle pitch detection event from SwiftF0, applying conditioner modifier.
    pub fn on_pitch_event(
        self: &Arc<Self>,
        event: PitchEvent,
        conditioner_output: &ConditionerOutput,
    ) {
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
            self.conditioner
                .borrow_mut()
                .calibrate_reference(conditioner_output.rms_db, event.confidence);
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
            let Ok(result) = tuning.quantize(hz) else {
                return;
            };
            let pc = result.pitch_class;
            let absolute_pitch = result.absolute_pitch;
            let native_pitch = TunedPeriodicPitch {
                tuning_id: tuning.id(),
                pitch: result.periodic_pitch,
            };
            let pitch_count = tuning.pitch_class_count() as i16;
            drop(tuning);

            // Check if this pitch is a fifth away from locked pitch (harmonic rejection)
            let effective_threshold = if let Some(locked) = *self.locked_pitch.borrow() {
                let diff = (pc.index() as i16 - locked.index() as i16).abs();
                // In 12-TET: fifth = 7 semitones, fourth = 5 semitones
                // Generalize: fifth ≈ 7/12 of pitch count, fourth ≈ 5/12
                let fifth_interval = (pitch_count * 7 / 12) as i16;
                let fourth_interval = (pitch_count * 5 / 12) as i16;

                if diff == fifth_interval
                    || diff == fourth_interval
                    || diff == pitch_count - fifth_interval
                    || diff == pitch_count - fourth_interval
                {
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

                    // Connected: dispatch the voice preview; the projection is
                    // the sole writer of `state.room` (voice presence echoes
                    // back as VoiceUpdated). Offline: the local adapter is
                    // authoritative, so write it directly - preserve octave info.
                    if !self.native_backend {
                        self.room
                            .lock_mut()
                            .set_voice(Some(absolute_pitch), Some(pc));
                    }
                    self.send_native_voice(Some(native_pitch));

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
        let next_session = self.voice_session.borrow().wrapping_add(1).max(1);
        *self.voice_session.borrow_mut() = next_session;
        self.send_native_voice(None);

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
                        web_sys::console::error_1(
                            &format!("Failed to create audio: {:?}", e).into(),
                        );
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
                                let _ = window
                                    .set_timeout_with_callback_and_timeout_and_arguments_0(
                                        &resolve, 50,
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
        if self.native_backend {
            if let Some(pitch) = self.committed_pitch.get() {
                self.set_native_degree(pitch, true);
            }
            // Dispatch only; the projection clears the voice adapter when the
            // SetVoicePreview(None) echo arrives. No optimistic local write.
            self.send_native_voice(None);
            self.voice_pitch.set(None);
        }

        // Re-sync keyboard (voice pitch -> lit/green, manual -> pressed/red)
        sync_active_pitches(self);

        // Update MIDI voice output
        self.sync_midi_voice_output();

        self.current_pitch.set(None);
    }

    /// Set the selected MIDI input device.
    pub fn set_midi_input(self: &Arc<Self>, device_id: Option<String>) {
        if self.tauri_backend() {
            self.midi_input_id.set(device_id.clone());
            self.dispatch_native(ClientCommand::SelectMidiInput { port_id: device_id });
            return;
        }
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
        if self.tauri_backend() {
            self.midi_output_id.set(device_id.clone());
            self.dispatch_native(ClientCommand::SelectMidiOutput { port_id: device_id });
            return;
        }
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
        let room_name = name.split('@').next().unwrap_or(&name).to_owned();
        set_url_hash(&room_name);
        self.room_name.set(room_name.clone());
        self.room_input.set(room_name.clone());
        self.dispatch_native(ClientCommand::EnterRoom { room_name });
    }

    pub fn enter_room_or_ticket(&self, input: String) {
        if let Some(room_name) = crate::words::parse_room_input(&input) {
            self.set_room_name(room_name);
        } else if self.native_backend && !input.trim().is_empty() {
            self.room_input.set(input.clone());
            self.dispatch_native(ClientCommand::JoinTicket {
                ticket: input.trim().to_owned(),
            });
        }
    }

    /// Poll MIDI input and route note events to toggle set.
    pub fn poll_midi_input(self: &Arc<Self>) {
        // Tauri owns MIDI natively; the browser (offline OR in-page host) polls
        // Web MIDI here.
        if self.tauri_backend() {
            return;
        }
        while let Some(event) = MidiManager::poll_input() {
            self.route_midi_event(event);
        }
    }

    /// Route one decoded Web MIDI input event into the store/room. Shared by the
    /// synchronous drain ([`Self::poll_midi_input`]) and the event-driven consumer
    /// that awaits the MIDI channel (the browser MIDI task in `run`), so the UI
    /// never polls on a timer for MIDI.
    fn route_midi_event(self: &Arc<Self>, event: MidiInputEvent) {
        // MIDI input is 12-TET frequency data, even when the active room tuning is
        // not. Quantize the frequency; never reduce modulo the room's degree count.
        let hz = 440.0 * 2.0_f64.powf((f64::from(event.note) - 69.0) / 12.0);
        let Ok(result) = self.tuning.lock_ref().quantize(hz) else {
            return;
        };
        let pitch_class = result.pitch_class;

        if self.browser_host {
            // The signed store is authoritative: route note-on/off as durable
            // degree commands; the projection updates the UI.
            self.set_native_degree(pitch_class, event.is_note_on);
        } else {
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
        // Web MIDI output stays live in browser-host mode: the projection keeps
        // the Yrs render adapter fresh, and this reads from it.
        if self.tauri_backend() {
            return;
        }
        // Skip if MIDI output is disabled
        if !self.midi_output_enabled() {
            return;
        }

        let room = self.room.lock_ref();
        let local_peer = room.local_peer_id();
        let pitch_count = self.tuning.lock_ref().pitch_class_count() as u16;

        // Get current pitch classes from local peer's set
        let mut current_notes: std::collections::HashSet<u8> =
            if let Some(set) = room.all_peer_sets().get(local_peer) {
                set.pitch_classes
                    .iter()
                    .filter_map(|pc| pitch_class_to_midi_note(pc.index(), pitch_count))
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
        if self.tauri_backend() {
            return;
        }
        // Skip if MIDI output is disabled
        if !self.midi_output_enabled() {
            return;
        }

        let (absolute_pitch, voice_pitch) = self.room.lock_ref().local_voice();
        let pitch_count = self.tuning.lock_ref().pitch_class_count() as u16;

        if let Ok(midi) = self.midi.try_borrow() {
            let mut output = midi.output.borrow_mut();

            if let Some(note) = absolute_pitch.and_then(|pitch| u8::try_from(pitch).ok()) {
                // Preserve the detected periodic position in standard 12-TET.
                output.voice_note_on(note);
            } else if let Some(pc) = voice_pitch
                && let Some(note) = pitch_class_to_midi_note(pc.index(), pitch_count)
            {
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
    ///
    /// Uses fixed-size chunks to maintain consistent timing for the voice conditioner,
    /// which assumes ~2048-sample frames for its EMA coefficients.
    pub async fn process_swiftf0(self: &Arc<Self>) {
        if !self.swiftf0_ready.get() || !self.voice_active.get() {
            return;
        }

        // Process in fixed chunks to maintain consistent conditioner timing
        const CHUNK_SIZE: usize = 2048;
        const MAX_CHUNKS_PER_CALL: usize = 2; // Limit to prevent blocking too long

        let mut chunks_processed = 0;

        loop {
            if chunks_processed >= MAX_CHUNKS_PER_CALL {
                break;
            }

            // Extract exactly one chunk (or break if not enough samples)
            let samples: Option<Vec<f32>> = {
                let mut buffer = self.swiftf0_buffer.borrow_mut();
                if buffer.len() >= CHUNK_SIZE {
                    Some(buffer.drain(..CHUNK_SIZE).collect())
                } else {
                    None
                }
            };

            let Some(samples) = samples else { break };
            chunks_processed += 1;

            // Run voice conditioner on fixed-size chunk
            let conditioner_output = {
                let mut conditioner = self.conditioner.borrow_mut();
                conditioner.decay_reference();
                conditioner.process(&samples)
            };

            // Update gate state for UI
            self.gate_open.set(conditioner_output.gate_open);

            // Only run pitch detection if gate is open
            if !conditioner_output.gate_open {
                self.continuous_hz.set(None);
                self.final_confidence.set(0.0);

                if self.voice_pitch.get().is_some() {
                    self.voice_pitch.set(None);
                    // Connected: dispatch only; the projection clears the voice
                    // adapter. Offline: the local adapter is authoritative.
                    if !self.native_backend {
                        self.room.lock_mut().set_voice(None, None);
                    }
                    self.send_native_voice(None);
                    self.sync_midi_voice_output();
                }
                continue; // Process next chunk without inference
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
                self.continuous_hz.set(None);
                self.final_confidence.set(0.0);
            }
        }
    }
}

fn apply_native_delta(snapshot: &mut AppSnapshot, event: AppEvent) {
    match event {
        AppEvent::Snapshot {
            snapshot: replacement,
        } => *snapshot = *replacement,
        AppEvent::RoomChanged {
            room_name,
            room_topic,
            ticket,
        } => {
            snapshot.room_name = room_name;
            snapshot.room_topic = room_topic;
            snapshot.room_ticket = ticket;
        }
        AppEvent::TuningChanged { definition } => {
            snapshot.tuning_id = Some(definition.id);
            snapshot.tuning = Some(definition);
        }
        AppEvent::DegreeAdded { pitch, .. } => {
            if !snapshot.active_degrees.contains(&pitch) {
                snapshot.active_degrees.push(pitch);
                snapshot.active_degrees.sort();
            }
        }
        AppEvent::DegreeRemoved { pitch } => {
            snapshot.active_degrees.retain(|current| *current != pitch);
        }
        AppEvent::PieceUpserted { piece } => {
            if let Some(current) = snapshot
                .pieces
                .iter_mut()
                .find(|current| current.id == piece.id)
            {
                *current = piece;
            } else {
                snapshot.pieces.push(piece);
                snapshot.pieces.sort_by_key(|piece| piece.id);
            }
        }
        AppEvent::PieceRemoved { piece } => {
            snapshot.pieces.retain(|current| current.id != piece);
        }
        AppEvent::RoomConfigChanged {
            pieces_locked,
            available_emojis,
        } => {
            snapshot.pieces_locked = pieces_locked;
            snapshot.available_emojis = available_emojis;
        }
        AppEvent::VoiceUpdated { voice } => {
            if let Some(current) = snapshot
                .voices
                .iter_mut()
                .find(|current| current.author == voice.author)
            {
                *current = voice;
            } else {
                snapshot.voices.push(voice);
                snapshot.voices.sort_by_key(|voice| voice.author);
            }
        }
        AppEvent::VoiceExpired { author, session } => snapshot
            .voices
            .retain(|voice| voice.author != author || voice.session != session),
        AppEvent::PeerUpdated { peer } => {
            if let Some(current) = snapshot
                .peers
                .iter_mut()
                .find(|current| current.author == peer.author)
            {
                *current = peer;
            } else {
                snapshot.peers.push(peer);
                snapshot.peers.sort_by_key(|peer| peer.author);
            }
        }
        AppEvent::PeerRemoved { author } => {
            snapshot.peers.retain(|peer| peer.author != author);
        }
        AppEvent::MidiPortsChanged { inputs, outputs } => {
            snapshot.midi_inputs = inputs;
            snapshot.midi_outputs = outputs;
        }
        AppEvent::Diagnostic { code, message } => {
            web_sys::console::warn_1(&format!("[native:{code}] {message}").into());
        }
    }
}

fn native_status(snapshot: &AppSnapshot) -> String {
    if snapshot.room_topic.is_none() {
        return "Not in a room".to_owned();
    }
    let direct = snapshot
        .peers
        .iter()
        .find(|peer| peer.path == PeerPath::Direct);
    let relayed = snapshot
        .peers
        .iter()
        .find(|peer| peer.path == PeerPath::Relayed);
    let connecting = snapshot
        .peers
        .iter()
        .filter(|peer| peer.path == PeerPath::Connecting)
        .count();
    let discovery = |source: DiscoverySource| match source {
        DiscoverySource::Mdns => "mDNS",
        DiscoverySource::Ticket => "ticket",
        DiscoverySource::Gossip => "gossip",
        DiscoverySource::AddressLookup => "address lookup",
    };
    let sync = |synchronized: bool| {
        if synchronized {
            "synchronized"
        } else {
            "reconciling"
        }
    };
    let rtt = |round_trip_ms: Option<u32>| {
        round_trip_ms
            .map(|value| format!(", {value} ms"))
            .unwrap_or_default()
    };

    if let Some(peer) = direct {
        format!(
            "✓ Direct via {}, {}{}",
            discovery(peer.discovery),
            sync(peer.synchronized),
            rtt(peer.round_trip_ms)
        )
    } else if let Some(peer) = relayed {
        format!(
            "↗ Relayed via {}, {}; direct hole punch unavailable{}",
            discovery(peer.discovery),
            sync(peer.synchronized),
            rtt(peer.round_trip_ms)
        )
    } else if connecting > 0 {
        format!("⏳ Connecting to {connecting} discovered peer(s)…")
    } else if snapshot.capabilities.mdns {
        "⏳ Searching this room with mDNS and relay discovery…".to_owned()
    } else {
        "⏳ Waiting for peers via the relay (share the ticket or link)…".to_owned()
    }
}

fn parse_piece_id(value: &str) -> Option<crate::room::v5::PieceId> {
    if value.len() != 64 {
        return None;
    }
    let mut bytes = [0_u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(crate::room::v5::PieceId(bytes))
}

/// Render the main application.
fn render_app(state: Arc<AppState>) -> Dom {
    html!("div", {
        .class("app")
        .children(&mut [
            // Header: connect | title | lock
            html!("header", {
                .class("header")
                .children(&mut [
                    room_header_button(state.clone()),
                    html!("span", { .class("title").text("walkie songie") }),
                    lock_button(state.clone()),
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

                    // Page 2: Scale Info
                    html!("div", {
                        .class("page")
                        .class("info-page")
                        .children(&mut [
                            // Active pitches section
                            html!("section", {
                                .class("active-pitches-section")
                                .child(active_pitches_list(state.clone()))
                            }),

                            // Info panel with scale names, bass/treble solfege
                            info_panel(state.clone()),
                        ])
                    }),

                    // Page 3: Settings
                    html!("div", {
                        .class("page")
                        .class("settings-page")
                        .children(&mut [
                            tuning_editor(state.clone()),
                            midi_settings(state.clone()),
                            clear_button(state.clone()),
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
                    html!("div", { .class("page-dot") }),
                ])
            }),
        ])
    })
}

/// List of currently active pitch classes.
fn active_pitches_list(state: Arc<AppState>) -> Dom {
    use futures_signals::signal::SignalExt;

    // Create signal from room events
    let (initial_data, events) = {
        let room = state.room.lock_ref();
        let tuning = state.tuning.lock_ref();
        let data = compute_active_pitches_data(&room, &tuning);
        (data, room.events())
    };

    let state_for_stream = state.clone();
    let state_stream = events
        .filter(|e| ready(e.affects_pitches() || e.affects_voice() || e.affects_pieces()))
        .map(move |_| {
            let room = state_for_stream.room.lock_ref();
            let tuning = state_for_stream.tuning.lock_ref();
            compute_active_pitches_data(&room, &tuning)
        });

    let full_stream = futures::stream::once(ready(initial_data)).chain(state_stream);
    let pitches_signal = from_stream(full_stream).map(|opt| opt.unwrap_or_default());

    html!("div", {
        .class("active-pitches")
        .child_signal(pitches_signal.map(|active| {
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
        }))
    })
}

/// Compute active pitches data for display.
/// Returns Vec of (name, is_voice, is_piece, sort_key).
fn compute_active_pitches_data(
    room: &RoomProjection,
    tuning: &Tuning,
) -> Vec<(String, bool, bool, i32)> {
    let pc_count = tuning.pitch_class_count() as i32;

    // Get combined pitches from the current Replica projection.
    let room_result = room.compute_room_result();

    // Get the local projected voice pitch (shows as green).
    let (_, local_voice_pc) = room.local_voice();

    // Build list of active pitches: (name, is_voice, is_piece, sort_key)
    let mut active: Vec<(String, bool, bool, i32)> = Vec::new();

    // Add all pitches from room result (toggle mode pitches)
    for pc in &room_result.pitch_classes {
        let is_voice = local_voice_pc == Some(*pc);
        active.push((
            tuning.note_name(*pc).to_string(),
            is_voice,
            false,
            pc.index() as i32,
        ));
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

    active
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
            // iroh's networking spans (connect, do_holepunching, relay actors,
            // gossip) fire constantly at INFO; tracing-wasm was turning every one
            // into a performance.mark/measure, flooding the devtools timeline and
            // burning main-thread time during interaction (visible as ~70ms of
            // mark/measure in a drag trace). Keep console logging, drop the marks.
            .set_report_logs_in_timings(false)
            .build(),
    );

    // Initialize app asynchronously (to load peer ID from IndexedDB)
    spawn_local(async {
        init_app().await;
    });
}

/// Initialize the application (async to support IndexedDB).
async fn init_app() {
    let tauri = super::native_bridge::is_available();
    // Native identity lives in Tauri app data. The browser identifier is used
    // only by the legacy rendering adapter in that mode (the in-page host's
    // cryptographic identity is a separate IndexedDB seed; see browser_host).
    let peer_id = if tauri {
        "native-ui".to_owned()
    } else {
        super::storage::get_or_create_peer_id().await
    };

    // Create application state
    let state = AppState::new(peer_id);

    if state.tauri_backend() {
        let event_state = state.clone();
        if let Err(error) = super::native_bridge::register_events(move |event| {
            event_state.apply_native_event(event);
        })
        .await
        {
            web_sys::console::error_1(
                &format!("could not register native event channel: {error:?}").into(),
            );
        }
        state.dispatch_native(ClientCommand::ListMidiPorts);
    } else {
        // Any browser (offline OR in-page host) retains Web MIDI and hot-plug.
        let midi_manager = state.midi.clone();
        let midi_version = state.midi_devices_version.clone();
        super::midi::init_midi_with_callback(midi_manager.clone(), move || {
            if let Ok(mut midi) = midi_manager.try_borrow_mut() {
                midi.refresh_devices();
            }
            midi_version.set(midi_version.get() + 1);
        });

        // Stand up the in-page iroh host BEFORE the EnterRoom dispatch below,
        // and feed its ordered events through the same apply seam Tauri uses.
        #[cfg(feature = "browser-net")]
        if state.browser_host {
            let event_state = state.clone();
            if let Err(error) = super::replica_host::init(move |event| {
                event_state.apply_native_event(event);
            })
            .await
            {
                web_sys::console::error_1(
                    &format!("could not start browser networking: {error}").into(),
                );
                state
                    .native_status
                    .set(format!("⚠ browser networking unavailable: {error}"));
            }
        }
    }

    // Initialize room name (from URL or generate new) and sync to hash
    // room_topic may include @peer-id for bootstrapping
    let room_topic = get_or_generate_room_name();
    // Extract just the room name for display/state (strip @peer-id if present)
    let room_name = room_topic
        .split('@')
        .next()
        .unwrap_or(&room_topic)
        .to_string();
    state.set_room_name(room_name.clone());

    let _room_topic = room_topic;

    // Initialize SwiftF0 ML model in background
    let state_for_init = state.clone();
    spawn_local(async move {
        state_for_init.init_swiftf0().await;
    });

    if !state.tauri_backend() {
        // Web MIDI is event-driven: the `onmidimessage` callback pushes decoded
        // events into an async channel (see `web::midi`). Await that channel rather
        // than waking on a 16ms `setTimeout` — the old poll loop allocated a
        // Promise + closure + timer ~60x/sec forever, churning the main thread
        // during drags/interaction (visible as a setTimeout/TimerFire storm in a
        // trace). Now we wake only when MIDI actually arrives, which also removes
        // up to ~16ms of input latency.
        let state_for_midi = state.clone();
        spawn_local(async move {
            let rx = MidiManager::input_receiver();
            while let Ok(event) = rx.recv().await {
                state_for_midi.route_midi_event(event);
            }
        });
    }

    // Subscribe to room events for UI updates and state persistence
    let state_for_events = state.clone();
    spawn_local(async move {
        // Get event stream from room
        let events = state_for_events.room.lock_ref().events();

        // Process each event
        events
            .for_each(|event| {
                // Handle the event
                handle_room_event(&state_for_events, &event);
                async {}
            })
            .await;
    });

    // Mount the app to the DOM
    dominator::append_dom(&dominator::body(), render_app(state.clone()));

    // Graph visualization is now disabled on page 2 (replaced with info panel)
    // Graph initialization is no longer needed
}

/// Update graph highlights - no-op since graph is disabled
/// (kept for API compatibility but does nothing)
#[allow(dead_code)]
fn update_graph_highlights(_state: &Arc<AppState>) {
    // Graph visualization is disabled - info panel handles scale display reactively
}

/// Handle a projected room event and update UI/MIDI consumers.
fn handle_room_event(state: &Arc<AppState>, event: &RoomEvent) {
    // Log event for debugging
    web_sys::console::log_1(&format!("[RoomEvent] {:?}", event).into());

    // Handle pitch-affecting events
    if event.affects_pitches() {
        sync_active_pitches(state);
        state.sync_midi_toggle_output();
        update_graph_highlights(state);
    }

    // Handle voice events
    if event.affects_voice() {
        sync_active_pitches(state);
        state.sync_midi_voice_output();
        update_graph_highlights(state);
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
        update_graph_highlights(state);
    }
}
