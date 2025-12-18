//! Web application using dominator and futures-signals.
//!
//! Provides the reactive UI for voice input and pitch state management.

#[cfg(target_arch = "wasm32")]
mod app;
#[cfg(target_arch = "wasm32")]
mod audio;
#[cfg(target_arch = "wasm32")]
mod components;
#[cfg(target_arch = "wasm32")]
mod keyboard;
#[cfg(target_arch = "wasm32")]
pub mod midi;
#[cfg(target_arch = "wasm32")]
pub mod onnx_bridge;
#[cfg(target_arch = "wasm32")]
mod libp2p_sync;
#[cfg(target_arch = "wasm32")]
mod voice_conditioner;

#[cfg(target_arch = "wasm32")]
pub use app::run_app;
