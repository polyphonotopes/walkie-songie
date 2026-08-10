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

pub mod courier;
pub mod identity;
pub mod loopback;
#[cfg(all(not(target_arch = "wasm32"), feature = "native-net"))]
pub mod native;
pub mod sync;

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
pub use loopback::{LoopbackStream, LoopbackTransport, loopback_pair};

#[cfg(any(
    all(not(target_arch = "wasm32"), feature = "native-net"),
    all(target_arch = "wasm32", feature = "browser-net")
))]
pub use iroh_common::{
    MAX_GOSSIP_MESSAGE_BYTES, NativeNetError, NativeNetworkEvent, NativeRoomNetworkConfig,
    NativeRoomTicket, NativeRoomTicketV4, PeerTransportPath, ROOM_V4_ALPNS, RelayPolicy, RoomTopic,
    room_mdns_service_name, room_mdns_service_name_v4,
};
#[cfg(any(
    all(not(target_arch = "wasm32"), feature = "native-net"),
    all(target_arch = "wasm32", feature = "browser-net")
))]
pub use rendezvous::{
    HelloV4, RendezvousHandle, RendezvousPeering, rendezvous_channel_v4, spawn_rendezvous,
    spawn_rendezvous_v4,
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

pub use sync::{
    DEFAULT_RECV_TIMEOUT, EXTENSION_COURIER_ALPN, EXTENSION_RBSR_ALPN, EXTENSION_STRATEGY_NAME,
    ExtensionLane, IncomingOp, LANE_STRATEGY_VERSION, LaneIngest, LaneProtocol, LaneSpec,
    LaneStoreAccess, LaneSyncSource, MAX_SYNC_FRAME_BYTES, MUSIC_COURIER_ALPN, MUSIC_RBSR_ALPN,
    MUSIC_STRATEGY_NAME, MusicLane, RBSR_ALPN, RoomSyncSource, SyncApply, SyncError, SyncLimits,
    SyncOutcome, SyncTimer, WALKIE_COURIER_ALPN, WalkieLane, drive_initiator, drive_responder,
    ingest_pairs,
};

pub use courier::{
    CourierFrame, CourierRefusal, CourierRequest, CourierResponder, CourierResponse,
    CourierWireAnswer, MAX_COURIER_CONTEXT_ENTRIES, MAX_COURIER_FRAME_BYTES,
    MAX_COURIER_LATER_BATCHES, MAX_COURIER_SIBLINGS, TrackedDiscardHistory, apply_courier_response,
    courier_request_for, exchange_courier, lift_deferred_over_stream, serve_courier_once,
};

// ---------------------------------------------------------------------------
// Pluggable transport seam — transport-design.md Addendum C.
//
// Dependency-free by construction: this file compiles on every target and
// feature combination (including `--no-default-features --target
// wasm32-unknown-unknown`), so no backend type may appear here. Backends
// (`native::NativeRoomNetwork` for iroh, JS-bridged libp2p/hyperswarm inside
// Agregore/Peersky) implement these traits behind their own cfg gates.
// ---------------------------------------------------------------------------

use core::future::Future;

use thiserror::Error;

use crate::client::{DiscoverySource, PeerPath};

/// A peer's *transport* identity: the raw 32-byte Ed25519 public key its
/// connection is authenticated under.
///
/// Deliberately distinct from [`AuthorId`](crate::room::ops::AuthorId). The two
/// coincide only for the local participant (see [`identity`], one seed → both
/// keys). An inbound frame's `PeerId` is whoever *delivered* it, which under
/// gossip relaying is routinely not whoever *signed* it: authorship comes from
/// `verify_signed_op_for_topic`, never from the transport.
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

/// Which backend carries a room. Selected at runtime; every mode drives the
/// same `RoomStore` over the same `SignedOp` bytes and the same HHHS
/// `SyncSession`, so a mode change is never a wire-format change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransportMode {
    /// Raw iroh 1.0 + iroh-gossip (Topology A). The default everywhere.
    #[default]
    Iroh,
    /// libp2p gossipsub, reached from a page via Agregore's `pubsub://` protocol
    /// handler (`fetch` to publish, SSE to subscribe). Agregore only — Peersky
    /// registers the scheme but ships no handler for it. Broadcast-only: it has
    /// no per-peer channel and no membership events. See Addendum C.3.2.
    Libp2p,
    /// Hypercore extension messages over hyperswarm-brokered replication
    /// streams, reached from a page via `hyper://<key>/$/extensions/` (`fetch`
    /// to send, SSE to receive) in both Agregore and Peersky. Unlike `Libp2p`
    /// this gives ordered per-peer channels and real peer up/down events.
    /// See Addendum C.3.3.
    Hyperswarm,
    /// In-process duplex pair. Exists so the sync driver is testable with no
    /// sockets at all.
    Loopback,
}

impl TransportMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Iroh => "iroh",
            Self::Libp2p => "libp2p",
            Self::Hyperswarm => "hyperswarm",
            Self::Loopback => "loopback",
        }
    }
}

impl core::str::FromStr for TransportMode {
    type Err = TransportError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "iroh" => Ok(Self::Iroh),
            "libp2p" => Ok(Self::Libp2p),
            "hyperswarm" => Ok(Self::Hyperswarm),
            "loopback" => Ok(Self::Loopback),
            other => Err(TransportError::UnsupportedMode(other.to_string())),
        }
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
    #[error("transport mode {0:?} is not supported in this build")]
    UnsupportedMode(String),
    #[error("transport backend failed: {0}")]
    Backend(String),
}

/// Everything the room layer learns from a transport.
///
/// A generalization of `native::NativeNetworkEvent`, with the iroh `Connection`
/// in `IncomingRepair` replaced by an abstract [`SyncStream`].
#[derive(Debug)]
pub enum TransportEvent<S> {
    PeerUp {
        peer: PeerId,
        discovery: DiscoverySource,
    },
    PeerDown {
        peer: PeerId,
    },
    /// One broadcast frame — the verbatim `SignedOp` wire bytes. `from` is the
    /// delivering peer, NOT the author.
    Message {
        from: PeerId,
        bytes: Vec<u8>,
    },
    /// A peer opened an anti-entropy stream; drive it as the HHHS responder.
    ///
    /// The stream is already open — a backend must not make the room loop await
    /// one here. Whoever handles this must also SPAWN the session rather than
    /// drive it inline: a session lasts as long as the peer takes to answer, and
    /// the room loop has commits and gossip to serve meanwhile.
    LaneRequested {
        peer: PeerId,
        protocol: LaneProtocol,
        stream: S,
    },
    /// Inbound broadcasts were dropped, so anti-entropy owes us the difference.
    Lagged,
    Closed,
    Diagnostic(String),
}

/// One bidirectional, ordered, peer-authenticated **frame** channel carrying a
/// single HHHS `SyncSession`.
///
/// Framed rather than byte-oriented on purpose: iroh gives a QUIC bi-stream that
/// still needs length prefixes (`repair::write_sync_frames`), while a JS-bridged
/// libp2p/hyperswarm socket crosses the wasm boundary as discrete messages
/// anyway. Framing is therefore the backend's job and `SyncMessage::encode` /
/// `decode` is all the driver needs.
pub trait SyncStream {
    /// Send one encoded `SyncMessage`.
    fn send_frame(&mut self, frame: &[u8]) -> impl Future<Output = Result<(), TransportError>>;

    /// Receive one encoded `SyncMessage`; `Ok(None)` is a clean end of stream.
    fn recv_frame(&mut self) -> impl Future<Output = Result<Option<Vec<u8>>, TransportError>>;

    /// Release the stream. Backends that must signal an error code to the peer
    /// do it here rather than on drop.
    fn close(self) -> impl Future<Output = ()>;
}

/// The seam every backend implements.
///
/// No `Send`/`Sync` bounds anywhere: browser backends hold `!Send` JS handles,
/// and — like `hhhs::sync_session::EntrySource` — one session lives
/// entirely inside one task. Async methods are `impl Future`, so the trait is
/// not dyn-compatible; runtime mode selection is an enum over the concrete
/// backends, not `Box<dyn Transport>`.
pub trait Transport {
    type Stream: SyncStream;

    fn mode(&self) -> TransportMode;

    /// Largest frame [`Self::broadcast`] will accept. A `SignedOp` above it must
    /// be left to anti-entropy instead of being silently dropped.
    fn max_broadcast_bytes(&self) -> usize;

    /// Fan one `SignedOp` frame out to the room.
    fn broadcast(&self, frame: Vec<u8>) -> impl Future<Output = Result<(), TransportError>>;

    /// Single-consumer event stream. `None` once the transport is finished.
    ///
    /// One consumer, but NOT one queue: a backend with back-pressure must keep
    /// broadcast delivery and [`TransportEvent::LaneRequested`] on separately
    /// bounded queues and select between them. Sharing one bounded queue lets a
    /// peer that opens repair sessions head-of-line block op delivery, and lets a
    /// slow consumer of either stall the other's producer.
    /// `NativeRoomNetwork::next_inbound` is the reference shape.
    fn next_event(&mut self) -> impl Future<Output = Option<TransportEvent<Self::Stream>>>;

    /// Dial `peer` for one lane-scoped repair or courier exchange.
    fn open_lane(
        &self,
        peer: PeerId,
        protocol: LaneProtocol,
    ) -> impl Future<Output = Result<Self::Stream, TransportError>>;

    /// Honest reachability for `peer`, for UI only.
    fn peer_path(&self, peer: PeerId) -> impl Future<Output = PeerPath>;

    fn shutdown(self) -> impl Future<Output = Result<(), TransportError>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_mode_round_trips_and_defaults_to_iroh() {
        assert_eq!(TransportMode::default(), TransportMode::Iroh);
        for mode in [
            TransportMode::Iroh,
            TransportMode::Libp2p,
            TransportMode::Hyperswarm,
            TransportMode::Loopback,
        ] {
            assert_eq!(mode.as_str().parse::<TransportMode>().unwrap(), mode);
        }
        assert!("gossipsub".parse::<TransportMode>().is_err());
    }

    #[test]
    fn peer_id_hex_is_lowercase_and_full_width() {
        let peer = PeerId([0xab; 32]);
        assert_eq!(peer.to_hex().len(), 64);
        assert!(peer.to_hex().starts_with("abab"));
    }
}
