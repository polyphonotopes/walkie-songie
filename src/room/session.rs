//! Worker-owned compact causal pitch-set sessions.
//!
//! The window transports opaque [`RoomSessionCarrier`] bytes and applies the
//! worker's provisional projection. It never interprets session causality.
//! Capability foundations, the HHHS kernel/projection/reification planner, and
//! the ordinary Replica confirmation path all remain in the worker.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
    rc::Rc,
    sync::Arc,
};

use futures::{
    Sink, SinkExt, StreamExt,
    channel::{mpsc, oneshot},
    future::poll_fn,
};
use hhhs::{DagRead, DagSnapshot, Digest, Entry, EntryHash, Position};
use hhhs_cap::{Area, CapabilitySnapshot, Right};
use hhhs_proof::{
    Ed25519Verifier, PresentationContext, PresentationEnvelope, SigningKey, VerifierRegistry,
};
use hhhs_replica::DurableEntryAdmission;
use hhhs_session::{
    AllowedMessageClasses, AuthorizedSession, AuthorizedSessionRenewal, DirectedSessionBinding,
    DurableProjection, DurableProjectionHorizon, EventClass, FoundationProfileId,
    ProjectionGeneration, ReificationError, ReificationPlan, ReificationPlanner,
    ReifiedSessionCommand, ReplayDisposition, SeatFoundationClaim, SessionAdmission, SessionDot,
    SessionEpoch, SessionEvent, SessionEventCode, SessionKernel, SessionKeyEpoch, SessionLeaseTime,
    SessionManifest, SessionPolicy, SessionProjectionChange, SessionProjectionHost,
    SessionProjector, SessionReceiverLane, SessionRenewalFloor, SessionSeat, SessionSenderLane,
    SimulationTime, VerifiedSeatFoundation, XChaCha20Poly1305Key, XChaCha20Poly1305Profile,
    XChaChaCompactPacketCodec, XChaChaCounterNonceSource, authorize_session,
    authorize_session_renewal, xchacha20poly1305_profile_id,
};
use hhhs_store::history_root;
use hhhs_web_browser::{WorkerApplicationChannel, WorkerEventPort};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tutti_music::{MusicOp, SharedPitchSet, TunedDegree, TunedPeriodicPitch};
use tutti_session::{
    ChannelBinding, EphemeralSecret, Offer, PeerIdentity, PendingInitiator, ProtocolId, SessionKeys,
};

use super::v5::{ActorId, RoomIdentity};

pub(crate) const ROOM_SESSION_CHANNEL: WorkerApplicationChannel =
    WorkerApplicationChannel::new(0x5455_5454);
const SESSION_PAYLOAD_VERSION: u16 = 1;
const SESSION_CARRIER_DOMAIN: &[u8] = b"walkie hhhs pitch session carrier v1\0";
const SESSION_PROTOCOL_LABEL: &[u8] = b"walkie hhhs pitch session establishment v1";
const SESSION_RULES: &[u8] = b"tutti observed-remove shared pitch set v1";
const SESSION_VOCABULARY: &[u8] = b"tutti canonical pitch edit cbor v1";
const FOUNDATION_PROFILE: &[u8] = b"walkie hhhs ed25519 seat foundation v1";
const EXPORT_DOMAIN: &[u8] = b"walkie hhhs xchacha exporter context v1";
const PITCH_EDIT: SessionEventCode = SessionEventCode::new(1);
const MAX_EVENTS_PER_SEAT: u32 = 64;
const MAX_SESSION_MESSAGE_BYTES: u32 = 2_048;
const SESSION_CAPACITY: usize = 128;
const REPLAY_WIDTH: usize = 64;
const LEASE_START: u64 = 100;
const LEASE_END: u64 = 100_000;

type SessionSender = SessionSenderLane<XChaCha20Poly1305Profile<XChaChaCounterNonceSource>, 2>;
type SessionReceiver =
    SessionReceiverLane<XChaCha20Poly1305Profile<XChaChaCounterNonceSource>, 2, REPLAY_WIDTH>;
type PitchKernel = SessionKernel<MusicOp, 2, SESSION_CAPACITY>;
type PitchProjection =
    SessionProjectionHost<MusicOp, PitchProjectionState, 2, SESSION_CAPACITY, SESSION_CAPACITY>;
type PitchPlanner = ReificationPlanner<2, SESSION_CAPACITY>;

#[derive(Clone)]
pub(crate) struct RoomSessionFoundation {
    pub identity: RoomIdentity,
    pub local: ActorId,
    pub peer: ActorId,
    pub signing_key: SigningKey,
    pub history: DagSnapshot,
    pub music_root: EntryHash,
    pub local_grants: Vec<EntryHash>,
    pub peer_grants: Vec<EntryHash>,
    pub durable_view: SharedPitchSet,
    pub durable_revision: u64,
}

pub(crate) enum RoomSessionTaskInput {
    Configure {
        events: WorkerEventPort,
        trace_enabled: bool,
        #[cfg(feature = "browser-acceptance-faults")]
        renewal_test_cut: bool,
    },
    StartPeer(RoomSessionFoundation),
    Application {
        channel: WorkerApplicationChannel,
        bytes: Vec<u8>,
    },
    LocalCommitted {
        peer: ActorId,
        plan: ReificationPlan,
        entry: Entry,
        durable_admission: DurableEntryAdmission,
        history: DagSnapshot,
        durable_view: SharedPitchSet,
        durable_revision: u64,
    },
    Observed {
        entry: Entry,
        durable_admission: DurableEntryAdmission,
        history: DagSnapshot,
        durable_view: SharedPitchSet,
        durable_revision: u64,
    },
    DurableAdvanced {
        history: DagSnapshot,
        durable_view: SharedPitchSet,
        durable_revision: u64,
    },
    ResetProjection {
        history: DagSnapshot,
        durable_view: SharedPitchSet,
        durable_revision: u64,
        reply: oneshot::Sender<Result<bool, String>>,
    },
    RepairResynchronized {
        history: DagSnapshot,
        durable_view: SharedPitchSet,
        durable_revision: u64,
    },
    RefreshFoundations(Vec<RoomSessionFoundation>),
    Shutdown,
}

pub(crate) struct RoomSessionReification {
    pub peer: ActorId,
    pub plan: ReificationPlan,
    pub command: MusicOp,
}

pub(crate) struct RoomSessionServicePort {
    pub task: mpsc::Sender<RoomSessionTaskInput>,
    pub reifications: mpsc::Receiver<RoomSessionReification>,
}

/// Receiver-local durable key for one pair-session renewal lineage.
///
/// The room object and both actor identities are included so a floor can never
/// be reused by another room or pair. Direction matters: each receiver owns and
/// persists its own anti-replay floor.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct RoomSessionRenewalKey {
    pub room: [u8; 32],
    pub local: [u8; 32],
    pub peer: [u8; 32],
}

/// Async, task-local persistence seam for rollback-sensitive renewal floors.
///
/// A successful `persist` is the transaction-complete durability boundary. It
/// is intentionally separate from the in-memory session install: callers stage
/// recoverable egress, persist the next floor, and only then activate. A crash
/// in that cut can cost availability, but must never reopen the old epoch.
pub(crate) trait RoomSessionRenewalStore {
    fn load<'a>(
        &'a self,
        key: RoomSessionRenewalKey,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Vec<u8>>, String>> + 'a>>;

    fn persist<'a>(
        &'a self,
        key: RoomSessionRenewalKey,
        floor: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + 'a>>;
}

/// Receiver-owned monotonic clock for session lease authorization.
///
/// Its ticks are local elapsed authority time. They are deliberately unrelated
/// to the sender-authenticated [`SimulationTime`] carried by a musical event.
pub(crate) trait RoomSessionLeaseClock {
    fn now_ticks(&self) -> Result<u64, String>;
}

/// Scheduling observation seam, intentionally separate from authority and
/// musical simulation clocks.
pub(crate) trait RoomSessionTraceClock {
    fn now_micros(&self) -> Option<u64>;
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub(crate) enum RoomSessionIngress {
    LocalPitchEdit {
        command: MusicOp,
        trace_token: Option<RoomSessionTraceToken>,
    },
    Carrier {
        bytes: Vec<u8>,
        received_at_micros: Option<u64>,
    },
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub(crate) enum RoomSessionEgress {
    Carrier(Vec<u8>),
    Realtime(RoomSessionRealtimeEgress),
    FallbackDurable(MusicOp),
    Diagnostic(String),
    RenewalTrace(RoomSessionRenewalTrace),
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub(crate) struct RoomSessionRenewalTrace {
    pub stage: RoomSessionRenewalTraceStage,
    pub peer: [u8; 32],
    pub epoch: u32,
    pub floor_epoch: Option<u32>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub(crate) enum RoomSessionRenewalTraceStage {
    RecoveredFloor,
    OfferStarted,
    StaleOfferRefused,
    #[cfg(feature = "browser-acceptance-faults")]
    FloorPersistedBeforeEgressCut,
    SessionInstalled,
}

/// One atomically accepted worker-to-window realtime update.
///
/// Acceptance means only that the bounded window queue owns this item. The
/// carrier is not yet delivered to a peer and the durable correlation is not
/// yet admitted. Keeping these fields together prevents rendering a local
/// prediction whose corresponding carrier was rejected by a second IPC hop.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub(crate) struct RoomSessionRealtimeEgress {
    pub projection: RoomSessionProjection,
    pub carrier: Option<Vec<u8>>,
    pub durable: Vec<RoomSessionDurableCorrelation>,
    pub trace: Option<RoomSessionCompactTrace>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub(crate) enum RoomSessionProjectionKind {
    Reset,
    Predicted,
    Confirmed,
    Corrected,
    Advanced,
}

/// Truthful ordering and durable-horizon metadata for a speculative view.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub(crate) struct RoomSessionProjection {
    pub manifest: [u8; 32],
    pub session_id: u64,
    pub epoch: u32,
    pub generation: u64,
    pub sequence: u64,
    pub durable_revision: u64,
    pub durable_root: [u8; 32],
    pub kind: RoomSessionProjectionKind,
    pub view: SharedPitchSet,
}

/// Stable identity of a compact event queued for ordinary Replica reification.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub(crate) struct RoomSessionDurableCorrelation {
    pub manifest: [u8; 32],
    pub epoch: u32,
    pub seat: u8,
    pub counter: u32,
    pub event: [u8; 32],
}

/// Window-minted identity for one application intent. The worker returns the
/// explicit token-to-causal-correlation mapping after minting the session dot.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub(crate) struct RoomSessionTraceToken {
    pub scope: u64,
    pub sequence: u64,
}

/// Correlation-keyed observations from the worker-owned compact path.
///
/// These timestamps are diagnostic only. In particular, they are neither
/// session authority time nor musical simulation time.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub(crate) struct RoomSessionCompactTrace {
    pub token: Option<RoomSessionTraceToken>,
    pub correlation: RoomSessionDurableCorrelation,
    pub direction: RoomSessionTraceDirection,
    pub worker_accepted_at_micros: Option<u64>,
    pub carrier_received_at_micros: Option<u64>,
    pub worker_authenticated_at_micros: Option<u64>,
    pub worker_authorized_at_micros: Option<u64>,
    pub worker_interpreted_at_micros: Option<u64>,
    pub worker_projected_at_micros: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub(crate) enum RoomSessionTraceDirection {
    Local,
    Remote,
}

#[derive(Clone, Copy)]
struct RoomSessionProjectionCursor {
    manifest: [u8; 32],
    session_id: u64,
    epoch: u32,
    generation: u64,
    sequence: u64,
}

/// Window-side continuity gate shared by every carrier implementation.
///
/// This contains no DOM or worker types, so the ordering contract is testable
/// natively as well as in the real browser placement.
#[derive(Default)]
pub(crate) struct RoomSessionProjectionGate {
    canonical_initialized: bool,
    canonical_revision: u64,
    canonical_root: [u8; 32],
    cursor: Option<RoomSessionProjectionCursor>,
    visible_session_horizon: Option<(u64, [u8; 32])>,
    awaiting_reset: bool,
}

impl RoomSessionProjectionGate {
    pub(crate) fn canonical(&mut self, revision: u64, root: [u8; 32]) -> Result<bool, String> {
        if self.canonical_initialized {
            if revision < self.canonical_revision {
                return Ok(false);
            }
            if revision == self.canonical_revision && root != self.canonical_root {
                self.awaiting_reset = true;
                return Err(format!(
                    "canonical music root conflicts at revision {revision}"
                ));
            }
        }
        self.canonical_initialized = true;
        self.canonical_revision = revision;
        self.canonical_root = root;
        Ok(match self.visible_session_horizon {
            Some((session_revision, session_root)) if session_revision == revision => {
                if session_root != root {
                    self.awaiting_reset = true;
                    return Err(format!(
                        "canonical music root contradicts visible session projection at revision {revision}"
                    ));
                }
                false
            }
            Some((session_revision, _)) if session_revision > revision => false,
            _ => {
                self.visible_session_horizon = None;
                true
            }
        })
    }

    pub(crate) fn accept(&mut self, projection: &RoomSessionProjection) -> Result<bool, String> {
        if self.awaiting_reset && projection.kind != RoomSessionProjectionKind::Reset {
            return Ok(false);
        }
        if projection.durable_revision < self.canonical_revision {
            return Ok(false);
        }
        if projection.durable_revision == self.canonical_revision
            && projection.durable_root != self.canonical_root
        {
            self.awaiting_reset = true;
            return Err(format!(
                "session projection durable root differs at revision {}",
                projection.durable_revision
            ));
        }

        match self.cursor.as_mut() {
            None => {
                if projection.kind != RoomSessionProjectionKind::Reset {
                    self.awaiting_reset = true;
                    return Err("first session projection was not a reset snapshot".into());
                }
                self.cursor = Some(RoomSessionProjectionCursor {
                    manifest: projection.manifest,
                    session_id: projection.session_id,
                    epoch: projection.epoch,
                    generation: projection.generation,
                    sequence: projection.sequence,
                });
            }
            Some(cursor)
                if cursor.manifest != projection.manifest
                    || cursor.session_id != projection.session_id =>
            {
                if projection.epoch <= cursor.epoch {
                    return Ok(false);
                }
                if projection.kind != RoomSessionProjectionKind::Reset {
                    self.awaiting_reset = true;
                    return Err("newer session superseded presentation without a reset".into());
                }
                *cursor = RoomSessionProjectionCursor {
                    manifest: projection.manifest,
                    session_id: projection.session_id,
                    epoch: projection.epoch,
                    generation: projection.generation,
                    sequence: projection.sequence,
                };
            }
            Some(cursor) if cursor.generation != projection.generation => {
                if projection.kind != RoomSessionProjectionKind::Reset
                    || projection.generation <= cursor.generation
                {
                    self.awaiting_reset = true;
                    return Err(
                        "session projection generation changed without a newer reset".into(),
                    );
                }
                cursor.generation = projection.generation;
                cursor.sequence = projection.sequence;
            }
            Some(cursor) => {
                let Some(expected) = cursor.sequence.checked_add(1) else {
                    self.awaiting_reset = true;
                    return Err("session projection sequence exhausted".into());
                };
                if projection.sequence != expected {
                    self.awaiting_reset = true;
                    return Err(format!(
                        "session projection sequence gap: expected {expected}, received {}",
                        projection.sequence
                    ));
                }
                cursor.sequence = projection.sequence;
            }
        }
        if projection.kind == RoomSessionProjectionKind::Reset {
            self.awaiting_reset = false;
        }
        self.visible_session_horizon = Some((projection.durable_revision, projection.durable_root));
        Ok(true)
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
enum SessionCarrierBody {
    Offer {
        source: ActorId,
        target: ActorId,
        session_id: u64,
        epoch: u32,
        base: Vec<[u8; 32]>,
        grants: Vec<[u8; 32]>,
        handshake: Vec<u8>,
        foundation: Vec<u8>,
    },
    Answer {
        source: ActorId,
        target: ActorId,
        session_id: u64,
        grants: Vec<[u8; 32]>,
        handshake: Vec<u8>,
        foundation: Vec<u8>,
    },
    Event {
        source: ActorId,
        target: ActorId,
        session_id: u64,
        frame: Vec<u8>,
    },
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
struct Versioned<T> {
    version: u16,
    body: T,
}

fn encode<T: Serialize>(body: &T) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    ciborium::into_writer(
        &Versioned {
            version: SESSION_PAYLOAD_VERSION,
            body,
        },
        &mut bytes,
    )
    .map_err(|error| format!("session payload encoding failed: {error}"))?;
    Ok(bytes)
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    let value: Versioned<T> = ciborium::from_reader(bytes)
        .map_err(|error| format!("session payload decoding failed: {error}"))?;
    if value.version != SESSION_PAYLOAD_VERSION {
        return Err(format!(
            "unsupported session payload version {}; expected {}",
            value.version, SESSION_PAYLOAD_VERSION
        ));
    }
    Ok(value.body)
}

pub(crate) fn encode_session_ingress(value: &RoomSessionIngress) -> Result<Vec<u8>, String> {
    encode(value)
}

pub(crate) fn decode_session_egress(bytes: &[u8]) -> Result<RoomSessionEgress, String> {
    decode(bytes)
}

impl SessionCarrierBody {
    fn encode(&self) -> Result<Vec<u8>, String> {
        let payload = encode(self)?;
        let mut bytes = Vec::with_capacity(SESSION_CARRIER_DOMAIN.len() + payload.len());
        bytes.extend_from_slice(SESSION_CARRIER_DOMAIN);
        bytes.extend_from_slice(&payload);
        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self, String> {
        let payload = bytes
            .strip_prefix(SESSION_CARRIER_DOMAIN)
            .ok_or("not a Walkie HHHS session carrier frame")?;
        decode(payload)
    }
}

pub(crate) fn is_session_carrier(bytes: &[u8]) -> bool {
    bytes.starts_with(SESSION_CARRIER_DOMAIN)
}

pub(crate) async fn run_room_session_task(
    mut inbox: mpsc::Receiver<RoomSessionTaskInput>,
    reifications: mpsc::Sender<RoomSessionReification>,
    renewal_store: Rc<dyn RoomSessionRenewalStore>,
    lease_clock: Rc<dyn RoomSessionLeaseClock>,
    trace_clock: Rc<dyn RoomSessionTraceClock>,
) {
    let mut task =
        RoomSessionTask::new_with_trace(reifications, renewal_store, lease_clock, trace_clock);
    while let Some(input) = inbox.next().await {
        if matches!(input, RoomSessionTaskInput::Shutdown) {
            break;
        }
        if let Err(error) = task.accept(input).await {
            task.diagnostic(error).await;
        }
    }
}

struct PendingSession {
    foundation: RoomSessionFoundation,
    pending: PendingInitiator,
    session_id: u64,
    manifest: SessionManifest<2>,
    local_foundation: VerifiedSeatFoundation,
    renewal_floor: Option<SessionRenewalFloor>,
}

enum PairAuthorization {
    Initial {
        session: AuthorizedSession<2>,
        floor: SessionRenewalFloor,
    },
    Renewal(AuthorizedSessionRenewal<2>),
}

struct ActiveSession {
    foundation: RoomSessionFoundation,
    session: AuthorizedSession<2>,
    session_id: u64,
    local_seat: u8,
    remote_seat: u8,
    sender_binding: DirectedSessionBinding<2>,
    receiver_binding: DirectedSessionBinding<2>,
    sender_codec: XChaChaCompactPacketCodec,
    receiver_codec: XChaChaCompactPacketCodec,
    sender: SessionSender,
    echo_receiver: SessionReceiver,
    receiver: SessionReceiver,
    kernel: PitchKernel,
    projection: PitchProjection,
    planner: PitchPlanner,
    logical_time: u64,
    lease_clock_origin: u64,
    lease_clock_last: u64,
    durable_revision: u64,
    awaiting_reification: BTreeMap<SessionDot, MusicOp>,
}

struct RoomSessionTask {
    events: Option<WorkerEventPort>,
    reifications: mpsc::Sender<RoomSessionReification>,
    foundations: BTreeMap<ActorId, RoomSessionFoundation>,
    pending: BTreeMap<ActorId, PendingSession>,
    active: BTreeMap<ActorId, ActiveSession>,
    presentation_peer: Option<ActorId>,
    renewal_store: Rc<dyn RoomSessionRenewalStore>,
    lease_clock: Rc<dyn RoomSessionLeaseClock>,
    trace_clock: Rc<dyn RoomSessionTraceClock>,
    trace_enabled: bool,
    #[cfg(feature = "browser-acceptance-faults")]
    renewal_test_cut: bool,
    #[cfg(feature = "browser-acceptance-faults")]
    renewal_test_cut_consumed: bool,
    renewal_floors: BTreeMap<ActorId, Option<SessionRenewalFloor>>,
    renewal_needed: BTreeSet<ActorId>,
}

impl RoomSessionTask {
    #[cfg(test)]
    fn new(
        reifications: mpsc::Sender<RoomSessionReification>,
        renewal_store: Rc<dyn RoomSessionRenewalStore>,
        lease_clock: Rc<dyn RoomSessionLeaseClock>,
    ) -> Self {
        Self::new_with_trace(
            reifications,
            renewal_store,
            lease_clock,
            Rc::new(DisabledRoomSessionTraceClock),
        )
    }

    fn new_with_trace(
        reifications: mpsc::Sender<RoomSessionReification>,
        renewal_store: Rc<dyn RoomSessionRenewalStore>,
        lease_clock: Rc<dyn RoomSessionLeaseClock>,
        trace_clock: Rc<dyn RoomSessionTraceClock>,
    ) -> Self {
        Self {
            events: None,
            reifications,
            foundations: BTreeMap::new(),
            pending: BTreeMap::new(),
            active: BTreeMap::new(),
            presentation_peer: None,
            renewal_store,
            lease_clock,
            trace_clock,
            trace_enabled: false,
            #[cfg(feature = "browser-acceptance-faults")]
            renewal_test_cut: false,
            #[cfg(feature = "browser-acceptance-faults")]
            renewal_test_cut_consumed: false,
            renewal_floors: BTreeMap::new(),
            renewal_needed: BTreeSet::new(),
        }
    }

    async fn accept(&mut self, input: RoomSessionTaskInput) -> Result<(), String> {
        match input {
            RoomSessionTaskInput::Configure {
                events,
                trace_enabled,
                #[cfg(feature = "browser-acceptance-faults")]
                renewal_test_cut,
            } => {
                self.events = Some(events);
                self.trace_enabled = trace_enabled;
                #[cfg(feature = "browser-acceptance-faults")]
                {
                    self.renewal_test_cut = trace_enabled && renewal_test_cut;
                }
                Ok(())
            }
            RoomSessionTaskInput::StartPeer(foundation) => self.start_peer(foundation).await,
            RoomSessionTaskInput::Application { channel, bytes } => {
                if channel != ROOM_SESSION_CHANNEL {
                    return Err(format!(
                        "unsupported worker session channel {}",
                        channel.get()
                    ));
                }
                let worker_accepted_at_micros = self
                    .trace_enabled
                    .then(|| self.trace_clock.now_micros())
                    .flatten();
                match decode::<RoomSessionIngress>(&bytes)? {
                    RoomSessionIngress::LocalPitchEdit {
                        command,
                        trace_token,
                    } => {
                        self.local_edit(command, trace_token, worker_accepted_at_micros)
                            .await
                    }
                    RoomSessionIngress::Carrier {
                        bytes,
                        received_at_micros,
                    } => {
                        self.carrier(bytes, received_at_micros, worker_accepted_at_micros)
                            .await
                    }
                }
            }
            RoomSessionTaskInput::LocalCommitted {
                peer,
                plan,
                entry,
                durable_admission,
                history,
                durable_view,
                durable_revision,
            } => {
                self.confirm_local(
                    peer,
                    plan,
                    entry,
                    durable_admission,
                    history,
                    durable_view,
                    durable_revision,
                )
                .await
            }
            RoomSessionTaskInput::Observed {
                entry,
                durable_admission,
                history,
                durable_view,
                durable_revision,
            } => {
                self.observe(
                    entry,
                    durable_admission,
                    history,
                    durable_view,
                    durable_revision,
                )
                .await
            }
            RoomSessionTaskInput::DurableAdvanced {
                history,
                durable_view,
                durable_revision,
            } => {
                self.advance_all(history, durable_view, durable_revision)
                    .await
            }
            RoomSessionTaskInput::ResetProjection {
                history,
                durable_view,
                durable_revision,
                reply,
            } => {
                let result = self
                    .reset_all_projections(history, durable_view, durable_revision)
                    .await;
                let response = result.clone();
                let _ = reply.send(response);
                result.map(|_| ())
            }
            RoomSessionTaskInput::RepairResynchronized {
                history,
                durable_view,
                durable_revision,
            } => self
                .reset_all_projections(history, durable_view, durable_revision)
                .await
                .map(|_| ()),
            RoomSessionTaskInput::RefreshFoundations(foundations) => {
                self.foundations = foundations
                    .into_iter()
                    .map(|foundation| (foundation.peer, foundation))
                    .collect();
                self.start_ready_renewals().await
            }
            RoomSessionTaskInput::Shutdown => Ok(()),
        }
    }

    async fn start_peer(&mut self, foundation: RoomSessionFoundation) -> Result<(), String> {
        let peer = foundation.peer;
        self.foundations.insert(peer, foundation.clone());
        if self.active.contains_key(&peer) || self.pending.contains_key(&peer) {
            return Ok(());
        }
        let recovering = self.load_renewal_floor(&foundation).await?.is_some();
        if foundation.local.0 > peer.0 && !recovering {
            return Ok(());
        }
        self.begin_session(foundation).await
    }

    async fn begin_session(&mut self, foundation: RoomSessionFoundation) -> Result<(), String> {
        let peer = foundation.peer;
        if self.pending.contains_key(&peer) {
            return Ok(());
        }
        if let Some(active) = self.active.get(&peer)
            && !active.is_drained()
        {
            return Err("session renewal requires the old speculative suffix to drain".into());
        }
        let renewal_floor = self.load_renewal_floor(&foundation).await?;
        let renewal_floor_epoch = renewal_floor.as_ref().map(SessionRenewalFloor::epoch);
        if self.active.contains_key(&peer) && renewal_floor.is_none() {
            return Err("active session has no durable renewal floor".into());
        }
        let epoch = proposed_epoch(renewal_floor.as_ref())?;
        let mut random = [0_u8; 40];
        getrandom::fill(&mut random)
            .map_err(|error| format!("session randomness failed: {error}"))?;
        let mut session_id = u64::from_le_bytes(random[..8].try_into().expect("fixed random"));
        if session_id == 0 {
            session_id = 1;
        }
        let manifest = build_manifest(&foundation, session_id, epoch)?;
        let binding =
            establishment_binding(&foundation.identity, foundation.local, peer, session_id);
        let (pending, offer) = PendingInitiator::begin(
            &foundation.signing_key,
            ProtocolId::derive(SESSION_PROTOCOL_LABEL),
            binding,
            session_id,
            EphemeralSecret::from_bytes(random[8..].try_into().expect("fixed random")),
        );
        let (local_foundation, presentation) = local_foundation(&foundation, &manifest)?;
        let carrier = SessionCarrierBody::Offer {
            source: foundation.local,
            target: peer,
            session_id,
            epoch: epoch.get(),
            base: position_bytes(manifest.base()),
            grants: hash_bytes(&foundation.local_grants),
            handshake: offer.as_bytes().to_vec(),
            foundation: presentation,
        }
        .encode()?;
        self.pending.insert(
            peer,
            PendingSession {
                foundation,
                pending,
                session_id,
                manifest,
                local_foundation,
                renewal_floor,
            },
        );
        self.renewal_needed.remove(&peer);
        self.emit_renewal_trace(
            RoomSessionRenewalTraceStage::OfferStarted,
            peer,
            epoch,
            renewal_floor_epoch,
        )
        .await?;
        self.emit(RoomSessionEgress::Carrier(carrier)).await
    }

    async fn load_renewal_floor(
        &mut self,
        foundation: &RoomSessionFoundation,
    ) -> Result<Option<SessionRenewalFloor>, String> {
        if let Some(floor) = self.renewal_floors.get(&foundation.peer) {
            return Ok(floor.clone());
        }
        let encoded = self.renewal_store.load(renewal_key(foundation)).await?;
        let floor = encoded
            .as_deref()
            .map(SessionRenewalFloor::from_bytes)
            .transpose()
            .map_err(|error| format!("invalid persisted session renewal floor: {error}"))?;
        if let Some(recovered) = floor.as_ref() {
            self.emit_renewal_trace(
                RoomSessionRenewalTraceStage::RecoveredFloor,
                foundation.peer,
                recovered.epoch(),
                Some(recovered.epoch()),
            )
            .await?;
        }
        self.renewal_floors.insert(foundation.peer, floor.clone());
        Ok(floor)
    }

    async fn install_session(
        &mut self,
        peer: ActorId,
        foundation: RoomSessionFoundation,
        authorization: PairAuthorization,
        session_id: u64,
        keys: SessionKeys,
        carrier: Option<Vec<u8>>,
    ) -> Result<(), String> {
        let key = renewal_key(&foundation);
        let persisted = match authorization {
            PairAuthorization::Initial { session, floor } => self
                .renewal_store
                .persist(key, floor.to_bytes())
                .await
                .map(|()| (session, floor)),
            PairAuthorization::Renewal(renewal) => {
                let store = Rc::clone(&self.renewal_store);
                renewal
                    .install_with_async(move |bytes| async move { store.persist(key, bytes).await })
                    .await
                    .map(|installed| installed.into_parts())
            }
        };
        let (authorized, next_floor) = match persisted {
            Ok(installed) => installed,
            Err(error) => {
                self.renewal_needed.insert(peer);
                return Err(format!(
                    "session renewal floor persistence failed before observable install egress: {error}"
                ));
            }
        };
        let active = ActiveSession::new(
            foundation,
            authorized,
            session_id,
            keys,
            self.lease_clock.now_ticks()?,
        )?;
        let installed_epoch = next_floor.epoch();
        // The task is the sole session mutator, so the already-drained old
        // session remains frozen across this await. Persist the new floor
        // before Answer/reset can become observable. A crash in the following
        // delivery/activation cut is healed by the durable higher-floor
        // counter-offer path and can never reopen the old epoch.
        self.renewal_floors.insert(peer, Some(next_floor));

        #[cfg(feature = "browser-acceptance-faults")]
        {
            if self.renewal_test_cut && !self.renewal_test_cut_consumed {
                self.renewal_test_cut_consumed = true;
                self.renewal_needed.insert(peer);
                self.emit_renewal_trace(
                    RoomSessionRenewalTraceStage::FloorPersistedBeforeEgressCut,
                    peer,
                    installed_epoch,
                    Some(installed_epoch),
                )
                .await?;
                return Err(
                    "injected renewal crash cut after durable floor persistence and before observable install egress"
                        .into(),
                );
            }
        }

        let present = self.presentation_peer.is_none() || self.presentation_peer == Some(peer);
        let staged = if present {
            let projection = active.projection_event(RoomSessionProjectionKind::Reset);
            self.emit(RoomSessionEgress::Realtime(RoomSessionRealtimeEgress {
                projection,
                carrier,
                durable: Vec::new(),
                trace: None,
            }))
            .await
        } else if let Some(carrier) = carrier {
            self.emit(RoomSessionEgress::Carrier(carrier)).await
        } else {
            Ok(())
        };
        if let Err(error) = staged {
            self.renewal_needed.insert(peer);
            return Err(format!(
                "session install egress failed after floor persistence; higher-epoch recovery is required: {error}"
            ));
        }

        self.active.insert(peer, active);
        if present {
            self.presentation_peer = Some(peer);
        }
        self.emit_renewal_trace(
            RoomSessionRenewalTraceStage::SessionInstalled,
            peer,
            installed_epoch,
            Some(installed_epoch),
        )
        .await
    }

    async fn start_ready_renewals(&mut self) -> Result<(), String> {
        let peers: Vec<_> = self
            .active
            .iter()
            .filter_map(|(peer, active)| {
                (((active.foundation.local.0 < peer.0 && active.is_saturated())
                    || self.renewal_needed.contains(peer))
                    && active.is_drained()
                    && !self.pending.contains_key(peer))
                .then_some(*peer)
            })
            .collect();
        for peer in peers {
            let foundation = self
                .foundations
                .get(&peer)
                .cloned()
                .or_else(|| {
                    self.active
                        .get(&peer)
                        .map(|active| active.foundation.clone())
                })
                .ok_or("renewing session lost its admitted foundation")?;
            self.begin_session(foundation).await?;
        }
        Ok(())
    }

    async fn carrier(
        &mut self,
        bytes: Vec<u8>,
        carrier_received_at_micros: Option<u64>,
        worker_accepted_at_micros: Option<u64>,
    ) -> Result<(), String> {
        match SessionCarrierBody::decode(&bytes)? {
            SessionCarrierBody::Offer {
                source,
                target,
                session_id,
                epoch,
                base,
                grants,
                handshake,
                foundation,
            } => {
                let local = self
                    .foundations
                    .get(&source)
                    .ok_or("session offer arrived before the peer foundation was ready")?
                    .clone();
                if target != local.local || base != position_bytes(&local.history.frontier()) {
                    return Err("session offer targets another actor or stale durable base".into());
                }
                if grants != hash_bytes(&local.peer_grants) {
                    return Err(
                        "session offer foundation grants differ from admitted history".into(),
                    );
                }
                let renewal_floor = self.load_renewal_floor(&local).await?;
                let offered_epoch = SessionEpoch::new(epoch);
                if offered_epoch.get() == 0 {
                    return Err("session offer epoch zero is invalid".into());
                }
                match renewal_floor.as_ref() {
                    Some(floor) if offered_epoch <= floor.epoch() => {
                        // The sender is behind a floor which may have persisted
                        // immediately before either process crashed. Do not
                        // mutate or reopen that epoch. Queue an authenticated
                        // counter-offer from our minimum successor once the old
                        // suffix is drained.
                        self.renewal_needed.insert(source);
                        self.emit_renewal_trace(
                            RoomSessionRenewalTraceStage::StaleOfferRefused,
                            source,
                            offered_epoch,
                            Some(floor.epoch()),
                        )
                        .await?;
                        if !self.active.contains_key(&source) && !self.pending.contains_key(&source)
                        {
                            return self.begin_session(local).await;
                        }
                        return self.start_ready_renewals().await;
                    }
                    _ => {}
                }
                if let Some(pending) = self.pending.get(&source) {
                    if !prefer_incoming_offer(
                        local.local,
                        source,
                        offered_epoch,
                        pending.manifest.epoch(),
                    ) {
                        return Ok(());
                    }
                    self.pending.remove(&source);
                }
                if renewal_floor.is_some()
                    && self
                        .active
                        .get(&source)
                        .is_some_and(|active| !active.is_drained())
                {
                    return Err(
                        "session renewal arrived before the old speculative suffix drained".into(),
                    );
                }
                let manifest = build_manifest(&local, session_id, offered_epoch)?;
                let binding = establishment_binding(&local.identity, source, target, session_id);
                let verified_offer = Offer::decode(&handshake)
                    .map_err(|error| error.to_string())?
                    .verify(ProtocolId::derive(SESSION_PROTOCOL_LABEL), binding)
                    .map_err(|error| error.to_string())?;
                if verified_offer.identity() != PeerIdentity::from_bytes(source.0) {
                    return Err("session offer identity does not match its claimed source".into());
                }
                let remote_foundation =
                    verify_foundation(&local, &manifest, source, &local.peer_grants, &foundation)?;
                let (local_foundation, local_presentation) = local_foundation(&local, &manifest)?;
                let mut random = [0_u8; 32];
                getrandom::fill(&mut random)
                    .map_err(|error| format!("session randomness failed: {error}"))?;
                let (answer, keys) = verified_offer
                    .respond(&local.signing_key, EphemeralSecret::from_bytes(random))
                    .map_err(|error| error.to_string())?;
                let authorization = authorize_pair(
                    &local,
                    manifest,
                    local_foundation,
                    remote_foundation,
                    renewal_floor.as_ref(),
                )?;
                let answer = SessionCarrierBody::Answer {
                    source: local.local,
                    target: source,
                    session_id,
                    grants: hash_bytes(&local.local_grants),
                    handshake: answer.as_bytes().to_vec(),
                    foundation: local_presentation,
                }
                .encode()?;
                self.install_session(source, local, authorization, session_id, keys, Some(answer))
                    .await
            }
            SessionCarrierBody::Answer {
                source,
                target,
                session_id,
                grants,
                handshake,
                foundation,
            } => {
                let pending = self
                    .pending
                    .remove(&source)
                    .ok_or("session answer has no pending offer")?;
                if target != pending.foundation.local || session_id != pending.session_id {
                    return Err("session answer targets another actor or handshake".into());
                }
                if grants != hash_bytes(&pending.foundation.peer_grants) {
                    return Err(
                        "session answer foundation grants differ from admitted history".into(),
                    );
                }
                let remote_foundation = verify_foundation(
                    &pending.foundation,
                    &pending.manifest,
                    source,
                    &pending.foundation.peer_grants,
                    &foundation,
                )?;
                let keys = pending
                    .pending
                    .complete(&handshake, PeerIdentity::from_bytes(source.0))
                    .map_err(|error| error.to_string())?;
                let current = self
                    .foundations
                    .get(&source)
                    .ok_or("session answer arrived after its peer foundation was revoked")?;
                if current.history.frontier() != *pending.manifest.base()
                    || self
                        .active
                        .get(&source)
                        .is_some_and(|active| !active.is_drained())
                {
                    return Err(
                        "session answer arrived after its durable base or old suffix changed"
                            .into(),
                    );
                }
                if self.renewal_floors.get(&source).and_then(Option::as_ref)
                    != pending.renewal_floor.as_ref()
                {
                    return Err("session renewal floor changed during establishment".into());
                }
                let authorization = authorize_pair(
                    &pending.foundation,
                    pending.manifest,
                    pending.local_foundation,
                    remote_foundation,
                    pending.renewal_floor.as_ref(),
                )?;
                self.install_session(
                    source,
                    pending.foundation,
                    authorization,
                    session_id,
                    keys,
                    None,
                )
                .await
            }
            SessionCarrierBody::Event {
                source,
                target,
                session_id,
                frame,
            } => {
                let matches_active = self.active.get(&source).is_some_and(|active| {
                    target == active.foundation.local && session_id == active.session_id
                });
                if !matches_active {
                    // A peer may already have installed the Answer which this
                    // receiver staged immediately before crashing. Do not try
                    // to interpret its packet under an old key. Start a bounded
                    // higher-epoch handshake; persisted floor comparison and
                    // normal capability checks arbitrate the recovery.
                    self.renewal_needed.insert(source);
                    if let Some(foundation) = self.foundations.get(&source).cloned() {
                        if !self.active.contains_key(&source) {
                            self.begin_session(foundation).await?;
                        } else {
                            self.start_ready_renewals().await?;
                        }
                    }
                    return Ok(());
                }
                let receiver_now = self.lease_clock.now_ticks()?;
                let trace_clock = Rc::clone(&self.trace_clock);
                let active = self
                    .active
                    .get_mut(&source)
                    .expect("matching active session checked above");
                let remote = active.ingest_remote(
                    &frame,
                    receiver_now,
                    trace_clock.as_ref(),
                    self.trace_enabled,
                    worker_accepted_at_micros,
                    carrier_received_at_micros,
                )?;
                let Some(mut remote) = remote else {
                    return Ok(());
                };
                if self.presentation_peer == Some(source)
                    && let Some(kind) = remote.changed
                {
                    let projection = active.projection_event(kind);
                    if let Some(trace) = &mut remote.trace {
                        trace.worker_projected_at_micros = trace_clock.now_micros();
                    }
                    self.emit(RoomSessionEgress::Realtime(RoomSessionRealtimeEgress {
                        projection,
                        carrier: None,
                        durable: Vec::new(),
                        trace: remote.trace,
                    }))
                    .await?;
                }
                Ok(())
            }
        }
    }

    async fn local_edit(
        &mut self,
        command: MusicOp,
        trace_token: Option<RoomSessionTraceToken>,
        worker_accepted_at_micros: Option<u64>,
    ) -> Result<(), String> {
        if !is_pitch_edit(&command) {
            return Err("compact session only accepts shared pitch-set edits".into());
        }
        let Some(peer) = self
            .presentation_peer
            .filter(|peer| self.active.contains_key(peer))
        else {
            return self.emit(RoomSessionEgress::FallbackDurable(command)).await;
        };
        if self
            .active
            .get(&peer)
            .is_some_and(ActiveSession::is_saturated)
        {
            // The old epoch remains valid through counter 64 but must never be
            // asked to seal counter 65. Keep the user's edit live by routing it
            // through the ordinary durable path; the ensuing canonical advance
            // supplies the exact base for renewal.
            return self.emit(RoomSessionEgress::FallbackDurable(command)).await;
        }
        // Reserve the bounded worker-side reification lane before mutating the
        // session kernel. Only this task produces reifications, so the permit
        // remains ours until start_send below.
        let mut reserved_reification = self.reifications.clone();
        poll_fn(|context| Pin::new(&mut reserved_reification).poll_ready(context))
            .await
            .map_err(|_| "session reification queue closed".to_owned())?;
        let receiver_now = self.lease_clock.now_ticks()?;
        let trace_clock = Rc::clone(&self.trace_clock);
        let (local, carrier, projection) = {
            let active = self.active.get_mut(&peer).expect("selected active session");
            let mut local = active.local_event(
                command,
                receiver_now,
                trace_clock.as_ref(),
                self.trace_enabled,
                trace_token,
                worker_accepted_at_micros,
            )?;
            let carrier = SessionCarrierBody::Event {
                source: active.foundation.local,
                target: peer,
                session_id: active.session_id,
                frame: local.frame.clone(),
            }
            .encode()?;
            let projection = active.projection_event(RoomSessionProjectionKind::Predicted);
            if let Some(trace) = &mut local.trace {
                trace.worker_projected_at_micros = trace_clock.now_micros();
            }
            (local, carrier, projection)
        };
        let durable = local
            .plan
            .as_ref()
            .map(|plan| durable_correlation(plan.correlation()))
            .into_iter()
            .collect();
        if let Some(plan) = local.plan.as_ref() {
            Pin::new(&mut reserved_reification)
                .start_send(RoomSessionReification {
                    peer,
                    plan: plan.clone(),
                    command: local.command.clone(),
                })
                .map_err(|_| "reserved session reification enqueue failed".to_owned())?;
        }
        let realtime = RoomSessionRealtimeEgress {
            projection,
            carrier: Some(carrier),
            durable,
            trace: local.trace,
        };
        self.emit_realtime_with_reset(peer, realtime).await
    }

    async fn confirm_local(
        &mut self,
        peer: ActorId,
        plan: ReificationPlan,
        entry: Entry,
        durable_admission: DurableEntryAdmission,
        history: DagSnapshot,
        durable_view: SharedPitchSet,
        durable_revision: u64,
    ) -> Result<(), String> {
        let (ready, projection) = {
            let active = self
                .active
                .get_mut(&peer)
                .ok_or("durable session confirmation has no active peer")?;
            let admission = active
                .planner
                .record_admission(
                    &plan,
                    &entry,
                    SessionAdmission::from_replica(
                        &entry,
                        durable_admission,
                        MAX_SESSION_MESSAGE_BYTES as usize,
                    )
                    .map_err(|error| error.to_string())?,
                )
                .map_err(|error| error.to_string())?;
            let kind = active
                .confirm(admission, durable_revision, &history, durable_view.clone())?
                .unwrap_or(RoomSessionProjectionKind::Confirmed);
            (active.retry_reifications()?, active.projection_event(kind))
        };
        for (other_peer, other) in &mut self.active {
            if *other_peer != peer {
                other.advance_durable(durable_revision, &history, durable_view.clone())?;
            }
        }
        self.update_foundation_horizon(&history, &durable_view, durable_revision);
        let durable = ready
            .iter()
            .map(|(plan, _)| durable_correlation(plan.correlation()))
            .collect();
        if self.presentation_peer == Some(peer) {
            self.emit_realtime_with_reset(
                peer,
                RoomSessionRealtimeEgress {
                    projection,
                    carrier: None,
                    durable,
                    trace: None,
                },
            )
            .await?;
        }
        self.enqueue_reifications(peer, ready).await?;
        self.start_ready_renewals().await
    }

    async fn observe(
        &mut self,
        entry: Entry,
        durable_admission: DurableEntryAdmission,
        history: DagSnapshot,
        durable_view: SharedPitchSet,
        durable_revision: u64,
    ) -> Result<(), String> {
        if !ReifiedSessionCommand::has_domain(&entry.payload) {
            return Ok(());
        }
        let reified =
            ReifiedSessionCommand::decode(&entry.payload, MAX_SESSION_MESSAGE_BYTES as usize)
                .map_err(|error| error.to_string())?;
        let manifest = reified.correlation().manifest();
        let Some(peer) = self.active.iter().find_map(|(peer, active)| {
            (active.session.manifest_digest() == manifest).then_some(*peer)
        }) else {
            for active in self.active.values_mut() {
                active.advance_durable(durable_revision, &history, durable_view.clone())?;
            }
            self.update_foundation_horizon(&history, &durable_view, durable_revision);
            return self.start_ready_renewals().await;
        };
        let (ready, projection) = {
            let active = self
                .active
                .get_mut(&peer)
                .expect("peer selected from active sessions");
            let admission = active
                .planner
                .record_observed_admission(
                    &active.kernel,
                    &entry,
                    SessionAdmission::from_replica(
                        &entry,
                        durable_admission,
                        MAX_SESSION_MESSAGE_BYTES as usize,
                    )
                    .map_err(|error| error.to_string())?,
                )
                .map_err(|error| error.to_string())?;
            let kind = active
                .confirm(admission, durable_revision, &history, durable_view.clone())?
                .unwrap_or(RoomSessionProjectionKind::Confirmed);
            (active.retry_reifications()?, active.projection_event(kind))
        };
        for (other_peer, other) in &mut self.active {
            if *other_peer != peer {
                other.advance_durable(durable_revision, &history, durable_view.clone())?;
            }
        }
        self.update_foundation_horizon(&history, &durable_view, durable_revision);
        let durable = ready
            .iter()
            .map(|(plan, _)| durable_correlation(plan.correlation()))
            .collect();
        if self.presentation_peer == Some(peer) {
            self.emit_realtime_with_reset(
                peer,
                RoomSessionRealtimeEgress {
                    projection,
                    carrier: None,
                    durable,
                    trace: None,
                },
            )
            .await?;
        }
        self.enqueue_reifications(peer, ready).await?;
        self.start_ready_renewals().await
    }

    async fn enqueue_reifications(
        &mut self,
        peer: ActorId,
        reifications: impl IntoIterator<Item = (ReificationPlan, MusicOp)>,
    ) -> Result<(), String> {
        for (plan, command) in reifications {
            self.reifications
                .send(RoomSessionReification {
                    peer,
                    plan,
                    command,
                })
                .await
                .map_err(|_| "session reification queue closed".to_owned())?;
        }
        Ok(())
    }

    async fn advance_all(
        &mut self,
        history: DagSnapshot,
        durable_view: SharedPitchSet,
        durable_revision: u64,
    ) -> Result<(), String> {
        let mut projections = Vec::new();
        for (peer, active) in &mut self.active {
            if let Some(kind) =
                active.advance_durable(durable_revision, &history, durable_view.clone())?
                && self.presentation_peer == Some(*peer)
            {
                projections.push(active.projection_event(kind));
            }
        }
        self.update_foundation_horizon(&history, &durable_view, durable_revision);
        for projection in projections {
            self.emit(RoomSessionEgress::Realtime(RoomSessionRealtimeEgress {
                projection,
                carrier: None,
                durable: Vec::new(),
                trace: None,
            }))
            .await?;
        }
        self.start_ready_renewals().await
    }

    async fn reset_all_projections(
        &mut self,
        history: DagSnapshot,
        durable_view: SharedPitchSet,
        durable_revision: u64,
    ) -> Result<bool, String> {
        let mut selected = None;
        for (peer, active) in &mut self.active {
            let projection =
                active.resynchronize_exact(durable_revision, &history, durable_view.clone())?;
            if self.presentation_peer == Some(*peer) {
                selected = Some(projection);
            }
        }
        self.update_foundation_horizon(&history, &durable_view, durable_revision);
        let Some(projection) = selected else {
            self.start_ready_renewals().await?;
            return Ok(false);
        };
        self.emit(RoomSessionEgress::Realtime(RoomSessionRealtimeEgress {
            projection,
            carrier: None,
            durable: Vec::new(),
            trace: None,
        }))
        .await?;
        self.start_ready_renewals().await?;
        Ok(true)
    }

    fn update_foundation_horizon(
        &mut self,
        history: &DagSnapshot,
        durable_view: &SharedPitchSet,
        durable_revision: u64,
    ) {
        for foundation in self.foundations.values_mut() {
            foundation.history = history.clone();
            foundation.durable_view = durable_view.clone();
            foundation.durable_revision = durable_revision;
        }
    }

    async fn emit_realtime_with_reset(
        &mut self,
        peer: ActorId,
        event: RoomSessionRealtimeEgress,
    ) -> Result<(), String> {
        let first = self.emit(RoomSessionEgress::Realtime(event.clone())).await;
        let Err(first_error) = first else {
            return Ok(());
        };

        // The session kernel has already advanced. A rejected bounded-window
        // acceptance must therefore establish a new projection continuity
        // generation before retrying; silently continuing would make the
        // mutation invisible while later deltas appeared contiguous.
        let projection = self
            .active
            .get_mut(&peer)
            .ok_or("session disappeared while resetting rejected egress")?
            .reset_projection()?;
        let recovery = RoomSessionRealtimeEgress {
            projection,
            carrier: event.carrier,
            durable: event.durable,
            trace: event.trace,
        };
        self.emit(RoomSessionEgress::Realtime(recovery))
            .await
            .map_err(|recovery_error| {
                format!(
                    "session egress was rejected ({first_error}); continuity reset was also rejected ({recovery_error})"
                )
            })
    }

    async fn emit(&self, event: RoomSessionEgress) -> Result<(), String> {
        self.events
            .as_ref()
            .ok_or("worker session event port is not configured")?
            .send_application_frame(ROOM_SESSION_CHANNEL, encode(&event)?)
            .await
            .map_err(|error| error.to_string())
    }

    async fn emit_renewal_trace(
        &self,
        stage: RoomSessionRenewalTraceStage,
        peer: ActorId,
        epoch: SessionEpoch,
        floor_epoch: Option<SessionEpoch>,
    ) -> Result<(), String> {
        if !self.trace_enabled {
            return Ok(());
        }
        self.emit(RoomSessionEgress::RenewalTrace(RoomSessionRenewalTrace {
            stage,
            peer: peer.0,
            epoch: epoch.get(),
            floor_epoch: floor_epoch.map(SessionEpoch::get),
        }))
        .await
    }

    async fn diagnostic(&self, message: String) {
        let _ = self.emit(RoomSessionEgress::Diagnostic(message)).await;
    }
}

struct LocalSessionEvent {
    command: MusicOp,
    frame: Vec<u8>,
    plan: Option<ReificationPlan>,
    trace: Option<RoomSessionCompactTrace>,
}

#[derive(Debug)]
struct RemoteSessionEvent {
    changed: Option<RoomSessionProjectionKind>,
    trace: Option<RoomSessionCompactTrace>,
}

#[cfg(test)]
struct DisabledRoomSessionTraceClock;

#[cfg(test)]
impl RoomSessionTraceClock for DisabledRoomSessionTraceClock {
    fn now_micros(&self) -> Option<u64> {
        None
    }
}

impl ActiveSession {
    fn new(
        foundation: RoomSessionFoundation,
        session: AuthorizedSession<2>,
        session_id: u64,
        keys: SessionKeys,
        lease_clock_origin: u64,
    ) -> Result<Self, String> {
        let local_seat = seat_for(foundation.local, foundation.peer);
        let remote_seat = 1 - local_seat;
        let sender_binding =
            DirectedSessionBinding::new(&session, SessionKeyEpoch::new(1), local_seat, remote_seat)
                .map_err(|error| error.to_string())?;
        let receiver_binding =
            DirectedSessionBinding::new(&session, SessionKeyEpoch::new(1), remote_seat, local_seat)
                .map_err(|error| error.to_string())?;
        let export_context = export_context(
            session.manifest_digest(),
            session.manifest().channel_binding(),
        );
        let mut secrets = keys.export_directional(&export_context);
        let send_key = XChaCha20Poly1305Key::from_bytes(secrets.take_send());
        let receive_key = XChaCha20Poly1305Key::from_bytes(secrets.take_receive());
        let sender = SessionSenderLane::new(
            sender_binding.clone(),
            XChaCha20Poly1305Profile::new(
                XChaChaCounterNonceSource::for_binding(&sender_binding)
                    .map_err(|error| error.to_string())?,
            ),
            send_key.clone(),
        )
        .map_err(|error| error.to_string())?;
        let echo_receiver = SessionReceiverLane::new(
            sender_binding.clone(),
            XChaCha20Poly1305Profile::new(
                XChaChaCounterNonceSource::for_binding(&sender_binding)
                    .map_err(|error| error.to_string())?,
            ),
            send_key,
        )
        .map_err(|error| error.to_string())?;
        let receiver = SessionReceiverLane::new(
            receiver_binding.clone(),
            XChaCha20Poly1305Profile::new(
                XChaChaCounterNonceSource::for_binding(&receiver_binding)
                    .map_err(|error| error.to_string())?,
            ),
            receive_key,
        )
        .map_err(|error| error.to_string())?;
        let sender_codec = XChaChaCompactPacketCodec::for_binding(&sender_binding)
            .map_err(|error| error.to_string())?;
        let receiver_codec = XChaChaCompactPacketCodec::for_binding(&receiver_binding)
            .map_err(|error| error.to_string())?;
        let kernel = session
            .kernel::<MusicOp, SESSION_CAPACITY>()
            .map_err(|error| error.to_string())?;
        let initial_cut = kernel
            .closed_cut(hhhs_session::CausalContext::zero())
            .map_err(|error| error.to_string())?;
        let projection = SessionProjectionHost::new(
            &session,
            initial_cut,
            ProjectionGeneration::new(1),
            SimulationTime::from_ticks(LEASE_START),
            DurableProjection::new(
                foundation.durable_revision,
                session.manifest().base().clone(),
                history_root(&foundation.history),
                PitchProjectionState::durable(foundation.durable_view.clone()),
            ),
        )
        .map_err(|error| error.to_string())?;
        let planner = session
            .reification_planner::<SESSION_CAPACITY>()
            .map_err(|error| error.to_string())?;
        let durable_revision = foundation.durable_revision;
        Ok(Self {
            foundation,
            session,
            session_id,
            local_seat,
            remote_seat,
            sender_binding,
            receiver_binding,
            sender_codec,
            receiver_codec,
            sender,
            echo_receiver,
            receiver,
            kernel,
            projection,
            planner,
            logical_time: LEASE_START,
            lease_clock_origin,
            lease_clock_last: lease_clock_origin,
            durable_revision,
            awaiting_reification: BTreeMap::new(),
        })
    }

    fn is_drained(&self) -> bool {
        self.projection.pending_len() == 0 && self.awaiting_reification.is_empty()
    }

    fn is_saturated(&self) -> bool {
        self.sender.highest_sealed_counter() >= MAX_EVENTS_PER_SEAT
            || self
                .kernel
                .ready_cut()
                .context()
                .counters()
                .iter()
                .any(|counter| *counter >= MAX_EVENTS_PER_SEAT)
    }

    fn local_event(
        &mut self,
        command: MusicOp,
        receiver_clock_ticks: u64,
        trace_clock: &dyn RoomSessionTraceClock,
        trace_enabled: bool,
        trace_token: Option<RoomSessionTraceToken>,
        worker_accepted_at_micros: Option<u64>,
    ) -> Result<LocalSessionEvent, String> {
        if self.is_saturated() {
            return Err("session epoch is saturated; use durable fallback until renewal".into());
        }
        if self.awaiting_reification.len() >= SESSION_CAPACITY {
            return Err(format!(
                "session has {} local events awaiting durable causal dependencies",
                SESSION_CAPACITY
            ));
        }
        self.logical_time = self.logical_time.saturating_add(1);
        let counter = self.sender.highest_sealed_counter().saturating_add(1);
        let header = self
            .sender_binding
            .header(
                counter,
                self.kernel.ready_cut().context(),
                EventClass::DurableCommand,
                PITCH_EDIT,
                SimulationTime::from_ticks(self.logical_time),
            )
            .map_err(|error| error.to_string())?;
        let payload = encode(&command)?;
        let packet = self
            .sender
            .seal(header, &payload)
            .map_err(|error| error.to_string())?;
        let frame = self
            .sender_codec
            .encode(&self.sender_binding, &packet)
            .map_err(|error| error.to_string())?;
        let decoded = self
            .sender_codec
            .decode(&self.sender_binding, &frame)
            .map_err(|error| error.to_string())?;
        let received = self
            .echo_receiver
            .receive(&decoded)
            .map_err(|error| error.to_string())?;
        let worker_authenticated_at_micros =
            trace_enabled.then(|| trace_clock.now_micros()).flatten();
        let receiver_now = self.receiver_lease_time(receiver_clock_ticks)?;
        let event = received
            .try_decode(decode_pitch_edit)
            .map_err(|error| error.to_owned())?;
        let worker_interpreted_at_micros =
            trace_enabled.then(|| trace_clock.now_micros()).flatten();
        let permitted = self
            .session
            .permit_event(receiver_now, event, frame.len())
            .map_err(|error| error.to_string())?;
        let worker_authorized_at_micros = trace_enabled.then(|| trace_clock.now_micros()).flatten();
        let trace = trace_enabled.then(|| RoomSessionCompactTrace {
            token: trace_token,
            correlation: compact_correlation(permitted.authenticated()),
            direction: RoomSessionTraceDirection::Local,
            worker_accepted_at_micros,
            carrier_received_at_micros: None,
            worker_authenticated_at_micros,
            worker_authorized_at_micros,
            worker_interpreted_at_micros,
            worker_projected_at_micros: None,
        });
        let dot = permitted.event().dot();
        self.ingest(permitted)?;
        let plan = match self.planner.plan(&self.kernel, dot) {
            Ok(plan) => Some(plan),
            Err(ReificationError::UnreifiedDependency(_)) => {
                self.awaiting_reification.insert(dot, command.clone());
                None
            }
            Err(error) => return Err(error.to_string()),
        };
        Ok(LocalSessionEvent {
            command,
            frame,
            plan,
            trace,
        })
    }

    fn retry_reifications(&mut self) -> Result<Vec<(ReificationPlan, MusicOp)>, String> {
        let mut ready = Vec::new();
        let dots: Vec<_> = self.awaiting_reification.keys().copied().collect();
        for dot in dots {
            match self.planner.plan(&self.kernel, dot) {
                Ok(plan) => {
                    let command = self
                        .awaiting_reification
                        .remove(&dot)
                        .expect("dot came from the bounded wait map");
                    ready.push((plan, command));
                }
                Err(ReificationError::UnreifiedDependency(_)) => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        Ok(ready)
    }

    fn ingest_remote(
        &mut self,
        frame: &[u8],
        receiver_clock_ticks: u64,
        trace_clock: &dyn RoomSessionTraceClock,
        trace_enabled: bool,
        worker_accepted_at_micros: Option<u64>,
        carrier_received_at_micros: Option<u64>,
    ) -> Result<Option<RemoteSessionEvent>, String> {
        let packet = self
            .receiver_codec
            .decode(&self.receiver_binding, frame)
            .map_err(|error| error.to_string())?;
        let at = packet.header().effective_at().ticks();
        let received = self
            .receiver
            .receive(&packet)
            .map_err(|error| error.to_string())?;
        if received.disposition() != ReplayDisposition::Fresh {
            return Ok(None);
        }
        let worker_authenticated_at_micros =
            trace_enabled.then(|| trace_clock.now_micros()).flatten();
        let receiver_now = self.receiver_lease_time(receiver_clock_ticks)?;
        let event = received
            .try_decode(decode_pitch_edit)
            .map_err(|error| error.to_owned())?;
        let worker_interpreted_at_micros =
            trace_enabled.then(|| trace_clock.now_micros()).flatten();
        let permitted = self
            .session
            .permit_event(receiver_now, event, frame.len())
            .map_err(|error| error.to_string())?;
        let worker_authorized_at_micros = trace_enabled.then(|| trace_clock.now_micros()).flatten();
        let trace = trace_enabled.then(|| RoomSessionCompactTrace {
            token: None,
            correlation: compact_correlation(permitted.authenticated()),
            direction: RoomSessionTraceDirection::Remote,
            worker_accepted_at_micros,
            carrier_received_at_micros,
            worker_authenticated_at_micros,
            worker_authorized_at_micros,
            worker_interpreted_at_micros,
            worker_projected_at_micros: None,
        });
        self.logical_time = self.logical_time.max(at);
        let changed = self.ingest(permitted)?;
        Ok(Some(RemoteSessionEvent { changed, trace }))
    }

    fn receiver_lease_time(&mut self, clock_ticks: u64) -> Result<SessionLeaseTime, String> {
        if clock_ticks < self.lease_clock_last {
            return Err("receiver session lease clock moved backwards".into());
        }
        self.lease_clock_last = clock_ticks;
        let elapsed = clock_ticks.saturating_sub(self.lease_clock_origin);
        Ok(SessionLeaseTime::from_ticks(
            LEASE_START.saturating_add(elapsed),
        ))
    }

    fn ingest(
        &mut self,
        event: hhhs_session::PermittedEvent<MusicOp, 2>,
    ) -> Result<Option<RoomSessionProjectionKind>, String> {
        // Effective time is authenticated session meaning, not arrival order.
        // Advance the deterministic simulation clock before prediction so an
        // event at tick N is not correctly retained as "future" by a
        // projection still parked at N-1.
        let time_changed = self
            .projection
            .advance_simulation(
                SimulationTime::from_ticks(self.logical_time),
                &PitchProjector,
            )
            .map_err(|error| error.to_string())?
            .is_some();
        let from = self.projection.projected_cut();
        let report = self
            .kernel
            .ingest(event)
            .map_err(|error| error.to_string())?;
        if !report.readiness().advanced() {
            return Ok(time_changed.then_some(RoomSessionProjectionKind::Predicted));
        }
        let transition = self
            .projection
            .predict_between(&self.kernel, from, self.kernel.ready_cut(), &PitchProjector)
            .map_err(|error| error.to_string())?;
        Ok((time_changed || transition.is_some()).then_some(RoomSessionProjectionKind::Predicted))
    }

    fn confirm(
        &mut self,
        admission: SessionAdmission,
        revision: u64,
        history: &DagSnapshot,
        durable_view: SharedPitchSet,
    ) -> Result<Option<RoomSessionProjectionKind>, String> {
        if revision <= self.durable_revision {
            return Ok(None);
        }
        let durable = DurableProjection::new(
            revision,
            history.frontier(),
            history_root(history),
            PitchProjectionState::durable(durable_view),
        );
        let kind = if revision == self.durable_revision.saturating_add(1) {
            let transition = self
                .projection
                .confirm(admission, durable, &PitchProjector)
                .map_err(|error| error.to_string())?;
            transition
                .as_ref()
                .map(|transition| projection_kind(transition.change()))
        } else {
            self.resynchronize(revision, history, durable)?;
            Some(RoomSessionProjectionKind::Reset)
        };
        self.durable_revision = revision;
        self.foundation.history = history.clone();
        self.foundation.durable_view = self.projection.durable().view().visible.clone();
        self.foundation.durable_revision = revision;
        Ok(kind)
    }

    fn advance_durable(
        &mut self,
        revision: u64,
        history: &DagSnapshot,
        durable_view: SharedPitchSet,
    ) -> Result<Option<RoomSessionProjectionKind>, String> {
        if revision <= self.durable_revision {
            return Ok(None);
        }
        let durable = DurableProjection::new(
            revision,
            history.frontier(),
            history_root(history),
            PitchProjectionState::durable(durable_view),
        );
        let kind = if revision == self.durable_revision.saturating_add(1) {
            let transition = self
                .projection
                .advance_durable(durable, &PitchProjector)
                .map_err(|error| error.to_string())?;
            Some(projection_kind(transition.change()))
        } else {
            self.resynchronize(revision, history, durable)?;
            Some(RoomSessionProjectionKind::Reset)
        };
        self.durable_revision = revision;
        self.foundation.history = history.clone();
        self.foundation.durable_view = self.projection.durable().view().visible.clone();
        self.foundation.durable_revision = revision;
        Ok(kind)
    }

    fn resynchronize(
        &mut self,
        revision: u64,
        history: &DagSnapshot,
        durable: DurableProjection<PitchProjectionState>,
    ) -> Result<(), String> {
        self.projection
            .resynchronize(
                ProjectionGeneration::new(self.projection.generation().get().saturating_add(1)),
                &self.kernel,
                SimulationTime::from_ticks(self.logical_time),
                DurableProjectionHorizon::new(durable, history, MAX_SESSION_MESSAGE_BYTES as usize),
                &PitchProjector,
            )
            .map_err(|error| error.to_string())?;
        self.durable_revision = revision;
        Ok(())
    }

    fn reset_projection(&mut self) -> Result<RoomSessionProjection, String> {
        let durable = self.projection.durable().clone();
        let history = self.foundation.history.clone();
        self.resynchronize(self.durable_revision, &history, durable)?;
        Ok(self.projection_event(RoomSessionProjectionKind::Reset))
    }

    fn resynchronize_exact(
        &mut self,
        revision: u64,
        history: &DagSnapshot,
        durable_view: SharedPitchSet,
    ) -> Result<RoomSessionProjection, String> {
        let durable = DurableProjection::new(
            revision,
            history.frontier(),
            history_root(history),
            PitchProjectionState::durable(durable_view),
        );
        self.resynchronize(revision, history, durable)?;
        self.foundation.history = history.clone();
        self.foundation.durable_view = self.projection.durable().view().visible.clone();
        self.foundation.durable_revision = revision;
        Ok(self.projection_event(RoomSessionProjectionKind::Reset))
    }

    fn projection_event(&self, kind: RoomSessionProjectionKind) -> RoomSessionProjection {
        let snapshot = self.projection.snapshot();
        RoomSessionProjection {
            manifest: *self.session.manifest_digest().as_bytes(),
            session_id: self.session_id,
            epoch: self.session.manifest().epoch().get(),
            generation: snapshot.generation().get(),
            sequence: snapshot.sequence(),
            durable_revision: snapshot.durable().revision(),
            durable_root: *snapshot.durable().history_root().as_bytes(),
            kind,
            view: snapshot.view().visible.clone(),
        }
    }
}

fn projection_kind(change: &SessionProjectionChange<2>) -> RoomSessionProjectionKind {
    match change {
        SessionProjectionChange::Predicted { .. }
        | SessionProjectionChange::SimulationAdvanced { .. } => {
            RoomSessionProjectionKind::Predicted
        }
        SessionProjectionChange::Confirmed { .. } => RoomSessionProjectionKind::Confirmed,
        SessionProjectionChange::Corrected { .. } | SessionProjectionChange::Rejected { .. } => {
            RoomSessionProjectionKind::Corrected
        }
        SessionProjectionChange::DurableAdvanced { .. } => RoomSessionProjectionKind::Advanced,
        SessionProjectionChange::Reset { .. } => RoomSessionProjectionKind::Reset,
        _ => RoomSessionProjectionKind::Reset,
    }
}

fn durable_correlation(
    correlation: hhhs_session::SessionCorrelation,
) -> RoomSessionDurableCorrelation {
    let dot = correlation.dot();
    RoomSessionDurableCorrelation {
        manifest: *correlation.manifest().as_bytes(),
        epoch: dot.epoch().get(),
        seat: dot.seat(),
        counter: dot.counter(),
        event: *correlation.event().as_bytes(),
    }
}

fn compact_correlation(
    event: &hhhs_session::AuthenticatedEvent<MusicOp, 2>,
) -> RoomSessionDurableCorrelation {
    let dot = event.event().dot();
    RoomSessionDurableCorrelation {
        manifest: *event.manifest().as_bytes(),
        epoch: dot.epoch().get(),
        seat: dot.seat(),
        counter: dot.counter(),
        event: *event.identity().as_bytes(),
    }
}

#[derive(Clone, PartialEq, Eq)]
struct PitchProjectionState {
    base: SharedPitchSet,
    removed_base_degrees: BTreeSet<TunedDegree>,
    removed_base_pitches: BTreeSet<TunedPeriodicPitch>,
    degree_adds: BTreeMap<PendingPitchFactId, TunedDegree>,
    pitch_adds: BTreeMap<PendingPitchFactId, TunedPeriodicPitch>,
    visible: SharedPitchSet,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct PendingPitchFactId {
    manifest: [u8; 32],
    dot: SessionDot,
    event: [u8; 32],
}

impl PendingPitchFactId {
    fn from_correlation(correlation: hhhs_session::SessionCorrelation) -> Self {
        Self {
            manifest: *correlation.manifest().as_bytes(),
            dot: correlation.dot(),
            event: *correlation.event().as_bytes(),
        }
    }
}

impl PitchProjectionState {
    fn durable(base: SharedPitchSet) -> Self {
        Self {
            visible: base.clone(),
            base,
            removed_base_degrees: BTreeSet::new(),
            removed_base_pitches: BTreeSet::new(),
            degree_adds: BTreeMap::new(),
            pitch_adds: BTreeMap::new(),
        }
    }

    fn rebuild_visible(&mut self) {
        let mut visible = self.base.clone();
        for degree in &self.removed_base_degrees {
            visible.pitch_classes.remove(degree);
        }
        for pitch in &self.removed_base_pitches {
            visible.pitches.remove(pitch);
        }
        visible
            .pitch_classes
            .extend(self.degree_adds.values().copied());
        visible.pitches.extend(self.pitch_adds.values().copied());
        self.visible = visible;
    }

    fn apply_event(
        &mut self,
        fact: PendingPitchFactId,
        event: &SessionEvent<MusicOp, 2>,
    ) -> Result<(), &'static str> {
        match event.payload() {
            MusicOp::AddDegree { degree } => {
                self.degree_adds.insert(fact, *degree);
            }
            MusicOp::RemoveDegree { degree } => {
                self.removed_base_degrees.insert(*degree);
                self.degree_adds.retain(|add, candidate| {
                    candidate != degree
                        || add.manifest != fact.manifest
                        || !event.dependencies().observes(add.dot)
                });
            }
            MusicOp::AddPitch { pitch } => {
                self.pitch_adds.insert(fact, *pitch);
            }
            MusicOp::RemovePitch { pitch } => {
                self.removed_base_pitches.insert(*pitch);
                self.pitch_adds.retain(|add, candidate| {
                    candidate != pitch
                        || add.manifest != fact.manifest
                        || !event.dependencies().observes(add.dot)
                });
            }
            _ => return Err("non-pitch command entered the pitch session projector"),
        }
        self.rebuild_visible();
        Ok(())
    }
}

struct PitchProjector;

impl SessionProjector<MusicOp, PitchProjectionState, 2> for PitchProjector {
    type Error = &'static str;

    fn apply(
        &self,
        view: &mut PitchProjectionState,
        correlation: hhhs_session::SessionCorrelation,
        event: &SessionEvent<MusicOp, 2>,
    ) -> Result<(), Self::Error> {
        view.apply_event(PendingPitchFactId::from_correlation(correlation), event)
    }
}

fn decode_pitch_edit(code: SessionEventCode, bytes: Vec<u8>) -> Result<MusicOp, &'static str> {
    if code != PITCH_EDIT {
        return Err("unknown pitch session event code");
    }
    let command = decode::<MusicOp>(&bytes).map_err(|_| "malformed pitch session command")?;
    is_pitch_edit(&command)
        .then_some(command)
        .ok_or("non-pitch command in pitch session")
}

fn is_pitch_edit(command: &MusicOp) -> bool {
    matches!(
        command,
        MusicOp::AddDegree { .. }
            | MusicOp::RemoveDegree { .. }
            | MusicOp::AddPitch { .. }
            | MusicOp::RemovePitch { .. }
    )
}

fn build_manifest(
    foundation: &RoomSessionFoundation,
    session_id: u64,
    epoch: SessionEpoch,
) -> Result<SessionManifest<2>, String> {
    let profile = FoundationProfileId::for_domain(FOUNDATION_PROFILE);
    let (first, second) = ordered(foundation.local, foundation.peer);
    SessionManifest::builder()
        .epoch(epoch)
        .namespace(foundation.identity.music)
        .base(foundation.history.frontier())
        .rules(Digest::of(SESSION_RULES))
        .vocabulary(Digest::of(SESSION_VOCABULARY))
        .area(tutti_music_hhhs::notes_area(foundation.identity.music))
        .allowed(AllowedMessageClasses::DURABLE_COMMAND)
        .lease(
            SessionLeaseTime::from_ticks(LEASE_START),
            SessionLeaseTime::from_ticks(LEASE_END),
        )
        .max_events_per_seat(MAX_EVENTS_PER_SEAT)
        .max_message_bytes(MAX_SESSION_MESSAGE_BYTES)
        .security_profile(xchacha20poly1305_profile_id())
        .channel_binding(Digest(
            *establishment_binding(&foundation.identity, first, second, session_id).as_bytes(),
        ))
        .seats([
            SessionSeat::new(first.receiver(), profile),
            SessionSeat::new(second.receiver(), profile),
        ])
        .build()
        .map_err(|error| error.to_string())
}

fn authorize_pair(
    foundation: &RoomSessionFoundation,
    manifest: SessionManifest<2>,
    local: VerifiedSeatFoundation,
    remote: VerifiedSeatFoundation,
    renewal_floor: Option<&SessionRenewalFloor>,
) -> Result<PairAuthorization, String> {
    let policy = SessionPolicy::builder()
        .namespace(foundation.identity.music)
        .rules(manifest.rules())
        .vocabulary(manifest.vocabulary())
        .area(Area::root(foundation.identity.music))
        .supported(AllowedMessageClasses::DURABLE_COMMAND)
        .foundation_profiles([FoundationProfileId::for_domain(FOUNDATION_PROFILE)])
        .security_profiles([xchacha20poly1305_profile_id()])
        .max_seats(2)
        .max_duration(LEASE_END - LEASE_START)
        .max_events_per_seat(MAX_EVENTS_PER_SEAT)
        .max_message_bytes(MAX_SESSION_MESSAGE_BYTES)
        .build()
        .map_err(|error| error.to_string())?;
    let capabilities = CapabilitySnapshot::capture(&foundation.history, [foundation.music_root]);
    let foundations = if seat_for(foundation.local, foundation.peer) == 0 {
        [local, remote]
    } else {
        [remote, local]
    };
    match renewal_floor {
        Some(floor) => {
            authorize_session_renewal(&capabilities, &policy, floor, manifest, &foundations)
                .map(PairAuthorization::Renewal)
                .map_err(|error| error.to_string())
        }
        None => authorize_session(&capabilities, &policy, manifest, &foundations)
            .map(|session| {
                let floor = SessionRenewalFloor::from_authorized(&session);
                PairAuthorization::Initial { session, floor }
            })
            .map_err(|error| error.to_string()),
    }
}

fn proposed_epoch(floor: Option<&SessionRenewalFloor>) -> Result<SessionEpoch, String> {
    match floor {
        Some(floor) => floor.next_epoch().map_err(|error| error.to_string()),
        None => Ok(SessionEpoch::new(1)),
    }
}

fn prefer_incoming_offer(
    local: ActorId,
    remote: ActorId,
    incoming: SessionEpoch,
    pending: SessionEpoch,
) -> bool {
    incoming > pending || (incoming == pending && remote.0 < local.0)
}

fn renewal_key(foundation: &RoomSessionFoundation) -> RoomSessionRenewalKey {
    RoomSessionRenewalKey {
        room: *foundation.identity.object.as_bytes(),
        local: foundation.local.0,
        peer: foundation.peer.0,
    }
}

fn local_foundation(
    foundation: &RoomSessionFoundation,
    manifest: &SessionManifest<2>,
) -> Result<(VerifiedSeatFoundation, Vec<u8>), String> {
    let seat = seat_for(foundation.local, foundation.peer);
    let claim = SeatFoundationClaim::new(
        seat,
        foundation.local.receiver(),
        FoundationProfileId::for_domain(FOUNDATION_PROFILE),
        foundation.local_grants.clone(),
        manifest.digest(),
        manifest.base().clone(),
    )
    .map_err(|error| error.to_string())?;
    let context = foundation_context(foundation.identity.music, manifest, &claim)?;
    let presentation = Ed25519Verifier::present(
        &foundation.signing_key,
        foundation.local_grants.clone(),
        &context,
    )
    .map_err(|error| format!("session foundation signing failed: {error:?}"))?;
    let encoded = presentation.encode();
    let verified = verify_claim(claim, &presentation, &context)?;
    Ok((verified, encoded))
}

fn verify_foundation(
    foundation: &RoomSessionFoundation,
    manifest: &SessionManifest<2>,
    actor: ActorId,
    grants: &[EntryHash],
    encoded: &[u8],
) -> Result<VerifiedSeatFoundation, String> {
    let claim = SeatFoundationClaim::new(
        seat_for(actor, foundation.local),
        actor.receiver(),
        FoundationProfileId::for_domain(FOUNDATION_PROFILE),
        grants.to_vec(),
        manifest.digest(),
        manifest.base().clone(),
    )
    .map_err(|error| error.to_string())?;
    let context = foundation_context(foundation.identity.music, manifest, &claim)?;
    let presentation = PresentationEnvelope::decode(encoded)
        .map_err(|error| format!("session foundation decoding failed: {error:?}"))?;
    verify_claim(claim, &presentation, &context)
}

fn foundation_context(
    namespace: Digest,
    manifest: &SessionManifest<2>,
    claim: &SeatFoundationClaim,
) -> Result<PresentationContext, String> {
    PresentationContext::new(
        namespace,
        Digest::of(&claim.canonical_bytes()),
        manifest.base().clone(),
        tutti_music_hhhs::notes_area(namespace),
        Right::Invoke,
    )
    .map_err(|error| format!("session foundation context failed: {error:?}"))
}

fn verify_claim(
    claim: SeatFoundationClaim,
    presentation: &PresentationEnvelope,
    context: &PresentationContext,
) -> Result<VerifiedSeatFoundation, String> {
    let mut verifiers = VerifierRegistry::new();
    verifiers
        .register(Arc::new(Ed25519Verifier))
        .map_err(|error| format!("session verifier registration failed: {error:?}"))?;
    let verified = verifiers
        .verify(presentation, context)
        .map_err(|error| format!("session foundation verification failed: {error:?}"))?;
    if verified.receiver() != claim.receiver()
        || verified.presented() != claim.presented()
        || verified.context() != context
    {
        return Err("session foundation proof claims do not match the manifest seat".into());
    }
    Ok(VerifiedSeatFoundation::assume_verified(claim))
}

fn ordered(left: ActorId, right: ActorId) -> (ActorId, ActorId) {
    if left.0 <= right.0 {
        (left, right)
    } else {
        (right, left)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use futures::executor::block_on;

    use super::*;
    use crate::room::v5::RoomReplicas;

    #[derive(Default)]
    struct TestRenewalStore {
        values: RefCell<BTreeMap<RoomSessionRenewalKey, Vec<u8>>>,
        persist_calls: Cell<usize>,
        fail_persist: Cell<bool>,
    }

    impl RoomSessionRenewalStore for TestRenewalStore {
        fn load<'a>(
            &'a self,
            key: RoomSessionRenewalKey,
        ) -> Pin<Box<dyn Future<Output = Result<Option<Vec<u8>>, String>> + 'a>> {
            Box::pin(async move { Ok(self.values.borrow().get(&key).cloned()) })
        }

        fn persist<'a>(
            &'a self,
            key: RoomSessionRenewalKey,
            floor: Vec<u8>,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + 'a>> {
            Box::pin(async move {
                self.persist_calls
                    .set(self.persist_calls.get().saturating_add(1));
                if self.fail_persist.get() {
                    return Err("injected persistence failure".into());
                }
                self.values.borrow_mut().insert(key, floor);
                Ok(())
            })
        }
    }

    #[derive(Default)]
    struct TestLeaseClock(Cell<u64>);

    impl RoomSessionLeaseClock for TestLeaseClock {
        fn now_ticks(&self) -> Result<u64, String> {
            Ok(self.0.get())
        }
    }

    fn test_lease_clock() -> Rc<dyn RoomSessionLeaseClock> {
        Rc::new(TestLeaseClock::default())
    }

    #[derive(Default)]
    struct CountingTraceClock(Cell<u32>);

    impl RoomSessionTraceClock for CountingTraceClock {
        fn now_micros(&self) -> Option<u64> {
            let next = self.0.get().saturating_add(1);
            self.0.set(next);
            Some(u64::from(next))
        }
    }

    fn renewal_foundations() -> (RoomSessionFoundation, RoomSessionFoundation) {
        let owner_key = SigningKey::from_bytes(&[41; 32]);
        let member_key = SigningKey::from_bytes(&[42; 32]);
        let owner = ActorId::from_signing_key(&owner_key);
        let member = ActorId::from_signing_key(&member_key);
        let room = RoomReplicas::memory("session-renewal-gate", owner).unwrap();
        room.grant_member(&owner_key, member).unwrap();
        let snapshot = room.music_snapshot();
        let music_root = room.owner_capabilities().music[0];
        let owner_grants = room.capabilities_for_lane(owner, super::super::v5::RoomLane::Music);
        let member_grants = room.capabilities_for_lane(member, super::super::v5::RoomLane::Music);
        let durable_view = room.view().music.shared_pitches;
        let owner_foundation = RoomSessionFoundation {
            identity: room.identity().clone(),
            local: owner,
            peer: member,
            signing_key: owner_key,
            history: snapshot.history.clone(),
            music_root,
            local_grants: owner_grants.clone(),
            peer_grants: member_grants.clone(),
            durable_view: durable_view.clone(),
            durable_revision: snapshot.sequence,
        };
        let member_foundation = RoomSessionFoundation {
            identity: room.identity().clone(),
            local: member,
            peer: owner,
            signing_key: member_key,
            history: snapshot.history,
            music_root,
            local_grants: member_grants,
            peer_grants: owner_grants,
            durable_view,
            durable_revision: snapshot.sequence,
        };
        (owner_foundation, member_foundation)
    }

    fn authorize_fixture(
        owner: &RoomSessionFoundation,
        member: &RoomSessionFoundation,
        session_id: u64,
        epoch: u32,
        floor: Option<&SessionRenewalFloor>,
    ) -> Result<(AuthorizedSession<2>, SessionRenewalFloor), String> {
        let manifest = build_manifest(owner, session_id, SessionEpoch::new(epoch))?;
        let (owner_foundation, _) = local_foundation(owner, &manifest)?;
        let (member_foundation, _) = local_foundation(member, &manifest)?;
        match authorize_pair(owner, manifest, owner_foundation, member_foundation, floor)? {
            PairAuthorization::Initial { session, floor } => Ok((session, floor)),
            PairAuthorization::Renewal(renewal) => renewal
                // Unit fixtures use an explicit successful in-memory install
                // boundary. Production renewal crosses RoomSessionRenewalStore.
                .install_with(|_| Ok::<(), String>(()))
                .map(|installed| installed.into_parts()),
        }
    }

    fn initiator_keys(
        owner: &RoomSessionFoundation,
        member: &RoomSessionFoundation,
        session_id: u64,
    ) -> SessionKeys {
        session_key_pair(owner, member, session_id).0
    }

    fn session_key_pair(
        owner: &RoomSessionFoundation,
        member: &RoomSessionFoundation,
        session_id: u64,
    ) -> (SessionKeys, SessionKeys) {
        let binding = establishment_binding(&owner.identity, owner.local, member.local, session_id);
        let (pending, offer) = PendingInitiator::begin(
            &owner.signing_key,
            ProtocolId::derive(SESSION_PROTOCOL_LABEL),
            binding,
            session_id,
            EphemeralSecret::from_bytes([71; 32]),
        );
        let verified = Offer::decode(offer.as_bytes())
            .unwrap()
            .verify(ProtocolId::derive(SESSION_PROTOCOL_LABEL), binding)
            .unwrap();
        let (answer, responder_keys) = verified
            .respond(&member.signing_key, EphemeralSecret::from_bytes([72; 32]))
            .unwrap();
        let initiator_keys = pending
            .complete(answer.as_bytes(), PeerIdentity::from_bytes(member.local.0))
            .unwrap();
        (initiator_keys, responder_keys)
    }

    fn degree() -> TunedDegree {
        TunedDegree::new(&tutti_music::Tuning::twelve_tet(), 7).unwrap()
    }

    #[test]
    fn renewal_floor_heals_skew_and_never_reopens_a_persisted_epoch() {
        let (owner, member) = renewal_foundations();
        let first_manifest = build_manifest(&owner, 11, SessionEpoch::new(1)).unwrap();
        let same_epoch_other_session = build_manifest(&owner, 99, SessionEpoch::new(1)).unwrap();
        assert_eq!(first_manifest.epoch(), SessionEpoch::new(1));
        assert_eq!(same_epoch_other_session.epoch(), SessionEpoch::new(1));
        assert_ne!(first_manifest.digest(), same_epoch_other_session.digest());

        let (_, floor_one) = authorize_fixture(&owner, &member, 11, 1, None).unwrap();
        // A receiver is allowed to heal a partial install by accepting any
        // strictly higher authorized epoch, not only its local minimum.
        let (_, floor_three) = authorize_fixture(&owner, &member, 33, 3, Some(&floor_one)).unwrap();
        let persisted = floor_three.to_bytes();

        // Crash after floor persistence but before in-memory activation. On
        // restart, the recovered floor cannot authorize either the old epoch or
        // the just-persisted epoch again; it proposes the minimum successor.
        let recovered = SessionRenewalFloor::from_bytes(&persisted).unwrap();
        assert_eq!(
            proposed_epoch(Some(&recovered)).unwrap(),
            SessionEpoch::new(4)
        );
        assert!(authorize_fixture(&owner, &member, 11, 1, Some(&recovered)).is_err());
        assert!(authorize_fixture(&owner, &member, 33, 3, Some(&recovered)).is_err());
        assert!(authorize_fixture(&owner, &member, 44, 4, Some(&recovered)).is_ok());
    }

    #[test]
    fn old_epoch_accepts_64_events_but_never_attempts_counter_65() {
        let (owner, member) = renewal_foundations();
        let session_id = 51;
        let (authorized, _) = authorize_fixture(&owner, &member, session_id, 1, None).unwrap();
        let keys = initiator_keys(&owner, &member, session_id);
        let mut active = ActiveSession::new(owner, authorized, session_id, keys, 0).unwrap();
        for _ in 0..MAX_EVENTS_PER_SEAT {
            active
                .local_event(
                    MusicOp::AddDegree { degree: degree() },
                    0,
                    &DisabledRoomSessionTraceClock,
                    false,
                    None,
                    None,
                )
                .unwrap();
        }
        assert_eq!(active.sender.highest_sealed_counter(), MAX_EVENTS_PER_SEAT);
        assert!(active.is_saturated());
        assert!(
            active
                .local_event(
                    MusicOp::AddDegree { degree: degree() },
                    0,
                    &DisabledRoomSessionTraceClock,
                    false,
                    None,
                    None,
                )
                .is_err()
        );
        assert_eq!(active.sender.highest_sealed_counter(), MAX_EVENTS_PER_SEAT);
        assert_eq!(active.kernel.fault(), None);
    }

    #[test]
    fn compact_trace_is_absent_and_clock_is_unsampled_unless_explicitly_enabled() {
        let (owner, member) = renewal_foundations();
        let token = RoomSessionTraceToken {
            scope: 17,
            sequence: 23,
        };
        let disabled_clock = CountingTraceClock::default();
        let (disabled_authorized, _) = authorize_fixture(&owner, &member, 61, 1, None).unwrap();
        let disabled_keys = initiator_keys(&owner, &member, 61);
        let mut disabled =
            ActiveSession::new(owner.clone(), disabled_authorized, 61, disabled_keys, 0).unwrap();
        let event = disabled
            .local_event(
                MusicOp::AddDegree { degree: degree() },
                0,
                &disabled_clock,
                false,
                Some(token),
                Some(99),
            )
            .unwrap();
        assert_eq!(event.trace, None);
        assert_eq!(disabled_clock.0.get(), 0);

        let enabled_clock = CountingTraceClock::default();
        let (enabled_authorized, _) = authorize_fixture(&owner, &member, 62, 1, None).unwrap();
        let enabled_keys = initiator_keys(&owner, &member, 62);
        let mut enabled =
            ActiveSession::new(owner, enabled_authorized, 62, enabled_keys, 0).unwrap();
        let trace = enabled
            .local_event(
                MusicOp::AddDegree { degree: degree() },
                0,
                &enabled_clock,
                true,
                Some(token),
                Some(99),
            )
            .unwrap()
            .trace
            .expect("explicit tracing should attach one application-sideband trace");
        assert_eq!(trace.token, Some(token));
        assert!(enabled_clock.0.get() > 0);
    }

    #[test]
    fn expired_receiver_clock_refuses_an_authentic_in_lease_simulation_event() {
        let (owner, member) = renewal_foundations();
        let session_id = 52;
        let (owner_authorized, _) =
            authorize_fixture(&owner, &member, session_id, 1, None).unwrap();
        let (member_authorized, _) =
            authorize_fixture(&member, &owner, session_id, 1, None).unwrap();
        let (owner_keys, member_keys) = session_key_pair(&owner, &member, session_id);
        let mut sender =
            ActiveSession::new(owner, owner_authorized, session_id, owner_keys, 0).unwrap();
        let mut receiver =
            ActiveSession::new(member, member_authorized, session_id, member_keys, 0).unwrap();
        let local = sender
            .local_event(
                MusicOp::AddDegree { degree: degree() },
                0,
                &DisabledRoomSessionTraceClock,
                false,
                None,
                None,
            )
            .unwrap();

        // The frame is cryptographically valid and its authenticated musical
        // simulation time lies inside the manifest interval. Neither fact says
        // the receiver's authority lease is still live when it arrives.
        let packet = receiver
            .receiver_codec
            .decode(&receiver.receiver_binding, &local.frame)
            .unwrap();
        assert!(packet.header().effective_at().ticks() >= LEASE_START);
        assert!(packet.header().effective_at().ticks() < LEASE_END);

        let kernel_before = receiver.kernel.ready_cut();
        let projection_before = receiver.projection.snapshot();
        let logical_time_before = receiver.logical_time;
        let expiry_elapsed = LEASE_END - LEASE_START;
        let error = receiver
            .ingest_remote(
                &local.frame,
                expiry_elapsed,
                &DisabledRoomSessionTraceClock,
                false,
                None,
                None,
            )
            .unwrap_err();
        assert!(error.contains("OutsideLease"));
        assert_eq!(receiver.kernel.ready_cut(), kernel_before);
        assert!(receiver.projection.snapshot() == projection_before);
        assert_eq!(receiver.logical_time, logical_time_before);
    }

    #[test]
    fn install_persists_before_answer_or_reset_becomes_observable() {
        let (owner, member) = renewal_foundations();
        let session_id = 61;
        let (authorized, floor) = authorize_fixture(&owner, &member, session_id, 1, None).unwrap();
        let keys = initiator_keys(&owner, &member, session_id);
        let store = Rc::new(TestRenewalStore::default());
        store.fail_persist.set(true);
        let (reifications, _) = mpsc::channel(1);
        let mut task = RoomSessionTask::new(reifications, store.clone(), test_lease_clock());

        let error = block_on(task.install_session(
            member.local,
            owner,
            PairAuthorization::Initial {
                session: authorized,
                floor,
            },
            session_id,
            keys,
            Some(vec![1, 2, 3]),
        ))
        .unwrap_err();
        assert!(error.contains("before observable install egress"));
        assert_eq!(store.persist_calls.get(), 1);
        assert!(store.values.borrow().is_empty());
        assert!(task.active.is_empty());
        assert!(task.renewal_floors.is_empty());
    }

    #[test]
    fn persisted_floor_survives_lost_answer_and_lost_reset_cuts() {
        for carrier in [Some(vec![4, 5, 6]), None] {
            let (owner, member) = renewal_foundations();
            let session_id = 62;
            let (authorized, floor) =
                authorize_fixture(&owner, &member, session_id, 1, None).unwrap();
            let keys = initiator_keys(&owner, &member, session_id);
            let store = Rc::new(TestRenewalStore::default());
            let (reifications, _) = mpsc::channel(1);
            let mut task = RoomSessionTask::new(reifications, store.clone(), test_lease_clock());

            // No event port is configured, deliberately modeling Answer/reset
            // loss immediately after the durable install boundary.
            let error = block_on(task.install_session(
                member.local,
                owner.clone(),
                PairAuthorization::Initial {
                    session: authorized,
                    floor,
                },
                session_id,
                keys,
                carrier,
            ))
            .unwrap_err();
            assert!(error.contains("after floor persistence"));
            let encoded = store
                .values
                .borrow()
                .get(&renewal_key(&owner))
                .cloned()
                .unwrap();
            let recovered = SessionRenewalFloor::from_bytes(&encoded).unwrap();
            assert_eq!(recovered.epoch(), SessionEpoch::new(1));
            assert_eq!(
                proposed_epoch(Some(&recovered)).unwrap(),
                SessionEpoch::new(2)
            );
            assert!(task.active.is_empty());
        }
    }

    #[test]
    fn simultaneous_same_epoch_offers_use_actor_identity_tie_break() {
        let lower = ActorId([1; 32]);
        let higher = ActorId([2; 32]);
        let epoch = SessionEpoch::new(7);
        assert!(prefer_incoming_offer(higher, lower, epoch, epoch));
        assert!(!prefer_incoming_offer(lower, higher, epoch, epoch));
        assert!(prefer_incoming_offer(
            lower,
            higher,
            SessionEpoch::new(8),
            epoch,
        ));
        assert!(!prefer_incoming_offer(
            higher,
            lower,
            SessionEpoch::new(6),
            epoch,
        ));
    }

    fn fact(manifest: u8, seat: u8, counter: u32) -> PendingPitchFactId {
        PendingPitchFactId {
            manifest: [manifest; 32],
            dot: SessionDot::new(SessionEpoch::new(1), seat, counter),
            event: [manifest.wrapping_add(seat); 32],
        }
    }

    fn pitch_event(
        seat: u8,
        counter: u32,
        dependencies: [u32; 2],
        payload: MusicOp,
    ) -> SessionEvent<MusicOp, 2> {
        SessionEvent::new(
            SessionDot::new(SessionEpoch::new(1), seat, counter),
            hhhs_session::CausalContext::from_counters(dependencies),
            SimulationTime::from_ticks(u64::from(counter)),
            EventClass::DurableCommand,
            payload,
        )
    }

    #[test]
    fn pitch_projection_is_add_wins_in_both_concurrent_visit_orders() {
        let degree = degree();
        let add = pitch_event(0, 1, [0, 0], MusicOp::AddDegree { degree });
        let remove = pitch_event(1, 1, [0, 0], MusicOp::RemoveDegree { degree });
        for add_first in [true, false] {
            let mut state = PitchProjectionState::durable(SharedPitchSet::default());
            let events = if add_first {
                [(fact(1, 0, 1), &add), (fact(1, 1, 1), &remove)]
            } else {
                [(fact(1, 1, 1), &remove), (fact(1, 0, 1), &add)]
            };
            for (fact, event) in events {
                state.apply_event(fact, event).unwrap();
            }
            assert!(state.visible.pitch_classes.contains(&degree));
        }
    }

    #[test]
    fn pitch_projection_removes_only_observed_same_manifest_adds() {
        let degree = degree();
        let add_left = pitch_event(0, 1, [0, 0], MusicOp::AddDegree { degree });
        let add_right = pitch_event(1, 1, [0, 0], MusicOp::AddDegree { degree });
        let remove_left = pitch_event(0, 2, [1, 0], MusicOp::RemoveDegree { degree });
        let mut state = PitchProjectionState::durable(SharedPitchSet::default());
        state.apply_event(fact(1, 0, 1), &add_left).unwrap();
        state.apply_event(fact(1, 1, 1), &add_right).unwrap();
        state.apply_event(fact(1, 0, 2), &remove_left).unwrap();
        assert!(state.visible.pitch_classes.contains(&degree));

        let observed_remove = pitch_event(1, 2, [1, 1], MusicOp::RemoveDegree { degree });
        state.apply_event(fact(1, 1, 2), &observed_remove).unwrap();
        assert!(!state.visible.pitch_classes.contains(&degree));
    }

    #[test]
    fn pitch_projection_does_not_invent_cross_pair_observation() {
        let degree = degree();
        let add = pitch_event(0, 1, [0, 0], MusicOp::AddDegree { degree });
        let remove_other_pair = pitch_event(0, 2, [1, 0], MusicOp::RemoveDegree { degree });
        let mut state = PitchProjectionState::durable(SharedPitchSet::default());
        state.apply_event(fact(1, 0, 1), &add).unwrap();
        state
            .apply_event(fact(2, 0, 2), &remove_other_pair)
            .unwrap();
        assert!(state.visible.pitch_classes.contains(&degree));
    }

    fn projection(
        generation: u64,
        sequence: u64,
        durable_revision: u64,
        durable_root: [u8; 32],
        kind: RoomSessionProjectionKind,
    ) -> RoomSessionProjection {
        RoomSessionProjection {
            manifest: [7; 32],
            session_id: 19,
            epoch: 1,
            generation,
            sequence,
            durable_revision,
            durable_root,
            kind,
            view: SharedPitchSet::default(),
        }
    }

    #[test]
    fn projection_gate_requires_reset_and_contiguous_delivery() {
        let root = [3; 32];
        let mut gate = RoomSessionProjectionGate::default();
        assert_eq!(gate.canonical(5, root), Ok(true));
        assert!(
            gate.accept(&projection(
                1,
                0,
                5,
                root,
                RoomSessionProjectionKind::Predicted,
            ))
            .is_err()
        );
        assert_eq!(
            gate.accept(&projection(
                1,
                4,
                5,
                root,
                RoomSessionProjectionKind::Predicted,
            )),
            Ok(false)
        );
        assert_eq!(
            gate.accept(&projection(1, 0, 5, root, RoomSessionProjectionKind::Reset)),
            Ok(true)
        );
        assert_eq!(
            gate.accept(&projection(
                1,
                1,
                5,
                root,
                RoomSessionProjectionKind::Predicted,
            )),
            Ok(true)
        );
        assert!(
            gate.accept(&projection(
                1,
                3,
                5,
                root,
                RoomSessionProjectionKind::Predicted,
            ))
            .is_err()
        );
        assert_eq!(
            gate.accept(&projection(
                1,
                4,
                5,
                root,
                RoomSessionProjectionKind::Predicted,
            )),
            Ok(false)
        );
        assert_eq!(
            gate.accept(&projection(2, 0, 5, root, RoomSessionProjectionKind::Reset)),
            Ok(true)
        );
    }

    #[test]
    fn projection_gate_fences_restarts_stale_horizons_and_wrong_roots() {
        let root = [3; 32];
        let mut gate = RoomSessionProjectionGate::default();
        assert_eq!(gate.canonical(5, root), Ok(true));
        assert_eq!(
            gate.accept(&projection(1, 0, 5, root, RoomSessionProjectionKind::Reset,)),
            Ok(true)
        );
        assert!(
            gate.accept(&projection(
                2,
                0,
                5,
                root,
                RoomSessionProjectionKind::Predicted,
            ))
            .is_err()
        );
        assert_eq!(
            gate.accept(&projection(2, 0, 5, root, RoomSessionProjectionKind::Reset,)),
            Ok(true)
        );
        assert_eq!(
            gate.accept(&projection(
                2,
                1,
                4,
                [2; 32],
                RoomSessionProjectionKind::Predicted,
            )),
            Ok(false)
        );
        assert!(
            gate.accept(&projection(
                2,
                1,
                5,
                [9; 32],
                RoomSessionProjectionKind::Predicted,
            ))
            .is_err()
        );
        assert_eq!(
            gate.accept(&projection(
                2,
                2,
                5,
                root,
                RoomSessionProjectionKind::Predicted,
            )),
            Ok(false)
        );
        assert_eq!(
            gate.accept(&projection(3, 0, 5, root, RoomSessionProjectionKind::Reset)),
            Ok(true)
        );
        assert_eq!(gate.canonical(5, root), Ok(false));
        assert_eq!(gate.canonical(6, [4; 32]), Ok(true));
        assert_eq!(gate.canonical(5, root), Ok(false));
    }

    #[test]
    fn projection_gate_recovers_when_speculation_beats_canonical_with_wrong_root() {
        let mut gate = RoomSessionProjectionGate::default();
        let canonical_root = [1; 32];
        let speculative_root = [2; 32];
        assert_eq!(gate.canonical(5, canonical_root), Ok(true));
        assert_eq!(
            gate.accept(&projection(
                1,
                0,
                6,
                speculative_root,
                RoomSessionProjectionKind::Reset,
            )),
            Ok(true)
        );
        assert!(gate.canonical(6, canonical_root).is_err());
        assert_eq!(
            gate.accept(&projection(
                1,
                1,
                6,
                canonical_root,
                RoomSessionProjectionKind::Predicted,
            )),
            Ok(false)
        );
        assert_eq!(
            gate.accept(&projection(
                2,
                0,
                6,
                canonical_root,
                RoomSessionProjectionKind::Reset,
            )),
            Ok(true)
        );
        assert_eq!(gate.canonical(6, canonical_root), Ok(false));
    }

    #[test]
    fn projection_gate_bounds_renewal_and_rejects_superseded_session_ids() {
        let root = [6; 32];
        let mut gate = RoomSessionProjectionGate::default();
        assert_eq!(gate.canonical(9, root), Ok(true));
        let initial = projection(1, 0, 9, root, RoomSessionProjectionKind::Reset);
        assert_eq!(gate.accept(&initial), Ok(true));

        let mut renewed = projection(1, 0, 9, root, RoomSessionProjectionKind::Reset);
        renewed.session_id = 20;
        renewed.epoch = 2;
        renewed.manifest = [8; 32];
        assert_eq!(gate.accept(&renewed), Ok(true));

        let mut late_old = projection(1, 1, 9, root, RoomSessionProjectionKind::Predicted);
        late_old.epoch = 1;
        assert_eq!(gate.accept(&late_old), Ok(false));

        let mut same_epoch_other = renewed.clone();
        same_epoch_other.session_id = 21;
        same_epoch_other.manifest = [9; 32];
        assert_eq!(gate.accept(&same_epoch_other), Ok(false));
        assert!(gate.canonical(9, [7; 32]).is_err());
    }
}

fn seat_for(actor: ActorId, other: ActorId) -> u8 {
    u8::from(actor.0 > other.0)
}

fn establishment_binding(
    identity: &RoomIdentity,
    left: ActorId,
    right: ActorId,
    session_id: u64,
) -> ChannelBinding {
    let (first, second) = ordered(left, right);
    let mut bytes = Vec::with_capacity(32 + 32 + 32 + 8 + SESSION_PROTOCOL_LABEL.len());
    bytes.extend_from_slice(SESSION_PROTOCOL_LABEL);
    bytes.extend_from_slice(identity.object.as_bytes());
    bytes.extend_from_slice(&first.0);
    bytes.extend_from_slice(&second.0);
    bytes.extend_from_slice(&session_id.to_le_bytes());
    ChannelBinding::derive(&bytes)
}

fn export_context(manifest: Digest, binding: Digest) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(EXPORT_DOMAIN.len() + 64);
    bytes.extend_from_slice(EXPORT_DOMAIN);
    bytes.extend_from_slice(manifest.as_bytes());
    bytes.extend_from_slice(binding.as_bytes());
    bytes
}

fn position_bytes(position: &Position) -> Vec<[u8; 32]> {
    position.0.iter().map(|entry| *entry.as_bytes()).collect()
}

fn hash_bytes(hashes: &[EntryHash]) -> Vec<[u8; 32]> {
    hashes.iter().map(|entry| *entry.as_bytes()).collect()
}
