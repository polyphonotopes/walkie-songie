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
const KEY_PITCH_SETS: &str = "pitch_sets";  // Legacy per-peer sets (unused now)
const KEY_SHARED_PITCHES: &str = "shared_pitches";  // Shared pitch set (anyone can add/remove)
const KEY_COMBINATION_METHOD: &str = "combination_method";
const KEY_VOICE_STATE: &str = "voice_state";
const KEY_PIECES: &str = "pieces";  // Draggable pieces with absolute pitch (YMap<piece_id, pitch>)

/// Keys within each peer's voice state map.
const VOICE_PITCH: &str = "pitch";        // i32 - absolute pitch number (like MIDI note, no modulus)
const VOICE_PITCHCLASS: &str = "pitchclass";  // u8 - the quantized pitch class (pitch % scale_size)

/// A draggable piece with an absolute pitch (includes octave).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    pub id: String,
    pub pitch: i32,  // Absolute pitch like MIDI note (60 = C4)
}

impl Piece {
    /// Get the pitch class (0-11 for 12-TET).
    pub fn pitch_class(&self, notes_per_octave: u8) -> u8 {
        self.pitch.rem_euclid(notes_per_octave as i32) as u8
    }

    /// Get the octave (assuming middle C = octave 4, pitch 60).
    pub fn octave(&self) -> i32 {
        (self.pitch - 60).div_euclid(12) + 4
    }
}

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

            // Create shared_pitches map (single shared set, anyone can add/remove)
            let _shared_pitches: MapRef = txn.get_or_insert_map(KEY_SHARED_PITCHES);

            // Create combination_method with default
            let meta: MapRef = txn.get_or_insert_map(KEY_COMBINATION_METHOD);
            meta.insert(&mut txn, "method", "union");

            // Create voice_state map (will hold per-peer voice pitch data)
            let voice_state: MapRef = txn.get_or_insert_map(KEY_VOICE_STATE);
            // Create our peer's voice state map
            voice_state.insert(&mut txn, peer_id.clone(), MapPrelim::default());

            // Create pieces map (draggable pieces with absolute pitch)
            let _pieces: MapRef = txn.get_or_insert_map(KEY_PIECES);
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
    /// NOTE: This does NOT call notify() because it's a remote change.
    /// Only local changes should trigger notify() to avoid feedback loops.
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

        // NOTE: We intentionally do NOT call notify() here!
        // apply_update() is for REMOTE changes, and notify() signals that
        // LOCAL changes need to be broadcast. Calling notify() here would
        // cause feedback loops where we rebroadcast remote updates.
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
        let shared_pitches: MapRef = txn.get_or_insert_map(KEY_SHARED_PITCHES);

        // Insert pitch as key (idempotent - inserting same key twice is fine)
        let key = pc.0.to_string();
        shared_pitches.insert(&mut txn, key, true);
        drop(txn);
        self.notify();
    }

    fn remove_pitch(&mut self, pc: PitchClass) {
        debug!("[CRDT] remove_pitch({}) for peer {}", pc.0, self.peer_id);

        let mut txn = self.doc.transact_mut();
        let shared_pitches: MapRef = txn.get_or_insert_map(KEY_SHARED_PITCHES);

        // Remove pitch key from shared map
        let key = pc.0.to_string();
        shared_pitches.remove(&mut txn, &key);
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
        let shared_pitches: MapRef = txn.get_or_insert_map(KEY_SHARED_PITCHES);

        // Collect existing keys to remove
        let keys_to_remove: Vec<String> = shared_pitches
            .keys(&txn)
            .map(|k| k.to_string())
            .collect();

        // Remove all existing pitches
        for key in keys_to_remove {
            shared_pitches.remove(&mut txn, &key);
        }

        // Add the single pitch
        let key = pc.0.to_string();
        shared_pitches.insert(&mut txn, key, true);
        drop(txn);
        self.notify();
    }

    fn clear_pitches(&mut self) {
        let mut txn = self.doc.transact_mut();
        let shared_pitches: MapRef = txn.get_or_insert_map(KEY_SHARED_PITCHES);

        // Collect keys to remove
        let keys_to_remove: Vec<String> = shared_pitches
            .keys(&txn)
            .map(|k| k.to_string())
            .collect();

        // Remove all pitches
        for key in keys_to_remove {
            shared_pitches.remove(&mut txn, &key);
        }
        drop(txn);
        self.notify();
    }

    fn all_peer_sets(&self) -> HashMap<String, PeerPitchSet> {
        let txn = self.doc.transact();
        let mut result = HashMap::new();

        // Get shared pitch toggles (manually clicked pitches that anyone can add/remove)
        if let Some(shared_pitches) = txn.get_map(KEY_SHARED_PITCHES) {
            let shared_set = result.entry("shared".to_string()).or_insert_with(PeerPitchSet::new);

            for (pitch_key, _) in shared_pitches.iter(&txn) {
                if let Ok(pitch_num) = pitch_key.parse::<u8>() {
                    shared_set.add(PitchClass(pitch_num));
                }
            }

            debug!("[CRDT] all_peer_sets: shared_pitches = {:?}",
                shared_set.pitch_classes.iter().map(|pc| pc.0).collect::<Vec<_>>());
        }

        // Include voice pitches from KEY_VOICE_STATE (per-peer voice input)
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
    /// Check if a pitch is in the shared pitch set (O(1) lookup).
    pub fn contains_pitch(&self, pc: PitchClass) -> bool {
        let txn = self.doc.transact();
        let shared_pitches: MapRef = match txn.get_map(KEY_SHARED_PITCHES) {
            Some(m) => m,
            None => return false,
        };

        let key = pc.0.to_string();
        shared_pitches.get(&txn, &key).is_some()
    }

    /// Get all shared pitches as a set.
    pub fn shared_pitches(&self) -> HashSet<PitchClass> {
        let txn = self.doc.transact();
        let shared_pitches: MapRef = match txn.get_map(KEY_SHARED_PITCHES) {
            Some(m) => m,
            None => return HashSet::new(),
        };

        shared_pitches
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

    // ========== Piece Methods (draggable pieces with absolute pitch) ==========

    /// Add a new piece at the given absolute pitch. Returns the piece ID.
    pub fn add_piece(&mut self, pitch: i32) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        debug!("[CRDT] add_piece(pitch={}) -> id={}", pitch, id);

        let mut txn = self.doc.transact_mut();
        let pieces: MapRef = txn.get_or_insert_map(KEY_PIECES);
        pieces.insert(&mut txn, id.clone(), pitch as i64);
        drop(txn);
        self.notify();

        id
    }

    /// Remove a piece by ID.
    pub fn remove_piece(&mut self, piece_id: &str) {
        debug!("[CRDT] remove_piece(id={})", piece_id);

        let mut txn = self.doc.transact_mut();
        let pieces: MapRef = txn.get_or_insert_map(KEY_PIECES);
        pieces.remove(&mut txn, piece_id);
        drop(txn);
        self.notify();
    }

    /// Move a piece to a new pitch (for drag operations).
    pub fn move_piece(&mut self, piece_id: &str, new_pitch: i32) {
        debug!("[CRDT] move_piece(id={}, new_pitch={})", piece_id, new_pitch);

        let mut txn = self.doc.transact_mut();
        let pieces: MapRef = txn.get_or_insert_map(KEY_PIECES);

        // Only move if piece exists
        if pieces.get(&txn, piece_id).is_some() {
            pieces.insert(&mut txn, piece_id, new_pitch as i64);
        }
        drop(txn);
        self.notify();
    }

    /// Get all pieces.
    pub fn all_pieces(&self) -> Vec<Piece> {
        let txn = self.doc.transact();
        let pieces: MapRef = match txn.get_map(KEY_PIECES) {
            Some(m) => m,
            None => return Vec::new(),
        };

        pieces
            .iter(&txn)
            .filter_map(|(id, value)| {
                let pitch = value.cast::<i64>().ok()? as i32;
                Some(Piece {
                    id: id.to_string(),
                    pitch,
                })
            })
            .collect()
    }

    /// Get a specific piece by ID.
    pub fn get_piece(&self, piece_id: &str) -> Option<Piece> {
        let txn = self.doc.transact();
        let pieces: MapRef = txn.get_map(KEY_PIECES)?;

        let pitch = pieces.get(&txn, piece_id)?.cast::<i64>().ok()? as i32;
        Some(Piece {
            id: piece_id.to_string(),
            pitch,
        })
    }

    /// Clear all pieces.
    pub fn clear_pieces(&mut self) {
        let mut txn = self.doc.transact_mut();
        let pieces: MapRef = txn.get_or_insert_map(KEY_PIECES);

        let ids_to_remove: Vec<String> = pieces
            .keys(&txn)
            .map(|k| k.to_string())
            .collect();

        for id in ids_to_remove {
            pieces.remove(&mut txn, &id);
        }
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
        // Pitches are now shared, not per-peer
        assert!(sets["shared"].contains(PitchClass(5)));

        state.remove_pitch(PitchClass(5));
        let sets = state.all_peer_sets();
        assert!(!sets["shared"].contains(PitchClass(5)));
    }

    #[test]
    fn test_yrs_toggle() {
        let mut state = YrsRoomState::new("peer1".to_string());

        assert!(state.toggle_pitch(PitchClass(7))); // Added
        let sets = state.all_peer_sets();
        // Pitches are now shared, not per-peer
        assert!(sets["shared"].contains(PitchClass(7)));

        assert!(!state.toggle_pitch(PitchClass(7))); // Removed
        let sets = state.all_peer_sets();
        assert!(!sets["shared"].contains(PitchClass(7)));
    }

    #[test]
    fn test_yrs_single_pitch() {
        let mut state = YrsRoomState::new("peer1".to_string());

        state.add_pitch(PitchClass(1));
        state.add_pitch(PitchClass(2));
        let sets = state.all_peer_sets();
        // Pitches are now shared, not per-peer
        assert_eq!(sets["shared"].pitch_classes.len(), 2);

        state.set_single_pitch(PitchClass(5));
        let sets = state.all_peer_sets();
        assert_eq!(sets["shared"].pitch_classes.len(), 1);
        assert!(sets["shared"].contains(PitchClass(5)));
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

        // Peer 1 adds a pitch (goes to shared set)
        state1.add_pitch(PitchClass(5));

        // Sync state1 -> state2
        let update = state1.encode_state_as_update();
        state2.apply_update(&update).unwrap();

        // Peer 2 should see the shared pitch
        let sets = state2.all_peer_sets();
        assert!(sets.get("shared").map(|s| s.contains(PitchClass(5))).unwrap_or(false));
    }

    #[test]
    fn test_yrs_sync_shared_removal() {
        let mut state1 = YrsRoomState::new("peer1".to_string());
        let mut state2 = YrsRoomState::new("peer2".to_string());

        // Peer 1 adds a pitch
        state1.add_pitch(PitchClass(5));

        // Sync state1 -> state2
        let update = state1.encode_state_as_update();
        state2.apply_update(&update).unwrap();

        // Peer 2 removes the pitch (this is the bug we fixed!)
        state2.remove_pitch(PitchClass(5));

        // Sync state2 -> state1
        let update = state2.encode_state_as_update();
        state1.apply_update(&update).unwrap();

        // Both should see the pitch removed
        let sets1 = state1.all_peer_sets();
        let sets2 = state2.all_peer_sets();
        assert!(!sets1.get("shared").map(|s| s.contains(PitchClass(5))).unwrap_or(true));
        assert!(!sets2.get("shared").map(|s| s.contains(PitchClass(5))).unwrap_or(true));
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

    #[test]
    fn test_piece_helpers() {
        let piece = Piece {
            id: "test".to_string(),
            pitch: 60, // Middle C
        };
        assert_eq!(piece.pitch_class(12), 0); // C is pitch class 0
        assert_eq!(piece.octave(), 4); // Middle C is octave 4

        let piece_high = Piece {
            id: "test2".to_string(),
            pitch: 72, // C5
        };
        assert_eq!(piece_high.pitch_class(12), 0);
        assert_eq!(piece_high.octave(), 5);

        let piece_low = Piece {
            id: "test3".to_string(),
            pitch: 48, // C3
        };
        assert_eq!(piece_low.pitch_class(12), 0);
        assert_eq!(piece_low.octave(), 3);
    }

    #[test]
    fn test_yrs_pieces() {
        let mut state = YrsRoomState::new("peer1".to_string());

        // Initially empty
        assert!(state.all_pieces().is_empty());

        // Add a piece at middle C (60)
        let id = state.add_piece(60);

        // Should have one piece
        let pieces = state.all_pieces();
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].id, id);
        assert_eq!(pieces[0].pitch, 60);

        // Get specific piece
        let piece = state.get_piece(&id).unwrap();
        assert_eq!(piece.pitch, 60);

        // Move the piece to D (62)
        state.move_piece(&id, 62);
        let piece = state.get_piece(&id).unwrap();
        assert_eq!(piece.pitch, 62);

        // Remove the piece
        state.remove_piece(&id);
        assert!(state.all_pieces().is_empty());
        assert!(state.get_piece(&id).is_none());
    }

    #[test]
    fn test_yrs_pieces_sync() {
        let mut state1 = YrsRoomState::new("peer1".to_string());
        let mut state2 = YrsRoomState::new("peer2".to_string());

        // Peer 1 adds a piece
        let id = state1.add_piece(60);

        // Sync state1 -> state2
        let update = state1.encode_state_as_update();
        state2.apply_update(&update).unwrap();

        // Peer 2 should see the piece
        let pieces = state2.all_pieces();
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].id, id);
        assert_eq!(pieces[0].pitch, 60);

        // Peer 2 moves the piece
        state2.move_piece(&id, 72);

        // Sync state2 -> state1
        let update = state2.encode_state_as_update();
        state1.apply_update(&update).unwrap();

        // Peer 1 should see the updated pitch
        let piece = state1.get_piece(&id).unwrap();
        assert_eq!(piece.pitch, 72);
    }

    #[test]
    fn test_yrs_clear_pieces() {
        let mut state = YrsRoomState::new("peer1".to_string());

        // Add multiple pieces
        state.add_piece(60);
        state.add_piece(64);
        state.add_piece(67);
        assert_eq!(state.all_pieces().len(), 3);

        // Clear all
        state.clear_pieces();
        assert!(state.all_pieces().is_empty());
    }
}
