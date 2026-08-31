//! Shared identity and native Iroh transport types.
//!
//! The portable core owns the Ed25519 identity and wire values. Endpoint, gossip,
//! mDNS, relay, and hole-punching machinery is native-only behind `native-net`.
//! Tauri and optional browser-shell adapters consume the same command/event seam
//! rather than hosting separate transports.
//!
//! The portable identity lives in [`identity`]. Native endpoint, ticket, gossip,
//! room-scoped mDNS, relay policy, and observable path state live in [`native`]
//! behind `native-net`.

pub mod identity;
#[cfg(all(not(target_arch = "wasm32"), feature = "native-net"))]
pub mod native;
/// Signed, bounded Room-v5 realtime messages carried by gossip.
pub mod realtime;
/// HHHS 0.4 Replica repair over any walkie-owned framed carrier.
pub mod replica;

// Shared by BOTH iroh transports (native + browser): topics, tickets, relay
// policy, wire limits, and the framed QUIC sync stream.
#[cfg(any(
    all(not(target_arch = "wasm32"), feature = "native-net"),
    all(target_arch = "wasm32", feature = "browser-net")
))]
pub mod iroh_common;
#[cfg(any(
    all(not(target_arch = "wasm32"), feature = "native-net"),
    all(target_arch = "wasm32", feature = "browser-net")
))]
pub mod repair;

// Topic rendezvous: two clients that type the same room code auto-peer with no
// ticket exchange. Same cfg as `iroh_common` — it needs iroh's `Endpoint`,
// `MemoryLookup`, and gossip `GossipSender`, present on both transports.
#[cfg(any(
    all(not(target_arch = "wasm32"), feature = "native-net"),
    all(target_arch = "wasm32", feature = "browser-net")
))]
pub mod rendezvous;

// Browser iroh transport: relay discovery/fallback plus a WebRTC custom direct
// carrier when rendezvous establishes one.
#[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
pub mod browser;

// WebRTC-as-an-iroh-custom-transport: the browser direct-peering carrier (M4). Sits
// BELOW the iroh `Endpoint` (a `CustomTransport`), so it adds a direct path beside
// the relay without changing anything above the endpoint; the relay stays as
// discovery + fallback. Browser/wasm only — native direct is already iroh UDP+mDNS.
#[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
pub mod webrtc_transport;

#[cfg(all(not(target_arch = "wasm32"), feature = "native-net"))]
pub use identity::FileSeedStore;
pub use identity::{MemorySeedStore, SeedStore, WalkieIdentity};
pub use realtime::{RoomRealtime, RoomRealtimeError};
pub use replica::{
    ReplicaFrameStream, ReplicaLiveRecord, ReplicaProtocol, ReplicaRepairHint, ReplicaRepairProbe,
    ReplicaTimer, drive_replica_initiator, drive_replica_responder, is_routine_repair_initiator,
    repair_lane, replica_frontier_digest,
};

#[cfg(any(
    all(not(target_arch = "wasm32"), feature = "native-net"),
    all(target_arch = "wasm32", feature = "browser-net")
))]
pub use iroh_common::{
    MAX_GOSSIP_MESSAGE_BYTES, NativeNetError, NativeNetworkEvent, NativeRoomTicketV5,
    PeerTransportPath, ROOM_V5_ALPNS, RelayPolicy, ReplicaRoomNetworkConfig, RoomTopic,
    room_mdns_service_name_v5,
};
#[cfg(any(
    all(not(target_arch = "wasm32"), feature = "native-net"),
    all(target_arch = "wasm32", feature = "browser-net")
))]
pub use rendezvous::{
    HelloV5, RendezvousHandle, RendezvousPeering, rendezvous_channel_v5, spawn_rendezvous_v5,
};
#[cfg(any(
    all(not(target_arch = "wasm32"), feature = "native-net"),
    all(target_arch = "wasm32", feature = "browser-net")
))]
pub use repair::{IrohSyncStream, MAX_REPAIR_FRAME_BYTES, read_sync_frame, write_sync_frames};

#[cfg(all(not(target_arch = "wasm32"), feature = "native-net"))]
pub use native::{IncomingRepair, NativeRoomNetwork, RoomInbound};
#[cfg(all(not(target_arch = "wasm32"), feature = "native-net"))]
pub use repair::TokioTimer;

#[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
pub use browser::{
    BrowserIncomingRepair, BrowserNetHandle, BrowserRoomInbound, BrowserRoomNetwork, BrowserTimer,
};

#[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
pub use webrtc_transport::{
    Command as WebRtcCommand, RtcPayload, SignalOut as WebRtcSignalOut, WEBRTC_TRANSPORT_ID,
    WebRtcSignalPort, WebRtcTransport, webrtc_custom_addr,
};

use core::future::Future;
use std::time::Duration;

use thiserror::Error;

/// A peer's *transport* identity: the raw 32-byte Ed25519 public key its
/// connection is authenticated under.
///
/// This is routing metadata, not room authority. An inbound frame's `PeerId` is
/// whoever delivered it; accepted commands and presence still require an HHHS
/// capability presentation by the claimed actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PeerId(pub [u8; 32]);

impl PeerId {
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        use std::fmt::Write;
        self.0.iter().fold(String::with_capacity(64), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
    }
}

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("transport is closed")]
    Closed,
    #[error("frame is {actual} bytes; this transport accepts at most {limit}")]
    FrameTooLarge { actual: usize, limit: usize },
    #[error("peer {0} is unreachable")]
    Unreachable(String),
    #[error("transport backend failed: {0}")]
    Backend(String),
}

/// One bidirectional, ordered, peer-authenticated **frame** channel carrying a
/// single HHHS `SyncSession`.
///
/// Framed rather than byte-oriented on purpose: iroh gives a QUIC bi-stream that
/// still needs length prefixes (`repair::write_sync_frames`), while a JS-bridged
/// browser or native socket crosses its runtime boundary as discrete messages
/// anyway. Framing is therefore the backend's job and `SyncMessage::encode` /
/// `decode` is all the driver needs.
pub trait SyncStream {
    /// Send one encoded `SyncMessage`.
    fn send_frame(&mut self, frame: &[u8]) -> impl Future<Output = Result<(), TransportError>>;

    /// Receive one encoded `SyncMessage`; `Ok(None)` is a clean end of stream.
    fn recv_frame(&mut self) -> impl Future<Output = Result<Option<Vec<u8>>, TransportError>>;

    /// Complete the carrier close handshake. EOF/drop is not success: the
    /// backend must report whether the peer confirmed the terminal boundary.
    fn close(self) -> impl Future<Output = Result<(), TransportError>>;
}

/// Runtime-owned clock used only to adapt the application's task runtime to the
/// sans-I/O HHHS repair driver.
pub trait SyncTimer {
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_id_hex_is_lowercase_and_full_width() {
        let peer = PeerId([0xab; 32]);
        assert_eq!(peer.to_hex().len(), 64);
        assert!(peer.to_hex().starts_with("abab"));
    }
}
