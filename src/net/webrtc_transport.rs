//! WebRTC as an iroh **custom transport**: the browser direct-peering carrier (M4).
//!
//! # Why this exists
//!
//! A browser walkie peer is relay-only by construction — iroh's wasm build has no
//! UDP and no WebRTC, so every byte between two tabs detours through
//! `relay.wondering.xyz` (see [`super::browser`]). That relay round-trip is the
//! latency users feel. This module removes it *without* touching anything above
//! iroh's `Endpoint`: it teaches that one endpoint a second way to move QUIC
//! packets — over a WebRTC `RTCDataChannel` — so iroh's own path selector migrates
//! the connection onto the direct link the moment it comes up, and falls back to
//! the relay automatically if it never does.
//!
//! # The seam (iroh 1.0.3 `unstable-custom-transports`)
//!
//! iroh exposes a socket-level plug-in point: `CustomTransport` is a factory for a
//! `CustomEndpoint`, which advertises local [`CustomAddr`]s, hands out a
//! `CustomSender`, and `poll_recv`s inbound datagrams. iroh routes a QUIC packet to
//! a `TransportAddr::Custom(addr)` by calling `CustomSender::poll_send`; inbound
//! datagrams surface through `CustomEndpoint::poll_recv`. It is a **datagram**
//! carrier (packets, not a stream), which is exactly why the data channel is
//! created *unreliable + unordered* (`{ordered:false, maxRetransmits:0}`): QUIC
//! runs its own loss recovery on top, so SCTP must not also retransmit.
//!
//! A `CustomAddr` is `(transport id: u64, opaque bytes)`. We encode a peer as
//! `(WEBRTC_TRANSPORT_ID, <32-byte endpoint id>)` — no wire negotiation of the
//! address is needed, since either side derives the other's custom addr straight
//! from the endpoint id it already learned over rendezvous.
//!
//! # The `Send` problem, and how the pump solves it
//!
//! `CustomTransport`/`CustomEndpoint`/`CustomSender` are all `Send + Sync + 'static`,
//! but web-sys `RtcPeerConnection`/`RtcDataChannel` are `!Send`. On
//! `wasm32-unknown-unknown` there is only one thread, so the handshake state machine
//! runs in a `spawn_local` **driver task** that owns the JS handles, and the trait
//! objects hold only a [`SendWrapper`]`<Rc<RefCell<Shared>>>` — `SendWrapper` is
//! unconditionally `Send + Sync` and asserts single-thread access at runtime, which
//! always holds here. The trait bound is satisfied at the type level; nothing ever
//! actually crosses a thread.
//!
//! # The poll ↔ callback bridge
//!
//! iroh's `poll_recv`/`poll_send` are poll-based; the data channel is callback-based
//! (`onmessage`). They meet at [`Shared`]:
//!
//! * an `onmessage` closure pushes inbound bytes (tagged with the remote
//!   [`CustomAddr`]) into `recv_queue` and wakes the stored `recv_waker`;
//! * `poll_recv` drains `recv_queue` into iroh's buffers, or parks its waker;
//! * `poll_send` looks up the peer's open data channel and writes the packet
//!   synchronously; with no channel yet it kicks a lazy dial and drops the packet
//!   (QUIC retransmits, and the relay path carries it meanwhile).
//!
//! Signaling (SDP offer/answer + ICE) rides the *existing* rendezvous WebSocket —
//! see [`super::rendezvous`], which pumps [`Command`]s in and [`SignalOut`]s out
//! through the [`WebRtcSignalPort`] this module hands it.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    fmt, io,
    num::NonZeroUsize,
    rc::Rc,
    sync::Arc,
    task::{Context, Poll, Waker},
};

use futures::{StreamExt, channel::mpsc};
use iroh::endpoint::transports::{
    CustomEndpoint, CustomSender, CustomTransport, RecvInfo, Transmit,
};
use iroh_base::CustomAddr;
use noq_udp::RecvMeta;
use send_wrapper::SendWrapper;
use serde::{Deserialize, Serialize};
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{
    MessageEvent, RtcConfiguration, RtcDataChannel, RtcDataChannelEvent, RtcDataChannelInit,
    RtcDataChannelState, RtcDataChannelType, RtcIceCandidateInit, RtcPeerConnection,
    RtcPeerConnectionIceEvent, RtcSdpType, RtcSessionDescriptionInit, RtcSignalingState,
};

/// Our custom-transport id. The bytes spell `WebRTC` (`0x57 65 62 52 54 43`); it is
/// not in iroh's `TRANSPORTS.md` registry (interop with other iroh apps is not a
/// goal — only two walkie tabs ever speak it), just a stable, self-documenting tag
/// that distinguishes our custom addrs from any other custom transport's.
pub const WEBRTC_TRANSPORT_ID: u64 = 0x5765_6252_5443;

/// The data-channel label. Versioned so a future wire-incompatible change can bump
/// it and refuse to interop, mirroring the ALPN discipline in [`super::iroh_common`].
const DATA_CHANNEL_LABEL: &str = "walkie/iroh-quic/1";

/// STUN gets server-reflexive candidates so two peers behind different NATs can
/// find a direct path. **No TURN, ever** — TURN is a relay, and iroh-relay is
/// already our (better) fallback (design §4.1, §6). Same-machine tabs and same-LAN
/// peers connect on host candidates without touching this at all.
const STUN_SERVERS: &[&str] = &["stun:stun.l.google.com:19302"];

// ---------------------------------------------------------------------------
// CustomAddr codec: peer endpoint id <-> TransportAddr::Custom.
// ---------------------------------------------------------------------------

/// The custom address a peer is reachable at over this transport: the endpoint id
/// verbatim under [`WEBRTC_TRANSPORT_ID`]. Deterministic, so neither side has to
/// advertise it — both derive it from the id learned over rendezvous.
pub fn webrtc_custom_addr(endpoint_id: &[u8; 32]) -> CustomAddr {
    CustomAddr::from_parts(WEBRTC_TRANSPORT_ID, endpoint_id)
}

/// Inverse of [`webrtc_custom_addr`]: recover the 32-byte endpoint id, or `None` if
/// the addr belongs to a different transport or is malformed.
pub fn endpoint_bytes_of(addr: &CustomAddr) -> Option<[u8; 32]> {
    if addr.id() != WEBRTC_TRANSPORT_ID {
        return None;
    }
    addr.data().try_into().ok()
}

fn short(peer: &[u8; 32]) -> String {
    super::iroh_common::encode_hex(&peer[..4])
}

// ---------------------------------------------------------------------------
// Signaling messages exchanged over the rendezvous channel.
// ---------------------------------------------------------------------------

/// One SDP/ICE payload. Carried inside a `walkie-rtc` envelope on the rendezvous
/// channel (the addressing `from`/`to` lives on the envelope; see
/// [`super::rendezvous`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum RtcPayload {
    /// The offerer's session description.
    Offer { sdp: String },
    /// The answerer's session description.
    Answer { sdp: String },
    /// A trickled ICE candidate.
    Ice {
        candidate: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mid: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<u16>,
    },
}

/// A signaling payload to publish toward `to`, produced by the driver / ICE
/// callbacks and drained by the rendezvous loop.
#[derive(Debug, Clone)]
pub struct SignalOut {
    pub to: [u8; 32],
    pub payload: RtcPayload,
}

/// Work items fed to the driver task. Rendezvous injects both (a discovered
/// rtc-capable peer becomes a [`Command::Dial`]; an inbound `walkie-rtc` payload
/// becomes a [`Command::Signal`]); [`WebRtcSender::poll_send`] injects a `Dial` the
/// first time iroh probes an unconnected custom path.
#[derive(Debug)]
pub enum Command {
    /// Establish (or confirm) a link toward this peer.
    Dial([u8; 32]),
    /// A signaling payload arrived from `from`.
    Signal { from: [u8; 32], payload: RtcPayload },
}

/// The handles the rendezvous loop needs to bridge signaling for this transport.
/// Flows to rendezvous through `RendezvousPeering` (populated by
/// `BrowserNetHandle::rendezvous_peering`), so `browser_host` wiring is unchanged.
///
/// `Clone` (the `outbound` receiver is wrapped so the whole `RendezvousPeering`
/// stays `Clone`); the receiver is `take`n once by the loop that owns it.
#[derive(Debug, Clone)]
pub struct WebRtcSignalPort {
    /// Our own endpoint id — used to fill the `from` field and to drop self-echoes.
    pub local_id: [u8; 32],
    /// Rendezvous → driver: discovered peers and inbound signaling.
    pub commands: mpsc::UnboundedSender<Command>,
    /// Driver → rendezvous: signaling to publish. `take`n once by the loop.
    pub outbound: Rc<RefCell<Option<mpsc::UnboundedReceiver<SignalOut>>>>,
}

// ---------------------------------------------------------------------------
// Shared state: the poll <-> callback rendezvous point.
// ---------------------------------------------------------------------------

/// Everything the JS callbacks, the driver task, and iroh's poll methods share.
/// Single-threaded (`Rc<RefCell<..>>`); never borrowed across an `.await` or a JS
/// call that could re-enter it.
struct Shared {
    local_id: [u8; 32],
    /// Inbound datagrams, each tagged with the remote custom addr, awaiting
    /// `poll_recv`.
    recv_queue: VecDeque<(CustomAddr, Vec<u8>)>,
    /// iroh's recv waker, parked when `recv_queue` is empty.
    recv_waker: Option<Waker>,
    /// Per-peer link state, keyed by remote endpoint id bytes.
    links: HashMap<[u8; 32], PeerLink>,
    /// Peers with an in-flight `Dial` command, so `poll_send` fires at most one
    /// dial per peer before a link exists.
    dialing: HashSet<[u8; 32]>,
    /// `poll_send` uses this to kick a lazy dial.
    cmd_tx: mpsc::UnboundedSender<Command>,
}

/// One peer's WebRTC connection state.
struct PeerLink {
    pc: RtcPeerConnection,
    /// The data channel once created (offerer) or received (answerer).
    channel: Option<RtcDataChannel>,
    /// True once the remote description is set — ICE candidates that arrive before
    /// that must be buffered in `pending_ice` and flushed here.
    remote_desc_set: bool,
    pending_ice: Vec<(String, Option<String>, Option<u16>)>,
    logged_first_send: bool,
    logged_first_recv: bool,
    /// JS closures kept alive for the connection's lifetime.
    _closures: Vec<AnyClosure>,
}

/// Type-erased storage so heterogeneous closures can share one `Vec`; dropping the
/// `PeerLink` drops them and detaches the JS handlers. The inner closures are never
/// read after construction — they exist purely to be kept alive (and dropped) at the
/// right time — so the variants' payloads are intentionally "unused".
#[allow(dead_code)]
enum AnyClosure {
    Unit(Closure<dyn FnMut()>),
    Ice(Closure<dyn FnMut(RtcPeerConnectionIceEvent)>),
    Msg(Closure<dyn FnMut(MessageEvent)>),
    Chan(Closure<dyn FnMut(RtcDataChannelEvent)>),
}

type SharedRef = Rc<RefCell<Shared>>;

// ---------------------------------------------------------------------------
// The three trait objects iroh holds. Each wraps only the Send-safe SharedRef.
// ---------------------------------------------------------------------------

/// The `CustomTransport` iroh registers via `add_custom_transport`. A factory: iroh
/// calls [`CustomTransport::bind`] once when building the endpoint.
#[derive(Clone)]
pub struct WebRtcTransport {
    shared: SendWrapper<SharedRef>,
}

impl fmt::Debug for WebRtcTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WebRtcTransport")
    }
}

impl WebRtcTransport {
    /// Build the transport (and its signaling port) for the local endpoint id.
    ///
    /// Spawns the driver task immediately, so the WebRTC handshake machinery is live
    /// before iroh's endpoint is even built. `local_id` is known up front (it is the
    /// public half of the endpoint secret key). The returned [`WebRtcSignalPort`]
    /// must be handed to the rendezvous loop for any connection to form.
    pub fn new(local_id: [u8; 32]) -> (Self, WebRtcSignalPort) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded::<Command>();
        let (signal_tx, signal_rx) = mpsc::unbounded::<SignalOut>();

        let shared: SharedRef = Rc::new(RefCell::new(Shared {
            local_id,
            recv_queue: VecDeque::new(),
            recv_waker: None,
            links: HashMap::new(),
            dialing: HashSet::new(),
            cmd_tx: cmd_tx.clone(),
        }));

        spawn_local(run_driver(shared.clone(), cmd_rx, signal_tx));

        let transport = Self {
            shared: SendWrapper::new(shared),
        };
        let port = WebRtcSignalPort {
            local_id,
            commands: cmd_tx,
            outbound: Rc::new(RefCell::new(Some(signal_rx))),
        };
        (transport, port)
    }
}

impl CustomTransport for WebRtcTransport {
    fn bind(&self) -> io::Result<Box<dyn CustomEndpoint>> {
        let local_id = self.shared.borrow().local_id;
        // Advertise our own custom addr so `endpoint.addr()` reports it (tickets,
        // diagnostics). Peers do not consume this — they derive it from our id.
        let local_addrs = n0_watcher::Watchable::new(vec![webrtc_custom_addr(&local_id)]);
        tracing::info!(
            target: "walkie::webrtc",
            "custom transport bound; local custom addr = {}",
            webrtc_custom_addr(&local_id)
        );
        Ok(Box::new(WebRtcEndpoint {
            shared: self.shared.clone(),
            local_addrs: SendWrapper::new(local_addrs),
        }))
    }
}

/// The bound `CustomEndpoint`: advertises the local addr and drains inbound
/// datagrams into iroh.
pub struct WebRtcEndpoint {
    shared: SendWrapper<SharedRef>,
    local_addrs: SendWrapper<n0_watcher::Watchable<Vec<CustomAddr>>>,
}

impl fmt::Debug for WebRtcEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WebRtcEndpoint")
    }
}

impl CustomEndpoint for WebRtcEndpoint {
    fn watch_local_addrs(&self) -> n0_watcher::Direct<Vec<CustomAddr>> {
        self.local_addrs.watch()
    }

    fn create_sender(&self) -> Arc<dyn CustomSender> {
        Arc::new(WebRtcSender {
            shared: self.shared.clone(),
        })
    }

    fn poll_recv(
        &mut self,
        cx: &mut Context,
        bufs: &mut [io::IoSliceMut<'_>],
        metas: &mut [RecvMeta],
        recv_infos: &mut [RecvInfo],
    ) -> Poll<io::Result<usize>> {
        assert_eq!(bufs.len(), metas.len(), "non matching bufs & metas");
        assert_eq!(
            bufs.len(),
            recv_infos.len(),
            "non matching bufs & recv_infos"
        );
        let n = bufs.len();
        if n == 0 {
            return Poll::Ready(Ok(0));
        }

        let mut shared = self.shared.borrow_mut();
        let local_addr = webrtc_custom_addr(&shared.local_id);
        let mut count = 0;
        while count < n {
            // Peek the next datagram's size before committing to a buffer slot.
            let next_len = match shared.recv_queue.front() {
                Some((_, data)) => data.len(),
                None => break,
            };
            if bufs[count].len() < next_len {
                // Buffer too small for this datagram. QUIC's recv buffers are sized
                // for a full datagram, so this should not happen; break rather than
                // spin or silently truncate.
                break;
            }
            let (from, data) = shared.recv_queue.pop_front().expect("front just checked");
            bufs[count][..data.len()].copy_from_slice(&data);
            metas[count].len = data.len();
            metas[count].stride = data.len();
            recv_infos[count] = RecvInfo::new(from, Some(local_addr.clone()));
            count += 1;
        }

        if count > 0 {
            Poll::Ready(Ok(count))
        } else {
            // Park the waker; the data-channel `onmessage` closure wakes it.
            shared.recv_waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    fn max_transmit_segments(&self) -> NonZeroUsize {
        NonZeroUsize::MIN
    }
}

/// The `CustomSender`: writes QUIC packets onto the peer's data channel, or kicks a
/// dial if none is up yet.
pub struct WebRtcSender {
    shared: SendWrapper<SharedRef>,
}

impl fmt::Debug for WebRtcSender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WebRtcSender")
    }
}

impl CustomSender for WebRtcSender {
    fn is_valid_send_addr(&self, addr: &CustomAddr) -> bool {
        addr.id() == WEBRTC_TRANSPORT_ID
    }

    fn poll_send(
        &self,
        _cx: &mut Context,
        dst: &CustomAddr,
        _src: Option<&CustomAddr>,
        transmit: &Transmit<'_>,
    ) -> Poll<io::Result<()>> {
        let Some(peer) = endpoint_bytes_of(dst) else {
            return Poll::Ready(Err(io::Error::other("not a webrtc custom addr")));
        };

        let mut shared = self.shared.borrow_mut();
        // GSO: split into per-datagram chunks exactly like the reference transport.
        let seg = transmit
            .segment_size
            .unwrap_or(transmit.contents.len())
            .max(1);

        if let Some(link) = shared.links.get_mut(&peer) {
            if let Some(channel) = link.channel.as_ref() {
                if channel.ready_state() == RtcDataChannelState::Open {
                    for chunk in transmit.contents.chunks(seg) {
                        // Unreliable/unordered send; a JS error just drops the
                        // datagram, which QUIC treats as loss.
                        let _ = channel.send_with_u8_array(chunk);
                    }
                    if !link.logged_first_send {
                        link.logged_first_send = true;
                        tracing::info!(
                            target: "walkie::webrtc",
                            "first datagram sent over DIRECT webrtc path to {}",
                            short(&peer)
                        );
                    }
                    return Poll::Ready(Ok(()));
                }
            }
        }

        // No open channel yet: kick a single lazy dial and drop the packet. QUIC
        // retransmits; the relay path (backup) carries traffic until the channel is
        // up, at which point iroh's path selector migrates onto it.
        if !shared.links.contains_key(&peer) && shared.dialing.insert(peer) {
            let _ = shared.cmd_tx.unbounded_send(Command::Dial(peer));
        }
        Poll::Ready(Ok(()))
    }
}

// ---------------------------------------------------------------------------
// The driver task: owns the RtcPeerConnection handshake state machine.
// ---------------------------------------------------------------------------

/// Serially processes [`Command`]s. Serial handling is what dedupes dials: each
/// handler fully creates a peer's link (inserting it into `Shared.links`) before the
/// next command is dequeued, so a later `Dial` for the same peer is a no-op.
async fn run_driver(
    shared: SharedRef,
    mut commands: mpsc::UnboundedReceiver<Command>,
    signal_tx: mpsc::UnboundedSender<SignalOut>,
) {
    while let Some(cmd) = commands.next().await {
        match cmd {
            Command::Dial(peer) => handle_dial(&shared, &signal_tx, peer).await,
            Command::Signal { from, payload } => {
                handle_signal(&shared, &signal_tx, from, payload).await
            }
        }
    }
}

/// STUN-configured `RTCConfiguration`. Rebuilt per connection (cheap).
fn configuration() -> RtcConfiguration {
    let ice_servers = js_sys::Array::new();
    for url in STUN_SERVERS {
        let server = js_sys::Object::new();
        let urls = js_sys::Array::new();
        urls.push(&JsValue::from_str(url));
        let _ = js_sys::Reflect::set(&server, &JsValue::from_str("urls"), &urls);
        ice_servers.push(&server);
    }
    let config = RtcConfiguration::new();
    config.set_ice_servers(&ice_servers);
    config
}

/// Wire a data channel's `onmessage`/`onopen` and register its closures. Captures a
/// clone of the `Rc` (not a borrow), so it is safe to call while `Shared` is
/// borrowed elsewhere.
fn wire_channel(
    shared: &SharedRef,
    peer: [u8; 32],
    channel: &RtcDataChannel,
    closures: &mut Vec<AnyClosure>,
) {
    channel.set_binary_type(RtcDataChannelType::Arraybuffer);

    let shared_msg = shared.clone();
    let remote_addr = webrtc_custom_addr(&peer);
    let onmessage = Closure::wrap(Box::new(move |event: MessageEvent| {
        let bytes = js_sys::Uint8Array::new(&event.data()).to_vec();
        let mut s = shared_msg.borrow_mut();
        if let Some(link) = s.links.get_mut(&peer) {
            if !link.logged_first_recv {
                link.logged_first_recv = true;
                tracing::info!(
                    target: "walkie::webrtc",
                    "first datagram received over DIRECT webrtc path from {}",
                    short(&peer)
                );
            }
        }
        s.recv_queue.push_back((remote_addr.clone(), bytes));
        if let Some(waker) = s.recv_waker.take() {
            waker.wake();
        }
    }) as Box<dyn FnMut(MessageEvent)>);
    channel.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));

    let onopen = Closure::wrap(Box::new(move || {
        tracing::info!(
            target: "walkie::webrtc",
            "data channel OPEN to {} — direct path is live",
            short(&peer)
        );
    }) as Box<dyn FnMut()>);
    channel.set_onopen(Some(onopen.as_ref().unchecked_ref()));

    closures.push(AnyClosure::Msg(onmessage));
    closures.push(AnyClosure::Unit(onopen));
}

/// Store an inbound (answerer-side) data channel into its link and wire it.
fn attach_incoming_channel(shared: &SharedRef, peer: [u8; 32], channel: RtcDataChannel) {
    let mut closures = Vec::new();
    wire_channel(shared, peer, &channel, &mut closures);
    let mut s = shared.borrow_mut();
    if let Some(link) = s.links.get_mut(&peer) {
        link.channel = Some(channel);
        link._closures.extend(closures);
    }
}

/// Ensure a `PeerLink` exists toward `peer`, creating and wiring the
/// `RtcPeerConnection` if not. Returns `(pc, created)` — `created` is true only on
/// first creation, so the caller offers exactly once. Returns `None` for our own id
/// or on `RtcPeerConnection` construction failure.
fn ensure_link(
    shared: &SharedRef,
    signal_tx: &mpsc::UnboundedSender<SignalOut>,
    peer: [u8; 32],
    offerer: bool,
) -> Option<(RtcPeerConnection, bool)> {
    let mut s = shared.borrow_mut();
    if peer == s.local_id {
        return None;
    }
    if let Some(link) = s.links.get(&peer) {
        return Some((link.pc.clone(), false));
    }

    let pc = match RtcPeerConnection::new_with_configuration(&configuration()) {
        Ok(pc) => pc,
        Err(error) => {
            tracing::warn!(
                target: "walkie::webrtc",
                "RtcPeerConnection::new failed for {}: {error:?}",
                short(&peer)
            );
            return None;
        }
    };

    let mut closures: Vec<AnyClosure> = Vec::new();

    // Trickle ICE: forward each local candidate to the peer over signaling.
    let ice_tx = signal_tx.clone();
    let onicecandidate = Closure::wrap(Box::new(move |event: RtcPeerConnectionIceEvent| {
        if let Some(candidate) = event.candidate() {
            let _ = ice_tx.unbounded_send(SignalOut {
                to: peer,
                payload: RtcPayload::Ice {
                    candidate: candidate.candidate(),
                    mid: candidate.sdp_mid(),
                    index: candidate.sdp_m_line_index(),
                },
            });
        }
    }) as Box<dyn FnMut(RtcPeerConnectionIceEvent)>);
    pc.set_onicecandidate(Some(onicecandidate.as_ref().unchecked_ref()));
    closures.push(AnyClosure::Ice(onicecandidate));

    // Observability: log every connectionState transition (design risk #6).
    let pc_for_state = pc.clone();
    let onstate = Closure::wrap(Box::new(move || {
        tracing::info!(
            target: "walkie::webrtc",
            "peer {} connectionState = {:?}",
            short(&peer),
            pc_for_state.connection_state()
        );
    }) as Box<dyn FnMut()>);
    pc.set_onconnectionstatechange(Some(onstate.as_ref().unchecked_ref()));
    closures.push(AnyClosure::Unit(onstate));

    let mut channel = None;
    if offerer {
        // The offerer creates the (unreliable, unordered) data channel *before* the
        // offer, so the generated SDP carries its m-line.
        let init = RtcDataChannelInit::new();
        init.set_ordered(false);
        init.set_max_retransmits(0);
        let dc = pc.create_data_channel_with_data_channel_dict(DATA_CHANNEL_LABEL, &init);
        wire_channel(shared, peer, &dc, &mut closures);
        channel = Some(dc);
    } else {
        // The answerer receives the channel via `ondatachannel`.
        let shared_for_dc = shared.clone();
        let ondatachannel = Closure::wrap(Box::new(move |event: RtcDataChannelEvent| {
            attach_incoming_channel(&shared_for_dc, peer, event.channel());
        }) as Box<dyn FnMut(RtcDataChannelEvent)>);
        pc.set_ondatachannel(Some(ondatachannel.as_ref().unchecked_ref()));
        closures.push(AnyClosure::Chan(ondatachannel));
    }

    s.links.insert(
        peer,
        PeerLink {
            pc: pc.clone(),
            channel,
            remote_desc_set: false,
            pending_ice: Vec::new(),
            logged_first_send: false,
            logged_first_recv: false,
            _closures: closures,
        },
    );
    s.dialing.remove(&peer);
    Some((pc, true))
}

/// Handle a dial request: the offerer (lower id) creates and sends an offer; the
/// answerer just readies its `RtcPeerConnection` and waits for the incoming offer.
async fn handle_dial(
    shared: &SharedRef,
    signal_tx: &mpsc::UnboundedSender<SignalOut>,
    peer: [u8; 32],
) {
    let local_id = shared.borrow().local_id;
    if peer == local_id {
        return;
    }
    let offerer = local_id < peer;
    let Some((pc, created)) = ensure_link(shared, signal_tx, peer, offerer) else {
        return;
    };
    if !offerer || !created {
        return; // answerer waits; a repeat dial to an existing link is a no-op.
    }

    // Offerer: createOffer -> setLocalDescription -> publish the offer.
    let offer = match JsFuture::from(pc.create_offer()).await {
        Ok(offer) => offer.unchecked_into::<RtcSessionDescriptionInit>(),
        Err(error) => {
            tracing::warn!(target: "walkie::webrtc", "createOffer failed for {}: {error:?}", short(&peer));
            return;
        }
    };
    if let Err(error) = JsFuture::from(pc.set_local_description(&offer)).await {
        tracing::warn!(target: "walkie::webrtc", "setLocalDescription(offer) failed for {}: {error:?}", short(&peer));
        return;
    }
    if let Some(sdp) = offer.get_sdp() {
        tracing::info!(target: "walkie::webrtc", "dialing {} — sending offer", short(&peer));
        let _ = signal_tx.unbounded_send(SignalOut {
            to: peer,
            payload: RtcPayload::Offer { sdp },
        });
    }
}

/// Handle an inbound signaling payload from `from`.
async fn handle_signal(
    shared: &SharedRef,
    signal_tx: &mpsc::UnboundedSender<SignalOut>,
    from: [u8; 32],
    payload: RtcPayload,
) {
    if from == shared.borrow().local_id {
        return;
    }
    match payload {
        RtcPayload::Offer { sdp } => {
            // We are the answerer. Create the link if the offer beat our own dial.
            let Some((pc, _)) = ensure_link(shared, signal_tx, from, false) else {
                return;
            };
            let desc = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
            desc.set_sdp(&sdp);
            if let Err(error) = JsFuture::from(pc.set_remote_description(&desc)).await {
                tracing::warn!(target: "walkie::webrtc", "setRemoteDescription(offer) failed for {}: {error:?}", short(&from));
                return;
            }
            flush_after_remote_desc(shared, from, &pc).await;

            let answer = match JsFuture::from(pc.create_answer()).await {
                Ok(answer) => answer.unchecked_into::<RtcSessionDescriptionInit>(),
                Err(error) => {
                    tracing::warn!(target: "walkie::webrtc", "createAnswer failed for {}: {error:?}", short(&from));
                    return;
                }
            };
            if let Err(error) = JsFuture::from(pc.set_local_description(&answer)).await {
                tracing::warn!(target: "walkie::webrtc", "setLocalDescription(answer) failed for {}: {error:?}", short(&from));
                return;
            }
            if let Some(sdp) = answer.get_sdp() {
                tracing::info!(target: "walkie::webrtc", "answering {}", short(&from));
                let _ = signal_tx.unbounded_send(SignalOut {
                    to: from,
                    payload: RtcPayload::Answer { sdp },
                });
            }
        }
        RtcPayload::Answer { sdp } => {
            // We are the offerer; the link already exists.
            let pc = shared.borrow().links.get(&from).map(|link| link.pc.clone());
            let Some(pc) = pc else {
                tracing::warn!(target: "walkie::webrtc", "answer from {} with no link", short(&from));
                return;
            };
            // Only apply an answer while we are the offerer awaiting one. A
            // duplicate or late answer (e.g. two same-identity browser tabs crossing
            // signaling over the relay) would otherwise hit setRemoteDescription on
            // an already-`stable` PC and throw InvalidStateError. Perfect-negotiation
            // guard: ignore any answer outside `have-local-offer`.
            if pc.signaling_state() != RtcSignalingState::HaveLocalOffer {
                tracing::debug!(
                    target: "walkie::webrtc",
                    "ignoring answer from {} in signaling state {:?} (duplicate/late)",
                    short(&from),
                    pc.signaling_state()
                );
                return;
            }
            let desc = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
            desc.set_sdp(&sdp);
            if let Err(error) = JsFuture::from(pc.set_remote_description(&desc)).await {
                tracing::warn!(target: "walkie::webrtc", "setRemoteDescription(answer) failed for {}: {error:?}", short(&from));
                return;
            }
            flush_after_remote_desc(shared, from, &pc).await;
        }
        RtcPayload::Ice {
            candidate,
            mid,
            index,
        } => {
            // Add now if the remote description is set; otherwise buffer (adding a
            // candidate before setRemoteDescription throws). Create an answerer link
            // if ICE somehow beat the offer.
            let state = shared
                .borrow()
                .links
                .get(&from)
                .map(|link| (link.pc.clone(), link.remote_desc_set));
            match state {
                Some((pc, true)) => {
                    add_ice(&pc, &candidate, mid.as_deref(), index).await;
                }
                Some((_, false)) => buffer_ice(shared, from, candidate, mid, index),
                None => {
                    let _ = ensure_link(shared, signal_tx, from, false);
                    buffer_ice(shared, from, candidate, mid, index);
                }
            }
        }
    }
}

fn buffer_ice(
    shared: &SharedRef,
    peer: [u8; 32],
    candidate: String,
    mid: Option<String>,
    index: Option<u16>,
) {
    if let Some(link) = shared.borrow_mut().links.get_mut(&peer) {
        link.pending_ice.push((candidate, mid, index));
    }
}

/// Mark the remote description set and flush any ICE candidates that arrived early.
async fn flush_after_remote_desc(shared: &SharedRef, peer: [u8; 32], pc: &RtcPeerConnection) {
    let pending = {
        let mut s = shared.borrow_mut();
        match s.links.get_mut(&peer) {
            Some(link) => {
                link.remote_desc_set = true;
                std::mem::take(&mut link.pending_ice)
            }
            None => Vec::new(),
        }
    };
    for (candidate, mid, index) in pending {
        add_ice(pc, &candidate, mid.as_deref(), index).await;
    }
}

async fn add_ice(pc: &RtcPeerConnection, candidate: &str, mid: Option<&str>, index: Option<u16>) {
    let init = RtcIceCandidateInit::new(candidate);
    init.set_sdp_mid(mid);
    init.set_sdp_m_line_index(index);
    if let Err(error) =
        JsFuture::from(pc.add_ice_candidate_with_opt_rtc_ice_candidate_init(Some(&init))).await
    {
        tracing::debug!(target: "walkie::webrtc", "addIceCandidate failed: {error:?}");
    }
}

#[cfg(test)]
mod tests {
    //! These compile and run under a wasm test runner (`wasm-bindgen-test`); the
    //! whole module is `wasm32`-gated, so they do not run under a native
    //! `cargo test`. The build gate for this work is the wasm `cargo build`.
    use super::*;

    #[test]
    fn custom_addr_round_trips_through_the_transport_id() {
        let id = [0x5a_u8; 32];
        let addr = webrtc_custom_addr(&id);
        assert_eq!(addr.id(), WEBRTC_TRANSPORT_ID);
        assert_eq!(endpoint_bytes_of(&addr), Some(id));
    }

    #[test]
    fn foreign_transport_ids_are_rejected() {
        let foreign = CustomAddr::from_parts(0x20, &[0x11; 32]);
        assert_eq!(endpoint_bytes_of(&foreign), None);
    }

    #[test]
    fn malformed_data_is_rejected() {
        let short = CustomAddr::from_parts(WEBRTC_TRANSPORT_ID, &[0x01, 0x02, 0x03]);
        assert_eq!(endpoint_bytes_of(&short), None);
    }
}
