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

use crate::client::PeerPath;
use crate::room::v5::{
    ActorId, EXTENSION_REPAIR_ALPN, MUSIC_REPAIR_ALPN, ProtocolSupport, ROOM_PROTOCOL_GENERATION,
    RoomIdentity,
};

/// Room-v5 capability-native ticket payload format.
pub const ROOM_TICKET_FORMAT_V5: u16 = 3;
/// Tickets are user-facing bootstrap material, not an unbounded data channel.
const MAX_ROOM_TICKET_BYTES: usize = 64 * 1024;
/// Gossip carries only bounded repair hints and ephemeral presence. Canonical
/// entries always travel through Replica repair.
pub const MAX_GOSSIP_MESSAGE_BYTES: usize = 1_200_000;

/// How long an accepted repair connection may take to open its stream before we
/// give up on it. Without this a peer that dials and then says nothing pins a
/// handler task (and a queue slot) forever.
pub(crate) const REPAIR_ACCEPT_TIMEOUT: Duration = Duration::from_secs(20);
/// Walkie's Room-v5 endpoint surface. Music-only peers register only gossip
/// and `MUSIC_REPAIR_ALPN`; support metadata is an optimization, while QUIC
/// negotiation remains the connectivity truth.
pub const ROOM_V5_ALPNS: [&[u8]; 3] = [GOSSIP_ALPN, MUSIC_REPAIR_ALPN, EXTENSION_REPAIR_ALPN];
/// Production home relay. A trailing dot avoids relative DNS interpretation.
pub const PRODUCTION_RELAY_URL: &str = "https://relay.wondering.xyz/";
/// Topic-rendezvous signaling server (y-webrtc `funnyzak/y-webrtc-signaling`,
/// behind traefik `Host(signal.wondering.xyz)` → port 4444, wss on 443). The
/// y-webrtc server upgrades any WebSocket at the root path, so no path segment
/// is appended. See [`super::rendezvous`] for the protocol; swapping this to an
/// owned rendezvous (design §3 Option 2) is a one-line change here.
pub const SIGNALING_SERVER_URL: &str = "wss://signal.wondering.xyz";

/// Inbound lane-protocol connections are queued separately from gossip so a
/// peer that opens repair sessions cannot head-of-line block gossip
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
    /// Room v5 uses the capability object itself as opaque discovery identity.
    pub fn from_room_identity(room: &RoomIdentity) -> Self {
        Self(*room.object.as_bytes())
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

pub fn room_mdns_service_name_v5(topic: RoomTopic) -> String {
    format!("walkie-{}-v5", encode_hex(&topic.as_bytes()[..10]))
}

/// Relay selection is explicit so offline-LAN tests never silently use Internet
/// infrastructure and production never silently changes relay homes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum RelayPolicy {
    /// The self-hosted production relay.
    #[default]
    Production,
    /// Number 0's production relay map, intended for development fallback.
    N0Development,
    /// Direct/mDNS-only mode.
    Disabled,
    /// An explicit custom relay list.
    Custom(Vec<String>),
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

/// Network-only construction for a capability-native Room-v5 host.
///
/// Authority remains in `RoomReplicas`; this carries only object discovery,
/// root reconstruction identity, carrier support, relay policy, and an
/// optional bootstrap address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaRoomNetworkConfig {
    pub room: RoomIdentity,
    pub owner: ActorId,
    pub support: ProtocolSupport,
    pub relay: RelayPolicy,
    pub bootstrap: Option<EndpointAddr>,
    pub bootstrap_support: Option<ProtocolSupport>,
}

impl ReplicaRoomNetworkConfig {
    pub fn create(room_name: &str, owner: ActorId) -> Self {
        Self {
            room: RoomIdentity::from_name(room_name),
            owner,
            support: ProtocolSupport::WALKIE,
            relay: RelayPolicy::default(),
            bootstrap: None,
            bootstrap_support: None,
        }
    }

    pub fn join(ticket: &NativeRoomTicketV5) -> Self {
        Self {
            room: ticket.room_identity(),
            owner: ticket.owner(),
            support: ProtocolSupport::WALKIE,
            relay: RelayPolicy::default(),
            bootstrap: Some(ticket.endpoint_addr().clone()),
            bootstrap_support: Some(ticket.support()),
        }
    }

    pub fn topic(&self) -> RoomTopic {
        RoomTopic::from_room_identity(&self.room)
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
    /// A browser custom-transport offer attempt opened after exact attempt
    /// fencing. This is distinct from gossip membership: same endpoint identity
    /// may retain its neighbor slot while the concrete WebRTC placement changes.
    #[cfg(target_arch = "wasm32")]
    DirectReady {
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

/// Capability-native Room-v5 bootstrap ticket.
///
/// The ticket conveys connectivity plus the room object and root owner needed
/// to reconstruct deterministic capability roots. `support` only avoids ALPN
/// attempts; it grants no HHHS authority. A joining actor still needs explicit
/// receiver-bound grants in replicated lane history.
///
/// Payload: `[format=3:u16le][generation=5:u32le][support:u8][object:32]
/// [owner:32][endpoint_len:u32le][endpoint]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeRoomTicketV5 {
    topic: RoomTopic,
    owner: ActorId,
    support: ProtocolSupport,
    endpoint: EndpointTicket,
}

impl NativeRoomTicketV5 {
    pub fn new(
        room: &RoomIdentity,
        owner: ActorId,
        support: ProtocolSupport,
        endpoint: EndpointAddr,
    ) -> Self {
        Self {
            topic: RoomTopic::from_room_identity(room),
            owner,
            support,
            endpoint: EndpointTicket::new(endpoint),
        }
    }

    pub const fn topic(&self) -> RoomTopic {
        self.topic
    }

    pub fn room_identity(&self) -> RoomIdentity {
        RoomIdentity::from_object(hhhs::Digest(*self.topic.as_bytes()))
    }

    pub const fn owner(&self) -> ActorId {
        self.owner
    }

    pub const fn support(&self) -> ProtocolSupport {
        self.support
    }

    pub fn endpoint_addr(&self) -> &EndpointAddr {
        self.endpoint.endpoint_addr()
    }
}

impl Ticket for NativeRoomTicketV5 {
    const KIND: &'static str = "walkieroom5";

    fn encode_bytes(&self) -> Vec<u8> {
        let endpoint = self.endpoint.encode_bytes();
        let mut bytes = Vec::with_capacity(75 + endpoint.len());
        bytes.extend_from_slice(&ROOM_TICKET_FORMAT_V5.to_le_bytes());
        bytes.extend_from_slice(&ROOM_PROTOCOL_GENERATION.to_le_bytes());
        bytes.push(self.support.bits());
        bytes.extend_from_slice(self.topic.as_bytes());
        bytes.extend_from_slice(&self.owner.0);
        bytes.extend_from_slice(&(endpoint.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&endpoint);
        bytes
    }

    fn decode_bytes(bytes: &[u8]) -> Result<Self, TicketParseError> {
        const FIXED: usize = 75;
        if bytes.len() < FIXED || bytes.len() > MAX_ROOM_TICKET_BYTES {
            return Err(TicketParseError::verification_failed(
                "Room-v5 ticket has an invalid length",
            ));
        }
        if u16::from_le_bytes([bytes[0], bytes[1]]) != ROOM_TICKET_FORMAT_V5 {
            return Err(TicketParseError::verification_failed(
                "unsupported Room-v5 ticket format",
            ));
        }
        if u32::from_le_bytes(bytes[2..6].try_into().expect("fixed slice"))
            != ROOM_PROTOCOL_GENERATION
        {
            return Err(TicketParseError::verification_failed(
                "unsupported Room protocol generation",
            ));
        }
        let support = ProtocolSupport::from_bits(bytes[6]).ok_or_else(|| {
            TicketParseError::verification_failed("Room-v5 ticket has invalid support bits")
        })?;
        let mut topic = [0; 32];
        topic.copy_from_slice(&bytes[7..39]);
        let mut owner = [0; 32];
        owner.copy_from_slice(&bytes[39..71]);
        let endpoint_len =
            u32::from_le_bytes(bytes[71..75].try_into().expect("fixed slice")) as usize;
        if endpoint_len != bytes.len() - FIXED {
            return Err(TicketParseError::verification_failed(
                "Room-v5 ticket endpoint length mismatch",
            ));
        }
        Ok(Self {
            topic: RoomTopic::from_bytes(topic),
            owner: ActorId(owner),
            support,
            endpoint: EndpointTicket::decode_bytes(&bytes[FIXED..])?,
        })
    }
}

impl fmt::Display for NativeRoomTicketV5 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.encode_string())
    }
}

impl FromStr for NativeRoomTicketV5 {
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
    fn room_v5_ticket_carries_object_owner_and_non_authoritative_support() {
        let room = RoomIdentity::from_name("quiet-cactus-song");
        let owner = ActorId([23; 32]);
        let endpoint = EndpointAddr::new(iroh::SecretKey::from_bytes(&[43; 32]).public());
        let ticket = NativeRoomTicketV5::new(&room, owner, ProtocolSupport::WALKIE, endpoint);
        assert_eq!(ticket.topic(), RoomTopic::from_room_identity(&room));
        assert_eq!(ticket.room_identity(), room);
        assert_eq!(ticket.owner(), owner);
        assert_eq!(ticket.support(), ProtocolSupport::WALKIE);
        assert_eq!(NativeRoomTicketV5::KIND, "walkieroom5");
        assert_eq!(
            ticket.to_string().parse::<NativeRoomTicketV5>().unwrap(),
            ticket
        );

        let valid = ticket.encode_bytes();
        for bits in [0x00, 0x04, 0x80, 0xff] {
            let mut invalid = valid.clone();
            invalid[6] = bits;
            assert!(NativeRoomTicketV5::decode_bytes(&invalid).is_err());
        }
        let mut wrong_generation = valid.clone();
        wrong_generation[2..6].copy_from_slice(&4_u32.to_le_bytes());
        assert!(NativeRoomTicketV5::decode_bytes(&wrong_generation).is_err());
        let mut wrong_length = valid.clone();
        wrong_length[71..75].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(NativeRoomTicketV5::decode_bytes(&wrong_length).is_err());
        assert!(room_mdns_service_name_v5(ticket.topic()).ends_with("-v5"));
    }

    #[test]
    fn room_v5_alpn_set_has_repair_only_and_no_courier_surface() {
        assert_eq!(
            ROOM_V5_ALPNS,
            [GOSSIP_ALPN, MUSIC_REPAIR_ALPN, EXTENSION_REPAIR_ALPN]
        );
        assert!(!ROOM_V5_ALPNS.contains(&b"tutti/music/courier/1".as_slice()));
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
