//! Room state management using CRDTs.
//!
//! The CRDT is the single source of truth for room state:
//! - Shared pitch class set (toggleable keyboard)
//! - Emoji pieces with positions
//! - Per-peer voice state
//! - Room tuning (SCL content)
//! - Combination method for computing room result

pub mod events;
#[cfg(not(target_arch = "wasm32"))]
pub mod journal;
pub mod ops;
pub mod presence;
pub mod store;
pub mod streams;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod yrs_state;

// `view.rs` (per-author-union hand fold) and `mirror.rs` (opaque-entry HHHS mirror)
// were removed: the data design consolidated onto HHHS-native materialization
// (content-keyed add-wins pitches, op-id-keyed pieces, causal-maxima registers). The
// replacement lands in `store.rs` (RoomStore: verbatim-bytes lift + cover/register
// views). `ops.rs` (WalkieOp v2) is the shared op alphabet.

pub use events::RoomEvent;
pub use streams::{
    ActivePitchesSnapshot,
    // Delta streams (for MIDI output)
    PitchClassDelta,
    PitchDelta,
    piece_pitch_deltas,
    snapshot_active_pitches,
    unified_pitch_class_deltas,
    voice_pitch_deltas,
};

// State signals (for UI) - only available in wasm32
#[cfg(target_arch = "wasm32")]
pub use streams::{
    available_emojis_signal, pieces_locked_signal, pieces_signal, shared_pitches_signal,
    unified_pitch_classes_signal,
};
pub use yrs_state::{Piece, RoomState};

use std::collections::{HashMap, HashSet};

use crate::tuning::PitchClass;

/// Method for combining peer pitch class sets into a room result.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CombinationMethod {
    /// Room result = union of all peer sets (any peer's pitch classes)
    #[default]
    Union,
    /// Room result = intersection of all peer sets (only shared pitch classes)
    Intersection,
    /// Custom combination method (for future extensibility)
    Custom(String),
}

impl CombinationMethod {
    /// Parse a combination method from a string identifier.
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "union" => Self::Union,
            "intersection" => Self::Intersection,
            _ => Self::Custom(s.to_string()),
        }
    }

    /// Convert to string identifier.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Union => "union",
            Self::Intersection => "intersection",
            Self::Custom(s) => s.as_str(),
        }
    }
}

/// A peer's contribution to the room state.
#[derive(Debug, Clone, Default)]
pub struct PeerPitchSet {
    /// Active pitch class indices for this peer
    pub pitch_classes: HashSet<PitchClass>,
}

impl PeerPitchSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a pitch class to the set.
    pub fn add(&mut self, pc: PitchClass) {
        self.pitch_classes.insert(pc);
    }

    /// Remove a pitch class from the set.
    pub fn remove(&mut self, pc: PitchClass) {
        self.pitch_classes.remove(&pc);
    }

    /// Toggle a pitch class (add if absent, remove if present).
    pub fn toggle(&mut self, pc: PitchClass) -> bool {
        if self.pitch_classes.contains(&pc) {
            self.pitch_classes.remove(&pc);
            false
        } else {
            self.pitch_classes.insert(pc);
            true
        }
    }

    /// Check if a pitch class is active.
    pub fn contains(&self, pc: PitchClass) -> bool {
        self.pitch_classes.contains(&pc)
    }

    /// Set to a single pitch class (for voice input single-pitch mode).
    pub fn set_single(&mut self, pc: PitchClass) {
        self.pitch_classes.clear();
        self.pitch_classes.insert(pc);
    }

    /// Clear all pitch classes.
    pub fn clear(&mut self) {
        self.pitch_classes.clear();
    }
}

/// Result of combining peer pitch sets.
#[derive(Debug, Clone, Default)]
pub struct RoomPitchResult {
    /// Combined pitch classes based on the combination method
    pub pitch_classes: HashSet<PitchClass>,
    /// Attribution: which peers contributed each pitch class
    pub attribution: HashMap<PitchClass, Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_peer_pitch_set_toggle() {
        let mut set = PeerPitchSet::new();
        let pc = PitchClass(5);

        assert!(!set.contains(pc));
        assert!(set.toggle(pc)); // Added
        assert!(set.contains(pc));
        assert!(!set.toggle(pc)); // Removed
        assert!(!set.contains(pc));
    }

    #[test]
    fn test_single_pitch_mode() {
        let mut set = PeerPitchSet::new();
        set.add(PitchClass(1));
        set.add(PitchClass(2));
        assert_eq!(set.pitch_classes.len(), 2);

        set.set_single(PitchClass(5));
        assert_eq!(set.pitch_classes.len(), 1);
        assert!(set.contains(PitchClass(5)));
    }
}
