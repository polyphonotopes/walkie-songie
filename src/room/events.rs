//! Room state change events.
//!
//! Provides a unified event model for all room state changes, enabling
//! stream-based reactivity for both web app and plugin consumers.

use crate::tuning::PitchClass;

/// All possible state changes in a room.
/// Each variant represents an atomic change that occurred.
#[derive(Debug, Clone, PartialEq)]
pub enum RoomEvent {
    // === Shared Pitch Toggles (manual keyboard clicks) ===
    /// A pitch class was toggled on in the shared set
    PitchAdded { pitch_class: PitchClass },
    /// A pitch class was toggled off in the shared set
    PitchRemoved { pitch_class: PitchClass },
    /// All pitches were cleared
    PitchesCleared,

    // === Voice State (singing/microphone input) ===
    /// A peer's voice pitch changed
    VoiceChanged {
        peer_id: String,
        pitch: Option<i32>,
        pitch_class: Option<PitchClass>,
    },
    /// A peer's voice was cleared (stopped singing)
    /// Includes previous pitch for delta computation
    VoiceCleared { peer_id: String, pitch: Option<i32> },

    // === Emoji Pieces (draggable items) ===
    /// A new piece was added
    PieceAdded {
        id: String,
        pitch: i32,
        emoji: String,
    },
    /// A piece was moved to a new pitch
    PieceMoved {
        id: String,
        old_pitch: i32,
        new_pitch: i32,
    },
    /// A piece was removed (includes pitch for delta computation)
    PieceRemoved { id: String, pitch: i32 },
    /// All pieces were cleared
    PiecesCleared,

    // === Room Configuration ===
    /// Pieces lock state changed
    PiecesLockChanged { locked: bool },
    /// Tuning (SCL content) changed
    TuningChanged { scl: String },
    /// Combination method changed
    CombinationMethodChanged { method: String },
    /// Available emojis palette changed
    EmojisChanged { emojis: Vec<String> },

    // === Peer Lifecycle ===
    /// A peer appeared in the application projection.
    PeerJoined { peer_id: String },
    /// A peer left the room
    PeerLeft { peer_id: String },

    // === Sync Events ===
    /// Full state sync completed (initial load or reconnect).
    /// Contains snapshot of all current state.
    FullStateSync {
        pitches: Vec<PitchClass>,
        pieces: Vec<(String, i32, String)>, // (id, pitch, emoji)
        voices: Vec<(String, Option<i32>, Option<PitchClass>)>, // (peer_id, pitch, pc)
        pieces_locked: bool,
    },
}

impl RoomEvent {
    /// Returns true if this event affects pitch output (for MIDI routing).
    pub fn affects_pitches(&self) -> bool {
        matches!(
            self,
            RoomEvent::PitchAdded { .. }
                | RoomEvent::PitchRemoved { .. }
                | RoomEvent::PitchesCleared
                | RoomEvent::PieceAdded { .. }
                | RoomEvent::PieceMoved { .. }
                | RoomEvent::PieceRemoved { .. }
                | RoomEvent::PiecesCleared
                | RoomEvent::FullStateSync { .. }
        )
    }

    /// Returns true if this event affects voice state.
    pub fn affects_voice(&self) -> bool {
        matches!(
            self,
            RoomEvent::VoiceChanged { .. }
                | RoomEvent::VoiceCleared { .. }
                | RoomEvent::FullStateSync { .. }
        )
    }

    /// Returns true if this event affects piece state.
    pub fn affects_pieces(&self) -> bool {
        matches!(
            self,
            RoomEvent::PieceAdded { .. }
                | RoomEvent::PieceMoved { .. }
                | RoomEvent::PieceRemoved { .. }
                | RoomEvent::PiecesCleared
                | RoomEvent::PiecesLockChanged { .. }
                | RoomEvent::FullStateSync { .. }
        )
    }

    /// Returns true if this is a configuration change.
    pub fn is_config_change(&self) -> bool {
        matches!(
            self,
            RoomEvent::PiecesLockChanged { .. }
                | RoomEvent::TuningChanged { .. }
                | RoomEvent::CombinationMethodChanged { .. }
                | RoomEvent::EmojisChanged { .. }
        )
    }

    /// Returns true if this is a full state sync event.
    pub fn is_full_sync(&self) -> bool {
        matches!(self, RoomEvent::FullStateSync { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_affects_pitches() {
        assert!(
            RoomEvent::PitchAdded {
                pitch_class: PitchClass(0)
            }
            .affects_pitches()
        );
        assert!(
            RoomEvent::PieceAdded {
                id: "test".to_string(),
                pitch: 60,
                emoji: "🪨".to_string()
            }
            .affects_pitches()
        );
        assert!(
            !RoomEvent::VoiceChanged {
                peer_id: "peer".to_string(),
                pitch: Some(60),
                pitch_class: Some(PitchClass(0))
            }
            .affects_pitches()
        );
    }

    #[test]
    fn test_affects_voice() {
        assert!(
            RoomEvent::VoiceChanged {
                peer_id: "peer".to_string(),
                pitch: Some(60),
                pitch_class: Some(PitchClass(0))
            }
            .affects_voice()
        );
        assert!(
            !RoomEvent::PitchAdded {
                pitch_class: PitchClass(0)
            }
            .affects_voice()
        );
    }
}
