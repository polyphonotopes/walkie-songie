//! yrs-based room state implementation.
//!
//! Uses Y-CRDT for synchronizing room state across peers:
//! - YText for SCL tuning content
//! - YMap<peer_id, YMap<pitch, true>> for per-peer pitch class sets (set semantics via map keys)
//! - YMap entry for combination method
//!
//! Using nested YMaps instead of YArrays gives us:
//! - O(1) add/remove/contains operations
//! - Automatic deduplication on concurrent adds
//! - Proper set semantics under CRDT merge

use std::collections::{HashMap, HashSet};

use tokio::sync::watch;
use tracing::debug;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, GetString, Map, MapPrelim, MapRef, ReadTxn, Text, TextRef, Transact, Update, WriteTxn};

use crate::tuning::PitchClass;

use super::{CombinationMethod, PeerPitchSet, RoomState};

/// Document keys for the room state.
const KEY_TUNING: &str = "tuning";
const KEY_PITCH_SETS: &str = "pitch_sets";
const KEY_COMBINATION_METHOD: &str = "combination_method";
const KEY_VOICE_STATE: &str = "voice_state";

/// Keys within each peer's voice state map.
const VOICE_PITCH: &str = "pitch";        // i32 - absolute pitch number (like MIDI note, no modulus)
const VOICE_PITCHCLASS: &str = "pitchclass";  // u8 - the quantized pitch class (pitch % scale_size)

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

            // Create pitch_sets map (will hold nested maps per peer)
            let pitch_sets: MapRef = txn.get_or_insert_map(KEY_PITCH_SETS);

            // Create our peer's pitch map (empty map, keys are pitch classes)
            pitch_sets.insert(&mut txn, peer_id.clone(), MapPrelim::default());

            // Create combination_method with default
            let meta: MapRef = txn.get_or_insert_map(KEY_COMBINATION_METHOD);
            meta.insert(&mut txn, "method", "union");

            // Create voice_state map (will hold per-peer voice pitch data)
            let voice_state: MapRef = txn.get_or_insert_map(KEY_VOICE_STATE);
            // Create our peer's voice state map
            voice_state.insert(&mut txn, peer_id.clone(), MapPrelim::default());
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

        // Log the state after applying
        let peer_sets = self.all_peer_sets();
        debug!("[CRDT] After apply_update: {} peer sets", peer_sets.len());
        for (peer_id, set) in &peer_sets {
            debug!(
                "[CRDT]   peer {}: {:?}",
                peer_id,
                set.pitch_classes.iter().map(|pc| pc.0).collect::<Vec<_>>()
            );
        }

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
        debug!("[CRDT] add_pitch({}) for peer {}", pc.0, self.peer_id);

        let mut txn = self.doc.transact_mut();
        let pitch_sets: MapRef = txn.get_or_insert_map(KEY_PITCH_SETS);

        // Get or create our peer's pitch map
        let peer_map: MapRef = pitch_sets
            .get(&txn, &self.peer_id)
            .and_then(|v| v.cast().ok())
            .unwrap_or_else(|| {
                pitch_sets.insert(&mut txn, self.peer_id.clone(), MapPrelim::default());
                pitch_sets.get(&txn, &self.peer_id).unwrap().cast().unwrap()
            });

        // Insert pitch as key (idempotent - inserting same key twice is fine)
        let key = pc.0.to_string();
        peer_map.insert(&mut txn, key, true);
        drop(txn);
        self.notify();
    }

    fn remove_pitch(&mut self, pc: PitchClass) {
        let mut txn = self.doc.transact_mut();
        let pitch_sets: MapRef = txn.get_or_insert_map(KEY_PITCH_SETS);

        let peer_map: MapRef = match pitch_sets.get(&txn, &self.peer_id).and_then(|v| v.cast().ok()) {
            Some(m) => m,
            None => return,
        };

        // Remove pitch key
        let key = pc.0.to_string();
        peer_map.remove(&mut txn, &key);
        drop(txn);
        self.notify();
    }

    fn toggle_pitch(&mut self, pc: PitchClass) -> bool {
        if self.contains_pitch(pc) {
            self.remove_pitch(pc);
            false
        } else {
            self.add_pitch(pc);
            true
        }
    }

    fn set_single_pitch(&mut self, pc: PitchClass) {
        let mut txn = self.doc.transact_mut();
        let pitch_sets: MapRef = txn.get_or_insert_map(KEY_PITCH_SETS);

        // Get or create our peer's pitch map
        let peer_map: MapRef = pitch_sets
            .get(&txn, &self.peer_id)
            .and_then(|v| v.cast().ok())
            .unwrap_or_else(|| {
                pitch_sets.insert(&mut txn, self.peer_id.clone(), MapPrelim::default());
                pitch_sets.get(&txn, &self.peer_id).unwrap().cast().unwrap()
            });

        // Collect existing keys to remove
        let keys_to_remove: Vec<String> = peer_map
            .keys(&txn)
            .map(|k| k.to_string())
            .collect();

        // Remove all existing pitches
        for key in keys_to_remove {
            peer_map.remove(&mut txn, &key);
        }

        // Add the single pitch
        let key = pc.0.to_string();
        peer_map.insert(&mut txn, key, true);
        drop(txn);
        self.notify();
    }

    fn clear_pitches(&mut self) {
        let mut txn = self.doc.transact_mut();
        let pitch_sets: MapRef = txn.get_or_insert_map(KEY_PITCH_SETS);

        let peer_map: MapRef = match pitch_sets.get(&txn, &self.peer_id).and_then(|v| v.cast().ok()) {
            Some(m) => m,
            None => return,
        };

        // Collect keys to remove
        let keys_to_remove: Vec<String> = peer_map
            .keys(&txn)
            .map(|k| k.to_string())
            .collect();

        // Remove all pitches
        for key in keys_to_remove {
            peer_map.remove(&mut txn, &key);
        }
        drop(txn);
        self.notify();
    }

    fn all_peer_sets(&self) -> HashMap<String, PeerPitchSet> {
        let txn = self.doc.transact();
        let mut result = HashMap::new();

        // Get manual pitch toggles from KEY_PITCH_SETS
        if let Some(pitch_sets) = txn.get_map(KEY_PITCH_SETS) {
            for (peer_key, value) in pitch_sets.iter(&txn) {
                let peer_id = peer_key.to_string();
                let peer_set = result.entry(peer_id).or_insert_with(PeerPitchSet::new);

                if let Ok(peer_map) = value.cast::<MapRef>() {
                    // Each key in the peer's map is a pitch class
                    for (pitch_key, _) in peer_map.iter(&txn) {
                        if let Ok(pitch_num) = pitch_key.parse::<u8>() {
                            peer_set.add(PitchClass(pitch_num));
                        }
                    }
                }
            }
        }

        // Also include voice pitches from KEY_VOICE_STATE
        if let Some(voice_state) = txn.get_map(KEY_VOICE_STATE) {
            for (peer_key, value) in voice_state.iter(&txn) {
                let peer_id = peer_key.to_string();
                let peer_set = result.entry(peer_id).or_insert_with(PeerPitchSet::new);

                if let Ok(peer_map) = value.cast::<MapRef>() {
                    // Get voice pitch class if present
                    if let Some(pc_val) = peer_map.get(&txn, VOICE_PITCHCLASS) {
                        if let Some(pc_num) = pc_val.cast::<i64>().ok() {
                            peer_set.add(PitchClass(pc_num as u8));
                        }
                    }
                }
            }
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
    /// Check if a pitch is in the local peer's set (O(1) lookup).
    pub fn contains_pitch(&self, pc: PitchClass) -> bool {
        let txn = self.doc.transact();
        let pitch_sets: MapRef = match txn.get_map(KEY_PITCH_SETS) {
            Some(m) => m,
            None => return false,
        };

        let peer_map: MapRef = match pitch_sets.get(&txn, &self.peer_id).and_then(|v| v.cast().ok()) {
            Some(m) => m,
            None => return false,
        };

        let key = pc.0.to_string();
        peer_map.get(&txn, &key).is_some()
    }

    /// Get the local peer's pitches as a set (without building full HashMap).
    pub fn local_pitches(&self) -> HashSet<PitchClass> {
        let txn = self.doc.transact();
        let pitch_sets: MapRef = match txn.get_map(KEY_PITCH_SETS) {
            Some(m) => m,
            None => return HashSet::new(),
        };

        let peer_map: MapRef = match pitch_sets.get(&txn, &self.peer_id).and_then(|v| v.cast().ok()) {
            Some(m) => m,
            None => return HashSet::new(),
        };

        peer_map
            .keys(&txn)
            .filter_map(|k| k.parse::<u8>().ok())
            .map(PitchClass)
            .collect()
    }

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
            pitch_sets.insert(&mut txn, peer_id, MapPrelim::default());
        }
    }

    /// Remove a peer's entry (called when a peer disconnects).
    pub fn remove_peer(&mut self, peer_id: &str) {
        let mut txn = self.doc.transact_mut();
        let pitch_sets: MapRef = txn.get_or_insert_map(KEY_PITCH_SETS);
        pitch_sets.remove(&mut txn, peer_id);

        // Also remove voice state
        let voice_state: MapRef = txn.get_or_insert_map(KEY_VOICE_STATE);
        voice_state.remove(&mut txn, peer_id);

        drop(txn);
        self.notify();
    }

    /// Set the local peer's voice pitch (absolute pitch number, like MIDI note).
    pub fn set_voice_pitch(&mut self, pitch: Option<i32>) {
        let mut txn = self.doc.transact_mut();
        let voice_state: MapRef = txn.get_or_insert_map(KEY_VOICE_STATE);

        let peer_map: MapRef = voice_state
            .get(&txn, &self.peer_id)
            .and_then(|v| v.cast().ok())
            .unwrap_or_else(|| {
                voice_state.insert(&mut txn, self.peer_id.clone(), MapPrelim::default());
                voice_state.get(&txn, &self.peer_id).unwrap().cast().unwrap()
            });

        if let Some(p) = pitch {
            peer_map.insert(&mut txn, VOICE_PITCH, p as i64);
        } else {
            peer_map.remove(&mut txn, VOICE_PITCH);
        }

        drop(txn);
        self.notify();
    }

    /// Set the local peer's voice pitch class.
    pub fn set_voice_pitchclass(&mut self, pc: Option<PitchClass>) {
        debug!("[CRDT] set_voice_pitchclass({:?}) for peer {}", pc.map(|p| p.0), self.peer_id);

        let mut txn = self.doc.transact_mut();
        let voice_state: MapRef = txn.get_or_insert_map(KEY_VOICE_STATE);

        let peer_map: MapRef = voice_state
            .get(&txn, &self.peer_id)
            .and_then(|v| v.cast().ok())
            .unwrap_or_else(|| {
                voice_state.insert(&mut txn, self.peer_id.clone(), MapPrelim::default());
                voice_state.get(&txn, &self.peer_id).unwrap().cast().unwrap()
            });

        if let Some(p) = pc {
            peer_map.insert(&mut txn, VOICE_PITCHCLASS, p.0 as i64);
        } else {
            peer_map.remove(&mut txn, VOICE_PITCHCLASS);
        }

        drop(txn);
        debug!("[CRDT] Calling notify() after voice change");
        self.notify();
    }

    /// Set both voice pitch and pitch class atomically.
    pub fn set_voice(&mut self, pitch: Option<i32>, pc: Option<PitchClass>) {
        let mut txn = self.doc.transact_mut();
        let voice_state: MapRef = txn.get_or_insert_map(KEY_VOICE_STATE);

        let peer_map: MapRef = voice_state
            .get(&txn, &self.peer_id)
            .and_then(|v| v.cast().ok())
            .unwrap_or_else(|| {
                voice_state.insert(&mut txn, self.peer_id.clone(), MapPrelim::default());
                voice_state.get(&txn, &self.peer_id).unwrap().cast().unwrap()
            });

        if let Some(p) = pitch {
            peer_map.insert(&mut txn, VOICE_PITCH, p as i64);
        } else {
            peer_map.remove(&mut txn, VOICE_PITCH);
        }

        if let Some(p) = pc {
            peer_map.insert(&mut txn, VOICE_PITCHCLASS, p.0 as i64);
        } else {
            peer_map.remove(&mut txn, VOICE_PITCHCLASS);
        }

        drop(txn);
        self.notify();
    }

    /// Clear the local peer's voice state.
    pub fn clear_voice(&mut self) {
        self.set_voice(None, None);
    }

    /// Get all peers' voice states.
    pub fn all_voice_states(&self) -> HashMap<String, (Option<i32>, Option<PitchClass>)> {
        let txn = self.doc.transact();
        let voice_state: MapRef = match txn.get_map(KEY_VOICE_STATE) {
            Some(m) => m,
            None => return HashMap::new(),
        };

        let mut result = HashMap::new();

        for (peer_key, value) in voice_state.iter(&txn) {
            let peer_id = peer_key.to_string();

            if let Ok(peer_map) = value.cast::<MapRef>() {
                let pitch = peer_map
                    .get(&txn, VOICE_PITCH)
                    .and_then(|v| v.cast::<i64>().ok())
                    .map(|i| i as i32);

                let pitch_class = peer_map
                    .get(&txn, VOICE_PITCHCLASS)
                    .and_then(|v| v.cast::<i64>().ok())
                    .map(|i| PitchClass(i as u8));

                result.insert(peer_id, (pitch, pitch_class));
            }
        }

        result
    }

    /// Get a specific peer's voice state.
    pub fn get_peer_voice(&self, peer_id: &str) -> (Option<i32>, Option<PitchClass>) {
        let txn = self.doc.transact();
        let voice_state: MapRef = match txn.get_map(KEY_VOICE_STATE) {
            Some(m) => m,
            None => return (None, None),
        };

        let peer_map: MapRef = match voice_state.get(&txn, peer_id).and_then(|v| v.cast().ok()) {
            Some(m) => m,
            None => return (None, None),
        };

        let pitch = peer_map
            .get(&txn, VOICE_PITCH)
            .and_then(|v| v.cast::<i64>().ok())
            .map(|i| i as i32);

        let pitch_class = peer_map
            .get(&txn, VOICE_PITCHCLASS)
            .and_then(|v| v.cast::<i64>().ok())
            .map(|i| PitchClass(i as u8));

        (pitch, pitch_class)
    }

    /// Get the local peer's voice state.
    pub fn local_voice(&self) -> (Option<i32>, Option<PitchClass>) {
        self.get_peer_voice(&self.peer_id)
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

    #[test]
    fn test_yrs_voice_state() {
        let mut state = YrsRoomState::new("peer1".to_string());

        // Initially empty
        assert_eq!(state.local_voice(), (None, None));

        // Set voice pitch (absolute pitch number, like MIDI note 69 = A4)
        state.set_voice_pitch(Some(69));
        let (pitch, pc) = state.local_voice();
        assert_eq!(pitch, Some(69));
        assert_eq!(pc, None);

        // Set voice pitch class
        state.set_voice_pitchclass(Some(PitchClass(9)));
        let (pitch, pc) = state.local_voice();
        assert_eq!(pitch, Some(69));
        assert_eq!(pc, Some(PitchClass(9)));

        // Set both atomically
        state.set_voice(Some(81), Some(PitchClass(0)));
        let (pitch, pc) = state.local_voice();
        assert_eq!(pitch, Some(81));
        assert_eq!(pc, Some(PitchClass(0)));

        // Clear voice
        state.clear_voice();
        assert_eq!(state.local_voice(), (None, None));
    }

    #[test]
    fn test_yrs_voice_state_sync() {
        let mut state1 = YrsRoomState::new("peer1".to_string());
        let mut state2 = YrsRoomState::new("peer2".to_string());

        // Peer 1 sets voice (pitch 69 = A4, pitch class 9 = A)
        state1.set_voice(Some(69), Some(PitchClass(9)));

        // Sync state1 -> state2
        let update = state1.encode_state_as_update();
        state2.apply_update(&update).unwrap();

        // Peer 2 should see peer 1's voice state
        let voice_states = state2.all_voice_states();
        let (pitch, pc) = voice_states.get("peer1").cloned().unwrap_or((None, None));
        assert_eq!(pitch, Some(69));
        assert_eq!(pc, Some(PitchClass(9)));

        // Query specific peer
        let (pitch2, pc2) = state2.get_peer_voice("peer1");
        assert_eq!(pitch2, Some(69));
        assert_eq!(pc2, Some(PitchClass(9)));
    }
}
