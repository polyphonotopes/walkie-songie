//! Capability-native Room-v5 protocol and application projections.

pub mod events;
pub mod projection;
#[cfg(any(test, all(target_arch = "wasm32", feature = "browser-net")))]
pub(crate) mod session;
pub mod streams;
/// Room v5: capability-native HHHS Replicas with application-owned carriers.
pub mod v5;
#[cfg(any(test, all(target_arch = "wasm32", feature = "browser-net")))]
pub(crate) mod worker;

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
pub use projection::{Piece, RoomProjection};
#[cfg(target_arch = "wasm32")]
pub use streams::{
    available_emojis_signal, pieces_locked_signal, pieces_signal, shared_pitches_signal,
    unified_pitch_classes_signal,
};
