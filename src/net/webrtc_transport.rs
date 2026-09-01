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
    time::Duration,
};

use futures::{
    StreamExt,
    channel::{mpsc, oneshot},
};
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
const DATA_CHANNEL_LABEL: &str = "walkie/iroh-quic/2";

/// A reliable-unordered side channel for already authenticated compact-session
/// carriers. QUIC still uses [`DATA_CHANNEL_LABEL`] with datagram semantics;
/// this lane only accelerates tiny pair-session Events while the ordinary gossip
/// broadcast remains the interoperable native/multi-hop fallback.
const SESSION_CHANNEL_LABEL: &str = "walkie/session-carrier/1";
const MAX_DIRECT_SESSION_FRAME_BYTES: usize = 16 * 1024;
const DIRECT_SESSION_QUEUE_DEPTH: usize = 64;
const MAX_DIRECT_SESSION_BUFFERED_BYTES: u32 = 64 * 1024;

/// One WebRTC offer attempt. The endpoint id is intentionally stable across a
/// room-placement restart, so it cannot fence callbacks or signaling from the
/// retired browser `RTCPeerConnection`. A fresh random attempt is minted by the
/// deterministic offerer for every replacement link and carried by every SDP/ICE
/// payload belonging to that link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RtcAttempt([u8; 8]);

impl RtcAttempt {
    fn fresh() -> Self {
        loop {
            let value = rand::random::<[u8; 8]>();
            if value != [0; 8] {
                return Self(value);
            }
        }
    }
}

impl fmt::Display for RtcAttempt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// STUN gets server-reflexive candidates so two peers behind different NATs can
/// find a direct path. **No TURN, ever** — TURN is a relay, and iroh-relay is
/// already our (better) fallback (design §4.1, §6). Same-machine tabs and same-LAN
/// peers connect on host candidates without touching this at all.
const STUN_SERVERS: &[&str] = &["stun:stun.l.google.com:19302"];
const MAX_SIGNALING_PEERS: usize = 64;

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
    Offer { attempt: RtcAttempt, sdp: String },
    /// The answerer's session description.
    Answer { attempt: RtcAttempt, sdp: String },
    /// A trickled ICE candidate.
    Ice {
        attempt: RtcAttempt,
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
    /// Rendezvous observed a previously known endpoint id under a fresh browser
    /// placement incarnation. Retire the old carrier immediately; the
    /// deterministic offerer originates a fresh attempt, while the answerer waits.
    PeerReincarnated {
        peer: [u8; 32],
        acknowledged: oneshot::Sender<ReincarnationFence>,
    },
    /// A signaling payload arrived from `from`.
    Signal { from: [u8; 32], payload: RtcPayload },
    /// A callback observed a terminal state for this exact offer attempt. Late
    /// callbacks from a retired link cannot evict its replacement.
    LinkTerminal { peer: [u8; 32], attempt: RtcAttempt },
}

#[derive(Debug, Clone, Copy)]
pub struct ReincarnationFence {
    retired_attempt: Option<RtcAttempt>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DirectReady {
    pub(crate) peer: [u8; 32],
    pub(crate) attempt: RtcAttempt,
}

#[derive(Debug)]
pub(crate) struct DirectSessionFrame {
    pub(crate) peer: [u8; 32],
    pub(crate) bytes: Vec<u8>,
}

/// The handles the rendezvous loop needs to bridge signaling for this transport.
/// Flows to rendezvous through `RendezvousPeering` (populated by
/// `BrowserNetHandle::rendezvous_peering`), so `browser_host` wiring is unchanged.
///
/// `Clone` (the `outbound` receiver is wrapped so the whole `RendezvousPeering`
/// stays `Clone`); the receiver is `take`n once by the loop that owns it.
#[derive(Clone)]
pub struct WebRtcSignalPort {
    /// Our own endpoint id — used to fill the `from` field and to drop self-echoes.
    pub local_id: [u8; 32],
    /// Rendezvous → driver: discovered peers and inbound signaling.
    pub commands: mpsc::UnboundedSender<Command>,
    /// Driver → rendezvous: signaling to publish. `take`n once by the loop.
    pub outbound: Rc<RefCell<Option<mpsc::UnboundedReceiver<SignalOut>>>>,
    shared: SharedRef,
    ready: async_broadcast::Sender<DirectReady>,
    /// Keeps the broadcast open across brief intervals with no active waiter.
    /// `async-broadcast` closes permanently when its final receiver disappears.
    _ready_keepalive: async_broadcast::InactiveReceiver<DirectReady>,
    direct_session_inbound: Rc<RefCell<Option<mpsc::Receiver<DirectSessionFrame>>>>,
    incarnation: [u8; 8],
}

impl fmt::Debug for WebRtcSignalPort {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebRtcSignalPort")
            .finish_non_exhaustive()
    }
}

impl WebRtcSignalPort {
    /// Random identity for this concrete browser transport placement. It changes
    /// when a room generation reopens even though the persisted endpoint id does
    /// not, allowing rendezvous peers to distinguish reincarnation from keepalive.
    pub fn incarnation(&self) -> [u8; 8] {
        self.incarnation
    }

    /// Queue a same-identity placement replacement and resolve only after the
    /// serial carrier driver has retired the old attempt (and originated a new
    /// one when this endpoint is the deterministic offerer).
    pub async fn reincarnate(&self, peer: [u8; 32]) -> Option<ReincarnationFence> {
        let (acknowledged, response) = oneshot::channel();
        self.commands
            .unbounded_send(Command::PeerReincarnated { peer, acknowledged })
            .ok()?;
        response.await.ok()
    }

    pub(crate) fn subscribe_ready(&self) -> async_broadcast::Receiver<DirectReady> {
        self.ready.new_receiver()
    }

    pub(crate) fn take_direct_session_inbound(&self) -> Option<mpsc::Receiver<DirectSessionFrame>> {
        self.direct_session_inbound.borrow_mut().take()
    }

    /// Best-effort low-latency delivery of one compact session Event to a known
    /// browser peer. `false` means the caller must rely on its ordinary gossip
    /// broadcast; it never means the authenticated Event was rejected.
    pub(crate) fn try_send_session(&self, peer: [u8; 32], bytes: &[u8]) -> bool {
        if bytes.len() > MAX_DIRECT_SESSION_FRAME_BYTES {
            return false;
        }
        let mut shared = self.shared.borrow_mut();
        let Some(link) = shared.links.get_mut(&peer) else {
            return false;
        };
        if link.pc.connection_state() != web_sys::RtcPeerConnectionState::Connected {
            return false;
        }
        let Some(channel) = link.session_channel.as_ref() else {
            return false;
        };
        if channel.ready_state() != RtcDataChannelState::Open
            || channel.buffered_amount().saturating_add(bytes.len() as u32)
                > MAX_DIRECT_SESSION_BUFFERED_BYTES
        {
            return false;
        }
        let sent = channel.send_with_u8_array(bytes).is_ok();
        if sent && !link.logged_first_session_send {
            link.logged_first_session_send = true;
            tracing::info!(
                target: "walkie::webrtc",
                "first compact session Event sent over DIRECT reliable WebRTC lane to {} attempt={}",
                short(&peer),
                link.attempt
            );
        }
        sent
    }

    pub fn is_connected(&self, peer: &[u8; 32]) -> bool {
        self.shared
            .try_borrow()
            .ok()
            .and_then(|shared| {
                shared
                    .links
                    .get(peer)
                    .and_then(|link| link.channel.as_ref().cloned())
            })
            .is_some_and(|channel| channel.ready_state() == RtcDataChannelState::Open)
    }

    /// Event-driven direct-path readiness used by rendezvous before it asks
    /// gossip to establish a long-lived neighbor connection.
    pub async fn wait_connected(&self, peer: [u8; 32], timeout: Duration) -> bool {
        if self.is_connected(&peer) {
            return true;
        }
        let mut ready = self.ready.new_receiver();
        if self.is_connected(&peer) {
            return true;
        }
        n0_future::time::timeout(timeout, async {
            loop {
                match ready.recv().await {
                    Ok(connected) if connected.peer == peer => return true,
                    Ok(_) | Err(async_broadcast::RecvError::Overflowed(_)) => {}
                    Err(async_broadcast::RecvError::Closed) => return false,
                }
            }
        })
        .await
        .unwrap_or(false)
    }

    /// Wait for an open link whose offer attempt is not the one acknowledged as
    /// retired. This cannot be satisfied by the old still-open channel between
    /// enqueueing and processing [`Command::PeerReincarnated`].
    pub async fn wait_fresh_connected(
        &self,
        peer: [u8; 32],
        fence: ReincarnationFence,
        timeout: Duration,
    ) -> bool {
        let is_fresh = || {
            self.shared.try_borrow().ok().is_some_and(|shared| {
                shared.links.get(&peer).is_some_and(|link| {
                    Some(link.attempt) != fence.retired_attempt
                        && link.channel.as_ref().is_some_and(|channel| {
                            channel.ready_state() == RtcDataChannelState::Open
                        })
                })
            })
        };
        if is_fresh() {
            return true;
        }
        let mut ready = self.ready.new_receiver();
        if is_fresh() {
            return true;
        }
        n0_future::time::timeout(timeout, async {
            loop {
                match ready.recv().await {
                    Ok(connected)
                        if connected.peer == peer
                            && Some(connected.attempt) != fence.retired_attempt =>
                    {
                        return true;
                    }
                    Ok(_) | Err(async_broadcast::RecvError::Overflowed(_)) => {}
                    Err(async_broadcast::RecvError::Closed) => return false,
                }
            }
        })
        .await
        .unwrap_or(false)
    }
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
    /// At most one pre-offer ICE attempt per peer. This keeps candidate ordering
    /// correct without allowing ICE alone to retire a live link. Each vector is
    /// independently bounded by [`MAX_EARLY_ICE_CANDIDATES`].
    early_ice: HashMap<[u8; 32], EarlyIce>,
    /// Peers with an in-flight `Dial` command, so `poll_send` fires at most one
    /// dial per peer before a link exists.
    dialing: HashSet<[u8; 32]>,
    /// `poll_send` uses this to kick a lazy dial.
    cmd_tx: mpsc::UnboundedSender<Command>,
    /// Data-channel readiness notifications for direct-first rendezvous.
    ready: async_broadcast::Sender<DirectReady>,
    direct_session_tx: mpsc::Sender<DirectSessionFrame>,
}

/// One peer's WebRTC connection state.
struct PeerLink {
    pc: RtcPeerConnection,
    attempt: RtcAttempt,
    /// The data channel once created (offerer) or received (answerer).
    channel: Option<RtcDataChannel>,
    /// Reliable-unordered application lane for compact pair-session Events.
    /// It carries only authenticated session bytes and never canonical records.
    session_channel: Option<RtcDataChannel>,
    /// True once the remote description is set — ICE candidates that arrive before
    /// that must be buffered in `pending_ice` and flushed here.
    remote_desc_set: bool,
    pending_ice: Vec<(String, Option<String>, Option<u16>)>,
    logged_first_send: bool,
    logged_first_recv: bool,
    logged_first_session_send: bool,
    logged_first_session_recv: bool,
    /// Cached offer makes a known-peer Hello a bounded signaling retry for the
    /// same attempt. This repairs a lost rendezvous fan-out without replacing
    /// a still-valid attempt or reopening any transport authority.
    offer_sdp: Option<String>,
    /// Cached answer makes an exact duplicate Offer idempotent without applying
    /// the same remote description twice.
    answer_sdp: Option<String>,
    /// JS closures kept alive for the connection's lifetime.
    _closures: Vec<AnyClosure>,
}

struct EarlyIce {
    attempt: RtcAttempt,
    candidates: Vec<(String, Option<String>, Option<u16>)>,
}

const MAX_EARLY_ICE_CANDIDATES: usize = 64;

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
        let (direct_session_tx, direct_session_rx) = mpsc::channel(DIRECT_SESSION_QUEUE_DEPTH);
        let (mut ready, ready_rx) = async_broadcast::broadcast(64);
        ready.set_overflow(true);
        let ready_keepalive = ready_rx.deactivate();
        let incarnation = RtcAttempt::fresh().0;

        let shared: SharedRef = Rc::new(RefCell::new(Shared {
            local_id,
            recv_queue: VecDeque::new(),
            recv_waker: None,
            links: HashMap::new(),
            early_ice: HashMap::new(),
            dialing: HashSet::new(),
            cmd_tx: cmd_tx.clone(),
            ready: ready.clone(),
            direct_session_tx,
        }));

        spawn_local(run_driver(shared.clone(), cmd_rx, signal_tx));

        let transport = Self {
            shared: SendWrapper::new(shared.clone()),
        };
        let port = WebRtcSignalPort {
            local_id,
            commands: cmd_tx,
            outbound: Rc::new(RefCell::new(Some(signal_rx))),
            shared,
            ready,
            _ready_keepalive: ready_keepalive,
            direct_session_inbound: Rc::new(RefCell::new(Some(direct_session_rx))),
            incarnation,
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
            Command::PeerReincarnated { peer, acknowledged } => {
                let prior = current_attempt(&shared, peer);
                if let Some(attempt) = prior {
                    tracing::info!(
                        target: "walkie::webrtc",
                        "peer {} placement reincarnated; retiring offer attempt {}",
                        short(&peer),
                        attempt
                    );
                    retire_link(&shared, peer, attempt);
                }
                handle_dial(&shared, &signal_tx, peer).await;
                let _ = acknowledged.send(ReincarnationFence {
                    retired_attempt: prior,
                });
            }
            Command::Signal { from, payload } => {
                handle_signal(&shared, &signal_tx, from, payload).await
            }
            Command::LinkTerminal { peer, attempt } => {
                if current_attempt(&shared, peer) == Some(attempt) {
                    retire_link(&shared, peer, attempt);
                    // Only the deterministic offerer originates a replacement;
                    // `handle_dial` leaves the answerer ready to receive one.
                    handle_dial(&shared, &signal_tx, peer).await;
                }
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

fn current_attempt(shared: &SharedRef, peer: [u8; 32]) -> Option<RtcAttempt> {
    shared.borrow().links.get(&peer).map(|link| link.attempt)
}

fn attempt_is_current(shared: &SharedRef, peer: [u8; 32], attempt: RtcAttempt) -> bool {
    current_attempt(shared, peer) == Some(attempt)
}

/// Remove only the exact attempt being retired. Every JS handler is detached
/// before close, and every callback also checks the attempt, so a queued callback
/// from this connection cannot mutate or evict its replacement.
fn retire_link(shared: &SharedRef, peer: [u8; 32], attempt: RtcAttempt) -> bool {
    let removed = {
        let mut state = shared.borrow_mut();
        if state.links.get(&peer).map(|link| link.attempt) != Some(attempt) {
            return false;
        }
        state.dialing.remove(&peer);
        state.links.remove(&peer)
    };
    let Some(link) = removed else {
        return false;
    };
    link.pc.set_onicecandidate(None);
    link.pc.set_onconnectionstatechange(None);
    link.pc.set_ondatachannel(None);
    if let Some(channel) = link.channel.as_ref() {
        channel.set_onmessage(None);
        channel.set_onopen(None);
        channel.close();
    }
    if let Some(channel) = link.session_channel.as_ref() {
        channel.set_onmessage(None);
        channel.set_onopen(None);
        channel.close();
    }
    link.pc.close();
    true
}

/// Wire a data channel's `onmessage`/`onopen` and register its closures. Captures a
/// clone of the `Rc` (not a borrow), so it is safe to call while `Shared` is
/// borrowed elsewhere.
fn wire_channel(
    shared: &SharedRef,
    ready: async_broadcast::Sender<DirectReady>,
    peer: [u8; 32],
    attempt: RtcAttempt,
    channel: &RtcDataChannel,
    closures: &mut Vec<AnyClosure>,
) {
    channel.set_binary_type(RtcDataChannelType::Arraybuffer);

    let shared_msg = shared.clone();
    let remote_addr = webrtc_custom_addr(&peer);
    let onmessage = Closure::wrap(Box::new(move |event: MessageEvent| {
        if !attempt_is_current(&shared_msg, peer, attempt) {
            return;
        }
        let bytes = js_sys::Uint8Array::new(&event.data()).to_vec();
        let mut s = shared_msg.borrow_mut();
        if let Some(link) = s
            .links
            .get_mut(&peer)
            .filter(|link| link.attempt == attempt)
        {
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
        let waker = s.recv_waker.take();
        drop(s);
        if let Some(waker) = waker {
            waker.wake();
        }
    }) as Box<dyn FnMut(MessageEvent)>);
    channel.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));

    let shared_open = shared.clone();
    let onopen = Closure::wrap(Box::new(move || {
        if !attempt_is_current(&shared_open, peer, attempt) {
            return;
        }
        tracing::info!(
            target: "walkie::webrtc",
            "data channel OPEN to {} attempt={} — fresh direct path is live",
            short(&peer),
            attempt
        );
        let _ = ready.try_broadcast(DirectReady { peer, attempt });
    }) as Box<dyn FnMut()>);
    channel.set_onopen(Some(onopen.as_ref().unchecked_ref()));

    closures.push(AnyClosure::Msg(onmessage));
    closures.push(AnyClosure::Unit(onopen));
}

fn wire_session_channel(
    shared: &SharedRef,
    peer: [u8; 32],
    attempt: RtcAttempt,
    channel: &RtcDataChannel,
    closures: &mut Vec<AnyClosure>,
) {
    channel.set_binary_type(RtcDataChannelType::Arraybuffer);
    let shared_message = shared.clone();
    let onmessage = Closure::wrap(Box::new(move |event: MessageEvent| {
        if !attempt_is_current(&shared_message, peer, attempt) {
            return;
        }
        let bytes = js_sys::Uint8Array::new(&event.data()).to_vec();
        if bytes.len() > MAX_DIRECT_SESSION_FRAME_BYTES {
            return;
        }
        // The callback cannot await. A full bounded fast-lane queue drops only
        // this acceleration copy; the ordinary gossip copy remains authoritative
        // carrier fallback and HHHS replay fencing deduplicates successful twins.
        let mut state = shared_message.borrow_mut();
        if state
            .direct_session_tx
            .try_send(DirectSessionFrame { peer, bytes })
            .is_ok()
            && let Some(link) = state
                .links
                .get_mut(&peer)
                .filter(|link| link.attempt == attempt)
            && !link.logged_first_session_recv
        {
            link.logged_first_session_recv = true;
            tracing::info!(
                target: "walkie::webrtc",
                "first compact session Event received over DIRECT reliable WebRTC lane from {} attempt={}",
                short(&peer),
                attempt
            );
        }
    }) as Box<dyn FnMut(MessageEvent)>);
    channel.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
    closures.push(AnyClosure::Msg(onmessage));
}

/// Store an inbound (answerer-side) data channel into its link and wire it.
fn attach_incoming_channel(
    shared: &SharedRef,
    ready: async_broadcast::Sender<DirectReady>,
    peer: [u8; 32],
    attempt: RtcAttempt,
    channel: RtcDataChannel,
) {
    if !attempt_is_current(shared, peer, attempt) {
        channel.close();
        return;
    }
    let label = channel.label();
    let mut closures = Vec::new();
    if label == DATA_CHANNEL_LABEL {
        wire_channel(shared, ready, peer, attempt, &channel, &mut closures);
    } else if label == SESSION_CHANNEL_LABEL {
        wire_session_channel(shared, peer, attempt, &channel, &mut closures);
    } else {
        channel.close();
        return;
    }
    let mut s = shared.borrow_mut();
    if let Some(link) = s
        .links
        .get_mut(&peer)
        .filter(|link| link.attempt == attempt)
    {
        if label == DATA_CHANNEL_LABEL {
            link.channel = Some(channel);
        } else {
            link.session_channel = Some(channel);
        }
        link._closures.extend(closures);
    }
}

/// Ensure the exact offer `attempt` has a `PeerLink`, creating and wiring the
/// `RtcPeerConnection` if absent. A caller must explicitly retire a different
/// attempt first; Answer/ICE are never allowed to replace one implicitly.
fn ensure_link(
    shared: &SharedRef,
    signal_tx: &mpsc::UnboundedSender<SignalOut>,
    peer: [u8; 32],
    attempt: RtcAttempt,
    offerer: bool,
) -> Option<(RtcPeerConnection, bool)> {
    let mut s = shared.borrow_mut();
    if peer == s.local_id {
        return None;
    }
    if let Some(link) = s.links.get(&peer) {
        return (link.attempt == attempt).then(|| (link.pc.clone(), false));
    }
    if s.links.len() >= MAX_SIGNALING_PEERS {
        tracing::debug!(
            target: "walkie::webrtc",
            "refusing WebRTC link for {}: peer capacity reached",
            short(&peer)
        );
        return None;
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
    let ready = s.ready.clone();

    // Trickle ICE: forward each local candidate to the peer over signaling.
    let ice_tx = signal_tx.clone();
    let onicecandidate = Closure::wrap(Box::new(move |event: RtcPeerConnectionIceEvent| {
        if let Some(candidate) = event.candidate() {
            let _ = ice_tx.unbounded_send(SignalOut {
                to: peer,
                payload: RtcPayload::Ice {
                    attempt,
                    candidate: candidate.candidate(),
                    mid: candidate.sdp_mid(),
                    index: candidate.sdp_m_line_index(),
                },
            });
        }
    }) as Box<dyn FnMut(RtcPeerConnectionIceEvent)>);
    pc.set_onicecandidate(Some(onicecandidate.as_ref().unchecked_ref()));
    closures.push(AnyClosure::Ice(onicecandidate));

    // Observability and terminal cleanup. `Disconnected` is deliberately not
    // terminal: browsers use it for transient ICE disruption. A fresh placement
    // Hello/Offer remains the stronger reincarnation signal.
    let pc_for_state = pc.clone();
    let terminal_tx = s.cmd_tx.clone();
    let onstate = Closure::wrap(Box::new(move || {
        let state = pc_for_state.connection_state();
        tracing::info!(
            target: "walkie::webrtc",
            "peer {} attempt={} connectionState = {:?}",
            short(&peer),
            attempt,
            state
        );
        if matches!(
            state,
            web_sys::RtcPeerConnectionState::Failed | web_sys::RtcPeerConnectionState::Closed
        ) {
            let _ = terminal_tx.unbounded_send(Command::LinkTerminal { peer, attempt });
        }
    }) as Box<dyn FnMut()>);
    pc.set_onconnectionstatechange(Some(onstate.as_ref().unchecked_ref()));
    closures.push(AnyClosure::Unit(onstate));

    let mut channel = None;
    let mut session_channel = None;
    if offerer {
        // The offerer creates the (unreliable, unordered) data channel *before* the
        // offer, so the generated SDP carries its m-line.
        let init = RtcDataChannelInit::new();
        init.set_ordered(false);
        init.set_max_retransmits(0);
        let dc = pc.create_data_channel_with_data_channel_dict(DATA_CHANNEL_LABEL, &init);
        wire_channel(shared, ready.clone(), peer, attempt, &dc, &mut closures);
        channel = Some(dc);
        let session_init = RtcDataChannelInit::new();
        session_init.set_ordered(false);
        let session_dc =
            pc.create_data_channel_with_data_channel_dict(SESSION_CHANNEL_LABEL, &session_init);
        wire_session_channel(shared, peer, attempt, &session_dc, &mut closures);
        session_channel = Some(session_dc);
    } else {
        // The answerer receives the channel via `ondatachannel`.
        let shared_for_dc = shared.clone();
        let ready_for_dc = ready;
        let ondatachannel = Closure::wrap(Box::new(move |event: RtcDataChannelEvent| {
            attach_incoming_channel(
                &shared_for_dc,
                ready_for_dc.clone(),
                peer,
                attempt,
                event.channel(),
            );
        }) as Box<dyn FnMut(RtcDataChannelEvent)>);
        pc.set_ondatachannel(Some(ondatachannel.as_ref().unchecked_ref()));
        closures.push(AnyClosure::Chan(ondatachannel));
    }

    s.links.insert(
        peer,
        PeerLink {
            pc: pc.clone(),
            attempt,
            channel,
            session_channel,
            remote_desc_set: false,
            pending_ice: Vec::new(),
            logged_first_send: false,
            logged_first_recv: false,
            logged_first_session_send: false,
            logged_first_session_recv: false,
            offer_sdp: None,
            answer_sdp: None,
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
    if !local_is_offerer(local_id, peer) {
        return;
    }
    let retry = shared.borrow().links.get(&peer).and_then(|link| {
        let open = link
            .channel
            .as_ref()
            .is_some_and(|channel| channel.ready_state() == RtcDataChannelState::Open);
        (!open)
            .then(|| link.offer_sdp.clone().map(|sdp| (link.attempt, sdp)))
            .flatten()
    });
    if let Some((attempt, sdp)) = retry {
        tracing::info!(target: "walkie::webrtc", "retrying offer to {} attempt={}", short(&peer), attempt);
        let _ = signal_tx.unbounded_send(SignalOut {
            to: peer,
            payload: RtcPayload::Offer { attempt, sdp },
        });
        return;
    }
    if current_attempt(shared, peer).is_some() {
        return;
    }
    let attempt = RtcAttempt::fresh();
    let Some((pc, created)) = ensure_link(shared, signal_tx, peer, attempt, true) else {
        return;
    };
    if !created {
        return;
    }

    // Offerer: createOffer -> setLocalDescription -> publish the offer.
    let offer = match JsFuture::from(pc.create_offer()).await {
        Ok(offer) => offer.unchecked_into::<RtcSessionDescriptionInit>(),
        Err(error) => {
            tracing::warn!(target: "walkie::webrtc", "createOffer failed for {}: {error:?}", short(&peer));
            retire_link(shared, peer, attempt);
            return;
        }
    };
    if let Err(error) = JsFuture::from(pc.set_local_description(&offer)).await {
        tracing::warn!(target: "walkie::webrtc", "setLocalDescription(offer) failed for {}: {error:?}", short(&peer));
        retire_link(shared, peer, attempt);
        return;
    }
    if let Some(sdp) = offer.get_sdp() {
        let retained = {
            let mut state = shared.borrow_mut();
            if let Some(link) = state
                .links
                .get_mut(&peer)
                .filter(|link| link.attempt == attempt)
            {
                link.offer_sdp = Some(sdp.clone());
                true
            } else {
                false
            }
        };
        if !retained {
            return;
        }
        tracing::info!(target: "walkie::webrtc", "dialing {} attempt={} — sending offer", short(&peer), attempt);
        let _ = signal_tx.unbounded_send(SignalOut {
            to: peer,
            payload: RtcPayload::Offer { attempt, sdp },
        });
    } else {
        retire_link(shared, peer, attempt);
    }
}

fn local_is_offerer(local: [u8; 32], peer: [u8; 32]) -> bool {
    local < peer
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
        RtcPayload::Offer { attempt, sdp } => {
            let local = shared.borrow().local_id;
            if !local_is_offerer(from, local) {
                tracing::debug!(
                    target: "walkie::webrtc",
                    "ignoring offer from non-offerer {} attempt={}",
                    short(&from),
                    attempt
                );
                return;
            }
            // A changed attempt is the unambiguous same-endpoint reincarnation
            // signal. Retire the old connection even if the browser still calls it
            // Connected/Disconnected; those states can lag the remote restart.
            if let Some(current) = current_attempt(shared, from)
                && current != attempt
            {
                tracing::info!(
                    target: "walkie::webrtc",
                    "fresh offer from {} replaces attempt {} with {}",
                    short(&from),
                    current,
                    attempt
                );
                retire_link(shared, from, current);
            }
            let Some((pc, _)) = ensure_link(shared, signal_tx, from, attempt, false) else {
                return;
            };
            adopt_early_ice(shared, from, attempt);

            // An exact duplicate belongs to the current attempt. Re-send the
            // cached answer without reapplying its remote description.
            if let Some(answer_sdp) = shared
                .borrow()
                .links
                .get(&from)
                .filter(|link| link.attempt == attempt)
                .and_then(|link| link.answer_sdp.clone())
            {
                let _ = signal_tx.unbounded_send(SignalOut {
                    to: from,
                    payload: RtcPayload::Answer {
                        attempt,
                        sdp: answer_sdp,
                    },
                });
                return;
            }
            let desc = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
            desc.set_sdp(&sdp);
            if let Err(error) = JsFuture::from(pc.set_remote_description(&desc)).await {
                tracing::warn!(target: "walkie::webrtc", "setRemoteDescription(offer) failed for {} attempt={}: {error:?}", short(&from), attempt);
                retire_link(shared, from, attempt);
                return;
            }
            flush_after_remote_desc(shared, from, attempt, &pc).await;

            let answer = match JsFuture::from(pc.create_answer()).await {
                Ok(answer) => answer.unchecked_into::<RtcSessionDescriptionInit>(),
                Err(error) => {
                    tracing::warn!(target: "walkie::webrtc", "createAnswer failed for {}: {error:?}", short(&from));
                    retire_link(shared, from, attempt);
                    return;
                }
            };
            if let Err(error) = JsFuture::from(pc.set_local_description(&answer)).await {
                tracing::warn!(target: "walkie::webrtc", "setLocalDescription(answer) failed for {}: {error:?}", short(&from));
                retire_link(shared, from, attempt);
                return;
            }
            if let Some(sdp) = answer.get_sdp() {
                if let Some(link) = shared
                    .borrow_mut()
                    .links
                    .get_mut(&from)
                    .filter(|link| link.attempt == attempt)
                {
                    link.answer_sdp = Some(sdp.clone());
                } else {
                    return;
                }
                tracing::info!(target: "walkie::webrtc", "answering {} attempt={}", short(&from), attempt);
                let _ = signal_tx.unbounded_send(SignalOut {
                    to: from,
                    payload: RtcPayload::Answer { attempt, sdp },
                });
            }
        }
        RtcPayload::Answer { attempt, sdp } => {
            let local = shared.borrow().local_id;
            if !local_is_offerer(local, from) {
                tracing::debug!(
                    target: "walkie::webrtc",
                    "ignoring answer from non-answerer {} attempt={}",
                    short(&from),
                    attempt
                );
                return;
            }
            // We are the offerer; the link already exists.
            let pc = shared
                .borrow()
                .links
                .get(&from)
                .filter(|link| link.attempt == attempt)
                .map(|link| link.pc.clone());
            let Some(pc) = pc else {
                tracing::debug!(target: "walkie::webrtc", "ignoring stale answer from {} attempt={}", short(&from), attempt);
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
                tracing::warn!(target: "walkie::webrtc", "setRemoteDescription(answer) failed for {} attempt={}: {error:?}", short(&from), attempt);
                retire_link(shared, from, attempt);
                return;
            }
            flush_after_remote_desc(shared, from, attempt, &pc).await;
        }
        RtcPayload::Ice {
            attempt,
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
                .filter(|link| link.attempt == attempt)
                .map(|link| (link.pc.clone(), link.remote_desc_set));
            match state {
                Some((pc, true)) => {
                    add_ice(&pc, &candidate, mid.as_deref(), index).await;
                }
                Some((_, false)) => buffer_ice(shared, from, attempt, candidate, mid, index),
                None => buffer_early_ice(shared, from, attempt, candidate, mid, index),
            }
        }
    }
}

fn buffer_ice(
    shared: &SharedRef,
    peer: [u8; 32],
    attempt: RtcAttempt,
    candidate: String,
    mid: Option<String>,
    index: Option<u16>,
) {
    if let Some(link) = shared
        .borrow_mut()
        .links
        .get_mut(&peer)
        .filter(|link| link.attempt == attempt)
    {
        if link.pending_ice.len() < MAX_EARLY_ICE_CANDIDATES {
            link.pending_ice.push((candidate, mid, index));
        }
    }
}

fn buffer_early_ice(
    shared: &SharedRef,
    peer: [u8; 32],
    attempt: RtcAttempt,
    candidate: String,
    mid: Option<String>,
    index: Option<u16>,
) {
    let mut state = shared.borrow_mut();
    if !state.early_ice.contains_key(&peer) && state.early_ice.len() >= MAX_SIGNALING_PEERS {
        return;
    }
    let early = state.early_ice.entry(peer).or_insert_with(|| EarlyIce {
        attempt,
        candidates: Vec::new(),
    });
    if early.attempt != attempt {
        early.attempt = attempt;
        early.candidates.clear();
    }
    if early.candidates.len() < MAX_EARLY_ICE_CANDIDATES {
        early.candidates.push((candidate, mid, index));
    }
}

fn adopt_early_ice(shared: &SharedRef, peer: [u8; 32], attempt: RtcAttempt) {
    let mut state = shared.borrow_mut();
    let pending = state
        .early_ice
        .remove(&peer)
        .filter(|early| early.attempt == attempt)
        .map(|early| early.candidates)
        .unwrap_or_default();
    if let Some(link) = state
        .links
        .get_mut(&peer)
        .filter(|link| link.attempt == attempt)
    {
        link.pending_ice.extend(pending);
    }
}

/// Mark the remote description set and flush any ICE candidates that arrived early.
async fn flush_after_remote_desc(
    shared: &SharedRef,
    peer: [u8; 32],
    attempt: RtcAttempt,
    pc: &RtcPeerConnection,
) {
    let pending = {
        let mut s = shared.borrow_mut();
        match s.links.get_mut(&peer) {
            Some(link) if link.attempt == attempt => {
                link.remote_desc_set = true;
                std::mem::take(&mut link.pending_ice)
            }
            Some(_) | None => Vec::new(),
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

    #[test]
    fn deterministic_roles_cover_restarted_offerer_and_answerer() {
        let lower = [0x11; 32];
        let higher = [0x22; 32];
        assert!(local_is_offerer(lower, higher));
        assert!(!local_is_offerer(higher, lower));
    }

    #[test]
    fn every_signal_is_bound_to_the_offer_attempt() {
        let attempt = RtcAttempt([7; 8]);
        for payload in [
            RtcPayload::Offer {
                attempt,
                sdp: "offer".into(),
            },
            RtcPayload::Answer {
                attempt,
                sdp: "answer".into(),
            },
            RtcPayload::Ice {
                attempt,
                candidate: "candidate".into(),
                mid: None,
                index: Some(0),
            },
        ] {
            let encoded = serde_json::to_string(&payload).unwrap();
            assert!(encoded.contains("\"attempt\":[7,7,7,7,7,7,7,7]"));
            let decoded: RtcPayload = serde_json::from_str(&encoded).unwrap();
            let decoded_attempt = match decoded {
                RtcPayload::Offer { attempt, .. }
                | RtcPayload::Answer { attempt, .. }
                | RtcPayload::Ice { attempt, .. } => attempt,
            };
            assert_eq!(decoded_attempt, attempt);
        }
    }
}
