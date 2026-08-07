//! Walkie-Songie: P2P collaborative music application
//!
//! This library provides platform-agnostic abstractions for:
//! - Pitch detection (via SwiftF0 ML model)
//! - Room state: signed p2panda op-log + HHHS causal read model (`room`)
//! - Native P2P transport: Iroh + iroh-gossip and HHHS reconciliation (`net`)
//!
//! The core library has no UI dependencies and can be used from
//! Tauri, optional browser clients, plugins, or CLI applications.
pub mod client;
pub mod midi;
pub mod net;
pub mod pitch;
pub mod room;
pub mod tuning;
pub mod words;

#[cfg(all(target_arch = "wasm32", feature = "web-ui"))]
pub mod web;

#[cfg(all(feature = "plugin", not(target_arch = "wasm32")))]
pub mod plugin;

// Plugin exports (must be at crate root for nih-plug to find them)
#[cfg(all(feature = "plugin", not(target_arch = "wasm32")))]
nih_plug::nih_export_clap!(plugin::WalkieSongiePlugin);
#[cfg(all(feature = "plugin", not(target_arch = "wasm32")))]
nih_plug::nih_export_vst3!(plugin::WalkieSongiePlugin);

// Re-export core types
pub use pitch::{PitchDetectorConfig, PitchEvent, SwiftF0Detector};
pub use room::{CombinationMethod, RoomState};
pub use tuning::{
    KeyboardMapping, PeriodicPitch, PitchClass, ScaleDegree, TunedDegree, TunedPeriodicPitch,
    Tuning, TuningDefinition, TuningId,
};
pub use words::{
    generate_room_name, generate_room_qr_svg, is_valid_room_name, room_name_to_topic_id,
};
