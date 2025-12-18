//! Walkie-Songie: P2P collaborative music application
//!
//! This library provides platform-agnostic abstractions for:
//! - Pitch detection (via SwiftF0 ML model)
//! - Room state management (via `RoomState` trait)
//! - P2P transport (via libp2p with gossipsub)
//!
//! The core library has no UI dependencies and can be used from
//! web (dominator), native (Bevy), or CLI applications.
pub mod pitch;
pub mod room;
pub mod tuning;
pub mod words;

#[cfg(target_arch = "wasm32")]
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
pub use tuning::{PitchClass, Tuning};
pub use words::{generate_room_name, generate_room_qr_svg, is_valid_room_name, room_name_to_topic_id};
