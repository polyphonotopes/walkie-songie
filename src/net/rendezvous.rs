//! Topic rendezvous: auto-peering by three-word room code, no ticket exchange.
//!
//! iroh-gossip's `subscribe(topic, bootstrap)` needs at least one live peer
//! *id*; iroh has no "who is subscribed to topic T". Something outside iroh must
//! map **topic → endpoint ids**. This module is that map: a thin pub/sub client
//! over the y-webrtc signaling server at [`SIGNALING_SERVER_URL`], implementing
//! `docs/research/peer-discovery-design.md` §3 Option 1.
//!
//! Both tabs (or a browser and a desktop) that enter the same v5 room derive the
//! same [`RoomTopic`], subscribe to the same opaque channel
//! `walkie-rdv-v5-<topic-hex>` (never the human room name), and publish a
//! [`HelloV5`] carrying their endpoint id, protocol support, and home relay
//! URL. On hearing a peer's hello we seed iroh's [`MemoryLookup`] with
//! `{id → relay}`, ask gossip to [`join_peers`](GossipSender::join_peers), and
//! reply with our own hello so late joiners learn us with zero server-side
//! state.
//!
//! The wasm backend is a raw [`web_sys::WebSocket`]; the native backend is
//! `tokio-tungstenite`. Both sit behind [`SignalStream`] so the session loop
//! ([`run_rendezvous`]) is byte-identical on both targets. Spawn/sleep are the
//! only per-target seams: `n0-future` on wasm, tokio on native — the same split
//! the rest of `net` already makes.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::Duration,
};

use iroh::{Endpoint, EndpointAddr, EndpointId, RelayUrl, address_lookup::MemoryLookup};
use iroh_gossip::api::GossipSender;
use serde::{Deserialize, Serialize};

use super::iroh_common::{RoomTopic, SIGNALING_SERVER_URL};
use crate::room::v5::ProtocolSupport;

/// Hello discriminator; ignores (and is ignored by) any non-walkie publisher.
const HELLO_KIND: &str = "walkie-hello";
const RENDEZVOUS_CHANNEL_PREFIX_V5: &str = "walkie-rdv-v5-";
pub const HELLO_VERSION_V5: u32 = 5;
/// Discriminator for a WebRTC signaling envelope (SDP offer/answer + ICE) riding
/// the same channel as hellos. Non-`walkie-rtc` publishers are ignored; a peer that
/// does not understand it never sees one (it is addressed by endpoint id).
const RTC_KIND: &str = "walkie-rtc";
/// WebRTC signaling envelope version.
#[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
const RTC_VERSION: u32 = 2;
const RTC_INCARNATIONS_REMEMBERED_PER_PEER: usize = 8;
/// Keepalive re-hello once our relay is known. Also refreshes peers' addressing
/// and re-advertises us to anyone who joined since our last hello.
const RE_HELLO_INTERVAL: Duration = Duration::from_secs(30);
/// Faster re-hello while the home relay handshake is still settling, so the
/// first hello that actually carries a relay url goes out promptly.
const RETRY_HELLO_INTERVAL: Duration = Duration::from_secs(3);
/// Give browser peers a bounded chance to establish WebRTC before gossip
/// chooses its long-lived neighbor path; relay remains the deterministic
/// fallback.
#[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
const DIRECT_FIRST_TIMEOUT: Duration = Duration::from_millis(1_500);
/// Backoff before reconnecting after the signaling socket drops.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(5);
/// Cap on distinct peers we will ever `join_peers`/seed per rendezvous session.
///
/// Hellos are unauthenticated, so a topic-hash-knowing attacker can spray fake
/// ids. Ids are self-certifying (the QUIC handshake proves the key), so a bad id
/// only costs one failed dial — this cap bounds how many at once.
const MAX_RENDEZVOUS_PEERS: usize = 64;

pub fn rendezvous_channel_v5(topic: RoomTopic) -> String {
    format!("{RENDEZVOUS_CHANNEL_PREFIX_V5}{}", topic.to_hex())
}

#[derive(Debug, thiserror::Error)]
pub enum RendezvousError {
    #[error("could not connect to signaling server: {0}")]
    Connect(String),
    #[error("could not send to signaling server: {0}")]
    Send(String),
    #[error("signaling receive failed: {0}")]
    Recv(String),
    #[error("could not encode signaling message: {0}")]
    Encode(String),
}

impl From<serde_json::Error> for RendezvousError {
    fn from(error: serde_json::Error) -> Self {
        Self::Encode(error.to_string())
    }
}

/// Everything the session loop needs from the iroh side, target-independent
/// (these iroh handles exist and are cheap to clone on both native and wasm).
#[derive(Clone)]
pub struct RendezvousPeering {
    /// Read our endpoint id and (once online) home relay url.
    pub endpoint: Endpoint,
    /// `join_peers` a discovered id, queuing a gossip bootstrap dial.
    pub gossip_sender: GossipSender,
    /// The address lookup ticket joins already feed; rendezvous feeds it too.
    pub memory_lookup: MemoryLookup,
    /// Signaling hooks for the browser WebRTC custom transport (M4 direct peering).
    /// Present only on the browser build; `None` disables the extra pumping so the
    /// rendezvous loop stays byte-identical to before when WebRTC is not configured.
    /// The loop routes inbound `walkie-rtc` payloads into `commands` and publishes
    /// the driver's outbound SDP/ICE, and — on discovering an rtc-capable peer —
    /// seeds that peer's custom addr into `memory_lookup` and kicks a dial.
    #[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
    pub webrtc: Option<super::webrtc_transport::WebRtcSignalPort>,
}

// ---------------------------------------------------------------------------
// Wire protocol (y-webrtc pub/sub). The server never inspects `data`; it fans
// every `publish` out to every subscriber of the topic, sender included.
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ClientMessage {
    Subscribe {
        topics: Vec<String>,
    },
    /// `data` is an opaque JSON blob the server fans out verbatim. It carries either
    /// a [`Hello`] or (browser only) a WebRTC signaling envelope, discriminated by
    /// its `kind` field — so both kinds share the one publish shape.
    Publish {
        topic: String,
        data: serde_json::Value,
    },
    Pong,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ServerMessage {
    Publish {
        #[allow(dead_code)]
        topic: String,
        data: serde_json::Value,
    },
    Ping,
    /// `pong` and anything else the server may send.
    #[serde(other)]
    Other,
}

/// Room-v5 discovery metadata. `support` is required but non-authoritative: it
/// avoids futile ALPN attempts and never enters capability evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloV5 {
    pub kind: String,
    pub v: u32,
    pub support: u8,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtc: Option<bool>,
    /// Random browser-transport placement identity. Unlike `id`, this changes
    /// across a whole room-generation reopen, so a surviving deterministic
    /// offerer can replace its stale link before connectionState catches up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtc_incarnation: Option<[u8; 8]>,
}

impl HelloV5 {
    pub fn new(
        support: ProtocolSupport,
        id: String,
        relay: Option<String>,
        rtc: Option<bool>,
        rtc_incarnation: Option<[u8; 8]>,
    ) -> Self {
        Self {
            kind: HELLO_KIND.to_owned(),
            v: HELLO_VERSION_V5,
            support: support.bits(),
            id,
            relay,
            rtc,
            rtc_incarnation,
        }
    }

    pub fn validate(&self) -> Option<ProtocolSupport> {
        if self.kind != HELLO_KIND || self.v != HELLO_VERSION_V5 {
            return None;
        }
        ProtocolSupport::from_bits(self.support)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IncarnationObservation {
    First,
    Known,
    Reincarnated,
    CapacityRefused,
}

/// Bounded replay memory for browser transport incarnations. Remembering recent
/// values prevents a delayed Hello from an already-retired placement from
/// toggling the survivor back to that stale carrier.
#[derive(Default)]
struct RtcIncarnationTracker {
    seen: HashMap<EndpointId, VecDeque<[u8; 8]>>,
}

impl RtcIncarnationTracker {
    fn observe(&mut self, peer: EndpointId, incarnation: [u8; 8]) -> IncarnationObservation {
        if !self.seen.contains_key(&peer) && self.seen.len() >= MAX_RENDEZVOUS_PEERS {
            return IncarnationObservation::CapacityRefused;
        }
        let values = self.seen.entry(peer).or_default();
        if values.contains(&incarnation) {
            return IncarnationObservation::Known;
        }
        let observation = if values.is_empty() {
            IncarnationObservation::First
        } else {
            IncarnationObservation::Reincarnated
        };
        values.push_back(incarnation);
        if values.len() > RTC_INCARNATIONS_REMEMBERED_PER_PEER {
            values.pop_front();
        }
        observation
    }
}

#[derive(Debug, Clone, Copy)]
enum RendezvousGeneration {
    V5 { local_support: ProtocolSupport },
}

impl RendezvousGeneration {
    fn channel(self, topic: RoomTopic) -> String {
        match self {
            Self::V5 { .. } => rendezvous_channel_v5(topic),
        }
    }
}

/// A WebRTC signaling envelope (browser only). Addressed peer-to-peer over the
/// fan-out channel: everyone receives it, only `to` acts on it. Discriminated from
/// [`Hello`] by `kind == "walkie-rtc"`.
#[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
#[derive(Serialize, Deserialize)]
struct RtcEnvelope {
    kind: String,
    v: u32,
    /// Sender endpoint id, 64-char lowercase hex.
    from: String,
    /// Recipient endpoint id, 64-char lowercase hex.
    to: String,
    payload: super::webrtc_transport::RtcPayload,
}

/// A minimal text-frame WebSocket the [`run_rendezvous`] loop drives. The two
/// backends differ only in transport; the protocol logic never sees them.
///
/// Native requires `Send` futures so the session can ride tokio's work-stealing
/// `spawn`; wasm drops that bound because the browser socket holds `!Send` JS
/// handles and runs on `spawn_local` — the same split the [`Transport`] seam
/// makes.
///
/// [`Transport`]: super::Transport
#[cfg(not(target_arch = "wasm32"))]
pub trait SignalStream: Send {
    /// Send one text frame.
    fn send(
        &mut self,
        text: String,
    ) -> impl std::future::Future<Output = Result<(), RendezvousError>> + Send;

    /// Await the next text frame. `Ok(None)` is a clean/close end of stream.
    fn recv(
        &mut self,
    ) -> impl std::future::Future<Output = Result<Option<String>, RendezvousError>> + Send;
}

/// See the native definition above; wasm sockets are `!Send` by construction.
#[cfg(target_arch = "wasm32")]
pub trait SignalStream {
    /// Send one text frame.
    fn send(
        &mut self,
        text: String,
    ) -> impl std::future::Future<Output = Result<(), RendezvousError>>;

    /// Await the next text frame. `Ok(None)` is a clean/close end of stream.
    fn recv(
        &mut self,
    ) -> impl std::future::Future<Output = Result<Option<String>, RendezvousError>>;
}

/// Per-target sleep. tokio's timer needs its runtime (absent on wasm), so wasm
/// uses `n0-future`'s `setTimeout`-backed sleep and native uses tokio.
#[cfg(target_arch = "wasm32")]
async fn rdv_sleep(duration: Duration) {
    n0_future::time::sleep(duration).await;
}
#[cfg(not(target_arch = "wasm32"))]
async fn rdv_sleep(duration: Duration) {
    tokio::time::sleep(duration).await;
}

/// Publish one hello. Returns whether a home relay url was available (so the
/// caller can shorten the re-hello interval until the relay settles).
async fn send_hello<S: SignalStream>(
    socket: &mut S,
    channel: &str,
    peering: &RendezvousPeering,
    our_id: EndpointId,
    generation: RendezvousGeneration,
) -> Result<bool, RendezvousError> {
    // On wasm the endpoint address is relay-only; `relay_urls` filters native's
    // direct addrs out too, so a hello always advertises the relay alone.
    let relay = peering.endpoint.addr().relay_urls().next().cloned();
    // Advertise WebRTC capability only when the direct-peering transport is wired
    // in (browser build); a peer only offers to peers that flag `rtc`.
    #[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
    let rtc = peering.webrtc.is_some().then_some(true);
    #[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
    let rtc_incarnation = peering.webrtc.as_ref().map(|port| port.incarnation());
    #[cfg(not(all(target_arch = "wasm32", feature = "browser-net")))]
    let rtc = None;
    #[cfg(not(all(target_arch = "wasm32", feature = "browser-net")))]
    let rtc_incarnation = None;
    let relay_string = relay.as_ref().map(|url| url.as_str().to_owned());
    let RendezvousGeneration::V5 { local_support } = generation;
    let data = serde_json::to_value(HelloV5::new(
        local_support,
        our_id.to_string(),
        relay_string,
        rtc,
        rtc_incarnation,
    ))?;
    let message = ClientMessage::Publish {
        topic: channel.to_owned(),
        data,
    };
    socket.send(serde_json::to_string(&message)?).await?;
    Ok(relay.is_some())
}

/// One turn of the session loop: a frame arrived, the socket closed, or the
/// re-hello timer fired. Resolving the select inside its own scope drops the
/// borrowing futures before we touch `socket` again.
enum Turn {
    Message(String),
    Closed,
    ReHello,
    /// The WebRTC driver produced a signaling payload to publish (browser only).
    #[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
    SignalOut(super::webrtc_transport::SignalOut),
    /// One owned deferred direct-path/rejoin child completed (browser only).
    #[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
    ChildCompleted,
}

/// Drive one signaling session to completion (until the socket closes or errors).
///
/// `joined` persists across reconnects so we never re-`join_peers` a peer we
/// already bootstrapped (avoids HyParView Join spam) and stays capped.
/// `on_discovered` seeds the host's peer map for a genuinely new peer.
async fn run_rendezvous<S: SignalStream>(
    socket: &mut S,
    channel: &str,
    peering: &RendezvousPeering,
    generation: RendezvousGeneration,
    on_discovered: &impl Fn(EndpointId, Option<u8>),
    joined: &mut HashSet<EndpointId>,
    rtc_incarnations: &mut RtcIncarnationTracker,
) -> Result<(), RendezvousError> {
    let our_id = peering.endpoint.id();

    // The WebRTC driver's outbound signaling (browser only). Held here across the
    // select and restored to the shared slot on any exit (`OutboundGuard`'s `Drop`),
    // so after a signaling reconnect the next session re-takes it and keeps pumping.
    #[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
    let mut outbound = peering
        .webrtc
        .as_ref()
        .map(|port| OutboundGuard::take(&port.outbound));
    #[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
    let mut children = n0_future::task::JoinSet::new();

    let subscribe = ClientMessage::Subscribe {
        topics: vec![channel.to_owned()],
    };
    socket.send(serde_json::to_string(&subscribe)?).await?;
    let mut have_relay = send_hello(socket, channel, peering, our_id, generation).await?;

    loop {
        let interval = if have_relay {
            RE_HELLO_INTERVAL
        } else {
            RETRY_HELLO_INTERVAL
        };
        // Scope the select so the `recv`/`sleep` futures (the former borrows
        // `socket`) drop before we send anything below. The browser build adds a
        // third arm draining the WebRTC driver's outbound signaling; the native
        // build keeps the original two-way select verbatim.
        #[cfg(not(all(target_arch = "wasm32", feature = "browser-net")))]
        let turn = {
            use futures::future::{Either, select};
            let recv = std::pin::pin!(socket.recv());
            let timer = std::pin::pin!(rdv_sleep(interval));
            match select(recv, timer).await {
                Either::Left((message, _)) => match message? {
                    Some(text) => Turn::Message(text),
                    None => Turn::Closed,
                },
                Either::Right(((), _)) => Turn::ReHello,
            }
        };
        #[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
        let turn = {
            use futures::FutureExt;
            let recv = socket.recv().fuse();
            let timer = rdv_sleep(interval).fuse();
            let signal = next_outbound(outbound.as_mut().and_then(|guard| guard.rx())).fuse();
            let child = next_rendezvous_child(&mut children).fuse();
            futures::pin_mut!(recv, timer, signal, child);
            futures::select! {
                message = recv => match message? {
                    Some(text) => Turn::Message(text),
                    None => Turn::Closed,
                },
                () = timer => Turn::ReHello,
                out = signal => Turn::SignalOut(out),
                () = child => Turn::ChildCompleted,
            }
        };

        match turn {
            Turn::Closed => return Ok(()),
            Turn::ReHello => {
                have_relay = send_hello(socket, channel, peering, our_id, generation).await?;
            }
            #[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
            Turn::SignalOut(signal) => {
                // The driver produced an SDP/ICE payload; publish it addressed to
                // the peer. Everyone on the channel receives it; only `to` acts.
                if let Ok(to) = EndpointId::from_bytes(&signal.to) {
                    let envelope = RtcEnvelope {
                        kind: RTC_KIND.to_owned(),
                        v: RTC_VERSION,
                        from: our_id.to_string(),
                        to: to.to_string(),
                        payload: signal.payload,
                    };
                    let message = ClientMessage::Publish {
                        topic: channel.to_owned(),
                        data: serde_json::to_value(&envelope)?,
                    };
                    socket.send(serde_json::to_string(&message)?).await?;
                }
            }
            #[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
            Turn::ChildCompleted => {}
            Turn::Message(text) => {
                let Ok(message) = serde_json::from_str::<ServerMessage>(&text) else {
                    continue;
                };
                match message {
                    ServerMessage::Ping => {
                        socket
                            .send(serde_json::to_string(&ClientMessage::Pong)?)
                            .await?;
                    }
                    ServerMessage::Other => {}
                    ServerMessage::Publish { data, .. } => {
                        // Both hellos and WebRTC signaling ride `data`; dispatch on
                        // `kind`. A `walkie-rtc` envelope goes straight to the driver
                        // (browser only); anything else is treated as a hello.
                        if data.get("kind").and_then(|kind| kind.as_str()) == Some(RTC_KIND) {
                            #[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
                            route_rtc_envelope(peering, our_id, data);
                            continue;
                        }
                        let Ok(hello) = serde_json::from_value::<HelloV5>(data) else {
                            continue;
                        };
                        let Some(support) = hello.validate() else {
                            continue;
                        };
                        let (id_text, relay_text, rtc, rtc_incarnation, support_bits) = (
                            hello.id,
                            hello.relay,
                            hello.rtc,
                            hello.rtc_incarnation,
                            Some(support.bits()),
                        );
                        #[cfg(not(all(target_arch = "wasm32", feature = "browser-net")))]
                        let _ = rtc;
                        let Ok(id) = id_text.parse::<EndpointId>() else {
                            continue;
                        };
                        if id == our_id {
                            continue; // our own hello, fanned back to us
                        }
                        let reincarnated = rtc == Some(true)
                            && rtc_incarnation.is_some_and(|incarnation| {
                                rtc_incarnations.observe(id, incarnation)
                                    == IncarnationObservation::Reincarnated
                            });
                        #[cfg(not(all(target_arch = "wasm32", feature = "browser-net")))]
                        let _ = reincarnated;
                        let relay = relay_text
                            .as_deref()
                            .and_then(|url| url.parse::<RelayUrl>().ok());
                        let mut endpoint_addr = EndpointAddr::new(id);
                        if let Some(relay) = relay {
                            endpoint_addr = endpoint_addr.with_relay_url(relay);
                        }
                        // Seed the peer's WebRTC custom addr beside its relay so
                        // iroh treats the direct path as a candidate and probes it.
                        // Derived from the id — no need to advertise it on the wire.
                        // Both relay and custom go into ONE `add_endpoint_info` so a
                        // later refresh never drops the relay fallback.
                        #[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
                        if peering.webrtc.is_some() && rtc == Some(true) {
                            endpoint_addr =
                                endpoint_addr.with_addrs([iroh::TransportAddr::Custom(
                                    super::webrtc_transport::webrtc_custom_addr(id.as_bytes()),
                                )]);
                        }

                        if joined.contains(&id) {
                            // Known peer: refresh addressing (relay may have
                            // changed) but do not re-join or re-announce, or two
                            // peers ping-pong hellos forever.
                            peering.memory_lookup.add_endpoint_info(endpoint_addr);
                            #[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
                            if reincarnated && let Some(port) = peering.webrtc.as_ref() {
                                // Re-announce the transport reincarnation to the
                                // application and re-kick gossip only after the
                                // fresh direct attempt opens (or the existing
                                // bounded relay fallback expires).
                                on_discovered(id, support_bits);
                                let port = port.clone();
                                let gossip = peering.gossip_sender.clone();
                                if children.len() >= MAX_RENDEZVOUS_PEERS {
                                    tracing::debug!(
                                        target: "walkie::rendezvous",
                                        peer = %id,
                                        "refusing reincarnation child because the bounded rendezvous scope is full"
                                    );
                                    continue;
                                }
                                children.spawn_local(async move {
                                    let direct = match port.reincarnate(*id.as_bytes()).await {
                                        Some(fence) => {
                                            port.wait_fresh_connected(
                                                *id.as_bytes(),
                                                fence,
                                                DIRECT_FIRST_TIMEOUT,
                                            )
                                            .await
                                        }
                                        None => false,
                                    };
                                    tracing::info!(
                                        target: "walkie::rendezvous",
                                        peer = %id,
                                        direct,
                                        "rejoining gossip after peer transport reincarnation"
                                    );
                                    if let Err(error) = gossip.join_peers(vec![id]).await {
                                        tracing::debug!(
                                            target: "walkie::rendezvous",
                                            "rejoin_peers for reincarnated {id} failed: {error}"
                                        );
                                    }
                                });
                            }
                        } else if joined.len() < MAX_RENDEZVOUS_PEERS {
                            joined.insert(id);
                            #[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
                            let direct_first = rtc == Some(true) && peering.webrtc.is_some();
                            #[cfg(not(all(target_arch = "wasm32", feature = "browser-net")))]
                            let direct_first = false;
                            if direct_first {
                                #[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
                                peering.memory_lookup.add_endpoint_info(
                                    EndpointAddr::new(id).with_addrs([
                                        iroh::TransportAddr::Custom(
                                            super::webrtc_transport::webrtc_custom_addr(
                                                id.as_bytes(),
                                            ),
                                        ),
                                    ]),
                                );
                            } else {
                                peering
                                    .memory_lookup
                                    .add_endpoint_info(endpoint_addr.clone());
                            }
                            on_discovered(id, support_bits);
                            // Kick the WebRTC handshake for a newly discovered
                            // rtc-capable peer (browser only). The driver picks the
                            // role: lower endpoint id offers, the other answers. A
                            // detached waiter joins gossip as soon as direct is
                            // ready, or after a bounded relay-fallback timeout;
                            // the signaling loop must remain free to pump SDP/ICE.
                            #[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
                            let deferred_join = if rtc == Some(true) {
                                if let Some(port) = peering.webrtc.as_ref() {
                                    let _ = port.commands.unbounded_send(
                                        super::webrtc_transport::Command::Dial(*id.as_bytes()),
                                    );
                                    let port = port.clone();
                                    let gossip = peering.gossip_sender.clone();
                                    let lookup = peering.memory_lookup.clone();
                                    let fallback_addr = endpoint_addr.clone();
                                    if children.len() >= MAX_RENDEZVOUS_PEERS {
                                        tracing::debug!(
                                            target: "walkie::rendezvous",
                                            peer = %id,
                                            "refusing direct-path child because the bounded rendezvous scope is full"
                                        );
                                        continue;
                                    }
                                    children.spawn_local(async move {
                                        let direct = port
                                            .wait_connected(*id.as_bytes(), DIRECT_FIRST_TIMEOUT)
                                            .await;
                                        if !direct {
                                            lookup.add_endpoint_info(fallback_addr.clone());
                                        }
                                        tracing::debug!(
                                            target: "walkie::rendezvous",
                                            peer = %id,
                                            direct,
                                            "joining gossip after browser path selection"
                                        );
                                        if let Err(error) = gossip.join_peers(vec![id]).await {
                                            tracing::debug!(
                                                target: "walkie::rendezvous",
                                                "join_peers for {id} failed: {error}"
                                            );
                                        }
                                        if direct {
                                            // Let the custom path start QUIC before
                                            // restoring relay as a migration/failure
                                            // candidate for this long-lived peer.
                                            rdv_sleep(Duration::from_secs(2)).await;
                                            lookup.add_endpoint_info(fallback_addr);
                                        }
                                    });
                                    true
                                } else {
                                    false
                                }
                            } else {
                                false
                            };
                            #[cfg(not(all(target_arch = "wasm32", feature = "browser-net")))]
                            let deferred_join = false;
                            if !deferred_join
                                && let Err(error) = peering.gossip_sender.join_peers(vec![id]).await
                            {
                                tracing::debug!(
                                    target: "walkie::rendezvous",
                                    "join_peers for {id} failed: {error}"
                                );
                            }
                            // Reply so a newcomer learns us with zero server
                            // state. Only on first sight, else the ping-pong.
                            if let Err(error) =
                                send_hello(socket, channel, peering, our_id, generation).await
                            {
                                tracing::debug!(
                                    target: "walkie::rendezvous",
                                    "reply hello failed: {error}"
                                );
                            }
                        }
                        // else: cap reached; ignore new ids (bounds failed dials
                        // and MemoryLookup growth from a spray of bogus ids).
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// WebRTC signaling helpers (browser only). SDP/ICE rides the same channel as
// hellos, addressed peer-to-peer; the driver task does the actual RTC work.
// ---------------------------------------------------------------------------

/// The WebRTC driver's outbound-signaling receiver.
#[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
type SignalRx = futures::channel::mpsc::UnboundedReceiver<super::webrtc_transport::SignalOut>;

/// Borrows the outbound-signaling receiver out of its shared slot for one signaling
/// session and restores it on drop. Because the signaling socket reconnects (and so
/// re-enters [`run_rendezvous`]) while the WebRTC driver — and its sender half —
/// live on, the receiver must survive a session ending; taking and restoring it
/// through this guard keeps the pump alive across reconnects.
#[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
struct OutboundGuard<'a> {
    slot: &'a std::rc::Rc<std::cell::RefCell<Option<SignalRx>>>,
    rx: Option<SignalRx>,
}

#[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
impl<'a> OutboundGuard<'a> {
    fn take(slot: &'a std::rc::Rc<std::cell::RefCell<Option<SignalRx>>>) -> Self {
        let rx = slot.borrow_mut().take();
        Self { slot, rx }
    }

    fn rx(&mut self) -> Option<&mut SignalRx> {
        self.rx.as_mut()
    }
}

#[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
impl Drop for OutboundGuard<'_> {
    fn drop(&mut self) {
        *self.slot.borrow_mut() = self.rx.take();
    }
}

/// Await the next outbound signaling payload, or pend forever if the driver's
/// sender has no receiver here (WebRTC not configured / already closed) — so this
/// branch simply never wins the select in that case.
#[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
async fn next_outbound(rx: Option<&mut SignalRx>) -> super::webrtc_transport::SignalOut {
    use futures::StreamExt;
    match rx {
        Some(rx) => match rx.next().await {
            Some(signal) => signal,
            None => std::future::pending().await,
        },
        None => std::future::pending().await,
    }
}

/// Poll one child from the signaling session's owned scope. An empty scope is
/// deliberately pending so it cannot win the main socket/timer/signal select.
/// Dropping `run_rendezvous` drops this `JoinSet` and aborts every child.
#[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
async fn next_rendezvous_child(children: &mut n0_future::task::JoinSet<()>) {
    if children.is_empty() {
        std::future::pending().await
    } else {
        let _ = children.join_next().await;
    }
}

/// Route an inbound `walkie-rtc` envelope to the WebRTC driver, if it is addressed
/// to us and well-formed. Unauthenticated, like hellos: a forged payload at worst
/// yields a failed WebRTC handshake — the QUIC handshake *inside* the data channel
/// still proves the endpoint key, so nothing above the transport trusts it.
#[cfg(all(target_arch = "wasm32", feature = "browser-net"))]
fn route_rtc_envelope(peering: &RendezvousPeering, our_id: EndpointId, data: serde_json::Value) {
    let Some(port) = peering.webrtc.as_ref() else {
        return;
    };
    let Ok(envelope) = serde_json::from_value::<RtcEnvelope>(data) else {
        return;
    };
    if envelope.v != RTC_VERSION || envelope.to != our_id.to_string() {
        return;
    }
    let Ok(from) = envelope.from.parse::<EndpointId>() else {
        return;
    };
    if from == our_id {
        return; // our own signaling, fanned back to us
    }
    let _ = port
        .commands
        .unbounded_send(super::webrtc_transport::Command::Signal {
            from: *from.as_bytes(),
            payload: envelope.payload,
        });
}

/// The reconnecting outer loop. Owns `joined` so a socket drop does not re-spam
/// `join_peers`. Runs until the task is aborted (see [`RendezvousHandle`]).
async fn rendezvous_main(
    peering: RendezvousPeering,
    topic: RoomTopic,
    generation: RendezvousGeneration,
    on_discovered: impl Fn(EndpointId, Option<u8>),
) {
    let channel = generation.channel(topic);
    let mut joined: HashSet<EndpointId> = HashSet::new();
    let mut rtc_incarnations = RtcIncarnationTracker::default();
    loop {
        match connect_signal(SIGNALING_SERVER_URL).await {
            Ok(mut socket) => {
                if let Err(error) = run_rendezvous(
                    &mut socket,
                    &channel,
                    &peering,
                    generation,
                    &on_discovered,
                    &mut joined,
                    &mut rtc_incarnations,
                )
                .await
                {
                    tracing::debug!(
                        target: "walkie::rendezvous",
                        "rendezvous session ended: {error}"
                    );
                }
            }
            Err(error) => {
                tracing::debug!(
                    target: "walkie::rendezvous",
                    "signaling connect failed: {error}"
                );
            }
        }
        rdv_sleep(RECONNECT_BACKOFF).await;
    }
}

/// A running rendezvous task. Aborts on drop, so tying its lifetime to the room
/// task (hold the handle for the room's duration) is all the shutdown needed.
pub struct RendezvousHandle {
    #[cfg(target_arch = "wasm32")]
    task: n0_future::task::AbortOnDropHandle<()>,
    #[cfg(not(target_arch = "wasm32"))]
    task: tokio::task::JoinHandle<()>,
}

impl RendezvousHandle {
    /// Stop the rendezvous task. Also happens automatically on drop.
    pub fn stop(&self) {
        self.task.abort();
    }
}

impl Drop for RendezvousHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Start capability-native Room-v5 rendezvous. Support is connectivity
/// metadata only and remains distinct from receiver-bound grants.
#[cfg(not(target_arch = "wasm32"))]
pub fn spawn_rendezvous_v5(
    peering: RendezvousPeering,
    topic: RoomTopic,
    local_support: ProtocolSupport,
    on_discovered: impl Fn(EndpointId, ProtocolSupport) + Send + Sync + 'static,
) -> RendezvousHandle {
    RendezvousHandle {
        task: tokio::spawn(rendezvous_main(
            peering,
            topic,
            RendezvousGeneration::V5 { local_support },
            move |id, support| {
                if let Some(support) = support.and_then(ProtocolSupport::from_bits) {
                    on_discovered(id, support);
                }
            },
        )),
    }
}

#[cfg(target_arch = "wasm32")]
pub fn spawn_rendezvous_v5(
    peering: RendezvousPeering,
    topic: RoomTopic,
    local_support: ProtocolSupport,
    on_discovered: impl Fn(EndpointId, ProtocolSupport) + 'static,
) -> RendezvousHandle {
    RendezvousHandle {
        task: n0_future::task::AbortOnDropHandle::new(n0_future::task::spawn(rendezvous_main(
            peering,
            topic,
            RendezvousGeneration::V5 { local_support },
            move |id, support| {
                if let Some(support) = support.and_then(ProtocolSupport::from_bits) {
                    on_discovered(id, support);
                }
            },
        ))),
    }
}

// ---------------------------------------------------------------------------
// Native backend: tokio-tungstenite over wss.
// ---------------------------------------------------------------------------

#[cfg(not(target_arch = "wasm32"))]
async fn connect_signal(url: &str) -> Result<impl SignalStream, RendezvousError> {
    native_socket::NativeSignalStream::connect(url).await
}

#[cfg(not(target_arch = "wasm32"))]
mod native_socket {
    use super::{RendezvousError, SignalStream};
    use futures::{SinkExt, StreamExt};
    use tokio::net::TcpStream;
    use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

    pub struct NativeSignalStream {
        inner: WebSocketStream<MaybeTlsStream<TcpStream>>,
    }

    impl NativeSignalStream {
        pub async fn connect(url: &str) -> Result<Self, RendezvousError> {
            let (inner, _response) = connect_async(url)
                .await
                .map_err(|error| RendezvousError::Connect(error.to_string()))?;
            Ok(Self { inner })
        }
    }

    impl SignalStream for NativeSignalStream {
        async fn send(&mut self, text: String) -> Result<(), RendezvousError> {
            self.inner
                .send(Message::Text(text.into()))
                .await
                .map_err(|error| RendezvousError::Send(error.to_string()))
        }

        async fn recv(&mut self) -> Result<Option<String>, RendezvousError> {
            loop {
                match self.inner.next().await {
                    Some(Ok(Message::Text(text))) => return Ok(Some(text.to_string())),
                    // Ignore binary/control frames; tungstenite answers WS-level
                    // pings itself. Our y-webrtc ping is a text frame, handled up
                    // in the session loop.
                    Some(Ok(_)) => continue,
                    Some(Err(error)) => {
                        return Err(RendezvousError::Recv(error.to_string()));
                    }
                    None => return Ok(None),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Browser backend: raw web_sys::WebSocket, callbacks bridged to a channel.
// ---------------------------------------------------------------------------

#[cfg(target_arch = "wasm32")]
async fn connect_signal(url: &str) -> Result<impl SignalStream, RendezvousError> {
    browser_socket::BrowserSignalStream::connect(url).await
}

#[cfg(target_arch = "wasm32")]
mod browser_socket {
    use super::{RendezvousError, SignalStream};
    use futures::{
        StreamExt,
        channel::{mpsc, oneshot},
    };
    use std::{cell::RefCell, rc::Rc};
    use wasm_bindgen::{JsCast, closure::Closure};
    use web_sys::{MessageEvent, WebSocket};

    /// A text frame, or `None` on socket close/error.
    type Frame = Option<String>;

    pub struct BrowserSignalStream {
        socket: WebSocket,
        frames: mpsc::UnboundedReceiver<Frame>,
        // Keep the JS closures alive for the socket's lifetime.
        _on_message: Closure<dyn FnMut(MessageEvent)>,
        _on_close: Closure<dyn FnMut()>,
        _on_error: Closure<dyn FnMut()>,
    }

    impl BrowserSignalStream {
        pub async fn connect(url: &str) -> Result<Self, RendezvousError> {
            let socket = WebSocket::new(url)
                .map_err(|error| RendezvousError::Connect(format!("{error:?}")))?;
            wait_for_open(&socket).await?;

            let (tx, frames) = mpsc::unbounded::<Frame>();

            let message_tx = tx.clone();
            let on_message = Closure::wrap(Box::new(move |event: MessageEvent| {
                if let Some(text) = event.data().as_string() {
                    let _ = message_tx.unbounded_send(Some(text));
                }
            }) as Box<dyn FnMut(MessageEvent)>);
            socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

            let close_tx = tx.clone();
            let on_close = Closure::wrap(Box::new(move || {
                let _ = close_tx.unbounded_send(None);
            }) as Box<dyn FnMut()>);
            socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));

            let error_tx = tx;
            let on_error = Closure::wrap(Box::new(move || {
                let _ = error_tx.unbounded_send(None);
            }) as Box<dyn FnMut()>);
            socket.set_onerror(Some(on_error.as_ref().unchecked_ref()));

            Ok(Self {
                socket,
                frames,
                _on_message: on_message,
                _on_close: on_close,
                _on_error: on_error,
            })
        }
    }

    impl SignalStream for BrowserSignalStream {
        async fn send(&mut self, text: String) -> Result<(), RendezvousError> {
            self.socket
                .send_with_str(&text)
                .map_err(|error| RendezvousError::Send(format!("{error:?}")))
        }

        async fn recv(&mut self) -> Result<Option<String>, RendezvousError> {
            // `Some(None)` = a close/error frame; `None` = channel drained.
            Ok(self.frames.next().await.flatten())
        }
    }

    impl Drop for BrowserSignalStream {
        fn drop(&mut self) {
            self.socket.set_onmessage(None);
            self.socket.set_onclose(None);
            self.socket.set_onerror(None);
            let _ = self.socket.close();
        }
    }

    /// Resolve once the socket is OPEN; reject if it closes/errors first.
    async fn wait_for_open(socket: &WebSocket) -> Result<(), RendezvousError> {
        if socket.ready_state() == WebSocket::OPEN {
            return Ok(());
        }
        if socket.ready_state() >= WebSocket::CLOSING {
            return Err(RendezvousError::Connect("socket already closed".to_owned()));
        }

        let (tx, rx) = oneshot::channel::<Result<(), RendezvousError>>();
        let tx = Rc::new(RefCell::new(Some(tx)));

        let open_tx = tx.clone();
        let on_open = Closure::wrap(Box::new(move || {
            if let Some(tx) = open_tx.borrow_mut().take() {
                let _ = tx.send(Ok(()));
            }
        }) as Box<dyn FnMut()>);

        let fail_tx = tx;
        let on_fail = Closure::wrap(Box::new(move || {
            if let Some(tx) = fail_tx.borrow_mut().take() {
                let _ = tx.send(Err(RendezvousError::Connect(
                    "socket closed before opening".to_owned(),
                )));
            }
        }) as Box<dyn FnMut()>);

        socket.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        socket.set_onerror(Some(on_fail.as_ref().unchecked_ref()));
        socket.set_onclose(Some(on_fail.as_ref().unchecked_ref()));

        let result = rx
            .await
            .unwrap_or_else(|_| Err(RendezvousError::Connect("open notifier dropped".to_owned())));

        // Detach the handshake handlers; `connect` installs the pump handlers.
        socket.set_onopen(None);
        socket.set_onerror(None);
        socket.set_onclose(None);
        drop(on_open);
        drop(on_fail);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_v5_channel_and_support_are_strict() {
        let room = crate::room::v5::RoomIdentity::from_name("quiet-cactus-song");
        let topic = RoomTopic::from_room_identity(&room);
        let channel = rendezvous_channel_v5(topic);
        assert!(channel.starts_with(RENDEZVOUS_CHANNEL_PREFIX_V5));
        assert!(channel.ends_with(&topic.to_hex()));
        let hello = HelloV5::new(
            ProtocolSupport::WALKIE,
            "aa".repeat(32),
            Some("https://relay.wondering.xyz/".to_owned()),
            Some(true),
            Some([17; 8]),
        );
        let json = serde_json::to_string(&hello).unwrap();
        assert!(json.contains("\"support\":3"));
        assert!(json.contains("\"rtc_incarnation\":[17,17,17,17,17,17,17,17]"));
        assert!(!json.contains("\"lanes\""));
        assert_eq!(
            serde_json::from_str::<HelloV5>(&json).unwrap().validate(),
            Some(ProtocolSupport::WALKIE)
        );
        for support in [0x00, 0x04, 0x80, 0xff] {
            assert_eq!(
                HelloV5 {
                    support,
                    ..hello.clone()
                }
                .validate(),
                None
            );
        }
    }

    #[test]
    fn placement_incarnation_changes_once_and_delayed_old_hello_is_ignored() {
        let peer = iroh::SecretKey::from_bytes(&[0x42; 32]).public();
        let mut tracker = RtcIncarnationTracker::default();
        assert_eq!(
            tracker.observe(peer, [11; 8]),
            IncarnationObservation::First
        );
        assert_eq!(
            tracker.observe(peer, [11; 8]),
            IncarnationObservation::Known
        );
        assert_eq!(
            tracker.observe(peer, [12; 8]),
            IncarnationObservation::Reincarnated
        );
        assert_eq!(
            tracker.observe(peer, [11; 8]),
            IncarnationObservation::Known
        );
        assert_eq!(
            tracker.observe(peer, [12; 8]),
            IncarnationObservation::Known
        );
    }

    #[test]
    fn placement_incarnation_peer_cardinality_is_bounded() {
        let mut tracker = RtcIncarnationTracker::default();
        let peers: Vec<_> = (0..=MAX_RENDEZVOUS_PEERS)
            .map(|index| {
                let mut seed = [0x51; 32];
                seed[..8].copy_from_slice(&(index as u64).to_le_bytes());
                iroh::SecretKey::from_bytes(&seed).public()
            })
            .collect();
        for peer in &peers[..MAX_RENDEZVOUS_PEERS] {
            assert_eq!(
                tracker.observe(*peer, [1; 8]),
                IncarnationObservation::First
            );
        }
        assert_eq!(
            tracker.observe(peers[MAX_RENDEZVOUS_PEERS], [1; 8]),
            IncarnationObservation::CapacityRefused
        );
        assert_eq!(
            tracker.observe(peers[0], [1; 8]),
            IncarnationObservation::Known
        );
    }

    #[test]
    fn ping_and_pong_use_the_ywebrtc_shape() {
        assert_eq!(
            serde_json::to_string(&ClientMessage::Pong).unwrap(),
            "{\"type\":\"pong\"}"
        );
        assert!(matches!(
            serde_json::from_str::<ServerMessage>("{\"type\":\"ping\"}").unwrap(),
            ServerMessage::Ping
        ));
        assert!(matches!(
            serde_json::from_str::<ServerMessage>("{\"type\":\"pong\"}").unwrap(),
            ServerMessage::Other
        ));
    }
}
