//! Shared output streams and signals derived from room state.
//!
//! Provides both streams (for event sequences) and signals (for current state):
//! - Streams: delta computation for MIDI output (note-on/note-off)
//! - Signals: current state for UI rendering
//!
//! Pattern inspired by musical-graphs inputStreams.ts:
//! - Compute current state as a Set
//! - Diff against previous state to get (added, removed)
//! - Only emit MIDI for the delta

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use futures::{Stream, StreamExt, future::ready};

use super::events::RoomEvent;
use super::yrs_state::RoomState;

#[cfg(target_arch = "wasm32")]
use super::yrs_state::Piece;
#[cfg(target_arch = "wasm32")]
use crate::tuning::PitchClass;
#[cfg(target_arch = "wasm32")]
use futures_signals::signal::{Signal, SignalExt, from_stream};

/// Delta of pitch classes (for MIDI note-on/note-off).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PitchClassDelta {
    /// Pitch classes that just became active (send note-on)
    pub added: Vec<u16>,
    /// Pitch classes that just became inactive (send note-off)
    pub removed: Vec<u16>,
}

impl PitchClassDelta {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

/// Delta of absolute pitches (for MIDI note-on/note-off).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PitchDelta {
    /// Pitches that just became active (send note-on)
    pub added: Vec<i32>,
    /// Pitches that just became inactive (send note-off)
    pub removed: Vec<i32>,
}

impl PitchDelta {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

/// Snapshot of all active pitches/pitch classes from all sources.
/// Used internally for computing deltas.
#[derive(Debug, Clone, Default)]
pub struct ActivePitchesSnapshot {
    /// Toggle pitch classes (manual keyboard clicks)
    pub toggle_pitch_classes: HashSet<u16>,
    /// Piece absolute pitches
    pub piece_pitches: HashSet<i32>,
    /// Voice absolute pitches (all peers)
    pub voice_pitches: HashSet<i32>,
}

impl ActivePitchesSnapshot {
    /// Compute unified pitch classes from all sources.
    pub fn unified_pitch_classes(&self) -> HashSet<u16> {
        let mut unified = self.toggle_pitch_classes.clone();

        // Add piece pitch classes
        for &pitch in &self.piece_pitches {
            unified.insert(pitch.rem_euclid(12) as u16);
        }

        // Add voice pitch classes
        for &pitch in &self.voice_pitches {
            unified.insert(pitch.rem_euclid(12) as u16);
        }

        unified
    }

    /// Compute piece pitch classes (for separate output).
    pub fn piece_pitch_classes(&self) -> HashSet<u16> {
        self.piece_pitches
            .iter()
            .map(|&p| p.rem_euclid(12) as u16)
            .collect()
    }

    /// Compute voice pitch classes (for separate output).
    pub fn voice_pitch_classes(&self) -> HashSet<u16> {
        self.voice_pitches
            .iter()
            .map(|&p| p.rem_euclid(12) as u16)
            .collect()
    }
}

/// Compute current snapshot from room state.
pub fn snapshot_active_pitches(room: &RoomState) -> ActivePitchesSnapshot {
    ActivePitchesSnapshot {
        toggle_pitch_classes: room.shared_pitches().iter().map(|pc| pc.index()).collect(),
        piece_pitches: room.all_pieces().iter().map(|p| p.pitch).collect(),
        voice_pitches: room.all_voice_pitches(),
    }
}

// =============================================================================
// DELTA STREAMS (for MIDI output)
// =============================================================================

/// Stream of unified pitch class deltas.
/// This is THE key stream for MIDI output - note-off only fires when
/// a pitch class is inactive from ALL sources (toggles + pieces + voice).
///
/// The first emitted delta represents the initial state (empty → current).
/// Consumers should use this for the main MIDI output.
///
/// Takes Arc<RwLock<RoomState>> so it can query authoritative state on each event.
pub fn unified_pitch_class_deltas(
    room: Arc<RwLock<RoomState>>,
) -> impl Stream<Item = PitchClassDelta> {
    // Get initial state and event stream (read lock, release immediately)
    let (initial_unified, events) = {
        let room_guard = room.read().unwrap();
        let snapshot = snapshot_active_pitches(&room_guard);
        (snapshot.unified_pitch_classes(), room_guard.events())
    };

    // Emit initial state as first delta (empty → current)
    let initial_delta = if initial_unified.is_empty() {
        None
    } else {
        Some(PitchClassDelta {
            added: initial_unified.iter().copied().collect(),
            removed: vec![],
        })
    };

    let room_for_stream = room.clone();
    let event_deltas = events
        .scan(initial_unified, move |prev_unified, _event| {
            // Read lock, grab data, release immediately
            let new_unified = {
                let room_guard = room_for_stream.read().unwrap();
                snapshot_active_pitches(&room_guard).unified_pitch_classes()
            };

            // Compute delta (lock already released)
            let added: Vec<u16> = new_unified.difference(prev_unified).copied().collect();
            let removed: Vec<u16> = prev_unified.difference(&new_unified).copied().collect();

            *prev_unified = new_unified;

            ready(Some(PitchClassDelta { added, removed }))
        })
        .filter_map(|delta| ready(if delta.is_empty() { None } else { Some(delta) }));

    // Prepend initial delta (if any) before event-driven deltas
    futures::stream::iter(initial_delta).chain(event_deltas)
}

/// Stream of piece pitch deltas (absolute pitches).
/// For routing pieces to a separate MIDI channel with octave info.
///
/// The first emitted delta represents the initial state (empty → current).
pub fn piece_pitch_deltas(room: Arc<RwLock<RoomState>>) -> impl Stream<Item = PitchDelta> {
    // Get initial state and event stream (read lock, release immediately)
    let (initial, events) = {
        let room_guard = room.read().unwrap();
        let pitches: HashSet<i32> = room_guard.all_pieces().iter().map(|p| p.pitch).collect();
        (pitches, room_guard.events())
    };

    // Emit initial state as first delta
    let initial_delta = if initial.is_empty() {
        None
    } else {
        Some(PitchDelta {
            added: initial.iter().copied().collect(),
            removed: vec![],
        })
    };

    let room_for_stream = room.clone();
    let event_deltas = events
        .scan(initial, move |prev_pitches, event| {
            // Only recompute on piece-related events
            let new_pitches: HashSet<i32> = match &event {
                RoomEvent::PieceAdded { .. }
                | RoomEvent::PieceMoved { .. }
                | RoomEvent::PieceRemoved { .. }
                | RoomEvent::PiecesCleared
                | RoomEvent::FullStateSync { .. } => {
                    // Read lock, grab data, release immediately
                    let room_guard = room_for_stream.read().unwrap();
                    room_guard.all_pieces().iter().map(|p| p.pitch).collect()
                }
                _ => prev_pitches.clone(),
            };

            let added: Vec<i32> = new_pitches.difference(prev_pitches).copied().collect();
            let removed: Vec<i32> = prev_pitches.difference(&new_pitches).copied().collect();

            *prev_pitches = new_pitches;

            ready(Some(PitchDelta { added, removed }))
        })
        .filter_map(|delta| ready(if delta.is_empty() { None } else { Some(delta) }));

    futures::stream::iter(initial_delta).chain(event_deltas)
}

/// Stream of voice pitch deltas (absolute pitches).
/// For routing voice to a separate MIDI channel.
///
/// The first emitted delta represents the initial state (empty → current).
pub fn voice_pitch_deltas(room: Arc<RwLock<RoomState>>) -> impl Stream<Item = PitchDelta> {
    // Get initial state and event stream (read lock, release immediately)
    let (initial, events) = {
        let room_guard = room.read().unwrap();
        (room_guard.all_voice_pitches(), room_guard.events())
    };

    // Emit initial state as first delta
    let initial_delta = if initial.is_empty() {
        None
    } else {
        Some(PitchDelta {
            added: initial.iter().copied().collect(),
            removed: vec![],
        })
    };

    let room_for_stream = room.clone();
    let event_deltas = events
        .scan(initial, move |prev_pitches, event| {
            // Only recompute on voice-related events
            let new_pitches: HashSet<i32> = match &event {
                RoomEvent::VoiceChanged { .. }
                | RoomEvent::VoiceCleared { .. }
                | RoomEvent::FullStateSync { .. } => {
                    // Read lock, grab data, release immediately
                    let room_guard = room_for_stream.read().unwrap();
                    room_guard.all_voice_pitches()
                }
                _ => prev_pitches.clone(),
            };

            let added: Vec<i32> = new_pitches.difference(prev_pitches).copied().collect();
            let removed: Vec<i32> = prev_pitches.difference(&new_pitches).copied().collect();

            *prev_pitches = new_pitches;

            ready(Some(PitchDelta { added, removed }))
        })
        .filter_map(|delta| ready(if delta.is_empty() { None } else { Some(delta) }));

    futures::stream::iter(initial_delta).chain(event_deltas)
}

// =============================================================================
// SIGNALS (for UI - current state, not deltas)
// Only available in wasm32 where futures-signals is available.
// =============================================================================

/// Signal of unified pitch classes (all sources combined).
/// Emits the current set whenever any pitch-related change occurs.
/// Use this for keyboard highlighting, etc.
#[cfg(target_arch = "wasm32")]
pub fn unified_pitch_classes_signal(
    room: Arc<RwLock<RoomState>>,
) -> impl Signal<Item = HashSet<u16>> {
    // Get initial state and events
    let (initial, events) = {
        let room_guard = room.read().unwrap();
        let snapshot = snapshot_active_pitches(&room_guard);
        (snapshot.unified_pitch_classes(), room_guard.events())
    };

    // Stream that emits current state on each relevant event
    let room_for_stream = room.clone();
    let state_stream = events
        .filter(|e| ready(e.affects_pitches() || e.affects_voice()))
        .map(move |_| {
            let room_guard = room_for_stream.read().unwrap();
            snapshot_active_pitches(&room_guard).unified_pitch_classes()
        });

    // Prepend initial state, convert to signal
    let full_stream = futures::stream::once(ready(initial)).chain(state_stream);
    from_stream(full_stream).map(|opt| opt.unwrap_or_default())
}

/// Signal of shared pitch classes (toggle-based, not pieces/voice).
/// Use this for showing which keys are "locked on" via manual toggle.
#[cfg(target_arch = "wasm32")]
pub fn shared_pitches_signal(
    room: Arc<RwLock<RoomState>>,
) -> impl Signal<Item = HashSet<PitchClass>> {
    let (initial, events) = {
        let room_guard = room.read().unwrap();
        (room_guard.shared_pitches(), room_guard.events())
    };

    let room_for_stream = room.clone();
    let state_stream = events
        .filter(|e| {
            ready(matches!(
                e,
                RoomEvent::PitchAdded { .. }
                    | RoomEvent::PitchRemoved { .. }
                    | RoomEvent::PitchesCleared
                    | RoomEvent::FullStateSync { .. }
            ))
        })
        .map(move |_| room_for_stream.read().unwrap().shared_pitches());

    let full_stream = futures::stream::once(ready(initial)).chain(state_stream);
    from_stream(full_stream).map(|opt| opt.unwrap_or_default())
}

/// Signal of all pieces.
/// Emits the current piece list whenever pieces change.
#[cfg(target_arch = "wasm32")]
pub fn pieces_signal(room: Arc<RwLock<RoomState>>) -> impl Signal<Item = Vec<Piece>> {
    let (initial, events) = {
        let room_guard = room.read().unwrap();
        (room_guard.all_pieces(), room_guard.events())
    };

    let room_for_stream = room.clone();
    let state_stream = events
        .filter(|e| ready(e.affects_pieces()))
        .map(move |_| room_for_stream.read().unwrap().all_pieces());

    let full_stream = futures::stream::once(ready(initial)).chain(state_stream);
    from_stream(full_stream).map(|opt| opt.unwrap_or_default())
}

/// Signal of pieces lock state.
#[cfg(target_arch = "wasm32")]
pub fn pieces_locked_signal(room: Arc<RwLock<RoomState>>) -> impl Signal<Item = bool> {
    let (initial, events) = {
        let room_guard = room.read().unwrap();
        (room_guard.pieces_locked(), room_guard.events())
    };

    let room_for_stream = room.clone();
    let state_stream = events
        .filter(|e| {
            ready(matches!(
                e,
                RoomEvent::PiecesLockChanged { .. } | RoomEvent::FullStateSync { .. }
            ))
        })
        .map(move |_| room_for_stream.read().unwrap().pieces_locked());

    let full_stream = futures::stream::once(ready(initial)).chain(state_stream);
    from_stream(full_stream).map(|opt| opt.unwrap_or(false))
}

/// Signal of available emojis for the picker.
#[cfg(target_arch = "wasm32")]
pub fn available_emojis_signal(room: Arc<RwLock<RoomState>>) -> impl Signal<Item = Vec<String>> {
    let (initial, events) = {
        let room_guard = room.read().unwrap();
        (room_guard.available_emojis(), room_guard.events())
    };

    let room_for_stream = room.clone();
    let state_stream = events
        .filter(|e| {
            ready(matches!(
                e,
                RoomEvent::EmojisChanged { .. } | RoomEvent::FullStateSync { .. }
            ))
        })
        .map(move |_| room_for_stream.read().unwrap().available_emojis());

    let full_stream = futures::stream::once(ready(initial)).chain(state_stream);
    from_stream(full_stream).map(|opt| opt.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pitch_class_delta_empty() {
        let delta = PitchClassDelta::default();
        assert!(delta.is_empty());

        let delta = PitchClassDelta {
            added: vec![0],
            removed: vec![],
        };
        assert!(!delta.is_empty());
    }

    #[test]
    fn test_active_pitches_unified() {
        let mut snapshot = ActivePitchesSnapshot::default();

        // Toggle C
        snapshot.toggle_pitch_classes.insert(0);

        // Piece at C4 (pitch 60)
        snapshot.piece_pitches.insert(60);

        // Voice at G4 (pitch 67)
        snapshot.voice_pitches.insert(67);

        let unified = snapshot.unified_pitch_classes();

        // Should have C (0) from toggle and piece, G (7) from voice
        assert!(unified.contains(&0));
        assert!(unified.contains(&7));
        assert_eq!(unified.len(), 2);
    }
}
