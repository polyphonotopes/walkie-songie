//! Browser Iroh room transport (wasm32, relay fallback + WebRTC direct).
//!
//! The exact protocol surface of [`super::native`] — the same ALPNs, gossip
//! topic derivation, ticket format, and HHHS repair protocol — compiled to
//! `wasm32-unknown-unknown`. What differs is the runtime and the reachability:
//!
//! * No mDNS: browsers have no UDP. Peers meet through the relay via ticket
//!   bootstrap (`MemoryLookup`) or gossip.
//! * No native IP carrier: iroh's wasm build tunnels QUIC over the relay's
//!   WebSocket, while [`super::webrtc_transport`] supplies an optional custom
//!   direct carrier. [`BrowserNetHandle::peer_path`] combines WebRTC readiness
//!   with Iroh's active-address report instead of making the UI infer either.
//! * `n0-future` supplies spawn/sleep/timeout where native uses tokio, and the
//!   queues are `futures` channels — everything here is single-threaded and
//!   `!Send` by construction.

use std::{sync::Arc, time::Duration};

use futures::{SinkExt, StreamExt, TryStreamExt, channel::mpsc};
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

use super::SyncTimer;
use super::iroh_common::{
    EVENT_QUEUE_DEPTH, MAX_GOSSIP_MESSAGE_BYTES, NativeNetError, NativeNetworkEvent,
    NativeRoomTicketV5, PeerTransportPath, REPAIR_ACCEPT_TIMEOUT, REPAIR_QUEUE_DEPTH, RelayPolicy,
    ReplicaRoomNetworkConfig, RoomTopic, classify_peer_path,
};
use super::repair::IrohSyncStream;
use crate::client::DiscoverySource;

#[derive(Debug, Clone)]
struct ReplicaTicketMetadata {
    room: crate::room::v5::RoomIdentity,
    owner: crate::room::v5::ActorId,
    support: crate::room::v5::ProtocolSupport,
}

/// The browser runtime's clock for [`SyncTimer`]: `n0-future`'s wasm sleep
/// (a `setTimeout` under the hood).
#[derive(Debug, Clone, Copy, Default)]
pub struct BrowserTimer;

impl SyncTimer for BrowserTimer {
    async fn sleep(&self, duration: Duration) {
        n0_future::time::sleep(duration).await
    }
}

/// An inbound repair connection, delivered on its own queue. Browser twin of
/// `native::IncomingRepair`: the bi-stream is already accepted inside the
/// protocol handler, and `connection` keeps the QUIC state alive.
#[derive(Debug)]
pub struct BrowserIncomingRepair {
    pub endpoint_id: EndpointId,
    pub alpn: &'static [u8],
    pub connection: Connection,
    pub stream: IrohSyncStream,
}

/// One item from either of the room's two inbound queues.
#[derive(Debug)]
pub enum BrowserRoomInbound {
    Event(NativeNetworkEvent),
    Repair(BrowserIncomingRepair),
}

/// The shareable half of the browser room network: every operation that only
/// needs `&self` and no queue. Cheap to clone (`Endpoint`, `GossipSender`, and
/// `MemoryLookup` are all handles), so the room loop, commit path, and repair
/// dialer can each hold their own copy while the inbound queues live in one
/// place.
#[derive(Debug, Clone)]
pub struct BrowserNetHandle {
    topic: RoomTopic,
    endpoint: Endpoint,
    gossip_sender: GossipSender,
    memory_lookup: MemoryLookup,
    /// Signaling hooks for the WebRTC custom transport (M4 direct peering). Handed
    /// to the rendezvous loop via [`Self::rendezvous_peering`], which pumps the
    /// SDP/ICE handshake over the existing signaling channel.
    webrtc: super::webrtc_transport::WebRtcSignalPort,
    replica_ticket: Option<ReplicaTicketMetadata>,
}

impl BrowserNetHandle {
    pub const fn topic(&self) -> RoomTopic {
        self.topic
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    pub fn endpoint_id(&self) -> EndpointId {
        self.endpoint.id()
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
            self.endpoint.addr(),
        )
    }

    pub async fn settle_ticket(&self, timeout: Duration) -> NativeRoomTicketV5 {
        let _ = n0_future::time::timeout(timeout, self.endpoint.online()).await;
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
        self.endpoint
            .connect(endpoint_id, alpn)
            .await
            .map_err(|error| {
                NativeNetError::Gossip(format!(
                    "{} connection failed: {error}",
                    String::from_utf8_lossy(alpn)
                ))
            })
    }

    pub async fn peer_path(&self, endpoint_id: EndpointId) -> PeerTransportPath {
        if self.webrtc.is_connected(endpoint_id.as_bytes()) {
            return PeerTransportPath::Direct;
        }
        classify_peer_path(&self.endpoint, endpoint_id).await
    }

    /// The iroh handles the topic rendezvous needs to peer discovered ids —
    /// the same `add_endpoint_info` + `join_peers` primitives `join_ticket` uses.
    pub fn rendezvous_peering(&self) -> super::rendezvous::RendezvousPeering {
        super::rendezvous::RendezvousPeering {
            endpoint: self.endpoint.clone(),
            gossip_sender: self.gossip_sender.clone(),
            memory_lookup: self.memory_lookup.clone(),
            webrtc: Some(self.webrtc.clone()),
        }
    }
}

/// A bound browser Iroh endpoint and its active gossip topic.
pub struct BrowserRoomNetwork {
    handle: BrowserNetHandle,
    router: Router,
    events: mpsc::Receiver<NativeNetworkEvent>,
    repairs: mpsc::Receiver<BrowserIncomingRepair>,
    event_task: n0_future::task::JoinHandle<()>,
}

impl BrowserRoomNetwork {
    /// Bind an endpoint using one persistent identity seed.
    ///
    /// Identical to `NativeRoomNetwork::bind` minus the mDNS lookup; the
    /// `presets::N0` builder compiles the IP transport out on wasm by itself.
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
            Some(metadata),
        )
        .await
    }

    async fn bind_inner(
        secret_key: iroh::SecretKey,
        topic: RoomTopic,
        relay: RelayPolicy,
        bootstrap_address: Option<iroh::EndpointAddr>,
        alpns: Vec<&'static [u8]>,
        repair_alpns: Vec<&'static [u8]>,
        replica_ticket: Option<ReplicaTicketMetadata>,
    ) -> Result<Self, NativeNetError> {
        let relay_mode = relay.to_iroh()?;
        let memory = MemoryLookup::new();
        if let Some(bootstrap) = bootstrap_address.clone() {
            memory.add_endpoint_info(bootstrap);
        }
        let (mut events_tx, events) = mpsc::channel(EVENT_QUEUE_DEPTH);
        let (repairs_tx, repairs) = mpsc::channel(REPAIR_QUEUE_DEPTH);

        // WebRTC custom transport (M4 direct peering). Built BEFORE the endpoint so
        // it can be registered on the builder; the endpoint id is the public half of
        // the secret key, known here without consuming it. The relay path is
        // untouched — this only ADDS a candidate direct path, which iroh's default
        // path selector (custom = primary, relay = backup) prefers once it is up,
        // and falls back off automatically if it never connects. The signaling port
        // rides to the rendezvous loop via `rendezvous_peering`.
        let local_id = *secret_key.public().as_bytes();
        let (webrtc_transport, webrtc_port) =
            super::webrtc_transport::WebRtcTransport::new(local_id);
        let mut direct_ready = webrtc_port.subscribe_ready();

        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(secret_key)
            .alpns(alpns.into_iter().map(<[u8]>::to_vec).collect())
            .relay_mode(relay_mode)
            .add_custom_transport(Arc::new(webrtc_transport))
            .clear_address_lookup()
            .address_lookup(memory.clone())
            // Re-add iroh's built-in pkarr discovery that `presets::N0` installs
            // and `clear_address_lookup` just stripped: publish/resolve
            // endpoint-id -> relay-url over HTTP (fully wasm-capable; n0's dns
            // server sends `Access-Control-Allow-Origin: *`). Without a resolver,
            // a known node id whose address isn't already cached is undialable
            // -> gossip's "No addressing information available". Default
            // `AddrFilter::relay_only()` is exactly what the built-in browser
            // carrier wants; WebRTC direct addresses arrive separately through
            // the custom transport, so do not widen this filter.
            .address_lookup(PkarrPublisher::n0_dns())
            .address_lookup(PkarrResolver::n0_dns())
            .bind()
            .await
            .map_err(|error| NativeNetError::Bind(error.to_string()))?;

        let gossip = Gossip::builder()
            .max_message_size(MAX_GOSSIP_MESSAGE_BYTES)
            .spawn(endpoint.clone());
        let mut router = Router::builder(endpoint.clone()).accept(GOSSIP_ALPN, gossip.clone());
        for alpn in repair_alpns {
            router = router.accept(
                alpn,
                BrowserRepairProtocol {
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
        let bootstrap_ids: Vec<EndpointId> = bootstrap.clone();
        let gossip_topic = gossip
            .subscribe(topic.gossip_topic(), bootstrap)
            .await
            .map_err(|error| NativeNetError::Gossip(error.to_string()))?;
        let (gossip_sender, mut gossip_events) = gossip_topic.split();

        // No mDNS branch in a browser, so the event task is a single stream
        // pump: gossip events in, `NativeNetworkEvent`s out. Bootstrap peers
        // keep their `Ticket` attribution; everyone else arrived via gossip.
        let event_task = n0_future::task::spawn(async move {
            loop {
                let gossip_next = gossip_events.try_next();
                let direct_next = direct_ready.recv();
                futures::pin_mut!(gossip_next, direct_next);
                match futures::future::select(gossip_next, direct_next).await {
                    futures::future::Either::Right((Ok(ready), _)) => {
                        let Ok(endpoint_id) = EndpointId::from_bytes(&ready.peer) else {
                            continue;
                        };
                        tracing::info!(
                            target: "walkie::webrtc",
                            peer = %endpoint_id,
                            attempt = %ready.attempt,
                            "publishing fresh direct carrier readiness"
                        );
                        if events_tx
                            .send(NativeNetworkEvent::DirectReady { endpoint_id })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    futures::future::Either::Right((
                        Err(async_broadcast::RecvError::Overflowed(_)),
                        _,
                    )) => continue,
                    futures::future::Either::Right((
                        Err(async_broadcast::RecvError::Closed),
                        _,
                    )) => {
                        let _ = events_tx
                            .send(NativeNetworkEvent::Diagnostic(
                                "WebRTC direct-ready stream closed".into(),
                            ))
                            .await;
                        break;
                    }
                    futures::future::Either::Left((
                        Ok(Some(GossipEvent::NeighborUp(endpoint_id))),
                        _,
                    )) => {
                        let discovery = if bootstrap_ids.contains(&endpoint_id) {
                            DiscoverySource::Ticket
                        } else {
                            DiscoverySource::Gossip
                        };
                        if events_tx
                            .send(NativeNetworkEvent::NeighborUp {
                                endpoint_id,
                                discovery,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    futures::future::Either::Left((
                        Ok(Some(GossipEvent::NeighborDown(endpoint_id))),
                        _,
                    )) => {
                        if events_tx
                            .send(NativeNetworkEvent::NeighborDown { endpoint_id })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    futures::future::Either::Left((
                        Ok(Some(GossipEvent::Received(message))),
                        _,
                    )) => {
                        if events_tx
                            .send(NativeNetworkEvent::Message {
                                delivered_from: message.delivered_from,
                                bytes: message.content.to_vec(),
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    futures::future::Either::Left((Ok(Some(GossipEvent::Lagged)), _)) => {
                        if events_tx.send(NativeNetworkEvent::Lagged).await.is_err() {
                            break;
                        }
                    }
                    futures::future::Either::Left((Ok(None), _)) => {
                        let _ = events_tx.send(NativeNetworkEvent::Closed).await;
                        break;
                    }
                    futures::future::Either::Left((Err(error), _)) => {
                        let _ = events_tx
                            .send(NativeNetworkEvent::Diagnostic(format!(
                                "gossip event stream failed: {error}"
                            )))
                            .await;
                        break;
                    }
                }
            }
        });

        Ok(Self {
            handle: BrowserNetHandle {
                topic,
                endpoint,
                gossip_sender,
                memory_lookup: memory,
                webrtc: webrtc_port,
                replica_ticket,
            },
            router,
            events,
            repairs,
            event_task,
        })
    }

    /// A cheap clone of the `&self` operations (broadcast, dial, tickets),
    /// usable while `next_inbound` holds `&mut self` elsewhere.
    pub fn handle(&self) -> BrowserNetHandle {
        self.handle.clone()
    }

    pub const fn topic(&self) -> RoomTopic {
        self.handle.topic()
    }

    pub fn endpoint_id(&self) -> EndpointId {
        self.handle.endpoint_id()
    }

    pub async fn next_event(&mut self) -> Option<NativeNetworkEvent> {
        self.events.next().await
    }

    pub async fn next_repair(&mut self) -> Option<BrowserIncomingRepair> {
        self.repairs.next().await
    }

    /// Whichever of the two queues is ready first. The queues stay SEPARATE
    /// and separately bounded for the same head-of-line reasons as native.
    pub async fn next_inbound(&mut self) -> Option<BrowserRoomInbound> {
        futures::select! {
            repair = self.repairs.next() => repair.map(BrowserRoomInbound::Repair),
            event = self.events.next() => event.map(BrowserRoomInbound::Event),
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
struct BrowserRepairProtocol {
    repairs: mpsc::Sender<BrowserIncomingRepair>,
    alpn: &'static [u8],
}

impl ProtocolHandler for BrowserRepairProtocol {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let endpoint_id = connection.remote_id();
        // Take the bi-stream HERE, in the per-connection handler task, exactly
        // as native does — a peer that dials without opening one wastes only
        // its own connection.
        let stream = match n0_future::time::timeout(
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
        // Reject rather than queue when saturated; a dropped dial is
        // recoverable, an unbounded backlog of live QUIC state is not.
        let mut repairs = self.repairs.clone();
        match repairs.try_send(BrowserIncomingRepair {
            endpoint_id,
            alpn: self.alpn,
            connection,
            stream,
        }) {
            Ok(()) => Ok(()),
            Err(refused) => {
                let reason: &[u8] = if refused.is_full() {
                    b"repair queue full"
                } else {
                    b"shutting down"
                };
                refused.into_inner().connection.close(1u32.into(), reason);
                Ok(())
            }
        }
    }
}
