//! Native Iroh room transport.
//!
//! Iroh owns QUIC connectivity, relay-assisted hole punching, path migration,
//! and relay fallback. This module adds room scoping, mDNS discovery, tickets,
//! bounded gossip, and honest direct/relay reporting; it does not implement a
//! second NAT traversal protocol.
//!
//! Room topics, tickets, relay policy, and wire limits are shared with the
//! browser transport and live in [`super::iroh_common`].

use std::{collections::HashMap, time::Duration};

use futures::{StreamExt, TryStreamExt};
use iroh::{
    Endpoint, EndpointId,
    address_lookup::MemoryLookup,
    endpoint::{Connection, presets},
    protocol::{AcceptError, ProtocolHandler, Router},
};
use iroh_gossip::{
    api::{Event as GossipEvent, GossipSender},
    net::{GOSSIP_ALPN, Gossip},
};
use iroh_mdns_address_lookup::{DiscoveryEvent, MdnsAddressLookup};
use tokio::{sync::mpsc, task::JoinHandle};

use super::iroh_common::{
    EVENT_QUEUE_DEPTH, MAX_GOSSIP_MESSAGE_BYTES, NativeNetError, NativeNetworkEvent,
    NativeRoomNetworkConfig, NativeRoomTicket, PeerTransportPath, RBSR_ALPN,
    REPAIR_ACCEPT_TIMEOUT, REPAIR_QUEUE_DEPTH, RoomTopic, classify_peer_path, peer_of,
    room_mdns_service_name,
};
use super::repair::IrohSyncStream;
use super::{PeerId, Transport, TransportError, TransportEvent, TransportMode};
use crate::client::{DiscoverySource, PeerPath};

/// A bound native Iroh endpoint and its active gossip topic.
pub struct NativeRoomNetwork {
    topic: RoomTopic,
    router: Router,
    gossip_sender: GossipSender,
    /// The one address-lookup instance ticket joins feed. Iroh's
    /// `AddressLookupServices::add` only ever pushes — there is no removal and no
    /// dedup — so a fresh `MemoryLookup` per join would leak a service per call.
    memory_lookup: MemoryLookup,
    events: mpsc::Receiver<NativeNetworkEvent>,
    repairs: mpsc::Receiver<IncomingRepair>,
    event_task: JoinHandle<()>,
}

/// One item from either of the room's two inbound queues.
#[derive(Debug)]
pub enum RoomInbound {
    Event(NativeNetworkEvent),
    Repair(IncomingRepair),
}

/// An inbound repair connection, delivered on its own queue.
///
/// The bi-stream is already accepted: the wait happens inside the per-connection
/// protocol handler, so a peer that dials and stalls cannot delay anything the
/// room loop is doing. `connection` is retained to keep the QUIC state alive for
/// as long as the stream is in use.
#[derive(Debug)]
pub struct IncomingRepair {
    pub endpoint_id: EndpointId,
    pub connection: Connection,
    pub stream: IrohSyncStream,
}

impl NativeRoomNetwork {
    /// Bind an endpoint using one persistent identity seed.
    pub async fn bind(
        secret_key: iroh::SecretKey,
        config: NativeRoomNetworkConfig,
    ) -> Result<Self, NativeNetError> {
        let relay_mode = config.relay.to_iroh()?;
        let memory = MemoryLookup::new();
        if let Some(bootstrap) = config.bootstrap.clone() {
            memory.add_endpoint_info(bootstrap);
        }
        let (events_tx, events) = mpsc::channel(EVENT_QUEUE_DEPTH);
        let (repairs_tx, repairs) = mpsc::channel(REPAIR_QUEUE_DEPTH);

        // N0 supplies the native IP and relay transports. Address lookup is
        // deliberately rebuilt from memory + room-scoped mDNS below.
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(secret_key)
            .alpns(vec![GOSSIP_ALPN.to_vec(), RBSR_ALPN.to_vec()])
            .relay_mode(relay_mode)
            .clear_address_lookup()
            .address_lookup(memory.clone())
            .bind()
            .await
            .map_err(|error| NativeNetError::Bind(error.to_string()))?;

        let mdns = MdnsAddressLookup::builder()
            .service_name(room_mdns_service_name(config.topic))
            .build(endpoint.id())
            .map_err(|error| NativeNetError::Mdns(error.to_string()))?;
        endpoint
            .address_lookup()
            .map_err(|error| NativeNetError::Mdns(error.to_string()))?
            .add(mdns.clone());
        let mut mdns_events = mdns.subscribe().await;

        let gossip = Gossip::builder()
            .max_message_size(MAX_GOSSIP_MESSAGE_BYTES)
            .spawn(endpoint.clone());
        let router = Router::builder(endpoint.clone())
            .accept(GOSSIP_ALPN, gossip.clone())
            .accept(
                RBSR_ALPN,
                RepairProtocol {
                    repairs: repairs_tx,
                },
            )
            .spawn();

        let bootstrap = config
            .bootstrap
            .as_ref()
            .map(|address| vec![address.id])
            .unwrap_or_default();
        let mut discovery_sources: HashMap<EndpointId, DiscoverySource> = bootstrap
            .iter()
            .copied()
            .map(|endpoint_id| (endpoint_id, DiscoverySource::Ticket))
            .collect();
        let topic = gossip
            .subscribe(config.topic.gossip_topic(), bootstrap)
            .await
            .map_err(|error| NativeNetError::Gossip(error.to_string()))?;
        let (gossip_sender, mut gossip_events) = topic.split();
        let event_sender_for_mdns = gossip_sender.clone();
        let own_endpoint = endpoint.id();

        // Once the mDNS `ReceiverStream` is exhausted it yields `None` forever.
        // Latch it closed and park that select branch on a never-ready future,
        // or the loop spins — flooding the event queue with diagnostics and then
        // blocking on a full channel, which takes gossip delivery down with it.
        let mut mdns_open = true;
        let event_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    mdns = mdns_events.next(), if mdns_open => {
                        let Some(mdns) = mdns else {
                            mdns_open = false;
                            let _ = events_tx.try_send(NativeNetworkEvent::Diagnostic(
                                "room mDNS event stream closed".into()
                            ));
                            continue;
                        };
                        match mdns {
                            DiscoveryEvent::Discovered { endpoint_info, .. } => {
                                let endpoint_id = endpoint_info.endpoint_id;
                                if endpoint_id == own_endpoint {
                                    continue;
                                }
                                discovery_sources
                                    .insert(endpoint_id, DiscoverySource::Mdns);
                                if let Err(error) =
                                    event_sender_for_mdns.join_peers(vec![endpoint_id]).await
                                {
                                    let _ = events_tx.send(NativeNetworkEvent::Diagnostic(
                                        format!("could not join mDNS peer {endpoint_id}: {error}")
                                    )).await;
                                }
                                if events_tx.send(NativeNetworkEvent::MdnsDiscovered {
                                    endpoint_id,
                                }).await.is_err() {
                                    break;
                                }
                            }
                            DiscoveryEvent::Expired { endpoint_id } => {
                                // Drop the attribution too, or the map grows with
                                // every peer ever seen and a peer later re-found
                                // via gossip is still reported as mDNS-discovered.
                                discovery_sources.remove(&endpoint_id);
                                if events_tx.send(NativeNetworkEvent::MdnsExpired {
                                    endpoint_id,
                                }).await.is_err() {
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                    gossip = gossip_events.try_next() => {
                        match gossip {
                            Ok(Some(GossipEvent::NeighborUp(endpoint_id))) => {
                                let discovery = discovery_sources
                                    .get(&endpoint_id)
                                    .copied()
                                    .unwrap_or(DiscoverySource::Gossip);
                                if events_tx.send(NativeNetworkEvent::NeighborUp {
                                    endpoint_id,
                                    discovery,
                                }).await.is_err() {
                                    break;
                                }
                            }
                            Ok(Some(GossipEvent::NeighborDown(endpoint_id))) => {
                                discovery_sources.remove(&endpoint_id);
                                if events_tx.send(NativeNetworkEvent::NeighborDown {
                                    endpoint_id,
                                }).await.is_err() {
                                    break;
                                }
                            }
                            Ok(Some(GossipEvent::Received(message))) => {
                                if events_tx.send(NativeNetworkEvent::Message {
                                    delivered_from: message.delivered_from,
                                    bytes: message.content.to_vec(),
                                }).await.is_err() {
                                    break;
                                }
                            }
                            Ok(Some(GossipEvent::Lagged)) => {
                                if events_tx.send(NativeNetworkEvent::Lagged).await.is_err() {
                                    break;
                                }
                            }
                            Ok(None) => {
                                let _ = events_tx.send(NativeNetworkEvent::Closed).await;
                                break;
                            }
                            Err(error) => {
                                let _ = events_tx.send(NativeNetworkEvent::Diagnostic(
                                    format!("gossip event stream failed: {error}")
                                )).await;
                                break;
                            }
                        }
                    }
                }
            }
        });

        Ok(Self {
            topic: config.topic,
            router,
            gossip_sender,
            memory_lookup: memory,
            events,
            repairs,
            event_task,
        })
    }

    pub const fn topic(&self) -> RoomTopic {
        self.topic
    }

    pub fn endpoint(&self) -> &Endpoint {
        self.router.endpoint()
    }

    pub fn endpoint_id(&self) -> EndpointId {
        self.endpoint().id()
    }

    /// Current ticket. The relay address appears after Iroh has selected a home
    /// relay; direct addresses are available immediately after bind.
    pub fn ticket(&self) -> NativeRoomTicket {
        NativeRoomTicket::new(self.topic, self.endpoint().addr())
    }

    /// Wait briefly for relay readiness without preventing offline-LAN use.
    pub async fn settle_ticket(&self, timeout: Duration) -> NativeRoomTicket {
        let endpoint = self.endpoint().clone();
        let _ = tokio::time::timeout(timeout, endpoint.online()).await;
        self.ticket()
    }

    /// Add or refresh ticket addressing, then ask gossip to join that peer.
    pub async fn join_ticket(&self, ticket: &NativeRoomTicket) -> Result<(), NativeNetError> {
        if ticket.topic() != self.topic {
            return Err(NativeNetError::Gossip(
                "ticket belongs to a different room topic".into(),
            ));
        }
        // Feed the lookup registered at bind time. Registering a fresh one per
        // join would leak a service on every call: iroh's `add` is a bare push
        // with no dedup and no removal API.
        self.memory_lookup
            .add_endpoint_info(ticket.endpoint_addr().clone());
        self.gossip_sender
            .join_peers(vec![ticket.endpoint_addr().id])
            .await
            .map_err(|error| NativeNetError::Gossip(error.to_string()))
    }

    pub async fn broadcast(&self, bytes: Vec<u8>) -> Result<(), NativeNetError> {
        if bytes.len() > MAX_GOSSIP_MESSAGE_BYTES {
            return Err(NativeNetError::Gossip(format!(
                "message is {} bytes; limit is {MAX_GOSSIP_MESSAGE_BYTES}",
                bytes.len()
            )));
        }
        self.gossip_sender
            .broadcast(bytes.into())
            .await
            .map_err(|error| NativeNetError::Gossip(error.to_string()))
    }

    /// Open an authenticated Iroh connection for one HHHS H6 repair session.
    pub async fn begin_repair(
        &self,
        endpoint_id: EndpointId,
    ) -> Result<Connection, NativeNetError> {
        self.endpoint()
            .connect(endpoint_id, RBSR_ALPN)
            .await
            .map_err(|error| NativeNetError::Gossip(format!("repair connection failed: {error}")))
    }

    pub async fn next_event(&mut self) -> Option<NativeNetworkEvent> {
        self.events.recv().await
    }

    /// Next inbound repair connection. A queue of its own, so a peer opening
    /// repair sessions can never head-of-line block op delivery.
    pub async fn next_repair(&mut self) -> Option<IncomingRepair> {
        self.repairs.recv().await
    }

    /// Whichever of the two queues is ready first.
    ///
    /// The queues stay SEPARATE and separately bounded — that is the whole point
    /// of the repair queue, and merging them would let a peer opening repair
    /// sessions fill the gossip queue and head-of-line block op delivery. This
    /// only spares the caller from borrowing `self` mutably twice inside one
    /// `select!`; it does not couple the producers.
    pub async fn next_inbound(&mut self) -> Option<RoomInbound> {
        tokio::select! {
            repair = self.repairs.recv() => repair.map(RoomInbound::Repair),
            event = self.events.recv() => event.map(RoomInbound::Event),
        }
    }

    pub async fn peer_path(&self, endpoint_id: EndpointId) -> PeerTransportPath {
        classify_peer_path(self.endpoint(), endpoint_id).await
    }

    pub async fn shutdown(self) -> Result<(), NativeNetError> {
        self.event_task.abort();
        self.router
            .shutdown()
            .await
            .map_err(|_| NativeNetError::Closed)
    }
}

#[derive(Debug, Clone)]
struct RepairProtocol {
    repairs: mpsc::Sender<IncomingRepair>,
}

impl ProtocolHandler for RepairProtocol {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let endpoint_id = connection.remote_id();
        // Take the bi-stream HERE, in the per-connection handler task, rather
        // than making the room loop await it: `Transport::next_event` hands out
        // a ready stream, and a peer that dials without opening one wastes only
        // its own connection.
        let stream = match tokio::time::timeout(
            REPAIR_ACCEPT_TIMEOUT,
            IrohSyncStream::accept(&connection),
        )
        .await
        {
            Ok(Ok(stream)) => stream,
            Ok(Err(_)) | Err(_) => {
                connection.close(3u32.into(), b"no repair stream");
                return Ok(());
            }
        };
        // Reject rather than queue when the app is already saturated: a dropped
        // dial is recoverable, an unbounded backlog of live QUIC state is not.
        // Note this returns as soon as the connection is handed off, so the
        // router's graceful shutdown does not cover the session itself — see
        // `shutdown`'s note.
        match self.repairs.try_send(IncomingRepair {
            endpoint_id,
            connection,
            stream,
        }) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(repair)) => {
                repair.connection.close(1u32.into(), b"repair queue full");
                Ok(())
            }
            Err(mpsc::error::TrySendError::Closed(repair)) => {
                repair.connection.close(2u32.into(), b"shutting down");
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The pluggable transport seam.
// ---------------------------------------------------------------------------

/// The iroh backend behind the transport-neutral seam, so the production app
/// drives exactly the same [`crate::net::sync`] code the loopback tests do.
///
/// # Why `next_event` is a `select!` and not a merged channel
///
/// Gossip and repair keep the SEPARATE bounded queues they were built with.
/// Folding `SyncRequested` into the gossip queue would undo the reason the
/// repair queue exists: a peer opening repair sessions could fill the shared
/// queue and head-of-line block op delivery, and a slow consumer of one would
/// stall the other's producer. Selecting over two channels keeps the producers
/// independent — a saturated repair queue still closes connections via
/// `try_send` and never touches gossip capacity.
impl Transport for NativeRoomNetwork {
    type Stream = IrohSyncStream;

    fn mode(&self) -> TransportMode {
        TransportMode::Iroh
    }

    fn max_broadcast_bytes(&self) -> usize {
        MAX_GOSSIP_MESSAGE_BYTES
    }

    async fn broadcast(&self, frame: Vec<u8>) -> Result<(), TransportError> {
        let length = frame.len();
        NativeRoomNetwork::broadcast(self, frame)
            .await
            .map_err(|error| match error {
                NativeNetError::Gossip(_) if length > MAX_GOSSIP_MESSAGE_BYTES => {
                    TransportError::FrameTooLarge {
                        actual: length,
                        limit: MAX_GOSSIP_MESSAGE_BYTES,
                    }
                }
                other => TransportError::Backend(other.to_string()),
            })
    }

    async fn next_event(&mut self) -> Option<TransportEvent<Self::Stream>> {
        let native = match self.next_inbound().await? {
            RoomInbound::Repair(repair) => {
                // The connection rides along on the stream so QUIC state
                // outlives the handler task that accepted it.
                return Some(TransportEvent::SyncRequested {
                    peer: peer_of(repair.endpoint_id),
                    stream: repair.stream.owning(repair.connection),
                });
            }
            RoomInbound::Event(event) => event,
        };
        // Discovery-only events have no seam equivalent; they are surfaced as
        // diagnostics rather than dropped, so a backend swap does not lose
        // observability.
        Some(match native {
                NativeNetworkEvent::NeighborUp {
                    endpoint_id,
                    discovery,
                } => TransportEvent::PeerUp {
                    peer: peer_of(endpoint_id),
                    discovery,
                },
                NativeNetworkEvent::NeighborDown { endpoint_id } => TransportEvent::PeerDown {
                    peer: peer_of(endpoint_id),
                },
                NativeNetworkEvent::Message {
                    delivered_from,
                    bytes,
                } => TransportEvent::Message {
                    from: peer_of(delivered_from),
                    bytes,
                },
                NativeNetworkEvent::Lagged => TransportEvent::Lagged,
                NativeNetworkEvent::Closed => TransportEvent::Closed,
                NativeNetworkEvent::Diagnostic(message) => TransportEvent::Diagnostic(message),
                NativeNetworkEvent::MdnsDiscovered { endpoint_id } => {
                    TransportEvent::Diagnostic(format!("mdns discovered {endpoint_id}"))
                }
                NativeNetworkEvent::MdnsExpired { endpoint_id } => {
                    TransportEvent::Diagnostic(format!("mdns expired {endpoint_id}"))
                }
            })
    }

    async fn open_sync(&self, peer: PeerId) -> Result<Self::Stream, TransportError> {
        let endpoint_id = EndpointId::from_bytes(peer.as_bytes())
            .map_err(|_| TransportError::Unreachable(peer.to_hex()))?;
        let connection = self
            .begin_repair(endpoint_id)
            .await
            .map_err(|error| TransportError::Backend(error.to_string()))?;
        // The connection must outlive this call: dropping the last handle closes
        // it out from under the stream we just opened.
        Ok(IrohSyncStream::open(&connection).await?.owning(connection))
    }

    async fn peer_path(&self, peer: PeerId) -> PeerPath {
        let Ok(endpoint_id) = EndpointId::from_bytes(peer.as_bytes()) else {
            return PeerPath::Disconnected;
        };
        NativeRoomNetwork::peer_path(self, endpoint_id).await.into()
    }

    async fn shutdown(self) -> Result<(), TransportError> {
        NativeRoomNetwork::shutdown(self)
            .await
            .map_err(|error| TransportError::Backend(error.to_string()))
    }
}

// NOTE: there is deliberately no `From<EndpointId> for AuthorId`.
//
// It is sound only for the local participant (identity.rs derives both keys from
// one seed), but under Plumtree relaying `Message::delivered_from` is the
// FORWARDER, not the author — and a blanket conversion makes
// `delivered_from.into()` compile into a plausible-looking authorship claim.
// Authorship comes from `verify_signed_op_for_topic` and nothing else.

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercises the same direct UDP + gossip path used by offline LAN rooms.
    /// It is ignored in the default suite because some CI sandboxes disable
    /// multicast or UDP entirely; the native acceptance job runs it explicitly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires local UDP sockets and mDNS-capable interfaces"]
    async fn two_offline_endpoints_exchange_gossip_over_direct_ip() {
        use crate::{
            net::RelayPolicy,
            room::{
                ops::{SignedOp, WalkieOp, signing_key_from_seed, verify_signed_op_for_topic},
                store::RoomStore,
            },
            tuning::{TunedDegree, Tuning},
        };

        let topic = RoomTopic::from_room_name("quiet-cactus-song");
        let config = NativeRoomNetworkConfig {
            topic,
            relay: RelayPolicy::Disabled,
            bootstrap: None,
        };
        let mut first = NativeRoomNetwork::bind(iroh::SecretKey::from_bytes(&[21; 32]), config)
            .await
            .unwrap();
        let mut second = NativeRoomNetwork::bind(
            iroh::SecretKey::from_bytes(&[22; 32]),
            NativeRoomNetworkConfig {
                topic,
                relay: RelayPolicy::Disabled,
                bootstrap: None,
            },
        )
        .await
        .unwrap();
        let mut other_room = NativeRoomNetwork::bind(
            iroh::SecretKey::from_bytes(&[23; 32]),
            NativeRoomNetworkConfig {
                topic: RoomTopic::from_room_name("different-cactus-song"),
                relay: RelayPolicy::Disabled,
                bootstrap: None,
            },
        )
        .await
        .unwrap();

        assert_eq!(
            wait_for_neighbor(&mut second, first.endpoint_id()).await,
            DiscoverySource::Mdns
        );
        assert_eq!(
            wait_for_neighbor(&mut first, second.endpoint_id()).await,
            DiscoverySource::Mdns
        );
        assert_no_mdns_discovery(
            &mut other_room,
            &[first.endpoint_id(), second.endpoint_id()],
        )
        .await;
        let tuning = Tuning::twelve_tet();
        let degree = TunedDegree::new(&tuning, 7).unwrap();
        let mut second_store = RoomStore::new();
        let signed = second_store.commit(
            &signing_key_from_seed(&[22; 32]),
            &topic.to_string(),
            1,
            WalkieOp::AddDegree { pitch: degree },
        );
        second
            .broadcast(signed.to_wire_bytes().unwrap())
            .await
            .unwrap();

        let delivered = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(NativeNetworkEvent::Message { bytes, .. }) = first.next_event().await {
                    let signed = SignedOp::from_wire_bytes(&bytes).unwrap();
                    break verify_signed_op_for_topic(&signed, &topic.to_string()).unwrap();
                }
            }
        })
        .await
        .expect("direct gossip message timed out");
        let mut first_store = RoomStore::new();
        first_store.ingest_verified(delivered);
        assert!(first_store.view().pitches.contains(&degree));
        wait_for_path(&first, second.endpoint_id(), PeerTransportPath::Direct).await;

        second.shutdown().await.unwrap();
        first.shutdown().await.unwrap();
        other_room.shutdown().await.unwrap();
    }

    async fn wait_for_neighbor(
        network: &mut NativeRoomNetwork,
        expected: EndpointId,
    ) -> DiscoverySource {
        let result = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match network.next_event().await {
                    Some(NativeNetworkEvent::NeighborUp {
                        endpoint_id,
                        discovery,
                    }) if endpoint_id == expected => break discovery,
                    Some(NativeNetworkEvent::MdnsDiscovered { endpoint_id })
                        if endpoint_id != expected =>
                    {
                        panic!("room-scoped mDNS leaked unrelated endpoint {endpoint_id}");
                    }
                    _ => {}
                }
            }
        })
        .await;
        result.unwrap_or_else(|_| panic!("neighbor {expected} did not connect"))
    }

    async fn assert_no_mdns_discovery(network: &mut NativeRoomNetwork, forbidden: &[EndpointId]) {
        let _ = tokio::time::timeout(Duration::from_millis(750), async {
            loop {
                if let Some(NativeNetworkEvent::MdnsDiscovered { endpoint_id }) =
                    network.next_event().await
                    && forbidden.contains(&endpoint_id)
                {
                    panic!("room-scoped mDNS leaked unrelated endpoint {endpoint_id}");
                }
            }
        })
        .await;
    }

    async fn wait_for_path(
        network: &NativeRoomNetwork,
        endpoint_id: EndpointId,
        expected: PeerTransportPath,
    ) {
        let result = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let actual = network.peer_path(endpoint_id).await;
                if actual == expected {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await;
        result.unwrap_or_else(|_| panic!("peer {endpoint_id} did not reach {expected:?}"));
    }
}
