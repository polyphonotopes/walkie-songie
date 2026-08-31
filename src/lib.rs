//! Walkie-Songie: P2P collaborative music application
//!
//! This library provides platform-agnostic components for:
//! - Pitch detection (via SwiftF0 ML model)
//! - Capability-admitted Room-v5 Replicas and materialized views (`room`)
//! - Application-owned Iroh/WebRTC carriers for HHHS repair (`net`)
//!
//! The core library has no UI dependencies and can be used from
//! Tauri, optional browser clients, plugins, or CLI applications.
pub mod bridge;
pub mod client;
pub mod midi;
pub mod net;
pub mod pitch;
pub mod room;
pub mod tuning;
pub mod words;

#[cfg(all(not(target_arch = "wasm32"), feature = "plugin"))]
pub mod plugin;

#[cfg(all(target_arch = "wasm32", feature = "web-ui"))]
pub mod web;

// Re-export core types
pub use pitch::{PitchDetectorConfig, PitchEvent, SwiftF0Detector};
pub use room::RoomProjection;
pub use tuning::{
    KeyboardMapping, PeriodicPitch, PitchClass, ScaleDegree, TunedDegree, TunedPeriodicPitch,
    Tuning, TuningDefinition, TuningId,
};
pub use words::{
    generate_room_name, generate_room_qr_svg, is_valid_room_name, room_name_to_topic_id,
};
