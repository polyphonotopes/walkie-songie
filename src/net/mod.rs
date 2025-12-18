//! P2P networking layer - y-webrtc compatible signalling for matchbox.

#[cfg(target_arch = "wasm32")]
mod yjs_signaller;

#[cfg(target_arch = "wasm32")]
pub use yjs_signaller::YjsSignallerBuilder;
