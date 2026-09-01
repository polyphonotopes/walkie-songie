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
use hhhs_web_browser::{WorkerApplicationChannel, WorkerEventKind, WorkerEventPort};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tutti_music::{MusicOp, SharedPitchSet, TunedDegree, TunedPeriodicPitch};
use tutti_session::{
    ChannelBinding, EphemeralSecret, Offer, PeerIdentity, PendingInitiator, ProtocolId, SessionKeys,
};

use super::{
    performance_feedback::PerformanceIntentToken,
    v5::{ActorId, RoomIdentity},
};

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
/// Begin replacing a drained session while one final causal dot remains.
///
/// This is an application availability reserve, not an HHHS semantic limit.
/// If traffic consumes the reserve before the old prediction suffix drains,
/// the hard-limit durable fallback remains authoritative.
const SESSION_RENEWAL_RESERVE: u32 = 1;
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
        session: RoomSessionEpochIdentity,
        plan: ReificationPlan,
        intent_token: Option<PerformanceIntentToken>,
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
    pub session: RoomSessionEpochIdentity,
    pub plan: ReificationPlan,
    pub command: MusicOp,
    pub intent_token: Option<PerformanceIntentToken>,
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
        intent_token: PerformanceIntentToken,
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
    FallbackDurable {
        command: MusicOp,
        intent_token: PerformanceIntentToken,
    },
    IntentRejected {
        intent_token: PerformanceIntentToken,
        reason: String,
    },
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
    pub intent_token: Option<PerformanceIntentToken>,
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
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub(crate) struct RoomSessionDurableCorrelation {
    pub manifest: [u8; 32],
    pub epoch: u32,
    pub seat: u8,
    pub counter: u32,
    pub event: [u8; 32],
}

/// Exact causal epoch which minted one or more pending durable reifications.
///
/// Peer identity alone is insufficient because a replacement epoch may become
/// active before the old epoch's already-queued durable commits return.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct RoomSessionEpochIdentity {
    peer: ActorId,
    manifest: [u8; 32],
    session_id: u64,
    epoch: u32,
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

/// Acceptance-only classifier used to retain one real signed offer for a
/// deliberate stale-delivery browser gate. Production builds have neither the
/// classifier nor the replay control surface.
#[cfg(feature = "browser-acceptance-faults")]
pub(crate) fn is_session_offer(bytes: &[u8]) -> bool {
    session_offer_target(bytes).is_some()
}

#[cfg(feature = "browser-acceptance-faults")]
pub(crate) fn session_offer_target(bytes: &[u8]) -> Option<ActorId> {
    match SessionCarrierBody::decode(bytes).ok()? {
        SessionCarrierBody::Offer { target, .. } => Some(target),
        SessionCarrierBody::Answer { .. } | SessionCarrierBody::Event { .. } => None,
    }
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
            if task.events.is_none() {
                // Rejected combined application-frame acceptance invalidates
                // the whole compact generation. Dropping this receiver makes
                // the next worker request fail closed so the host reopens from
                // canonical durable state instead of continuing a truncated
                // session epoch.
                break;
            }
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
    awaiting_reification: BTreeMap<SessionDot, (MusicOp, Option<PerformanceIntentToken>)>,
}

struct RetiredSession {
    active: ActiveSession,
    projection: RoomSessionProjection,
    outstanding: BTreeSet<RoomSessionDurableCorrelation>,
}

struct RoomSessionTask {
    events: Option<WorkerEventPort>,
    reifications: mpsc::Sender<RoomSessionReification>,
    foundations: BTreeMap<ActorId, RoomSessionFoundation>,
    pending: BTreeMap<ActorId, PendingSession>,
    active: BTreeMap<ActorId, ActiveSession>,
    retired: BTreeMap<RoomSessionEpochIdentity, RetiredSession>,
    inflight_reifications:
        BTreeMap<RoomSessionEpochIdentity, BTreeSet<RoomSessionDurableCorrelation>>,
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
            retired: BTreeMap::new(),
            inflight_reifications: BTreeMap::new(),
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
                        intent_token,
                        trace_token,
                    } => {
                        self.local_edit(
                            command,
                            intent_token,
                            trace_token,
                            worker_accepted_at_micros,
                        )
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
                session,
                plan,
                intent_token,
                entry,
                durable_admission,
                history,
                durable_view,
                durable_revision,
            } => {
                self.confirm_local(
                    peer,
                    session,
                    plan,
                    intent_token,
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
        if self.peer_has_recovery_work(peer) {
            return Err(
                "session renewal requires every retired/in-flight recovery to drain".into(),
            );
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

    /// True while a replacement epoch could be based on a durable cut which
    /// does not yet cover work accepted by an older epoch.
    ///
    /// This is deliberately peer-wide and exact-epoch-aware.  `active` being
    /// absent is not evidence of a drained session: retirement removes the
    /// live kernel before already-queued Replica admissions return.
    fn peer_has_recovery_work(&self, peer: ActorId) -> bool {
        self.retired.iter().any(|(session, retired)| {
            session.peer == peer
                && (!retired.outstanding.is_empty() || retired.active.has_local_reification_wait())
        }) || self
            .inflight_reifications
            .iter()
            .any(|(session, inflight)| session.peer == peer && !inflight.is_empty())
    }

    /// True when replacing `peer`'s epoch could abandon a local durable
    /// obligation. A remote-only compact prediction is deliberately absent:
    /// it is reversible presentation state and an authorized higher-epoch
    /// Reset may replace it after the new floor is durable.
    fn incoming_renewal_has_local_obligations(&self, peer: ActorId) -> bool {
        self.active
            .get(&peer)
            .is_some_and(ActiveSession::has_local_reification_wait)
            || self.peer_has_recovery_work(peer)
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
        // The task is the sole session mutator, so the old session (either
        // drained or containing remote-only reversible speculation) remains
        // frozen across this await. Persist the new floor before Answer/reset
        // can become observable. A crash in the following delivery/activation
        // cut is healed by the durable higher-floor counter-offer path and can
        // never reopen the old epoch.
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
                intent_token: None,
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
        let retired: Vec<_> = self
            .renewal_needed
            .iter()
            .filter(|peer| !self.active.contains_key(peer) && !self.pending.contains_key(peer))
            .filter(|peer| !self.peer_has_recovery_work(**peer))
            .filter_map(|peer| self.foundations.get(peer).cloned())
            .collect();
        for foundation in retired {
            self.begin_session(foundation).await?;
        }
        let peers: Vec<_> = self
            .active
            .iter()
            .filter_map(|(peer, active)| {
                (((active.foundation.local.0 < peer.0 && active.needs_renewal())
                    || self.renewal_needed.contains(peer))
                    && active.is_drained()
                    && !self.peer_has_recovery_work(*peer)
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
                if target != local.local {
                    return Err("session offer targets another actor".into());
                }
                let offered_epoch = SessionEpoch::new(epoch);
                if offered_epoch.get() == 0 {
                    return Err("session offer epoch zero is invalid".into());
                }
                if base != position_bytes(&local.history.frontier()) {
                    self.emit_renewal_trace(
                        RoomSessionRenewalTraceStage::StaleOfferRefused,
                        source,
                        offered_epoch,
                        None,
                    )
                    .await?;
                    return Err("session offer is bound to a stale durable base".into());
                }
                if grants != hash_bytes(&local.peer_grants) {
                    return Err(
                        "session offer foundation grants differ from admitted history".into(),
                    );
                }
                let renewal_floor = self.load_renewal_floor(&local).await?;
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
                let replaces_pending = if let Some(pending) = self.pending.get(&source) {
                    if !prefer_incoming_offer(
                        local.local,
                        source,
                        offered_epoch,
                        pending.manifest.epoch(),
                    ) {
                        return Ok(());
                    }
                    true
                } else {
                    false
                };
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
                // Authentication, the exact durable base/foundations, and
                // renewal authority have all passed. A recovered peer may now
                // legitimately replace an old epoch which contains only that
                // peer's reversible compact prediction. Never cross this cut
                // while this placement still owes a local reification:
                // awaiting, in-flight, and retired work remains a hard drain
                // barrier because it may already have escaped as a durable
                // promise. `install_session` persists the new floor before its
                // Answer/Reset is observable.
                if renewal_floor.is_some() && self.incoming_renewal_has_local_obligations(source) {
                    return Err(
                        "session renewal arrived before local durable obligations drained".into(),
                    );
                }
                if replaces_pending {
                    self.pending.remove(&source);
                }
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
                    || self.peer_has_recovery_work(source)
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
                let remote = {
                    let active = self
                        .active
                        .get_mut(&source)
                        .expect("matching active session checked above");
                    active.ingest_remote(
                        &frame,
                        receiver_now,
                        trace_clock.as_ref(),
                        self.trace_enabled,
                        worker_accepted_at_micros,
                        carrier_received_at_micros,
                    )
                };
                let remote = match remote {
                    Ok(remote) => remote,
                    Err(RemoteIngestFailure::BeforeMutation(error)) => return Err(error),
                    Err(RemoteIngestFailure::ContinuityLost(error)) => {
                        return self.fail_session_generation(format!(
                            "remote compact event failed after Fresh replay state was consumed: {error}"
                        ));
                    }
                };
                let Some(mut remote) = remote else {
                    return Ok(());
                };
                if self.presentation_peer == Some(source)
                    && let Some(kind) = remote.changed
                {
                    let projection = self
                        .active
                        .get(&source)
                        .expect("matching active session retained")
                        .projection_event(kind);
                    if let Some(trace) = &mut remote.trace {
                        trace.worker_projected_at_micros = trace_clock.now_micros();
                    }
                    self.emit_realtime_or_fail_generation(
                        source,
                        RoomSessionRealtimeEgress {
                            projection,
                            carrier: None,
                            durable: Vec::new(),
                            intent_token: None,
                            trace: remote.trace,
                        },
                    )
                    .await?;
                }
                Ok(())
            }
        }
    }

    async fn local_edit(
        &mut self,
        command: MusicOp,
        intent_token: PerformanceIntentToken,
        trace_token: Option<RoomSessionTraceToken>,
        worker_accepted_at_micros: Option<u64>,
    ) -> Result<(), String> {
        let Some(events) = self.events.as_ref() else {
            return Err("worker session event port is not configured".into());
        };
        let worker_generation = events.generation().get();
        if intent_token.generation != worker_generation {
            return self
                .reject_intent(
                    intent_token,
                    format!(
                        "stale performance intent generation {}; current worker generation is {worker_generation}",
                        intent_token.generation
                    ),
                )
                .await;
        }
        if !is_pitch_edit(&command) {
            return self
                .reject_intent(
                    intent_token,
                    "compact session only accepts shared pitch-set edits".into(),
                )
                .await;
        }
        let Some(peer) = self
            .presentation_peer
            .filter(|peer| self.active.contains_key(peer))
        else {
            return self
                .emit(RoomSessionEgress::FallbackDurable {
                    command,
                    intent_token,
                })
                .await;
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
            return self
                .emit(RoomSessionEgress::FallbackDurable {
                    command,
                    intent_token,
                })
                .await;
        }
        if let Err(error) = self
            .active
            .get(&peer)
            .expect("selected active session")
            .preflight_local_event()
        {
            return self.reject_intent(intent_token, error).await;
        }
        // Reserve the bounded worker-side reification lane before mutating the
        // session kernel. Only this task produces reifications, so the permit
        // remains ours until start_send below.
        let mut reserved_reification = self.reifications.clone();
        if poll_fn(|context| Pin::new(&mut reserved_reification).poll_ready(context))
            .await
            .is_err()
        {
            return self
                .reject_intent(intent_token, "session reification queue closed".into())
                .await;
        }
        let receiver_now = match self.lease_clock.now_ticks() {
            Ok(now) => now,
            Err(error) => return self.reject_intent(intent_token, error).await,
        };
        let trace_clock = Rc::clone(&self.trace_clock);
        let local_result = {
            let active = self.active.get_mut(&peer).expect("selected active session");
            active.local_event(
                command,
                intent_token,
                receiver_now,
                trace_clock.as_ref(),
                self.trace_enabled,
                trace_token,
                worker_accepted_at_micros,
            )
        };
        let mut local = match local_result {
            Ok(local) => local,
            Err(error) => {
                return self.fail_session_generation(format!(
                    "compact local event failed after session mutation: {error}"
                ));
            }
        };
        let (source, session_id) = {
            let active = self.active.get(&peer).expect("selected active session");
            (active.foundation.local, active.session_id)
        };
        let carrier = match (SessionCarrierBody::Event {
            source,
            target: peer,
            session_id,
            frame: local.frame.clone(),
        })
        .encode()
        {
            Ok(carrier) => carrier,
            Err(error) => {
                return self.fail_session_generation(format!(
                    "compact carrier encoding failed after session mutation: {error}"
                ));
            }
        };
        let projection = {
            let active = self.active.get(&peer).expect("selected active session");
            let projection = active.projection_event(RoomSessionProjectionKind::Predicted);
            projection
        };
        if let Some(trace) = &mut local.trace {
            trace.worker_projected_at_micros = trace_clock.now_micros();
        }
        let durable = local
            .plan
            .as_ref()
            .map(|plan| durable_correlation(plan.correlation()))
            .into_iter()
            .collect();
        if let Some(plan) = local.plan.as_ref() {
            let session = self
                .active
                .get(&peer)
                .expect("selected active session")
                .identity();
            let correlation = durable_correlation(plan.correlation());
            self.inflight_reifications
                .entry(session)
                .or_default()
                .insert(correlation);
            if Pin::new(&mut reserved_reification)
                .start_send(RoomSessionReification {
                    peer,
                    session,
                    plan: plan.clone(),
                    command: local.command.clone(),
                    intent_token: Some(intent_token),
                })
                .is_err()
            {
                if let Some(inflight) = self.inflight_reifications.get_mut(&session) {
                    inflight.remove(&correlation);
                    if inflight.is_empty() {
                        self.inflight_reifications.remove(&session);
                    }
                }
                return self.fail_session_generation(
                    "reserved session reification enqueue failed after session mutation".into(),
                );
            }
        }
        let realtime = RoomSessionRealtimeEgress {
            projection,
            carrier: Some(carrier),
            durable,
            intent_token: Some(intent_token),
            trace: local.trace,
        };
        self.emit_realtime_or_fail_generation(peer, realtime).await
    }

    async fn confirm_local(
        &mut self,
        peer: ActorId,
        session: RoomSessionEpochIdentity,
        plan: ReificationPlan,
        intent_token: Option<PerformanceIntentToken>,
        entry: Entry,
        durable_admission: DurableEntryAdmission,
        history: DagSnapshot,
        durable_view: SharedPitchSet,
        durable_revision: u64,
    ) -> Result<(), String> {
        let correlation = durable_correlation(plan.correlation());
        let matches_active = self
            .active
            .get(&peer)
            .is_some_and(|active| active.identity() == session);
        if !matches_active {
            return self
                .confirm_retired(
                    session,
                    plan,
                    intent_token,
                    entry,
                    durable_admission,
                    history,
                    durable_view,
                    durable_revision,
                )
                .await;
        }
        let (ready, kind) = {
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
            (active.retry_reifications()?, kind)
        };
        self.remove_inflight_reification(session, correlation)?;
        let projection = self.advance_companions_and_select_projection(
            peer,
            kind,
            durable_revision,
            &history,
            &durable_view,
        )?;
        self.update_foundation_horizon(&history, &durable_view, durable_revision);
        let durable = ready
            .iter()
            .map(|(plan, _, _)| durable_correlation(plan.correlation()))
            .collect();
        if let Some(projection) = projection {
            self.emit_realtime_or_fail_generation(
                self.presentation_peer
                    .expect("selected projection requires a presentation peer"),
                RoomSessionRealtimeEgress {
                    projection,
                    carrier: None,
                    durable,
                    intent_token,
                    trace: None,
                },
            )
            .await?;
        }
        self.enqueue_reifications(peer, ready).await?;
        self.start_ready_renewals().await
    }

    async fn confirm_retired(
        &mut self,
        session: RoomSessionEpochIdentity,
        plan: ReificationPlan,
        intent_token: Option<PerformanceIntentToken>,
        entry: Entry,
        durable_admission: DurableEntryAdmission,
        history: DagSnapshot,
        durable_view: SharedPitchSet,
        durable_revision: u64,
    ) -> Result<(), String> {
        let correlation = durable_correlation(plan.correlation());
        let (projection, ready, drained) = {
            let retired = self
                .retired
                .get_mut(&session)
                .ok_or("durable session confirmation has neither an active nor retired epoch")?;
            if !retired.outstanding.contains(&correlation) {
                return Err(
                    "durable session confirmation did not match an outstanding retired reification"
                        .into(),
                );
            }
            let admission = retired
                .active
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
            retired
                .active
                .confirm(admission, durable_revision, &history, durable_view.clone())?;
            let ready = retired.active.retry_reifications()?;
            retired.outstanding.remove(&correlation);
            for (ready_plan, _, _) in &ready {
                retired
                    .outstanding
                    .insert(durable_correlation(ready_plan.correlation()));
            }
            retired.projection.sequence = retired
                .projection
                .sequence
                .checked_add(1)
                .ok_or("retired session projection sequence exhausted")?;
            retired.projection.durable_revision = durable_revision;
            retired.projection.durable_root = *history_root(&history).as_bytes();
            retired.projection.kind = RoomSessionProjectionKind::Confirmed;
            retired.projection.view = durable_view.clone();
            (
                retired.projection.clone(),
                ready,
                retired.outstanding.is_empty() && !retired.active.has_local_reification_wait(),
            )
        };
        self.remove_inflight_reification(session, correlation)?;
        self.enqueue_reifications_for(session.peer, session, ready)
            .await?;
        if drained {
            self.retired.remove(&session);
        }
        self.emit(RoomSessionEgress::Realtime(RoomSessionRealtimeEgress {
            projection,
            carrier: None,
            durable: Vec::new(),
            intent_token,
            trace: None,
        }))
        .await?;
        self.advance_all(history, durable_view, durable_revision)
            .await
    }

    fn remove_inflight_reification(
        &mut self,
        session: RoomSessionEpochIdentity,
        correlation: RoomSessionDurableCorrelation,
    ) -> Result<(), String> {
        let remove_session = {
            let inflight = self
                .inflight_reifications
                .get_mut(&session)
                .ok_or("durable confirmation has no matching in-flight session epoch")?;
            if !inflight.remove(&correlation) {
                return Err("durable confirmation has no matching in-flight correlation".into());
            }
            inflight.is_empty()
        };
        if remove_session {
            self.inflight_reifications.remove(&session);
        }
        Ok(())
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
            // A retained command from another/superseded compact session is
            // still ordinary canonical music. Advance every active projection
            // through the common path so the selected presentation receives
            // the corresponding ordered egress. Mutating it silently here
            // would consume a projection sequence and make the next visible
            // event appear to skip.
            return self
                .advance_all(history, durable_view, durable_revision)
                .await;
        };
        let (ready, kind) = {
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
            (active.retry_reifications()?, kind)
        };
        let projection = self.advance_companions_and_select_projection(
            peer,
            kind,
            durable_revision,
            &history,
            &durable_view,
        )?;
        self.update_foundation_horizon(&history, &durable_view, durable_revision);
        let durable = ready
            .iter()
            .map(|(plan, _, _)| durable_correlation(plan.correlation()))
            .collect();
        if let Some(projection) = projection {
            self.emit_realtime_or_fail_generation(
                self.presentation_peer
                    .expect("selected projection requires a presentation peer"),
                RoomSessionRealtimeEgress {
                    projection,
                    carrier: None,
                    durable,
                    intent_token: None,
                    trace: None,
                },
            )
            .await?;
        }
        self.enqueue_reifications(peer, ready).await?;
        self.start_ready_renewals().await
    }

    /// Advance every pair-session over one canonical revision while publishing
    /// exactly the projection selected as the room's presentation authority.
    ///
    /// A projection sequence belongs to its pair-session. Constructing an
    /// event for a non-selected pair and then discarding it creates an
    /// observable sequence gap if that pair is ever presented; discarding the
    /// selected companion's `advance_durable` transition hides canonical
    /// growth immediately. This helper makes both cases structurally
    /// impossible for local and observed confirmations.
    fn advance_companions_and_select_projection(
        &mut self,
        source_peer: ActorId,
        source_kind: RoomSessionProjectionKind,
        durable_revision: u64,
        history: &DagSnapshot,
        durable_view: &SharedPitchSet,
    ) -> Result<Option<RoomSessionProjection>, String> {
        let selected = self.presentation_peer;
        let mut projection = if selected == Some(source_peer) {
            Some(
                self.active
                    .get(&source_peer)
                    .ok_or("durable session confirmation has no active peer")?
                    .projection_event(source_kind),
            )
        } else {
            None
        };

        for (peer, active) in &mut self.active {
            if *peer == source_peer {
                continue;
            }
            if let Some(kind) =
                active.advance_durable(durable_revision, history, durable_view.clone())?
                && selected == Some(*peer)
            {
                projection = Some(active.projection_event(kind));
            }
        }
        Ok(projection)
    }

    async fn enqueue_reifications(
        &mut self,
        peer: ActorId,
        reifications: impl IntoIterator<
            Item = (ReificationPlan, MusicOp, Option<PerformanceIntentToken>),
        >,
    ) -> Result<(), String> {
        let session = self
            .active
            .get(&peer)
            .ok_or("session reification has no active peer")?
            .identity();
        self.enqueue_reifications_for(peer, session, reifications)
            .await
    }

    async fn enqueue_reifications_for(
        &mut self,
        peer: ActorId,
        session: RoomSessionEpochIdentity,
        reifications: impl IntoIterator<
            Item = (ReificationPlan, MusicOp, Option<PerformanceIntentToken>),
        >,
    ) -> Result<(), String> {
        for (plan, command, intent_token) in reifications {
            let correlation = durable_correlation(plan.correlation());
            self.inflight_reifications
                .entry(session)
                .or_default()
                .insert(correlation);
            if self
                .reifications
                .send(RoomSessionReification {
                    peer,
                    session,
                    plan,
                    command,
                    intent_token,
                })
                .await
                .is_err()
            {
                let remove_session =
                    self.inflight_reifications
                        .get_mut(&session)
                        .is_some_and(|inflight| {
                            inflight.remove(&correlation);
                            inflight.is_empty()
                        });
                if remove_session {
                    self.inflight_reifications.remove(&session);
                }
                return Err("session reification queue closed".to_owned());
            }
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
                intent_token: None,
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
            intent_token: None,
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

    async fn emit_realtime_or_fail_generation(
        &mut self,
        _peer: ActorId,
        event: RoomSessionRealtimeEgress,
    ) -> Result<(), String> {
        let first = self.emit(RoomSessionEgress::Realtime(event)).await;
        let Err(first_error) = first else {
            return Ok(());
        };
        self.fail_session_generation(format!(
            "combined session carrier/projection frame was rejected before window acceptance: {first_error}"
        ))
    }

    async fn reject_intent(
        &self,
        intent_token: PerformanceIntentToken,
        reason: String,
    ) -> Result<(), String> {
        self.emit(RoomSessionEgress::IntentRejected {
            intent_token,
            reason,
        })
        .await
    }

    fn fail_session_generation(&mut self, reason: String) -> Result<(), String> {
        let terminal = format!(
            "{reason}; compact generation terminated; reopen canonical state, repair, and establish a fresh epoch"
        );
        // This is deliberately the ordinary worker event lane, not the
        // rejected application-frame lane.  The window must be able to fail
        // the whole placement even when the bounded realtime queue is the
        // thing which refused delivery.
        if let Some(events) = self.events.as_ref() {
            let _ = events.emit(WorkerEventKind::Error, terminal.as_bytes().to_vec());
        }
        self.events = None;
        self.pending.clear();
        self.active.clear();
        self.retired.clear();
        self.inflight_reifications.clear();
        self.presentation_peer = None;
        self.renewal_needed.clear();
        Err(terminal)
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

enum RemoteIngestFailure {
    BeforeMutation(String),
    ContinuityLost(String),
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
    fn identity(&self) -> RoomSessionEpochIdentity {
        RoomSessionEpochIdentity {
            peer: self.foundation.peer,
            manifest: *self.session.manifest_digest().as_bytes(),
            session_id: self.session_id,
            epoch: self.session.manifest().epoch().get(),
        }
    }

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

    fn has_local_reification_wait(&self) -> bool {
        !self.awaiting_reification.is_empty()
    }

    fn is_saturated(&self) -> bool {
        self.sender.remaining_causal_events() == 0 || self.kernel.event_budget_exhausted()
    }

    fn preflight_local_event(&self) -> Result<(), String> {
        if self.is_saturated() {
            return Err("session epoch is saturated; use durable fallback until renewal".into());
        }
        if self.awaiting_reification.len() >= SESSION_CAPACITY {
            return Err(format!(
                "session has {} local events awaiting durable causal dependencies",
                SESSION_CAPACITY
            ));
        }
        Ok(())
    }

    fn needs_renewal(&self) -> bool {
        self.sender.remaining_causal_events() <= SESSION_RENEWAL_RESERVE
            || [self.local_seat, self.remote_seat].into_iter().any(|seat| {
                self.kernel
                    .remaining_event_budget(seat)
                    .is_some_and(|remaining| remaining <= SESSION_RENEWAL_RESERVE)
            })
    }

    fn local_event(
        &mut self,
        command: MusicOp,
        intent_token: PerformanceIntentToken,
        receiver_clock_ticks: u64,
        trace_clock: &dyn RoomSessionTraceClock,
        trace_enabled: bool,
        trace_token: Option<RoomSessionTraceToken>,
        worker_accepted_at_micros: Option<u64>,
    ) -> Result<LocalSessionEvent, String> {
        self.preflight_local_event()?;
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
        let correlation = compact_correlation(permitted.authenticated());
        let trace = trace_enabled.then(|| RoomSessionCompactTrace {
            token: trace_token,
            correlation,
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
                self.awaiting_reification
                    .insert(dot, (command.clone(), Some(intent_token)));
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

    fn retry_reifications(
        &mut self,
    ) -> Result<Vec<(ReificationPlan, MusicOp, Option<PerformanceIntentToken>)>, String> {
        let mut ready = Vec::new();
        let dots: Vec<_> = self.awaiting_reification.keys().copied().collect();
        for dot in dots {
            match self.planner.plan(&self.kernel, dot) {
                Ok(plan) => {
                    let (command, intent_token) = self
                        .awaiting_reification
                        .remove(&dot)
                        .expect("dot came from the bounded wait map");
                    ready.push((plan, command, intent_token));
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
    ) -> Result<Option<RemoteSessionEvent>, RemoteIngestFailure> {
        let packet = self
            .receiver_codec
            .decode(&self.receiver_binding, frame)
            .map_err(|error| RemoteIngestFailure::BeforeMutation(error.to_string()))?;
        let at = packet.header().effective_at().ticks();
        let received = self
            .receiver
            .receive(&packet)
            .map_err(|error| RemoteIngestFailure::BeforeMutation(error.to_string()))?;
        if received.disposition() != ReplayDisposition::Fresh {
            return Ok(None);
        }
        let worker_authenticated_at_micros =
            trace_enabled.then(|| trace_clock.now_micros()).flatten();
        let receiver_now = self
            .receiver_lease_time(receiver_clock_ticks)
            .map_err(RemoteIngestFailure::ContinuityLost)?;
        let event = received
            .try_decode(decode_pitch_edit)
            .map_err(|error| RemoteIngestFailure::ContinuityLost(error.to_owned()))?;
        let worker_interpreted_at_micros =
            trace_enabled.then(|| trace_clock.now_micros()).flatten();
        let permitted = self
            .session
            .permit_event(receiver_now, event, frame.len())
            .map_err(|error| RemoteIngestFailure::ContinuityLost(error.to_string()))?;
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
        let changed = self
            .ingest(permitted)
            .map_err(RemoteIngestFailure::ContinuityLost)?;
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
        let transition = self
            .projection
            .resynchronize(
                ProjectionGeneration::new(self.projection.generation().get().saturating_add(1)),
                &self.kernel,
                SimulationTime::from_ticks(self.logical_time),
                DurableProjectionHorizon::new(durable, history, MAX_SESSION_MESSAGE_BYTES as usize),
                &PitchProjector,
            )
            .map_err(|error| error.to_string())?;
        if !matches!(transition.change(), SessionProjectionChange::Reset { .. }) {
            return Err("session projection resynchronization did not produce a reset".into());
        }
        self.durable_revision = revision;
        Ok(())
    }

    fn reset_projection(&mut self) -> Result<RoomSessionProjection, String> {
        let durable = self.projection.durable().clone();
        let history = self.foundation.history.clone();
        self.resynchronize(self.durable_revision, &history, durable)?;
        Ok(self.projection_event(RoomSessionProjectionKind::Reset))
    }

    /// Produce a canonical-horizon reset for an epoch that can no longer be
    /// used safely. The caller removes this `ActiveSession` immediately after
    /// creating the snapshot; no transient kernel state crosses the boundary.
    fn retirement_projection(&self) -> Result<RoomSessionProjection, String> {
        let current = self.projection.snapshot();
        let generation = current
            .generation()
            .get()
            .checked_add(1)
            .ok_or("session projection generation exhausted during retirement")?;
        Ok(RoomSessionProjection {
            manifest: *self.session.manifest_digest().as_bytes(),
            session_id: self.session_id,
            epoch: self.session.manifest().epoch().get(),
            generation,
            sequence: 0,
            durable_revision: self.foundation.durable_revision,
            durable_root: *history_root(&self.foundation.history).as_bytes(),
            kind: RoomSessionProjectionKind::Reset,
            view: self.foundation.durable_view.clone(),
        })
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
    use hhhs_store::MemoryStorage;
    use hhhs_web_browser::{WorkerEvent, WorkerGeneration};

    use super::*;
    use crate::room::v5::RoomReplicas;

    fn intent(sequence: u64) -> PerformanceIntentToken {
        PerformanceIntentToken {
            generation: 1,
            sequence,
        }
    }

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

    fn renewal_room_foundations() -> (
        RoomReplicas<MemoryStorage, MemoryStorage>,
        RoomSessionFoundation,
        RoomSessionFoundation,
    ) {
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
        (room, owner_foundation, member_foundation)
    }

    fn renewal_foundations() -> (RoomSessionFoundation, RoomSessionFoundation) {
        let (_, owner, member) = renewal_room_foundations();
        (owner, member)
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

    fn signed_offer(foundation: &RoomSessionFoundation, session_id: u64, epoch: u32) -> Vec<u8> {
        let manifest = build_manifest(foundation, session_id, SessionEpoch::new(epoch)).unwrap();
        let binding = establishment_binding(
            &foundation.identity,
            foundation.local,
            foundation.peer,
            session_id,
        );
        let (_, offer) = PendingInitiator::begin(
            &foundation.signing_key,
            ProtocolId::derive(SESSION_PROTOCOL_LABEL),
            binding,
            session_id,
            EphemeralSecret::from_bytes([73; 32]),
        );
        let (_, presentation) = local_foundation(foundation, &manifest).unwrap();
        SessionCarrierBody::Offer {
            source: foundation.local,
            target: foundation.peer,
            session_id,
            epoch,
            base: position_bytes(manifest.base()),
            grants: hash_bytes(&foundation.local_grants),
            handshake: offer.as_bytes().to_vec(),
            foundation: presentation,
        }
        .encode()
        .unwrap()
    }

    fn task_state_signature(
        task: &RoomSessionTask,
    ) -> (
        usize,
        usize,
        usize,
        usize,
        Option<ActorId>,
        BTreeSet<ActorId>,
        usize,
    ) {
        (
            task.pending.len(),
            task.active.len(),
            task.retired.len(),
            task.inflight_reifications.len(),
            task.presentation_peer,
            task.renewal_needed.clone(),
            task.renewal_floors.len(),
        )
    }

    fn degree() -> TunedDegree {
        TunedDegree::new(&tutti_music::Tuning::twelve_tet(), 7).unwrap()
    }

    #[test]
    fn stale_base_offer_emits_one_typed_refusal_without_session_mutation() {
        let (owner, mut member) = renewal_foundations();
        let offer = signed_offer(&owner, 0x5101, 1);
        let mut entries = member.history.entries_topo();
        entries.push(Entry::new(
            b"durable edit after the retained offer".to_vec(),
            member.history.frontier(),
        ));
        member.history = DagSnapshot::from_entries(entries);

        let delivered = Rc::new(RefCell::new(Vec::<RoomSessionEgress>::new()));
        let captured = Rc::clone(&delivered);
        let events = WorkerEventPort::new(WorkerGeneration::new(1), move |event| {
            captured.borrow_mut().push(decode(event.payload())?);
            Ok(())
        });
        let (reifications, _) = mpsc::channel(1);
        let mut task = RoomSessionTask::new(
            reifications,
            Rc::new(TestRenewalStore::default()),
            test_lease_clock(),
        );
        task.events = Some(events);
        task.trace_enabled = true;
        task.foundations.insert(owner.local, member);
        let before = task_state_signature(&task);

        let error = block_on(task.carrier(offer, None, None)).unwrap_err();
        assert_eq!(error, "session offer is bound to a stale durable base");
        assert_eq!(task_state_signature(&task), before);
        assert_eq!(delivered.borrow().len(), 1);
        assert!(matches!(
            &delivered.borrow()[0],
            RoomSessionEgress::RenewalTrace(RoomSessionRenewalTrace {
                stage: RoomSessionRenewalTraceStage::StaleOfferRefused,
                peer,
                epoch: 1,
                floor_epoch: None,
            }) if *peer == owner.local.0
        ));
    }

    #[test]
    fn wrong_target_offer_emits_no_stale_trace_and_does_not_mutate_session() {
        let (owner, member) = renewal_foundations();
        let offer = SessionCarrierBody::decode(&signed_offer(&owner, 0x5102, 1)).unwrap();
        let SessionCarrierBody::Offer {
            source,
            session_id,
            epoch,
            base,
            grants,
            handshake,
            foundation,
            ..
        } = offer
        else {
            panic!("fixture did not encode an Offer");
        };
        let wrong_target = SessionCarrierBody::Offer {
            source,
            target: source,
            session_id,
            epoch,
            base,
            grants,
            handshake,
            foundation,
        }
        .encode()
        .unwrap();

        let delivered = Rc::new(RefCell::new(Vec::<RoomSessionEgress>::new()));
        let captured = Rc::clone(&delivered);
        let events = WorkerEventPort::new(WorkerGeneration::new(1), move |event| {
            captured.borrow_mut().push(decode(event.payload())?);
            Ok(())
        });
        let (reifications, _) = mpsc::channel(1);
        let mut task = RoomSessionTask::new(
            reifications,
            Rc::new(TestRenewalStore::default()),
            test_lease_clock(),
        );
        task.events = Some(events);
        task.trace_enabled = true;
        task.foundations.insert(owner.local, member);
        let before = task_state_signature(&task);

        let error = block_on(task.carrier(wrong_target, None, None)).unwrap_err();
        assert_eq!(error, "session offer targets another actor");
        assert_eq!(task_state_signature(&task), before);
        assert!(delivered.borrow().is_empty());
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
    fn session_requests_renewal_with_one_causal_dot_reserved() {
        let (owner, member) = renewal_foundations();
        let session_id = 50;
        let (authorized, _) = authorize_fixture(&owner, &member, session_id, 1, None).unwrap();
        let keys = initiator_keys(&owner, &member, session_id);
        let mut active = ActiveSession::new(owner, authorized, session_id, keys, 0).unwrap();
        for _ in 0..MAX_EVENTS_PER_SEAT.saturating_sub(SESSION_RENEWAL_RESERVE) {
            active
                .local_event(
                    MusicOp::AddDegree { degree: degree() },
                    intent(1),
                    0,
                    &DisabledRoomSessionTraceClock,
                    false,
                    None,
                    None,
                )
                .unwrap();
        }

        assert_eq!(
            active.sender.remaining_causal_events(),
            SESSION_RENEWAL_RESERVE
        );
        assert_eq!(
            active.kernel.remaining_event_budget(active.local_seat),
            Some(SESSION_RENEWAL_RESERVE)
        );
        assert!(active.needs_renewal());
        assert!(!active.is_saturated());
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
                    intent(1),
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
                    intent(65),
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
    fn retired_epoch_validates_and_drains_every_exact_durable_admission() {
        let (room, owner, member) = renewal_room_foundations();
        let session_id = 57;
        let (owner_authorized, _) =
            authorize_fixture(&owner, &member, session_id, 1, None).unwrap();
        let (member_authorized, _) =
            authorize_fixture(&member, &owner, session_id, 1, None).unwrap();
        let (owner_keys, member_keys) = session_key_pair(&owner, &member, session_id);
        let mut active =
            ActiveSession::new(owner.clone(), owner_authorized, session_id, owner_keys, 0).unwrap();
        let first = active
            .local_event(
                MusicOp::AddDegree { degree: degree() },
                intent(1),
                0,
                &DisabledRoomSessionTraceClock,
                false,
                None,
                None,
            )
            .unwrap();
        let first_plan = first.plan.clone().expect("first local dot is ready");
        let second = active
            .local_event(
                MusicOp::RemoveDegree { degree: degree() },
                intent(2),
                0,
                &DisabledRoomSessionTraceClock,
                false,
                None,
                None,
            )
            .unwrap();
        assert!(
            second.plan.is_none(),
            "second dot waits for the first admission"
        );

        // Add an unrelated remote-only compact prediction to the epoch before
        // retirement. It is not a local durable obligation and must not keep
        // the retired epoch alive after both exact local admissions drain.
        let remote_degree = TunedDegree::new(&tutti_music::Tuning::twelve_tet(), 9).unwrap();
        let mut remote_sender = ActiveSession::new(
            member.clone(),
            member_authorized,
            session_id,
            member_keys,
            0,
        )
        .unwrap();
        let remote = remote_sender
            .local_event(
                MusicOp::AddDegree {
                    degree: remote_degree,
                },
                intent(3),
                0,
                &DisabledRoomSessionTraceClock,
                false,
                None,
                None,
            )
            .unwrap();
        assert!(matches!(
            active.ingest_remote(
                &remote.frame,
                0,
                &DisabledRoomSessionTraceClock,
                false,
                None,
                None,
            ),
            Ok(Some(_))
        ));
        assert!(active.projection.pending_len() > 0);

        // Build a different but fully admitted session record. Its durable
        // receipt must not discharge the retired epoch's exact correlation.
        let wrong_session_id = session_id + 1;
        let (wrong_authorized, _) =
            authorize_fixture(&owner, &member, wrong_session_id, 1, None).unwrap();
        let wrong_keys = initiator_keys(&owner, &member, wrong_session_id);
        let mut wrong_active = ActiveSession::new(
            owner.clone(),
            wrong_authorized,
            wrong_session_id,
            wrong_keys,
            0,
        )
        .unwrap();
        let wrong = wrong_active
            .local_event(
                MusicOp::RemoveDegree { degree: degree() },
                intent(9),
                0,
                &DisabledRoomSessionTraceClock,
                false,
                None,
                None,
            )
            .unwrap();
        let wrong_plan = wrong.plan.expect("independent first dot is ready");
        let (wrong_entry, wrong_admission) = room
            .admit_reified_music_for_test(
                &owner.signing_key,
                &wrong_plan,
                MusicOp::RemoveDegree { degree: degree() },
            )
            .unwrap();

        let session = active.identity();
        let first_correlation = durable_correlation(first_plan.correlation());
        let delivered = Rc::new(RefCell::new(Vec::<RoomSessionEgress>::new()));
        let captured = Rc::clone(&delivered);
        let events = WorkerEventPort::new(WorkerGeneration::new(1), move |event| {
            captured.borrow_mut().push(decode(event.payload())?);
            Ok(())
        });
        let (reifications, mut queued) = mpsc::channel(2);
        let store = Rc::new(TestRenewalStore::default());
        let mut task = RoomSessionTask::new(reifications, store, test_lease_clock());
        task.events = Some(events);
        task.foundations.insert(owner.peer, owner.clone());
        task.retired.insert(
            session,
            RetiredSession {
                projection: active.retirement_projection().unwrap(),
                active,
                outstanding: BTreeSet::from([first_correlation]),
            },
        );
        task.inflight_reifications
            .insert(session, BTreeSet::from([first_correlation]));

        let wrong_snapshot = room.music_snapshot();
        let wrong_view = room.view().music.shared_pitches;
        let error = block_on(task.confirm_retired(
            session,
            wrong_plan,
            None,
            wrong_entry,
            wrong_admission,
            wrong_snapshot.history,
            wrong_view,
            wrong_snapshot.sequence,
        ))
        .unwrap_err();
        assert!(error.contains("did not match an outstanding"));
        assert!(
            task.retired[&session]
                .outstanding
                .contains(&first_correlation)
        );

        let (first_entry, first_admission) = room
            .admit_reified_music_for_test(&owner.signing_key, &first_plan, first.command)
            .unwrap();
        let first_snapshot = room.music_snapshot();
        let first_view = room.view().music.shared_pitches;
        block_on(task.confirm_retired(
            session,
            first_plan,
            Some(intent(1)),
            first_entry,
            first_admission,
            first_snapshot.history,
            first_view,
            first_snapshot.sequence,
        ))
        .unwrap();
        let ready = block_on(queued.next()).expect("second exact reification was released");
        let second_correlation = durable_correlation(ready.plan.correlation());
        assert_eq!(
            task.retired[&session].outstanding,
            BTreeSet::from([second_correlation])
        );
        assert!(
            task.retired[&session].active.projection.pending_len() > 0,
            "unrelated remote prediction remains reversible while local work drains"
        );

        let (second_entry, second_admission) = room
            .admit_reified_music_for_test(&owner.signing_key, &ready.plan, ready.command)
            .unwrap();
        let second_snapshot = room.music_snapshot();
        let second_view = room.view().music.shared_pitches;
        block_on(task.confirm_retired(
            session,
            ready.plan,
            ready.intent_token,
            second_entry,
            second_admission,
            second_snapshot.history,
            second_view,
            second_snapshot.sequence,
        ))
        .unwrap();
        assert!(!task.retired.contains_key(&session));
        assert!(!task.inflight_reifications.contains_key(&session));
        assert!(!task.peer_has_recovery_work(owner.peer));
        let sequences: Vec<_> = delivered
            .borrow()
            .iter()
            .filter_map(|event| match event {
                RoomSessionEgress::Realtime(event) => Some(event.projection.sequence),
                _ => None,
            })
            .collect();
        assert_eq!(sequences, vec![1, 2]);
    }

    #[test]
    fn renewal_waits_for_active_awaiting_reification_then_resumes() {
        let (owner, member) = renewal_foundations();
        let peer = owner.peer;
        let session_id = 61;
        let (authorized, floor) = authorize_fixture(&owner, &member, session_id, 1, None).unwrap();
        let keys = initiator_keys(&owner, &member, session_id);
        let mut active =
            ActiveSession::new(owner.clone(), authorized, session_id, keys, 0).unwrap();
        active.awaiting_reification.insert(
            SessionDot::new(SessionEpoch::new(1), active.local_seat, 1),
            (MusicOp::AddDegree { degree: degree() }, Some(intent(1))),
        );
        let events = WorkerEventPort::new(WorkerGeneration::new(1), |_| Ok(()));
        let (reifications, _) = mpsc::channel(2);
        let mut task = RoomSessionTask::new(
            reifications,
            Rc::new(TestRenewalStore::default()),
            test_lease_clock(),
        );
        task.events = Some(events);
        task.foundations.insert(peer, owner.clone());
        task.renewal_floors.insert(peer, Some(floor));
        task.active.insert(peer, active);
        task.renewal_needed.insert(peer);

        block_on(task.start_ready_renewals()).unwrap();
        assert!(!task.pending.contains_key(&peer));

        task.active
            .get_mut(&peer)
            .unwrap()
            .awaiting_reification
            .clear();
        block_on(task.start_ready_renewals()).unwrap();
        assert!(task.pending.contains_key(&peer));
    }

    #[test]
    fn renewal_waits_for_retired_inflight_epoch_then_resumes() {
        let (owner, member) = renewal_foundations();
        let peer = owner.peer;
        let session_id = 62;
        let (authorized, floor) = authorize_fixture(&owner, &member, session_id, 1, None).unwrap();
        let keys = initiator_keys(&owner, &member, session_id);
        let active = ActiveSession::new(owner.clone(), authorized, session_id, keys, 0).unwrap();
        let session = active.identity();
        let correlation = RoomSessionDurableCorrelation {
            manifest: session.manifest,
            epoch: session.epoch,
            seat: active.local_seat,
            counter: 1,
            event: [0x62; 32],
        };
        let projection = active.retirement_projection().unwrap();
        let events = WorkerEventPort::new(WorkerGeneration::new(1), |_| Ok(()));
        let (reifications, _) = mpsc::channel(2);
        let mut task = RoomSessionTask::new(
            reifications,
            Rc::new(TestRenewalStore::default()),
            test_lease_clock(),
        );
        task.events = Some(events);
        task.foundations.insert(peer, owner);
        task.renewal_floors.insert(peer, Some(floor));
        task.retired.insert(
            session,
            RetiredSession {
                active,
                projection,
                outstanding: BTreeSet::from([correlation]),
            },
        );
        task.inflight_reifications
            .insert(session, BTreeSet::from([correlation]));
        task.renewal_needed.insert(peer);

        block_on(task.start_ready_renewals()).unwrap();
        assert!(!task.pending.contains_key(&peer));

        task.retired.get_mut(&session).unwrap().outstanding.clear();
        task.inflight_reifications.remove(&session);
        block_on(task.start_ready_renewals()).unwrap();
        assert!(task.pending.contains_key(&peer));
    }

    #[test]
    fn higher_epoch_replaces_remote_only_prediction_without_abandoning_local_work() {
        let (room, owner, member) = renewal_room_foundations();
        let old_session_id = 63;
        let (owner_authorized, _) =
            authorize_fixture(&owner, &member, old_session_id, 1, None).unwrap();
        let (member_authorized, member_floor) =
            authorize_fixture(&member, &owner, old_session_id, 1, None).unwrap();
        let (owner_keys, member_keys) = session_key_pair(&owner, &member, old_session_id);
        let mut sender = ActiveSession::new(
            owner.clone(),
            owner_authorized,
            old_session_id,
            owner_keys,
            0,
        )
        .unwrap();
        let mut receiver = ActiveSession::new(
            member.clone(),
            member_authorized,
            old_session_id,
            member_keys,
            0,
        )
        .unwrap();
        let old = sender
            .local_event(
                MusicOp::AddDegree { degree: degree() },
                intent(1),
                0,
                &DisabledRoomSessionTraceClock,
                false,
                None,
                None,
            )
            .unwrap();
        let old_plan = old.plan.clone().expect("first old-epoch dot is ready");
        let remote = match receiver.ingest_remote(
            &old.frame,
            0,
            &DisabledRoomSessionTraceClock,
            false,
            None,
            None,
        ) {
            Ok(Some(remote)) => remote,
            Ok(None) => panic!("old compact event was unexpectedly classified as replay"),
            Err(_) => panic!("old compact event was unexpectedly refused"),
        };
        assert!(remote.changed.is_some());
        assert!(!receiver.is_drained());
        assert!(receiver.awaiting_reification.is_empty());
        assert!(
            receiver
                .projection_event(RoomSessionProjectionKind::Predicted)
                .view
                .pitch_classes
                .contains(&degree())
        );

        let peer = member.peer;
        let (reifications, _) = mpsc::channel(2);
        let mut task = RoomSessionTask::new(
            reifications,
            Rc::new(TestRenewalStore::default()),
            test_lease_clock(),
        );
        task.active.insert(peer, receiver);
        assert!(
            !task.incoming_renewal_has_local_obligations(peer),
            "remote-only speculative state must remain replaceable"
        );

        // The same undrained projection becomes a hard barrier as soon as this
        // placement owes a local causal reification.
        let local_dot = SessionDot::new(SessionEpoch::new(1), 1, 9);
        task.active
            .get_mut(&peer)
            .unwrap()
            .awaiting_reification
            .insert(
                local_dot,
                (MusicOp::RemoveDegree { degree: degree() }, Some(intent(2))),
            );
        assert!(task.incoming_renewal_has_local_obligations(peer));
        task.active
            .get_mut(&peer)
            .unwrap()
            .awaiting_reification
            .remove(&local_dot);
        assert!(!task.incoming_renewal_has_local_obligations(peer));

        // Model the fully authorized/persisted replacement boundary. The new
        // Reset removes only reversible presentation state, and the old
        // epoch's compact carrier cannot authenticate under the new binding.
        let new_session_id = 64;
        let (renewed, _) =
            authorize_fixture(&member, &owner, new_session_id, 2, Some(&member_floor)).unwrap();
        let (_, renewed_keys) = session_key_pair(&owner, &member, new_session_id);
        let mut replacement =
            ActiveSession::new(member.clone(), renewed, new_session_id, renewed_keys, 0).unwrap();
        let reset = replacement.projection_event(RoomSessionProjectionKind::Reset);
        assert!(!reset.view.pitch_classes.contains(&degree()));
        assert!(matches!(
            replacement.ingest_remote(
                &old.frame,
                0,
                &DisabledRoomSessionTraceClock,
                false,
                None,
                None,
            ),
            Err(RemoteIngestFailure::BeforeMutation(_))
        ));

        // Reset is not rejection. If the old event later becomes an ordinary
        // durable Replica admission, exact-history advancement corrects the
        // replacement epoch to the canonical value.
        let (_entry, _durable_admission) = room
            .admit_reified_music_for_test(
                &owner.signing_key,
                &old_plan,
                MusicOp::AddDegree { degree: degree() },
            )
            .unwrap();
        let snapshot = room.music_snapshot();
        let durable_view = room.view().music.shared_pitches;
        assert!(
            replacement
                .advance_durable(snapshot.sequence, &snapshot.history, durable_view.clone())
                .unwrap()
                .is_some()
        );
        assert!(durable_view.pitch_classes.contains(&degree()));
        assert!(
            replacement
                .projection_event(RoomSessionProjectionKind::Advanced)
                .view
                .pitch_classes
                .contains(&degree())
        );
    }

    #[test]
    fn rejected_combined_frame_terminates_generation_without_durable_reauthor() {
        let (owner, member) = renewal_foundations();
        let session_id = 58;
        let (authorized, _) = authorize_fixture(&owner, &member, session_id, 1, None).unwrap();
        let keys = initiator_keys(&owner, &member, session_id);
        let active = ActiveSession::new(owner.clone(), authorized, session_id, keys, 0).unwrap();
        let peer = active.foundation.peer;
        let projection = active.projection_event(RoomSessionProjectionKind::Predicted);
        let events = WorkerEventPort::new(WorkerGeneration::new(1), |_| {
            Err("injected bounded application-frame refusal".into())
        });
        let (reifications, _) = mpsc::channel(2);
        let mut task = RoomSessionTask::new(
            reifications,
            Rc::new(TestRenewalStore::default()),
            test_lease_clock(),
        );
        task.events = Some(events);
        task.presentation_peer = Some(peer);
        task.active.insert(peer, active);
        let token = intent(9);
        let error = block_on(task.emit_realtime_or_fail_generation(
            peer,
            RoomSessionRealtimeEgress {
                projection,
                carrier: Some(vec![1, 2, 3]),
                durable: Vec::new(),
                intent_token: Some(token),
                trace: None,
            },
        ))
        .unwrap_err();
        assert!(error.contains("generation terminated"));
        assert!(task.events.is_none());
        assert!(task.active.is_empty());
        assert!(task.pending.is_empty());
        assert!(task.retired.is_empty());
        assert!(task.inflight_reifications.is_empty());
    }

    #[test]
    fn accepted_combined_frame_remains_reversible_across_precommit_crash() {
        let (owner, member) = renewal_foundations();
        let session_id = 59;
        let (authorized, _) = authorize_fixture(&owner, &member, session_id, 1, None).unwrap();
        let keys = initiator_keys(&owner, &member, session_id);
        let active = ActiveSession::new(owner, authorized, session_id, keys, 0).unwrap();
        let peer = active.foundation.peer;
        let projection = active.projection_event(RoomSessionProjectionKind::Predicted);
        let delivered = Rc::new(RefCell::new(Vec::<RoomSessionEgress>::new()));
        let captured = Rc::clone(&delivered);
        let events = WorkerEventPort::new(WorkerGeneration::new(1), move |event| {
            captured.borrow_mut().push(decode(event.payload())?);
            Ok(())
        });
        let (reifications, _) = mpsc::channel(2);
        let mut task = RoomSessionTask::new(
            reifications,
            Rc::new(TestRenewalStore::default()),
            test_lease_clock(),
        );
        task.events = Some(events);
        task.presentation_peer = Some(peer);
        task.active.insert(peer, active);
        block_on(task.emit_realtime_or_fail_generation(
            peer,
            RoomSessionRealtimeEgress {
                projection,
                carrier: Some(vec![4, 5, 6]),
                durable: Vec::new(),
                intent_token: Some(intent(10)),
                trace: None,
            },
        ))
        .unwrap();
        assert!(task.active.contains_key(&peer));
        assert_eq!(delivered.borrow().len(), 1);
        assert!(
            !delivered
                .borrow()
                .iter()
                .any(|event| matches!(event, RoomSessionEgress::FallbackDurable { .. }))
        );

        let error = task
            .fail_session_generation("injected crash before durable admission".into())
            .unwrap_err();
        assert!(error.contains("reopen canonical state"));
        assert!(task.active.is_empty());
        assert!(
            !delivered
                .borrow()
                .iter()
                .any(|event| matches!(event, RoomSessionEgress::FallbackDurable { .. }))
        );
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
                intent(1),
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
                intent(1),
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
                intent(1),
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
        let RemoteIngestFailure::ContinuityLost(error) = error else {
            panic!("expired receiver lease was classified as pre-mutation");
        };
        assert!(error.contains("OutsideLease"));
        assert_eq!(receiver.kernel.ready_cut(), kernel_before);
        assert!(receiver.projection.snapshot() == projection_before);
        assert_eq!(receiver.logical_time, logical_time_before);
    }

    #[test]
    fn unrelated_durable_growth_is_emitted_before_the_next_session_projection() {
        let (owner, member) = renewal_foundations();
        let session_id = 53;
        let (authorized, _) = authorize_fixture(&owner, &member, session_id, 1, None).unwrap();
        let keys = initiator_keys(&owner, &member, session_id);
        let active = ActiveSession::new(owner.clone(), authorized, session_id, keys, 0).unwrap();

        let initial_root = *history_root(&owner.history).as_bytes();
        let mut gate = RoomSessionProjectionGate::default();
        assert_eq!(
            gate.canonical(owner.durable_revision, initial_root),
            Ok(true)
        );
        assert_eq!(
            gate.accept(&active.projection_event(RoomSessionProjectionKind::Reset)),
            Ok(true)
        );

        let delivered = Rc::new(RefCell::new(Vec::<WorkerEvent>::new()));
        let captured = Rc::clone(&delivered);
        let events = WorkerEventPort::new(WorkerGeneration::new(1), move |event| {
            captured.borrow_mut().push(event);
            Ok(())
        });
        let (reifications, _) = mpsc::channel(1);
        let mut task = RoomSessionTask::new(
            reifications,
            Rc::new(TestRenewalStore::default()),
            test_lease_clock(),
        );
        task.events = Some(events);
        task.presentation_peer = Some(member.local);
        task.active.insert(member.local, active);

        let mut entries = owner.history.entries_topo();
        entries.push(Entry::new(
            b"unrelated retained session command".to_vec(),
            owner.history.frontier(),
        ));
        let advanced_history = DagSnapshot::from_entries(entries);
        let advanced_revision = owner.durable_revision.saturating_add(1);
        let mut advanced_view = owner.durable_view.clone();
        advanced_view.pitch_classes.insert(degree());
        block_on(task.advance_all(advanced_history.clone(), advanced_view, advanced_revision))
            .unwrap();

        let first = delivered.borrow_mut().remove(0);
        let RoomSessionEgress::Realtime(first) = decode(first.payload()).unwrap() else {
            panic!("durable advance did not emit a realtime projection");
        };
        assert_eq!(first.projection.kind, RoomSessionProjectionKind::Advanced);
        assert_eq!(first.projection.sequence, 1);
        assert_eq!(
            gate.canonical(
                advanced_revision,
                *history_root(&advanced_history).as_bytes()
            ),
            Ok(true)
        );
        assert_eq!(gate.accept(&first.projection), Ok(true));

        let active = task.active.get_mut(&member.local).unwrap();
        active
            .local_event(
                MusicOp::RemoveDegree { degree: degree() },
                intent(1),
                0,
                &DisabledRoomSessionTraceClock,
                false,
                None,
                None,
            )
            .unwrap();
        let next = active.projection_event(RoomSessionProjectionKind::Predicted);
        assert_eq!(next.sequence, 2);
        assert_eq!(gate.accept(&next), Ok(true));
    }

    #[test]
    fn confirmation_advances_and_returns_the_selected_companion_projection() {
        let (owner, member) = renewal_foundations();
        let source_peer = member.local;
        let selected_peer = ActorId([99; 32]);

        let (source_authorized, _) = authorize_fixture(&owner, &member, 54, 1, None).unwrap();
        let source = ActiveSession::new(
            owner.clone(),
            source_authorized,
            54,
            initiator_keys(&owner, &member, 54),
            0,
        )
        .unwrap();
        let (selected_authorized, _) = authorize_fixture(&owner, &member, 55, 1, None).unwrap();
        let selected = ActiveSession::new(
            owner.clone(),
            selected_authorized,
            55,
            initiator_keys(&owner, &member, 55),
            0,
        )
        .unwrap();

        let (reifications, _) = mpsc::channel(1);
        let mut task = RoomSessionTask::new(
            reifications,
            Rc::new(TestRenewalStore::default()),
            test_lease_clock(),
        );
        task.active.insert(source_peer, source);
        task.active.insert(selected_peer, selected);
        task.presentation_peer = Some(selected_peer);

        let mut entries = owner.history.entries_topo();
        entries.push(Entry::new(
            b"canonical confirmation from another pair".to_vec(),
            owner.history.frontier(),
        ));
        let advanced_history = DagSnapshot::from_entries(entries);
        let advanced_revision = owner.durable_revision.saturating_add(1);
        let mut advanced_view = owner.durable_view.clone();
        advanced_view.pitch_classes.insert(degree());

        // Model the source pair having already consumed its matching durable
        // confirmation before the task advances every companion pair.
        assert!(
            task.active
                .get_mut(&source_peer)
                .unwrap()
                .advance_durable(advanced_revision, &advanced_history, advanced_view.clone(),)
                .unwrap()
                .is_some()
        );
        let projection = task
            .advance_companions_and_select_projection(
                source_peer,
                RoomSessionProjectionKind::Confirmed,
                advanced_revision,
                &advanced_history,
                &advanced_view,
            )
            .unwrap()
            .expect("selected companion advance must be published");

        assert_eq!(projection.session_id, 55);
        assert_eq!(projection.kind, RoomSessionProjectionKind::Advanced);
        assert_eq!(projection.sequence, 1);
        assert_eq!(projection.durable_revision, advanced_revision);
        assert_eq!(projection.view, advanced_view);
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
