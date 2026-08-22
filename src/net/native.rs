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
    address_lookup::{MemoryLookup, PkarrPublisher, PkarrResolver},
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
    NativeRoomTicketV5, PeerTransportPath, REPAIR_ACCEPT_TIMEOUT, REPAIR_QUEUE_DEPTH, RelayPolicy,
    ReplicaRoomNetworkConfig, RoomTopic, classify_peer_path, room_mdns_service_name_v5,
};
use super::repair::IrohSyncStream;
use crate::client::DiscoverySource;
use crate::room::v5::ProtocolSupport;

#[derive(Clone)]
struct ReplicaTicketMetadata {
    room: crate::room::v5::RoomIdentity,
    owner: crate::room::v5::ActorId,
    support: ProtocolSupport,
}

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
    replica_ticket: Option<ReplicaTicketMetadata>,
}

/// One item from either of the room's two independent inbound queues.
#[derive(Debug)]
pub enum RoomInbound {
    Event(NativeNetworkEvent),
    Repair(Box<IncomingRepair>),
}

/// An inbound lane-protocol connection, delivered on its own queue.
///
/// The bi-stream is already accepted: the wait happens inside the per-connection
/// protocol handler, so a peer that dials and stalls cannot delay anything the
/// room loop is doing. `connection` is retained to keep the QUIC state alive for
/// as long as the stream is in use.
#[derive(Debug)]
pub struct IncomingRepair {
    pub endpoint_id: EndpointId,
    /// The ALPN the connection was accepted under — both purpose and lane tag.
    /// It is the ONLY place a repair connection names its lane, so the
    /// responder dispatches on this to pick the lane handler.
    pub alpn: &'static [u8],
    pub connection: Connection,
    pub stream: IrohSyncStream,
}

impl NativeRoomNetwork {
    /// Bind a capability-native Room-v5 endpoint. Protocol registration follows
    /// local support and contains repair ALPNs only.
    pub async fn bind(
        secret_key: iroh::SecretKey,
        config: ReplicaRoomNetworkConfig,
    ) -> Result<Self, NativeNetError> {
        let topic = config.topic();
        let mut alpns = vec![GOSSIP_ALPN];
        let mut repair_alpns = Vec::new();
        for lane in [
            crate::room::v5::RoomLane::Music,
            crate::room::v5::RoomLane::Extension,
        ] {
            if config.support.supports(lane) {
                alpns.push(lane.repair_alpn());
                repair_alpns.push(lane.repair_alpn());
            }
        }
        let metadata = ReplicaTicketMetadata {
            room: config.room,
            owner: config.owner,
            support: config.support,
        };
        Self::bind_inner(
            secret_key,
            topic,
            config.relay,
            config.bootstrap,
            alpns,
            repair_alpns,
            room_mdns_service_name_v5(topic),
            Some(metadata),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn bind_inner(
        secret_key: iroh::SecretKey,
        topic: RoomTopic,
        relay: RelayPolicy,
        bootstrap_address: Option<iroh::EndpointAddr>,
        alpns: Vec<&'static [u8]>,
        repair_alpns: Vec<&'static [u8]>,
        mdns_service: String,
        replica_ticket: Option<ReplicaTicketMetadata>,
    ) -> Result<Self, NativeNetError> {
        let relay_mode = relay.to_iroh()?;
        let memory = MemoryLookup::new();
        if let Some(bootstrap) = bootstrap_address.clone() {
            memory.add_endpoint_info(bootstrap);
        }
        let (events_tx, events) = mpsc::channel(EVENT_QUEUE_DEPTH);
        let (repairs_tx, repairs) = mpsc::channel(REPAIR_QUEUE_DEPTH);

        // N0 supplies the native IP and relay transports. Address lookup is
        // deliberately rebuilt from memory + room-scoped mDNS below.
        // The registered ALPN set is this peer's live lane-capability
        // declaration. An unsupported lane fails at QUIC
        // negotiation before any lane frame.
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(secret_key)
            .alpns(alpns.into_iter().map(<[u8]>::to_vec).collect())
            .relay_mode(relay_mode)
            .clear_address_lookup()
            .address_lookup(memory.clone())
            // pkarr id->relay-url discovery (see browser.rs). Resolves peers
            // learned by id alone (ticket/gossip/rendezvous) without a cached
            // address, and gives native<->browser plus cross-network
            // native<->native reach that mDNS (LAN-only, added just below)
            // can't. Relay-only filter by default, same as the browser.
            .address_lookup(PkarrPublisher::n0_dns())
            .address_lookup(PkarrResolver::n0_dns())
            .bind()
            .await
            .map_err(|error| NativeNetError::Bind(error.to_string()))?;

        let mdns = MdnsAddressLookup::builder()
            .service_name(mdns_service)
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
        // One accept handler per lane protocol, each stamping the queued
        // connection with its authoritative ALPN. All four feed the same
        // bounded lane queue; gossip remains separately bounded.
        let mut router = Router::builder(endpoint.clone()).accept(GOSSIP_ALPN, gossip.clone());
        for alpn in repair_alpns {
            router = router.accept(
                alpn,
                RepairProtocol {
                    repairs: repairs_tx.clone(),
                    alpn,
                },
            );
        }
        let router = router.spawn();

        let bootstrap = bootstrap_address
            .as_ref()
            .map(|address| vec![address.id])
            .unwrap_or_default();
        let mut discovery_sources: HashMap<EndpointId, DiscoverySource> = bootstrap
            .iter()
            .copied()
            .map(|endpoint_id| (endpoint_id, DiscoverySource::Ticket))
            .collect();
        let gossip_topic = gossip
            .subscribe(topic.gossip_topic(), bootstrap)
            .await
            .map_err(|error| NativeNetError::Gossip(error.to_string()))?;
        let (gossip_sender, mut gossip_events) = gossip_topic.split();
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
            topic,
            router,
            gossip_sender,
            memory_lookup: memory,
            events,
            repairs,
            event_task,
            replica_ticket,
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

    pub fn ticket(&self) -> NativeRoomTicketV5 {
        let metadata = self
            .replica_ticket
            .as_ref()
            .expect("Room-v5 endpoint always carries ticket metadata");
        NativeRoomTicketV5::new(
            &metadata.room,
            metadata.owner,
            metadata.support,
            self.endpoint().addr(),
        )
    }

    pub async fn settle_ticket(&self, timeout: Duration) -> NativeRoomTicketV5 {
        let endpoint = self.endpoint().clone();
        let _ = tokio::time::timeout(timeout, endpoint.online()).await;
        self.ticket()
    }

    pub async fn join_ticket(&self, ticket: &NativeRoomTicketV5) -> Result<(), NativeNetError> {
        let metadata = self
            .replica_ticket
            .as_ref()
            .expect("Room-v5 endpoint always carries ticket metadata");
        if ticket.topic() != self.topic
            || ticket.room_identity() != metadata.room
            || ticket.owner() != metadata.owner
        {
            return Err(NativeNetError::Gossip(
                "ticket belongs to a different Room-v5 object or owner".into(),
            ));
        }
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

    pub async fn begin_replica(
        &self,
        endpoint_id: EndpointId,
        lane: crate::room::v5::RoomLane,
    ) -> Result<Connection, NativeNetError> {
        let alpn = lane.repair_alpn();
        self.endpoint()
            .connect(endpoint_id, alpn)
            .await
            .map_err(|error| {
                NativeNetError::Gossip(format!(
                    "{} connection failed: {error}",
                    String::from_utf8_lossy(alpn)
                ))
            })
    }

    pub async fn next_event(&mut self) -> Option<NativeNetworkEvent> {
        self.events.recv().await
    }

    /// Next inbound lane-protocol connection. A queue of its own, so a peer
    /// opening repair sessions cannot head-of-line block gossip delivery.
    pub async fn next_repair(&mut self) -> Option<IncomingRepair> {
        self.repairs.recv().await
    }

    /// Whichever of the two queues is ready first.
    ///
    /// The queues stay SEPARATE and separately bounded — that is the whole point
    /// of the lane queue, and merging them would let a peer opening lane
    /// sessions fill the gossip queue and head-of-line block op delivery. This
    /// only spares the caller from borrowing `self` mutably twice inside one
    /// `select!`; it does not couple the producers.
    pub async fn next_inbound(&mut self) -> Option<RoomInbound> {
        tokio::select! {
            repair = self.repairs.recv() => repair.map(Box::new).map(RoomInbound::Repair),
            event = self.events.recv() => event.map(RoomInbound::Event),
        }
    }

    pub async fn peer_path(&self, endpoint_id: EndpointId) -> PeerTransportPath {
        classify_peer_path(self.endpoint(), endpoint_id).await
    }

    /// The iroh handles the topic rendezvous needs to peer discovered ids —
    /// the same `add_endpoint_info` + `join_peers` primitives `join_ticket` uses.
    pub fn rendezvous_peering(&self) -> super::rendezvous::RendezvousPeering {
        super::rendezvous::RendezvousPeering {
            endpoint: self.endpoint().clone(),
            gossip_sender: self.gossip_sender.clone(),
            memory_lookup: self.memory_lookup.clone(),
        }
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
    /// The lane ALPN this handler is registered under.
    alpn: &'static [u8],
}

impl ProtocolHandler for RepairProtocol {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let endpoint_id = connection.remote_id();
        // Take the bi-stream HERE, in the per-connection handler task, rather
        // than making the room loop await it: `Transport::next_event` hands out
        // a ready stream, and a peer that dials without opening one wastes only
        // its own connection.
        let stream =
            match tokio::time::timeout(REPAIR_ACCEPT_TIMEOUT, IrohSyncStream::accept(&connection))
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
            alpn: self.alpn,
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
