//! Walkie-Songie: P2P collaborative music application
//!
//! This library provides platform-agnostic abstractions for:
//! - Pitch detection (via `PitchDetector` trait)
//! - Room state management (via `RoomState` trait)
//! - P2P transport (via existing matchbox/iroh signaller)
//!
//! The core library has no UI dependencies and can be used from
//! web (dominator), native (Bevy), or CLI applications.

pub mod net;
pub mod pitch;
pub mod room;
pub mod tuning;

#[cfg(target_arch = "wasm32")]
pub mod web;

// Re-export core traits
pub use pitch::PitchDetector;
pub use room::{CombinationMethod, RoomState};
pub use tuning::{PitchClass, Tuning};
