//! Iroh room material shared by the native and browser transports.
//!
//! Everything here is target-independent given an iroh dependency: room topics,
//! tickets, relay policy, wire limits, and path classification. The native
//! endpoint machinery (mDNS, tokio loops) stays in [`super::native`]; the
//! browser endpoint machinery (relay fallback, WebRTC custom direct transport,
//! `n0-future` runtime) lives in [`super::browser`]. Both speak the exact same
//! ALPNs, ticket format, and gossip topic derivation — a browser peer and a
//! desktop peer in the same room interoperate byte-for-byte.

use std::{fmt, str::FromStr, time::Duration};

use iroh::{EndpointAddr, EndpointId, RelayConfig, RelayMap, RelayMode, RelayUrl};
use iroh_gossip::{TopicId, net::GOSSIP_ALPN};
use iroh_tickets::{ParseError as TicketParseError, Ticket, endpoint::EndpointTicket};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tutti_core::OpLanguage;

use super::PeerId;
use crate::client::PeerPath;
use crate::room::v4::{ExtensionLang, LaneSet, MusicLang, ROOM_PROTOCOL_GENERATION, room_topic_v4};

/// Domain separator used when a human room name is converted into an Iroh topic.
const ROOM_TOPIC_CONTEXT: &str = "walkie-songie room topic v1";
/// Current room-ticket payload version.
const ROOM_TICKET_VERSION: u16 = 1;
/// The v4 ticket payload format, independent of the room protocol generation.
pub const ROOM_TICKET_FORMAT_V4: u16 = 2;
/// Tickets are user-facing bootstrap material, not an unbounded data channel.
const MAX_ROOM_TICKET_BYTES: usize = 64 * 1024;
/// Includes the largest accepted tuning definition plus signed framing overhead.
///
/// Part of the op size ladder rooted at
/// [`MAX_SIGNED_PAYLOAD_BYTES`](crate::room::ops::MAX_SIGNED_PAYLOAD_BYTES): an
/// op that gossip accepts but anti-entropy cannot re-serve is a permanent
/// divergence for every peer that missed the broadcast, so this must not sit
/// below what one op can weigh.
pub const MAX_GOSSIP_MESSAGE_BYTES: usize = 1_200_000;

const _: () = assert!(
    MAX_GOSSIP_MESSAGE_BYTES >= crate::room::ops::MAX_SIGNED_OP_WIRE_BYTES,
    "gossip must not accept an op that anti-entropy could never carry"
);

const MUSIC_MAX_WIRE_BYTES: usize = MusicLang::WIRE_MAGIC.len()
    + 8
    + tutti_core::MAX_SIGNED_HEADER_BYTES
    + MusicLang::MAX_PAYLOAD_BYTES;
const EXTENSION_MAX_WIRE_BYTES: usize = ExtensionLang::WIRE_MAGIC.len()
    + 8
    + tutti_core::MAX_SIGNED_HEADER_BYTES
    + ExtensionLang::MAX_PAYLOAD_BYTES;
const _: () = assert!(
    MAX_GOSSIP_MESSAGE_BYTES >= MUSIC_MAX_WIRE_BYTES,
    "gossip must carry the largest music-lane frame"
);
const _: () = assert!(
    MAX_GOSSIP_MESSAGE_BYTES >= EXTENSION_MAX_WIRE_BYTES,
    "gossip must carry the largest extension-lane frame"
);

/// How long an accepted repair connection may take to open its stream before we
/// give up on it. Without this a peer that dials and then says nothing pins a
/// handler task (and a queue slot) forever.
pub(crate) const REPAIR_ACCEPT_TIMEOUT: Duration = Duration::from_secs(20);
/// Repair ALPNs re-exported from the lane layer so every transport names them
/// from one shared place.
///
/// * [`RBSR_ALPN`] — the retired v3 single-lane generation
///   ([`super::sync::WalkieLane`]), retained only for refusal and legacy-kernel
///   fixtures. It is never included in [`ROOM_V4_ALPNS`].
/// * [`MUSIC_RBSR_ALPN`] — the tutti-music lane (generation 3), defined by
///   tutti-music itself so a bare peer speaks it without walkie.
/// * [`EXTENSION_RBSR_ALPN`] — walkie's extension lane (generation 3).
///
/// The registered ALPN set IS a peer's authoritative live lane-capability
/// declaration: an unsupported lane fails at QUIC negotiation, before any RBSR
/// byte. [`NativeRoomTicketV4`] and v4 rendezvous also advertise lane bits so a
/// dialer can avoid unsupported attempts; negotiation still decides.
pub use super::sync::{
    EXTENSION_COURIER_ALPN, EXTENSION_RBSR_ALPN, MUSIC_COURIER_ALPN, MUSIC_RBSR_ALPN, RBSR_ALPN,
};

/// The exact room-v4 endpoint surface. A v4 endpoint registers no v3 repair
/// or courier ALPN.
pub const ROOM_V4_ALPNS: [&[u8]; 5] = [
    GOSSIP_ALPN,
    MUSIC_RBSR_ALPN,
    EXTENSION_RBSR_ALPN,
    MUSIC_COURIER_ALPN,
    EXTENSION_COURIER_ALPN,
];
/// Production home relay. A trailing dot avoids relative DNS interpretation.
pub const PRODUCTION_RELAY_URL: &str = "https://relay.wondering.xyz/";
/// Topic-rendezvous signaling server (y-webrtc `funnyzak/y-webrtc-signaling`,
/// behind traefik `Host(signal.wondering.xyz)` → port 4444, wss on 443). The
/// y-webrtc server upgrades any WebSocket at the root path, so no path segment
/// is appended. See [`super::rendezvous`] for the protocol; swapping this to an
/// owned rendezvous (design §3 Option 2) is a one-line change here.
pub const SIGNALING_SERVER_URL: &str = "wss://signal.wondering.xyz";

/// Inbound lane-protocol connections are queued separately from gossip so a
/// peer that opens repair or courier sessions cannot head-of-line block op
/// delivery, and so a slow consumer of one never stalls the other.
pub(crate) const REPAIR_QUEUE_DEPTH: usize = 16;
/// Gossip event queue. Deliberately shallow: each slot may hold up to
/// [`MAX_GOSSIP_MESSAGE_BYTES`], so depth is a memory budget, not a comfort knob.
pub(crate) const EVENT_QUEUE_DEPTH: usize = 64;

/// The opaque, stable 32-byte topic used by mDNS, tickets, and gossip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RoomTopic([u8; 32]);

impl RoomTopic {
    /// Derive the retired v1 room topic for golden and refusal fixtures.
    /// Live runtimes use [`Self::from_room_name_v4`].
    pub fn from_room_name(room_name: &str) -> Self {
        Self(blake3::derive_key(ROOM_TOPIC_CONTEXT, room_name.as_bytes()))
    }

    /// Construct the room-v4 topic from its human-readable room name.
    pub fn from_room_name_v4(room_name: &str) -> Self {
        Self(room_topic_v4(room_name))
    }

    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub const fn gossip_topic(self) -> TopicId {
        TopicId::from_bytes(self.0)
    }

    pub fn to_hex(self) -> String {
        encode_hex(&self.0)
    }
}

impl fmt::Display for RoomTopic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&encode_hex(&self.0))
    }
}

/// Retired v1 DNS-SD label, retained for fixture and refusal coverage.
pub fn room_mdns_service_name(topic: RoomTopic) -> String {
    format!("walkie-{}-v1", encode_hex(&topic.as_bytes()[..10]))
}

/// Room-v4 DNS-SD label. The suffix isolates discovery generations before
/// ALPN negotiation; ALPN remains the authoritative lane capability check.
pub fn room_mdns_service_name_v4(topic: RoomTopic) -> String {
    format!("walkie-{}-v4", encode_hex(&topic.as_bytes()[..10]))
}

/// Relay selection is explicit so offline-LAN tests never silently use Internet
/// infrastructure and production never silently changes relay homes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayPolicy {
    /// The self-hosted production relay.
    Production,
    /// Number 0's production relay map, intended for development fallback.
    N0Development,
    /// Direct/mDNS-only mode.
    Disabled,
    /// An explicit custom relay list.
    Custom(Vec<String>),
}

impl Default for RelayPolicy {
    fn default() -> Self {
        Self::Production
    }
}

impl RelayPolicy {
    pub(crate) fn to_iroh(&self) -> Result<RelayMode, NativeNetError> {
        match self {
            Self::Production => {
                // Primary home relay is the self-hosted, token-restricted relay.
                // n0 is intentionally NOT mixed into this map: its relays don't
                // mesh with ours, so a peer that landed on an n0 home would never
                // meet a peer on ours. True n0 fallback belongs at the app level
                // (re-bind with `N0Development` only when this relay is down).
                let mut relay: RelayConfig = parse_relay(PRODUCTION_RELAY_URL)?.into();
                if let Some(token) = relay_auth_token() {
                    relay = relay.with_auth_token(token);
                }
                Ok(RelayMode::Custom(RelayMap::from_iter([relay])))
            }
            Self::N0Development => Ok(RelayMode::Default),
            Self::Disabled => Ok(RelayMode::Disabled),
            Self::Custom(urls) if urls.is_empty() => Err(NativeNetError::InvalidRelay(
                "custom relay list is empty".into(),
            )),
            Self::Custom(urls) => urls
                .iter()
                .map(|url| parse_relay(url))
                .collect::<Result<Vec<_>, _>>()
                .map(RelayMode::custom),
        }
    }
}

/// Bearer token authenticating this client to the self-hosted production relay.
///
/// Injected at build time via the `WALKIE_RELAY_TOKEN` environment variable so
/// the value is never committed to source. On native, iroh sends it as an
/// `Authorization: Bearer` header; under wasm it rides as a `?token=` query
/// parameter (browsers can't set WebSocket upgrade headers). Unset or empty
/// yields `None`, and the restricted relay rejects the client as "not
/// authorized". The token is a soft gate — it ships in the built artifact; see
/// `docs/research/zk-relay-attestation.md` for the stronger Origin/attestation
/// path.
fn relay_auth_token() -> Option<&'static str> {
    match option_env!("WALKIE_RELAY_TOKEN") {
        Some(token) if !token.is_empty() => Some(token),
        _ => None,
    }
}

fn parse_relay(value: &str) -> Result<RelayUrl, NativeNetError> {
    value
        .parse()
        .map_err(|error| NativeNetError::InvalidRelay(format!("{value}: {error}")))
}

/// Configuration for one active room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeRoomNetworkConfig {
    pub topic: RoomTopic,
    pub relay: RelayPolicy,
    /// Optional ticket endpoint used to bootstrap the topic.
    pub bootstrap: Option<EndpointAddr>,
    /// Capabilities advertised by the bootstrap ticket. Discovery paths that
    /// do not carry capabilities leave this `None` and optimistically try both
    /// repair lanes.
    pub bootstrap_lanes: Option<LaneSet>,
}

impl NativeRoomNetworkConfig {
    pub fn for_room(room_name: &str) -> Self {
        Self {
            topic: RoomTopic::from_room_name_v4(room_name),
            relay: RelayPolicy::default(),
            bootstrap: None,
            bootstrap_lanes: None,
        }
    }
}

/// Honest transport classification based only on Iroh's active address usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerTransportPath {
    Connecting,
    Direct,
    Relayed,
    Disconnected,
}

impl From<PeerTransportPath> for PeerPath {
    fn from(value: PeerTransportPath) -> Self {
        match value {
            PeerTransportPath::Connecting => PeerPath::Connecting,
            PeerTransportPath::Direct => PeerPath::Direct,
            PeerTransportPath::Relayed => PeerPath::Relayed,
            PeerTransportPath::Disconnected => PeerPath::Disconnected,
        }
    }
}

/// Classify how `endpoint` currently reaches `endpoint_id`, from active address
/// usage only. Shared verbatim by the native and browser transports (a browser
/// peer can only ever observe `Relayed`, but the classification logic is the
/// same honest code path).
pub(crate) async fn classify_peer_path(
    endpoint: &iroh::Endpoint,
    endpoint_id: EndpointId,
) -> PeerTransportPath {
    let Some(info) = endpoint.remote_info(endpoint_id).await else {
        return PeerTransportPath::Disconnected;
    };
    let mut active_relay = false;
    for address in info.addrs() {
        if !matches!(address.usage(), iroh::endpoint::TransportAddrUsage::Active) {
            continue;
        }
        match address.addr() {
            iroh::TransportAddr::Ip(_) => return PeerTransportPath::Direct,
            // A WebRTC custom-transport path is a direct data-channel link (M4);
            // without this arm iroh's non-IP/relay addresses fall through and a fast
            // direct browser path would be misreported as `Connecting`. Any custom
            // path is ours (walkie registers exactly one: WebRTC), so treat it as
            // Direct — the honest reachability the UI meter should show.
            iroh::TransportAddr::Custom(_) => return PeerTransportPath::Direct,
            iroh::TransportAddr::Relay(_) => active_relay = true,
            _ => {}
        }
    }
    if active_relay {
        PeerTransportPath::Relayed
    } else {
        PeerTransportPath::Connecting
    }
}

/// Events produced by a room transport task. Shared by the native and browser
/// backends; the mDNS variants simply never occur in a browser.
#[derive(Debug, Clone)]
pub enum NativeNetworkEvent {
    MdnsDiscovered {
        endpoint_id: EndpointId,
    },
    MdnsExpired {
        endpoint_id: EndpointId,
    },
    NeighborUp {
        endpoint_id: EndpointId,
        discovery: crate::client::DiscoverySource,
    },
    NeighborDown {
        endpoint_id: EndpointId,
    },
    Message {
        delivered_from: EndpointId,
        bytes: Vec<u8>,
    },
    Lagged,
    Closed,
    Diagnostic(String),
}

/// Retired v3 room bootstrap ticket, retained for fixture and refusal coverage.
/// Live runtimes use [`NativeRoomTicketV4`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeRoomTicket {
    topic: RoomTopic,
    endpoint: EndpointTicket,
}

impl NativeRoomTicket {
    pub fn new(topic: RoomTopic, endpoint: EndpointAddr) -> Self {
        Self {
            topic,
            endpoint: EndpointTicket::new(endpoint),
        }
    }

    pub const fn topic(&self) -> RoomTopic {
        self.topic
    }

    pub fn endpoint_addr(&self) -> &EndpointAddr {
        self.endpoint.endpoint_addr()
    }
}

impl Ticket for NativeRoomTicket {
    const KIND: &'static str = "walkieroom";

    fn encode_bytes(&self) -> Vec<u8> {
        let endpoint = self.endpoint.encode_bytes();
        let mut bytes = Vec::with_capacity(2 + 32 + 4 + endpoint.len());
        bytes.extend_from_slice(&ROOM_TICKET_VERSION.to_le_bytes());
        bytes.extend_from_slice(self.topic.as_bytes());
        bytes.extend_from_slice(&(endpoint.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&endpoint);
        bytes
    }

    fn decode_bytes(bytes: &[u8]) -> Result<Self, TicketParseError> {
        if bytes.len() < 38 || bytes.len() > MAX_ROOM_TICKET_BYTES {
            return Err(TicketParseError::verification_failed(
                "room ticket has an invalid length",
            ));
        }
        let version = u16::from_le_bytes([bytes[0], bytes[1]]);
        if version != ROOM_TICKET_VERSION {
            return Err(TicketParseError::verification_failed(
                "unsupported room ticket version",
            ));
        }
        let mut topic = [0_u8; 32];
        topic.copy_from_slice(&bytes[2..34]);
        let endpoint_len =
            u32::from_le_bytes(bytes[34..38].try_into().expect("fixed slice")) as usize;
        if endpoint_len != bytes.len() - 38 {
            return Err(TicketParseError::verification_failed(
                "room ticket endpoint length mismatch",
            ));
        }
        let endpoint = EndpointTicket::decode_bytes(&bytes[38..])?;
        Ok(Self {
            topic: RoomTopic::from_bytes(topic),
            endpoint,
        })
    }
}

impl fmt::Display for NativeRoomTicket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.encode_string())
    }
}

impl FromStr for NativeRoomTicket {
    type Err = TicketParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::decode_string(value)
    }
}

/// Room-v4 bootstrap ticket.
///
/// Payload: `[format=2:u16le][generation=4:u16le][lane_bits:u8][topic:32]
/// [endpoint_len:u32le][endpoint]`. The distinct ticket kind and strict
/// generation/lane validation make every v3 artifact fail closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeRoomTicketV4 {
    topic: RoomTopic,
    lanes: LaneSet,
    endpoint: EndpointTicket,
}

impl NativeRoomTicketV4 {
    pub fn new(topic: RoomTopic, lanes: LaneSet, endpoint: EndpointAddr) -> Self {
        Self {
            topic,
            lanes,
            endpoint: EndpointTicket::new(endpoint),
        }
    }

    pub const fn topic(&self) -> RoomTopic {
        self.topic
    }

    pub const fn lanes(&self) -> LaneSet {
        self.lanes
    }

    pub fn endpoint_addr(&self) -> &EndpointAddr {
        self.endpoint.endpoint_addr()
    }
}

impl Ticket for NativeRoomTicketV4 {
    const KIND: &'static str = "walkieroom4";

    fn encode_bytes(&self) -> Vec<u8> {
        let endpoint = self.endpoint.encode_bytes();
        let mut bytes = Vec::with_capacity(41 + endpoint.len());
        bytes.extend_from_slice(&ROOM_TICKET_FORMAT_V4.to_le_bytes());
        bytes.extend_from_slice(&ROOM_PROTOCOL_GENERATION.to_le_bytes());
        bytes.push(self.lanes.bits());
        bytes.extend_from_slice(self.topic.as_bytes());
        bytes.extend_from_slice(&(endpoint.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&endpoint);
        bytes
    }

    fn decode_bytes(bytes: &[u8]) -> Result<Self, TicketParseError> {
        const FIXED: usize = 41;
        if bytes.len() < FIXED || bytes.len() > MAX_ROOM_TICKET_BYTES {
            return Err(TicketParseError::verification_failed(
                "room-v4 ticket has an invalid length",
            ));
        }
        if u16::from_le_bytes([bytes[0], bytes[1]]) != ROOM_TICKET_FORMAT_V4 {
            return Err(TicketParseError::verification_failed(
                "unsupported room-v4 ticket format",
            ));
        }
        if u16::from_le_bytes([bytes[2], bytes[3]]) != ROOM_PROTOCOL_GENERATION {
            return Err(TicketParseError::verification_failed(
                "unsupported room protocol generation",
            ));
        }
        let lanes = LaneSet::from_bits(bytes[4]).ok_or_else(|| {
            TicketParseError::verification_failed("room-v4 ticket has invalid lane bits")
        })?;
        let mut topic = [0_u8; 32];
        topic.copy_from_slice(&bytes[5..37]);
        let endpoint_len =
            u32::from_le_bytes(bytes[37..41].try_into().expect("fixed slice")) as usize;
        if endpoint_len != bytes.len() - FIXED {
            return Err(TicketParseError::verification_failed(
                "room-v4 ticket endpoint length mismatch",
            ));
        }
        Ok(Self {
            topic: RoomTopic::from_bytes(topic),
            lanes,
            endpoint: EndpointTicket::decode_bytes(&bytes[FIXED..])?,
        })
    }
}

impl fmt::Display for NativeRoomTicketV4 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.encode_string())
    }
}

impl FromStr for NativeRoomTicketV4 {
    type Err = TicketParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::decode_string(value)
    }
}

#[derive(Debug, Error)]
pub enum NativeNetError {
    #[error("invalid relay configuration: {0}")]
    InvalidRelay(String),
    #[error("could not bind Iroh endpoint: {0}")]
    Bind(String),
    #[error("could not initialize room mDNS: {0}")]
    Mdns(String),
    #[error("could not initialize Iroh gossip: {0}")]
    Gossip(String),
    #[error("native room task is closed")]
    Closed,
}

pub(crate) fn peer_of(endpoint_id: EndpointId) -> PeerId {
    PeerId(*endpoint_id.as_bytes())
}

pub(crate) fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(
        String::with_capacity(bytes.len() * 2),
        |mut output, byte| {
            let _ = write!(output, "{byte:02x}");
            output
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::sync::LaneProtocol;
    use crate::room::v4::RoomLane;

    #[test]
    fn room_topics_and_mdns_names_are_stable_and_room_scoped() {
        let first = RoomTopic::from_room_name("quiet-cactus-song");
        assert_eq!(first, RoomTopic::from_room_name("quiet-cactus-song"));
        assert_ne!(first, RoomTopic::from_room_name("quiet-cactus-drum"));

        let service = room_mdns_service_name(first);
        assert!(service.starts_with("walkie-"));
        assert!(service.ends_with("-v1"));
        assert_eq!(service.len(), 30);
        assert!(!service.contains("quiet"));
    }

    #[test]
    fn room_ticket_round_trips_and_rejects_other_kinds() {
        let topic = RoomTopic::from_room_name("quiet-cactus-song");
        let endpoint = EndpointAddr::new(iroh::SecretKey::from_bytes(&[7; 32]).public());
        let ticket = NativeRoomTicket::new(topic, endpoint);
        let encoded = ticket.to_string();
        let decoded: NativeRoomTicket = encoded.parse().unwrap();
        assert_eq!(decoded, ticket);
        assert!("endpointabc".parse::<NativeRoomTicket>().is_err());
    }

    #[test]
    fn room_ticket_rejects_tampered_length() {
        let topic = RoomTopic::from_room_name("quiet-cactus-song");
        let endpoint = EndpointAddr::new(iroh::SecretKey::from_bytes(&[8; 32]).public());
        let ticket = NativeRoomTicket::new(topic, endpoint);
        let mut bytes = ticket.encode_bytes();
        bytes[34..38].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(NativeRoomTicket::decode_bytes(&bytes).is_err());
    }

    #[test]
    fn room_v4_topic_mdns_and_ticket_bytes_are_exact() {
        let topic = RoomTopic::from_room_name_v4("Quiet-Cactus-Song");
        assert_eq!(topic, RoomTopic::from_room_name_v4("quiet-cactus-song"));
        assert_eq!(topic.as_bytes(), &room_topic_v4("quiet-cactus-song"));
        assert_eq!(
            room_mdns_service_name_v4(topic),
            format!("walkie-{}-v4", encode_hex(&topic.as_bytes()[..10]))
        );

        let endpoint = EndpointAddr::new(iroh::SecretKey::from_bytes(&[41; 32]).public());
        let ticket = NativeRoomTicketV4::new(topic, LaneSet::WALKIE, endpoint);
        let endpoint_bytes = ticket.endpoint.encode_bytes();
        let mut expected = Vec::new();
        expected.extend_from_slice(&[0x02, 0x00, 0x04, 0x00, 0x03]);
        expected.extend_from_slice(topic.as_bytes());
        expected.extend_from_slice(&(endpoint_bytes.len() as u32).to_le_bytes());
        expected.extend_from_slice(&endpoint_bytes);
        assert_eq!(ticket.encode_bytes(), expected);
        assert_eq!(NativeRoomTicketV4::KIND, "walkieroom4");
        assert_eq!(
            ticket.to_string().parse::<NativeRoomTicketV4>().unwrap(),
            ticket
        );
    }

    #[test]
    fn room_v4_ticket_rejects_every_foreign_generation_and_lane_set() {
        let topic = RoomTopic::from_room_name_v4("quiet-cactus-song");
        let endpoint = EndpointAddr::new(iroh::SecretKey::from_bytes(&[42; 32]).public());
        let ticket = NativeRoomTicketV4::new(topic, LaneSet::MUSIC, endpoint);
        let valid = ticket.encode_bytes();

        for (offset, bytes) in [(0, [0x01, 0x00]), (2, [0x03, 0x00])] {
            let mut invalid = valid.clone();
            invalid[offset..offset + 2].copy_from_slice(&bytes);
            assert!(NativeRoomTicketV4::decode_bytes(&invalid).is_err());
        }
        for bits in [0x00, 0x04, 0x80, 0xff] {
            let mut invalid = valid.clone();
            invalid[4] = bits;
            assert!(NativeRoomTicketV4::decode_bytes(&invalid).is_err());
        }

        let v3 = NativeRoomTicket::new(topic, ticket.endpoint_addr().clone()).encode_bytes();
        assert!(NativeRoomTicketV4::decode_bytes(&v3).is_err());
        assert!(NativeRoomTicket::decode_bytes(&valid).is_err());
    }

    #[test]
    fn room_v4_alpn_set_and_dispatch_are_exact() {
        assert_eq!(
            ROOM_V4_ALPNS,
            [
                GOSSIP_ALPN,
                MUSIC_RBSR_ALPN,
                EXTENSION_RBSR_ALPN,
                MUSIC_COURIER_ALPN,
                EXTENSION_COURIER_ALPN,
            ]
        );
        assert!(!ROOM_V4_ALPNS.contains(&RBSR_ALPN));
        for protocol in [
            LaneProtocol::Repair(RoomLane::Music),
            LaneProtocol::Repair(RoomLane::Extension),
            LaneProtocol::Courier(RoomLane::Music),
            LaneProtocol::Courier(RoomLane::Extension),
        ] {
            assert_eq!(LaneProtocol::from_alpn(protocol.alpn()), Some(protocol));
        }
        assert_eq!(LaneProtocol::from_alpn(RBSR_ALPN), None);
        assert_eq!(LaneProtocol::from_alpn(b"walkie/unknown/1"), None);
    }

    #[test]
    fn production_relay_url_is_valid() {
        assert!(RelayPolicy::Production.to_iroh().is_ok());
        assert!(matches!(
            RelayPolicy::Disabled.to_iroh().unwrap(),
            RelayMode::Disabled
        ));
    }
}
