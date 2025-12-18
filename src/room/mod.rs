//! Room state management using CRDTs.
//!
//! Provides the `RoomState` trait for managing shared state:
//! - Per-peer pitch class sets
//! - Room tuning (SCL content)
//! - Combination method for computing room result

pub mod yrs_state;

pub use yrs_state::{Piece, YrsRoomState};

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

/// Trait for room state management.
///
/// Implementations handle CRDT synchronization with peers.
pub trait RoomState: Send + Sync {
    /// Get the local peer's ID.
    fn local_peer_id(&self) -> &str;

    /// Get the local peer's pitch set.
    fn local_pitch_set(&self) -> &PeerPitchSet;

    /// Get a mutable reference to the local peer's pitch set.
    fn local_pitch_set_mut(&mut self) -> &mut PeerPitchSet;

    /// Add a pitch class to the local peer's set.
    fn add_pitch(&mut self, pc: PitchClass);

    /// Remove a pitch class from the local peer's set.
    fn remove_pitch(&mut self, pc: PitchClass);

    /// Toggle a pitch class in the local peer's set.
    fn toggle_pitch(&mut self, pc: PitchClass) -> bool;

    /// Set the local peer's pitch to a single value (voice input mode).
    fn set_single_pitch(&mut self, pc: PitchClass);

    /// Clear the local peer's pitch set.
    fn clear_pitches(&mut self);

    /// Get all peer pitch sets.
    fn all_peer_sets(&self) -> HashMap<String, PeerPitchSet>;

    /// Get the current combination method.
    fn combination_method(&self) -> &CombinationMethod;

    /// Set the combination method.
    fn set_combination_method(&mut self, method: CombinationMethod);

    /// Compute the room result based on the combination method.
    fn compute_room_result(&self) -> RoomPitchResult {
        let peer_sets = self.all_peer_sets();
        let method = self.combination_method();

        let mut result = RoomPitchResult::default();

        // Build attribution map
        for (peer_id, peer_set) in &peer_sets {
            for &pc in &peer_set.pitch_classes {
                result
                    .attribution
                    .entry(pc)
                    .or_default()
                    .push(peer_id.clone());
            }
        }

        // Compute combined set based on method
        result.pitch_classes = match method {
            CombinationMethod::Union => {
                // Union: all pitch classes from any peer
                peer_sets
                    .values()
                    .flat_map(|s| s.pitch_classes.iter().copied())
                    .collect()
            }
            CombinationMethod::Intersection => {
                // Intersection: only pitch classes present in all peers
                if peer_sets.is_empty() {
                    HashSet::new()
                } else {
                    let mut iter = peer_sets.values();
                    let first = iter.next().unwrap().pitch_classes.clone();
                    iter.fold(first, |acc, set| {
                        acc.intersection(&set.pitch_classes).copied().collect()
                    })
                }
            }
            CombinationMethod::Custom(_) => {
                // For now, custom methods fall back to union
                peer_sets
                    .values()
                    .flat_map(|s| s.pitch_classes.iter().copied())
                    .collect()
            }
        };

        result
    }

    /// Get the room's SCL tuning content.
    fn tuning_scl(&self) -> &str;

    /// Set the room's SCL tuning content.
    fn set_tuning_scl(&mut self, scl: &str);

    /// Subscribe to state changes (for reactive UI).
    /// Returns a receiver that gets notified on any state change.
    fn subscribe(&self) -> tokio::sync::watch::Receiver<()>;
}

/// A simple in-memory implementation of RoomState for testing.
#[derive(Debug)]
pub struct LocalRoomState {
    peer_id: String,
    local_set: PeerPitchSet,
    peer_sets: HashMap<String, PeerPitchSet>,
    combination_method: CombinationMethod,
    tuning_scl: String,
    notify_tx: tokio::sync::watch::Sender<()>,
    notify_rx: tokio::sync::watch::Receiver<()>,
}

impl LocalRoomState {
    pub fn new(peer_id: String) -> Self {
        let (notify_tx, notify_rx) = tokio::sync::watch::channel(());
        let mut peer_sets = HashMap::new();
        peer_sets.insert(peer_id.clone(), PeerPitchSet::new());

        Self {
            peer_id,
            local_set: PeerPitchSet::new(),
            peer_sets,
            combination_method: CombinationMethod::default(),
            tuning_scl: String::new(),
            notify_tx,
            notify_rx,
        }
    }

    fn notify(&self) {
        let _ = self.notify_tx.send(());
    }
}

impl RoomState for LocalRoomState {
    fn local_peer_id(&self) -> &str {
        &self.peer_id
    }

    fn local_pitch_set(&self) -> &PeerPitchSet {
        &self.local_set
    }

    fn local_pitch_set_mut(&mut self) -> &mut PeerPitchSet {
        &mut self.local_set
    }

    fn add_pitch(&mut self, pc: PitchClass) {
        self.local_set.add(pc);
        self.peer_sets
            .get_mut(&self.peer_id)
            .unwrap()
            .add(pc);
        self.notify();
    }

    fn remove_pitch(&mut self, pc: PitchClass) {
        self.local_set.remove(pc);
        self.peer_sets
            .get_mut(&self.peer_id)
            .unwrap()
            .remove(pc);
        self.notify();
    }

    fn toggle_pitch(&mut self, pc: PitchClass) -> bool {
        let added = self.local_set.toggle(pc);
        self.peer_sets
            .get_mut(&self.peer_id)
            .unwrap()
            .pitch_classes = self.local_set.pitch_classes.clone();
        self.notify();
        added
    }

    fn set_single_pitch(&mut self, pc: PitchClass) {
        self.local_set.set_single(pc);
        self.peer_sets
            .get_mut(&self.peer_id)
            .unwrap()
            .pitch_classes = self.local_set.pitch_classes.clone();
        self.notify();
    }

    fn clear_pitches(&mut self) {
        self.local_set.clear();
        self.peer_sets
            .get_mut(&self.peer_id)
            .unwrap()
            .clear();
        self.notify();
    }

    fn all_peer_sets(&self) -> HashMap<String, PeerPitchSet> {
        self.peer_sets.clone()
    }

    fn combination_method(&self) -> &CombinationMethod {
        &self.combination_method
    }

    fn set_combination_method(&mut self, method: CombinationMethod) {
        self.combination_method = method;
        self.notify();
    }

    fn tuning_scl(&self) -> &str {
        &self.tuning_scl
    }

    fn set_tuning_scl(&mut self, scl: &str) {
        self.tuning_scl = scl.to_string();
        self.notify();
    }

    fn subscribe(&self) -> tokio::sync::watch::Receiver<()> {
        self.notify_rx.clone()
    }
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

    #[test]
    fn test_room_union() {
        let mut state = LocalRoomState::new("peer1".to_string());
        state.add_pitch(PitchClass(0));
        state.add_pitch(PitchClass(4));

        // Simulate another peer
        let mut peer2_set = PeerPitchSet::new();
        peer2_set.add(PitchClass(4));
        peer2_set.add(PitchClass(7));
        state.peer_sets.insert("peer2".to_string(), peer2_set);

        state.set_combination_method(CombinationMethod::Union);
        let result = state.compute_room_result();

        assert_eq!(result.pitch_classes.len(), 3); // 0, 4, 7
        assert!(result.pitch_classes.contains(&PitchClass(0)));
        assert!(result.pitch_classes.contains(&PitchClass(4)));
        assert!(result.pitch_classes.contains(&PitchClass(7)));

        // Check attribution
        assert_eq!(result.attribution[&PitchClass(4)].len(), 2); // Both peers
    }

    #[test]
    fn test_room_intersection() {
        let mut state = LocalRoomState::new("peer1".to_string());
        state.add_pitch(PitchClass(0));
        state.add_pitch(PitchClass(4));

        // Simulate another peer
        let mut peer2_set = PeerPitchSet::new();
        peer2_set.add(PitchClass(4));
        peer2_set.add(PitchClass(7));
        state.peer_sets.insert("peer2".to_string(), peer2_set);

        state.set_combination_method(CombinationMethod::Intersection);
        let result = state.compute_room_result();

        assert_eq!(result.pitch_classes.len(), 1); // Only 4
        assert!(result.pitch_classes.contains(&PitchClass(4)));
    }
}
