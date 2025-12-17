//! Iroh-gossip based signaller for matchbox WebRTC connections.
//!
//! Uses iroh-gossip for peer discovery and presence, with direct messages
//! for exchanging WebRTC offers/answers.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures::FutureExt;
use iroh::{protocol::Router, Endpoint, PublicKey};
use iroh_gossip::{
    net::{Event, Gossip, GossipEvent, GossipReceiver, GossipSender, GOSSIP_ALPN},
    proto::TopicId,
};
use matchbox_socket::{
    PeerEvent, PeerId, PeerRequest, SignalingError, Signaller, SignallerBuilder,
};
use n0_future::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use web_time::Instant;

use crate::net::direct_message::{send_direct_message, DirectMessageHandler, DIRECT_MESSAGE_ALPN};

/// Message broadcast via gossip to announce presence and ID mapping.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GossipPresence {
    /// Our matchbox peer ID
    matchbox_id: PeerId,
    /// Our iroh node ID
    iroh_id: PublicKey,
    /// Timestamp for deduplication
    timestamp_ms: u64,
}

/// Builder for creating iroh-gossip based signallers.
#[derive(Clone)]
pub struct IrohSignallerBuilder {
    endpoint: Endpoint,
    gossip: Gossip,
    matchbox_id: PeerId,
    iroh_id: PublicKey,
    dm_receiver: async_broadcast::InactiveReceiver<(PublicKey, PeerEvent)>,
    _router: Arc<Router>,
}

impl std::fmt::Debug for IrohSignallerBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IrohSignallerBuilder")
            .field("matchbox_id", &self.matchbox_id)
            .field("iroh_id", &self.iroh_id)
            .finish()
    }
}

impl IrohSignallerBuilder {
    /// Create a new signaller builder, initializing the iroh endpoint and gossip.
    pub async fn new() -> Result<Self> {
        info!("Initializing iroh endpoint...");

        let endpoint = Endpoint::builder()
            .discovery_n0()
            .alpns(vec![DIRECT_MESSAGE_ALPN.to_vec(), GOSSIP_ALPN.to_vec()])
            .bind()
            .await?;

        let iroh_id = endpoint.node_id();
        let matchbox_id = PeerId(uuid::Uuid::new_v4());

        info!("Iroh node ID: {iroh_id}");
        info!("Matchbox peer ID: {matchbox_id}");

        let gossip = Gossip::builder().spawn(endpoint.clone()).await?;

        // Set up direct message channel
        let (mut dm_sender, dm_receiver) = async_broadcast::broadcast(2048);
        dm_sender.set_overflow(true);
        let dm_receiver = dm_receiver.deactivate();

        // Set up protocol router
        let router = Router::builder(endpoint.clone())
            .accept(GOSSIP_ALPN, gossip.clone())
            .accept(DIRECT_MESSAGE_ALPN, DirectMessageHandler::new(dm_sender))
            .spawn();

        Ok(Self {
            endpoint,
            gossip,
            matchbox_id,
            iroh_id,
            dm_receiver,
            _router: Arc::new(router),
        })
    }

    /// Get our iroh node ID (for sharing with others to connect).
    pub fn iroh_id(&self) -> PublicKey {
        self.iroh_id
    }

    /// Get our matchbox peer ID.
    pub fn matchbox_id(&self) -> PeerId {
        self.matchbox_id
    }
}

#[cfg_attr(not(target_arch = "wasm32"), matchbox_socket::async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", matchbox_socket::async_trait::async_trait(?Send))]
impl SignallerBuilder for IrohSignallerBuilder {
    async fn new_signaller(
        &self,
        attempts: Option<u16>,
        room_url: String,
    ) -> Result<Box<dyn Signaller>, SignalingError> {
        // Parse room_url as an optional bootstrap peer
        let bootstrap_peer: Option<PublicKey> = if room_url.is_empty() {
            None
        } else {
            Some(room_url.parse().map_err(|e| {
                SignalingError::UserImplementationError(format!("Invalid room URL: {e}"))
            })?)
        };

        for attempt in 0..attempts.unwrap_or(3) {
            match self.try_create_signaller(bootstrap_peer).await {
                Ok(signaller) => return Ok(Box::new(signaller)),
                Err(e) => {
                    warn!("Signaller creation attempt {attempt} failed: {e:#}");
                    if attempt == attempts.unwrap_or(3) - 1 {
                        return Err(SignalingError::UserImplementationError(format!("{e:#}")));
                    }
                }
            }
        }
        unreachable!()
    }
}

impl IrohSignallerBuilder {
    async fn try_create_signaller(
        &self,
        bootstrap_peer: Option<PublicKey>,
    ) -> Result<IrohSignaller> {
        // Create topic ID from a fixed identifier (all peers join same topic)
        let topic_id = TopicId::from_bytes(*b"_walkie_songie_signalling_topic_");

        let bootstrap: Vec<_> = bootstrap_peer.into_iter().collect();
        let has_bootstrap = !bootstrap.is_empty();

        info!("Joining gossip topic with bootstrap: {bootstrap:?}");

        let mut gossip_topic = self.gossip.subscribe(topic_id, bootstrap)?;

        if has_bootstrap {
            // Wait for connection to bootstrap peer
            let _ = n0_future::time::timeout(Duration::from_secs(10), gossip_topic.joined()).await;
        }

        let (gossip_send, gossip_recv) = gossip_topic.split();

        // Channels for communicating with the signaller task
        let (req_tx, req_rx) = mpsc::channel(256);
        let (event_tx, event_rx) = mpsc::channel(256);

        // Spawn the background task
        let task = {
            let builder = self.clone();
            let dm_recv = self.dm_receiver.activate_cloned();
            n0_future::task::spawn(async move {
                if let Err(e) = builder
                    .run_signaller_task(gossip_recv, gossip_send, req_rx, event_tx, dm_recv)
                    .await
                {
                    error!("Signaller task error: {e:#}");
                }
            })
        };

        Ok(IrohSignaller {
            request_tx: req_tx,
            event_rx,
            _task: n0_future::task::AbortOnDropHandle::new(task),
        })
    }

    async fn run_signaller_task(
        self,
        mut gossip_recv: GossipReceiver,
        gossip_send: GossipSender,
        mut req_rx: mpsc::Receiver<PeerRequest>,
        event_tx: mpsc::Sender<PeerEvent>,
        mut dm_recv: async_broadcast::Receiver<(PublicKey, PeerEvent)>,
    ) -> Result<()> {
        const PRESENCE_INTERVAL: Duration = Duration::from_secs(5);
        const STALE_TIMEOUT: Duration = Duration::from_secs(15);

        // Send initial IdAssigned event
        event_tx.send(PeerEvent::IdAssigned(self.matchbox_id)).await?;

        // Broadcast our presence
        self.broadcast_presence(&gossip_send).await?;

        // Track peer ID mappings with timestamps
        let mut matchbox_to_iroh: BTreeMap<PeerId, (PublicKey, Instant)> = BTreeMap::new();
        let mut iroh_to_matchbox: BTreeMap<PublicKey, (PeerId, Instant)> = BTreeMap::new();

        let mut presence_ticker = n0_future::time::interval(PRESENCE_INTERVAL);

        loop {
            tokio::select! {
                // Handle gossip messages (peer presence)
                msg = gossip_recv.next().fuse() => {
                    let Some(Ok(event)) = msg else {
                        anyhow::bail!("Gossip stream closed");
                    };

                    if let Event::Gossip(GossipEvent::Received(msg)) = event {
                        if let Ok(presence) = serde_json::from_slice::<GossipPresence>(&msg.content) {
                            let now = Instant::now();
                            let is_new = !matchbox_to_iroh.contains_key(&presence.matchbox_id);

                            matchbox_to_iroh.insert(presence.matchbox_id, (presence.iroh_id, now));
                            iroh_to_matchbox.insert(presence.iroh_id, (presence.matchbox_id, now));

                            if is_new && presence.matchbox_id != self.matchbox_id {
                                info!("Discovered peer: {} (iroh: {})", presence.matchbox_id, presence.iroh_id);

                                // Only the peer with smaller ID sends NewPeer
                                // This ensures both peers agree on who initiates
                                if presence.matchbox_id < self.matchbox_id {
                                    event_tx.send(PeerEvent::NewPeer(presence.matchbox_id)).await?;
                                }

                                // Announce ourselves to the new peer
                                self.broadcast_presence(&gossip_send).await?;
                            }
                        }
                    }
                }

                // Handle requests from matchbox
                req = req_rx.recv().fuse() => {
                    let Some(req) = req else {
                        anyhow::bail!("Request channel closed");
                    };

                    match req {
                        PeerRequest::KeepAlive => {
                            self.broadcast_presence(&gossip_send).await?;
                        }
                        PeerRequest::Signal { receiver, data } => {
                            // Send WebRTC signal via direct message
                            if let Some((iroh_id, _)) = matchbox_to_iroh.get(&receiver) {
                                let event = PeerEvent::Signal {
                                    sender: self.matchbox_id,
                                    data,
                                };
                                if let Err(e) = send_direct_message(&self.endpoint, *iroh_id, event).await {
                                    warn!("Failed to send direct message to {receiver}: {e}");
                                }
                            } else {
                                warn!("Unknown peer: {receiver}");
                            }
                        }
                    }
                }

                // Handle direct messages (WebRTC signals)
                dm = dm_recv.next().fuse() => {
                    let Some((from_iroh, event)) = dm else {
                        anyhow::bail!("Direct message channel closed");
                    };

                    // Verify sender
                    if let PeerEvent::Signal { sender, .. } = &event {
                        if matchbox_to_iroh.get(sender).map(|(id, _)| id) == Some(&from_iroh) {
                            debug!("Received signal from {sender}");
                            event_tx.send(event).await?;
                        } else {
                            warn!("Signal from {from_iroh} claims to be {sender}, ignoring");
                        }
                    }
                }

                // Periodic presence broadcast and stale peer cleanup
                _ = presence_ticker.tick().fuse() => {
                    self.broadcast_presence(&gossip_send).await?;

                    // Remove stale peers
                    let now = Instant::now();
                    let stale: Vec<_> = matchbox_to_iroh
                        .iter()
                        .filter(|(_, (_, ts))| now.duration_since(*ts) > STALE_TIMEOUT)
                        .map(|(mid, (iid, _))| (*mid, *iid))
                        .collect();

                    for (matchbox_id, iroh_id) in stale {
                        info!("Peer timed out: {matchbox_id}");
                        matchbox_to_iroh.remove(&matchbox_id);
                        iroh_to_matchbox.remove(&iroh_id);
                        event_tx.send(PeerEvent::PeerLeft(matchbox_id)).await?;
                    }
                }
            }
        }
    }

    async fn broadcast_presence(&self, gossip_send: &GossipSender) -> Result<()> {
        let presence = GossipPresence {
            matchbox_id: self.matchbox_id,
            iroh_id: self.iroh_id,
            timestamp_ms: web_time::SystemTime::now()
                .duration_since(web_time::SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        };
        let data = serde_json::to_vec(&presence)?;
        gossip_send.broadcast(data.into()).await?;
        Ok(())
    }
}

/// The actual signaller that matchbox uses.
struct IrohSignaller {
    request_tx: mpsc::Sender<PeerRequest>,
    event_rx: mpsc::Receiver<PeerEvent>,
    _task: n0_future::task::AbortOnDropHandle<()>,
}

#[cfg_attr(not(target_arch = "wasm32"), matchbox_socket::async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", matchbox_socket::async_trait::async_trait(?Send))]
impl Signaller for IrohSignaller {
    async fn send(&mut self, request: PeerRequest) -> Result<(), SignalingError> {
        self.request_tx
            .send(request)
            .await
            .map_err(|e| SignalingError::UserImplementationError(format!("{e}")))?;
        Ok(())
    }

    async fn next_message(&mut self) -> Result<PeerEvent, SignalingError> {
        self.event_rx
            .recv()
            .await
            .ok_or(SignalingError::StreamExhausted)
    }
}

/// Convenience function to create a signaller builder.
pub async fn create_signaller() -> Result<IrohSignallerBuilder> {
    IrohSignallerBuilder::new().await
}
