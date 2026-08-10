//! Web application using dominator and futures-signals.
//!
//! Provides the reactive UI for voice input and pitch state management.

#[cfg(target_arch = "wasm32")]
mod app;
#[cfg(target_arch = "wasm32")]
mod audio;
#[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
mod browser_host;
#[cfg(target_arch = "wasm32")]
mod components;
#[cfg(target_arch = "wasm32")]
pub mod graph;
#[cfg(target_arch = "wasm32")]
mod keyboard;
#[cfg(target_arch = "wasm32")]
pub mod midi;
#[cfg(target_arch = "wasm32")]
mod native_bridge;
#[cfg(target_arch = "wasm32")]
pub mod onnx_bridge;
#[cfg(target_arch = "wasm32")]
mod solfege;
#[cfg(target_arch = "wasm32")]
mod storage;
#[cfg(target_arch = "wasm32")]
mod voice_conditioner;

#[cfg(target_arch = "wasm32")]
pub use app::run_app;
