//! P2P networking layer using iroh-gossip for signalling and matchbox for WebRTC.

mod direct_message;
mod signaller;

pub use signaller::create_signaller;
