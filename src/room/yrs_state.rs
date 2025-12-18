//! yrs-based room state implementation.
//!
//! Uses Y-CRDT for synchronizing room state across peers:
//! - YText for SCL tuning content
//! - YMap for per-peer pitch class sets
//! - YMap entry for combination method

use std::collections::HashMap;

use tokio::sync::watch;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Array, ArrayRef, Doc, GetString, Map, MapRef, ReadTxn, Text, TextRef, Transact, Update, WriteTxn};

use crate::tuning::PitchClass;

use super::{CombinationMethod, PeerPitchSet, RoomState};

/// Document keys for the room state.
const KEY_TUNING: &str = "tuning";
const KEY_PITCH_SETS: &str = "pitch_sets";
const KEY_COMBINATION_METHOD: &str = "combination_method";

/// yrs-based room state that syncs with peers.
pub struct YrsRoomState {
    peer_id: String,
    doc: Doc,
    notify_tx: watch::Sender<()>,
    notify_rx: watch::Receiver<()>,
}

impl YrsRoomState {
    /// Create a new yrs room state for the given peer.
    pub fn new(peer_id: String) -> Self {
        let doc = Doc::new();
        let (notify_tx, notify_rx) = watch::channel(());

        // Initialize the document structure
        {
            let mut txn = doc.transact_mut();

            // Create tuning text
            let _tuning: TextRef = txn.get_or_insert_text(KEY_TUNING);

            // Create pitch_sets map
            let pitch_sets: MapRef = txn.get_or_insert_map(KEY_PITCH_SETS);

            // Create our peer's array in pitch_sets
            pitch_sets.insert(&mut txn, peer_id.clone(), yrs::ArrayPrelim::default());

            // Create combination_method with default
            let meta: MapRef = txn.get_or_insert_map(KEY_COMBINATION_METHOD);
            meta.insert(&mut txn, "method", "union");
        }

        Self {
            peer_id,
            doc,
            notify_tx,
            notify_rx,
        }
    }

    /// Get the yrs document for sync operations.
    pub fn doc(&self) -> &Doc {
        &self.doc
    }

    /// Apply an update from another peer.
    pub fn apply_update(&mut self, update: &[u8]) -> anyhow::Result<()> {
        let update = Update::decode_v1(update)?;
        self.doc.transact_mut().apply_update(update)?;
        self.notify();
        Ok(())
    }

    /// Get the current state as an update for syncing to new peers.
    pub fn encode_state_as_update(&self) -> Vec<u8> {
        let txn = self.doc.transact();
        txn.encode_state_as_update_v1(&yrs::StateVector::default())
    }

    /// Encode a diff from a given state vector.
    pub fn encode_diff(&self, state_vector: &[u8]) -> anyhow::Result<Vec<u8>> {
        let sv = yrs::StateVector::decode_v1(state_vector)?;
        let txn = self.doc.transact();
        Ok(txn.encode_state_as_update_v1(&sv))
    }

    /// Get the state vector for sync protocol.
    pub fn state_vector(&self) -> Vec<u8> {
        let txn = self.doc.transact();
        txn.state_vector().encode_v1()
    }

    fn notify(&self) {
        let _ = self.notify_tx.send(());
    }
}

impl RoomState for YrsRoomState {
    fn local_peer_id(&self) -> &str {
        &self.peer_id
    }

    fn local_pitch_set(&self) -> &PeerPitchSet {
        // Note: This returns a snapshot, not a live reference
        // For the trait to work properly, we'd need interior mutability
        // For now, we'll compute it on demand
        unimplemented!("Use all_peer_sets() instead for yrs implementation")
    }

    fn local_pitch_set_mut(&mut self) -> &mut PeerPitchSet {
        unimplemented!("Use add_pitch/remove_pitch/toggle_pitch instead for yrs implementation")
    }

    fn add_pitch(&mut self, pc: PitchClass) {
        let mut txn = self.doc.transact_mut();
        let pitch_sets: MapRef = txn.get_or_insert_map(KEY_PITCH_SETS);
        let array: ArrayRef = pitch_sets
            .get(&txn, &self.peer_id)
            .and_then(|v| v.cast().ok())
            .unwrap_or_else(|| {
                pitch_sets.insert(&mut txn, self.peer_id.clone(), yrs::ArrayPrelim::default());
                pitch_sets.get(&txn, &self.peer_id).unwrap().cast().unwrap()
            });

        // Check if already present
        let len = array.len(&txn);
        for i in 0..len {
            if let Some(val) = array.get(&txn, i) {
                if let Ok(n) = val.cast::<i64>() {
                    if n as u8 == pc.0 {
                        return; // Already present
                    }
                }
            }
        }

        array.push_back(&mut txn, pc.0 as i64);
        drop(txn);
        self.notify();
    }

    fn remove_pitch(&mut self, pc: PitchClass) {
        let mut txn = self.doc.transact_mut();
        let pitch_sets: MapRef = txn.get_or_insert_map(KEY_PITCH_SETS);
        let array: ArrayRef = match pitch_sets.get(&txn, &self.peer_id).and_then(|v| v.cast().ok()) {
            Some(a) => a,
            None => return,
        };

        // Find and remove the pitch class
        let len = array.len(&txn);
        for i in 0..len {
            if let Some(val) = array.get(&txn, i) {
                if let Ok(n) = val.cast::<i64>() {
                    if n as u8 == pc.0 {
                        array.remove(&mut txn, i);
                        drop(txn);
                        self.notify();
                        return;
                    }
                }
            }
        }
    }

    fn toggle_pitch(&mut self, pc: PitchClass) -> bool {
        let txn = self.doc.transact();
        let pitch_sets: MapRef = match txn.get_map(KEY_PITCH_SETS) {
            Some(m) => m,
            None => {
                drop(txn);
                self.add_pitch(pc);
                return true;
            }
        };
        let array: ArrayRef = match pitch_sets.get(&txn, &self.peer_id).and_then(|v| v.cast().ok()) {
            Some(a) => a,
            None => {
                drop(txn);
                self.add_pitch(pc);
                return true;
            }
        };

        // Check if present
        let len = array.len(&txn);
        for i in 0..len {
            if let Some(val) = array.get(&txn, i) {
                if let Ok(n) = val.cast::<i64>() {
                    if n as u8 == pc.0 {
                        drop(txn);
                        self.remove_pitch(pc);
                        return false;
                    }
                }
            }
        }
        drop(txn);
        self.add_pitch(pc);
        true
    }

    fn set_single_pitch(&mut self, pc: PitchClass) {
        let mut txn = self.doc.transact_mut();
        let pitch_sets: MapRef = txn.get_or_insert_map(KEY_PITCH_SETS);
        let array: ArrayRef = pitch_sets
            .get(&txn, &self.peer_id)
            .and_then(|v| v.cast().ok())
            .unwrap_or_else(|| {
                pitch_sets.insert(&mut txn, self.peer_id.clone(), yrs::ArrayPrelim::default());
                pitch_sets.get(&txn, &self.peer_id).unwrap().cast().unwrap()
            });

        // Clear existing
        let len = array.len(&txn);
        if len > 0 {
            array.remove_range(&mut txn, 0, len);
        }

        // Add the single pitch
        array.push_back(&mut txn, pc.0 as i64);
        drop(txn);
        self.notify();
    }

    fn clear_pitches(&mut self) {
        let mut txn = self.doc.transact_mut();
        let pitch_sets: MapRef = txn.get_or_insert_map(KEY_PITCH_SETS);
        let array: ArrayRef = match pitch_sets.get(&txn, &self.peer_id).and_then(|v| v.cast().ok()) {
            Some(a) => a,
            None => return,
        };

        let len = array.len(&txn);
        if len > 0 {
            array.remove_range(&mut txn, 0, len);
        }
        drop(txn);
        self.notify();
    }

    fn all_peer_sets(&self) -> HashMap<String, PeerPitchSet> {
        let txn = self.doc.transact();
        let pitch_sets: MapRef = match txn.get_map(KEY_PITCH_SETS) {
            Some(m) => m,
            None => return HashMap::new(),
        };

        let mut result = HashMap::new();

        for (key, value) in pitch_sets.iter(&txn) {
            let peer_id = key.to_string();
            let mut peer_set = PeerPitchSet::new();

            if let Ok(array) = value.cast::<ArrayRef>() {
                let len = array.len(&txn);
                for i in 0..len {
                    if let Some(val) = array.get(&txn, i) {
                        if let Ok(n) = val.cast::<i64>() {
                            peer_set.add(PitchClass(n as u8));
                        }
                    }
                }
            }

            result.insert(peer_id, peer_set);
        }

        result
    }

    fn combination_method(&self) -> &CombinationMethod {
        // This returns a reference but we can't store it
        // Return a static reference for the default case
        // In a real implementation, we'd use interior mutability
        &CombinationMethod::Union
    }

    fn set_combination_method(&mut self, method: CombinationMethod) {
        let mut txn = self.doc.transact_mut();
        let meta: MapRef = txn.get_or_insert_map(KEY_COMBINATION_METHOD);
        meta.insert(&mut txn, "method", method.as_str());
        drop(txn);
        self.notify();
    }

    fn tuning_scl(&self) -> &str {
        // Return empty for now - need interior mutability for proper implementation
        ""
    }

    fn set_tuning_scl(&mut self, scl: &str) {
        let mut txn = self.doc.transact_mut();
        let tuning: TextRef = txn.get_or_insert_text(KEY_TUNING);

        // Clear existing content
        let len = tuning.len(&txn);
        if len > 0 {
            tuning.remove_range(&mut txn, 0, len);
        }

        // Insert new content
        tuning.insert(&mut txn, 0, scl);
        drop(txn);
        self.notify();
    }

    fn subscribe(&self) -> watch::Receiver<()> {
        self.notify_rx.clone()
    }
}

// Additional methods for getting values that can't be returned as references
impl YrsRoomState {
    /// Get the combination method as an owned value.
    pub fn get_combination_method(&self) -> CombinationMethod {
        let txn = self.doc.transact();
        let meta: MapRef = match txn.get_map(KEY_COMBINATION_METHOD) {
            Some(m) => m,
            None => return CombinationMethod::Union,
        };
        match meta.get(&txn, "method") {
            Some(val) => {
                if let Ok(s) = val.cast::<String>() {
                    CombinationMethod::from_str(&s)
                } else {
                    CombinationMethod::Union
                }
            }
            None => CombinationMethod::Union,
        }
    }

    /// Get the tuning SCL content as an owned string.
    pub fn get_tuning_scl(&self) -> String {
        let txn = self.doc.transact();
        let tuning: TextRef = match txn.get_text(KEY_TUNING) {
            Some(t) => t,
            None => return String::new(),
        };
        tuning.get_string(&txn)
    }

    /// Add a remote peer's entry (called when a peer joins).
    pub fn add_peer(&mut self, peer_id: &str) {
        let mut txn = self.doc.transact_mut();
        let pitch_sets: MapRef = txn.get_or_insert_map(KEY_PITCH_SETS);
        if pitch_sets.get(&txn, peer_id).is_none() {
            pitch_sets.insert(&mut txn, peer_id, yrs::ArrayPrelim::default());
        }
    }

    /// Remove a peer's entry (called when a peer disconnects).
    pub fn remove_peer(&mut self, peer_id: &str) {
        let mut txn = self.doc.transact_mut();
        let pitch_sets: MapRef = txn.get_or_insert_map(KEY_PITCH_SETS);
        pitch_sets.remove(&mut txn, peer_id);
        drop(txn);
        self.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_yrs_add_remove_pitch() {
        let mut state = YrsRoomState::new("peer1".to_string());

        state.add_pitch(PitchClass(5));
        let sets = state.all_peer_sets();
        assert!(sets["peer1"].contains(PitchClass(5)));

        state.remove_pitch(PitchClass(5));
        let sets = state.all_peer_sets();
        assert!(!sets["peer1"].contains(PitchClass(5)));
    }

    #[test]
    fn test_yrs_toggle() {
        let mut state = YrsRoomState::new("peer1".to_string());

        assert!(state.toggle_pitch(PitchClass(7))); // Added
        let sets = state.all_peer_sets();
        assert!(sets["peer1"].contains(PitchClass(7)));

        assert!(!state.toggle_pitch(PitchClass(7))); // Removed
        let sets = state.all_peer_sets();
        assert!(!sets["peer1"].contains(PitchClass(7)));
    }

    #[test]
    fn test_yrs_single_pitch() {
        let mut state = YrsRoomState::new("peer1".to_string());

        state.add_pitch(PitchClass(1));
        state.add_pitch(PitchClass(2));
        let sets = state.all_peer_sets();
        assert_eq!(sets["peer1"].pitch_classes.len(), 2);

        state.set_single_pitch(PitchClass(5));
        let sets = state.all_peer_sets();
        assert_eq!(sets["peer1"].pitch_classes.len(), 1);
        assert!(sets["peer1"].contains(PitchClass(5)));
    }

    #[test]
    fn test_yrs_tuning() {
        let mut state = YrsRoomState::new("peer1".to_string());

        let scl = "! Test\nTest\n1\n1200.0";
        state.set_tuning_scl(scl);
        assert_eq!(state.get_tuning_scl(), scl);
    }

    #[test]
    fn test_yrs_sync() {
        let mut state1 = YrsRoomState::new("peer1".to_string());
        let mut state2 = YrsRoomState::new("peer2".to_string());

        // Peer 1 adds a pitch
        state1.add_pitch(PitchClass(5));

        // Sync state1 -> state2
        let update = state1.encode_state_as_update();
        state2.apply_update(&update).unwrap();

        // Peer 2 should see peer 1's pitch
        let sets = state2.all_peer_sets();
        assert!(sets.get("peer1").map(|s| s.contains(PitchClass(5))).unwrap_or(false));
    }
}
