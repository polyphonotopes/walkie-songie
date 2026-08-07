//! Iroh room material shared by the native and browser transports.
//!
//! Everything here is target-independent given an iroh dependency: room topics,
//! tickets, relay policy, wire limits, and path classification. The native
//! endpoint machinery (mDNS, tokio loops) stays in [`super::native`]; the
//! browser endpoint machinery (relay-only, `n0-future` runtime) lives in
//! [`super::browser`]. Both speak the exact same ALPNs, ticket format, and
//! gossip topic derivation — a browser peer and a desktop peer in the same room
//! interoperate byte-for-byte.

use std::{fmt, str::FromStr, time::Duration};

use iroh::{EndpointAddr, EndpointId, RelayConfig, RelayMap, RelayMode, RelayUrl};
use iroh_gossip::TopicId;
use iroh_tickets::{ParseError as TicketParseError, Ticket, endpoint::EndpointTicket};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::PeerId;
use crate::client::PeerPath;

/// Domain separator used when a human room name is converted into an Iroh topic.
const ROOM_TOPIC_CONTEXT: &str = "walkie-songie room topic v1";
/// Current room-ticket payload version.
const ROOM_TICKET_VERSION: u16 = 1;
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

/// How long an accepted repair connection may take to open its stream before we
/// give up on it. Without this a peer that dials and then says nothing pins a
/// handler task (and a queue slot) forever.
pub(crate) const REPAIR_ACCEPT_TIMEOUT: Duration = Duration::from_secs(20);
/// HHHS H6 range-reconciliation protocol generation.
///
/// Generation 2 is the hardened kernel's breaking wire reshape
/// (`Question`/`Ack`, chunkable `Entries { pairs, more }`; see
/// `sync::SYNC_STRATEGY_VERSION`). Old and new peers must never attempt to
/// interop, so the ALPN — the earliest negotiation point — carries the bump.
pub const RBSR_ALPN: &[u8] = b"walkie/rbsr/2";
/// Production home relay. A trailing dot avoids relative DNS interpretation.
pub const PRODUCTION_RELAY_URL: &str = "https://relay.wondering.xyz/";

/// Inbound repair connections are queued separately from gossip so a peer that
/// opens repair sessions cannot head-of-line block op delivery, and so a slow
/// consumer of one never stalls the other.
pub(crate) const REPAIR_QUEUE_DEPTH: usize = 16;
/// Gossip event queue. Deliberately shallow: each slot may hold up to
/// [`MAX_GOSSIP_MESSAGE_BYTES`], so depth is a memory budget, not a comfort knob.
pub(crate) const EVENT_QUEUE_DEPTH: usize = 64;

/// The opaque, stable 32-byte topic used by mDNS, tickets, and gossip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RoomTopic([u8; 32]);

impl RoomTopic {
    /// Derive the room topic without exposing the human-readable room name.
    pub fn from_room_name(room_name: &str) -> Self {
        Self(blake3::derive_key(ROOM_TOPIC_CONTEXT, room_name.as_bytes()))
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

/// Room-isolated DNS-SD label. Only a truncated topic hash is advertised.
pub fn room_mdns_service_name(topic: RoomTopic) -> String {
    format!("walkie-{}-v1", encode_hex(&topic.as_bytes()[..10]))
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
}

impl NativeRoomNetworkConfig {
    pub fn for_room(room_name: &str) -> Self {
        Self {
            topic: RoomTopic::from_room_name(room_name),
            relay: RelayPolicy::default(),
            bootstrap: None,
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
        if !matches!(
            address.usage(),
            iroh::endpoint::TransportAddrUsage::Active
        ) {
            continue;
        }
        match address.addr() {
            iroh::TransportAddr::Ip(_) => return PeerTransportPath::Direct,
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

/// Versioned, room-scoped bootstrap ticket.
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
    fn production_relay_url_is_valid() {
        assert!(RelayPolicy::Production.to_iroh().is_ok());
        assert!(matches!(
            RelayPolicy::Disabled.to_iroh().unwrap(),
            RelayMode::Disabled
        ));
    }
}
