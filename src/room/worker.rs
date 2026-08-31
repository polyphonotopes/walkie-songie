//! Typed application service placed behind `hhhs-web-browser`.
//!
//! This module owns no DOM, audio, MIDI, endpoint, discovery, or carrier
//! handle. One service owns both durable Room-v5 lanes and advances repair one
//! already-framed message at a time. The same service runs through
//! `InProcessWorkerHost` in native tests and `serve_dedicated_worker` in a
//! browser Worker.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
};

use futures::SinkExt;
use hhhs::{Digest, EntryHash};
use hhhs_replica::{
    AsyncTransactionSink, DurableReplicaHost, ReplicaRecord, ReplicaRepairSnapshot,
};
use hhhs_store::{MemoryStorage, StorageTransaction, history_root};
use hhhs_sync::{
    CachedRepairHost, RepairAttemptStatus, RepairDisposition, RepairRetryReason, SessionOutcome,
    StepwiseRepairAttempt,
};
use hhhs_web_browser::{
    ProjectionRevision, ReplicaWorkerService, SubscriptionId, WorkerEventKind, WorkerEventPort,
    WorkerReply, WorkerRequest, WorkerRequestKind,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    net::{PeerId, ReplicaLiveRecord, repair_lane, replica_frontier_digest},
    room::v5::{
        ActorId, MusicOp, RoomCommand, RoomIdentity, RoomLane, RoomPresence, RoomReplicas, RoomView,
    },
    tuning::TunedPeriodicPitch,
};

use super::session::{RoomSessionFoundation, RoomSessionServicePort, RoomSessionTaskInput};

const ROOM_WORKER_PAYLOAD_VERSION: u16 = 2;
const MAX_ACTIVE_REPAIR_SESSIONS: usize = 8;

type DurableLane<D> =
    CachedRepairHost<DurableReplicaHost<MemoryStorage, super::v5::RoomAdmissionPolicy, D>>;
pub(crate) type RoomWorkerOpenFuture<'a, D> =
    Pin<Box<dyn Future<Output = Result<RoomDataPlane<D>, String>> + 'a>>;

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
struct Versioned<T> {
    version: u16,
    body: T,
}

fn encode<T: Serialize>(body: &T) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    ciborium::into_writer(
        &Versioned {
            version: ROOM_WORKER_PAYLOAD_VERSION,
            body,
        },
        &mut bytes,
    )
    .map_err(|error| format!("Room worker payload encoding failed: {error}"))?;
    Ok(bytes)
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    let payload: Versioned<T> = ciborium::from_reader(bytes)
        .map_err(|error| format!("Room worker payload decoding failed: {error}"))?;
    if payload.version != ROOM_WORKER_PAYLOAD_VERSION {
        return Err(format!(
            "unsupported Room worker payload version {}; expected {}",
            payload.version, ROOM_WORKER_PAYLOAD_VERSION
        ));
    }
    Ok(payload.body)
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub(crate) struct RoomWorkerOpen {
    pub object: [u8; 32],
    pub owner: ActorId,
    pub identity_seed: [u8; 32],
    pub authority_seed: Option<[u8; 32]>,
    pub session_trace: bool,
    #[cfg(feature = "browser-acceptance-faults")]
    pub session_renewal_test_cut: bool,
}

impl RoomWorkerOpen {
    pub(crate) fn new(
        identity: &RoomIdentity,
        owner: ActorId,
        identity_seed: [u8; 32],
        authority_seed: Option<[u8; 32]>,
    ) -> Self {
        Self {
            object: *identity.object.as_bytes(),
            owner,
            identity_seed,
            authority_seed,
            session_trace: false,
            #[cfg(feature = "browser-acceptance-faults")]
            session_renewal_test_cut: false,
        }
    }

    pub(crate) const fn with_session_trace(mut self, enabled: bool) -> Self {
        self.session_trace = enabled;
        self
    }

    #[cfg(feature = "browser-acceptance-faults")]
    pub(crate) const fn with_session_renewal_test_cut(mut self, enabled: bool) -> Self {
        self.session_renewal_test_cut = enabled;
        self
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub(crate) struct RoomWorkerProjection {
    pub view: RoomView,
    pub music_revision: u64,
    pub music_history_root: [u8; 32],
    pub music_frontier: [u8; 32],
    pub extension_frontier: [u8; 32],
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub(crate) enum RoomWorkerCommand {
    Commit(RoomCommand),
    GrantPeer(ActorId),
    StartSessionPeer(ActorId),
    ResetSessionProjection,
    DrainSession,
    SignPresence {
        session: u64,
        sequence: u64,
        pitch: Option<TunedPeriodicPitch>,
    },
    VerifyPresence(Vec<u8>),
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub(crate) enum RoomWorkerRepairRequest {
    StartInitiator {
        session: u64,
        lane: RoomLane,
    },
    StartResponder {
        session: u64,
        lane: RoomLane,
        hello: Vec<u8>,
    },
    Frame {
        session: u64,
        frame: Vec<u8>,
    },
    Finish {
        session: u64,
        close_error: Option<String>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub(crate) enum RoomWorkerRepairStatus {
    Exchanging,
    Complete,
    Divergent,
    Incomplete,
    PolicyDivergence,
    RetryFresh(RoomWorkerRepairRetryReason),
}

impl From<RepairAttemptStatus> for RoomWorkerRepairStatus {
    fn from(value: RepairAttemptStatus) -> Self {
        match value {
            RepairAttemptStatus::Exchanging => Self::Exchanging,
            RepairAttemptStatus::Complete => Self::Complete,
            RepairAttemptStatus::Divergent => Self::Divergent,
            RepairAttemptStatus::Incomplete => Self::Incomplete,
            RepairAttemptStatus::PolicyDivergence => Self::PolicyDivergence,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub(crate) enum RoomWorkerRepairRetryReason {
    NoCapturedCut,
    Incomplete,
    RootMismatch,
    HistoryAdvanced,
}

impl From<RepairRetryReason> for RoomWorkerRepairRetryReason {
    fn from(value: RepairRetryReason) -> Self {
        match value {
            RepairRetryReason::NoCapturedCut => Self::NoCapturedCut,
            RepairRetryReason::Incomplete => Self::Incomplete,
            RepairRetryReason::RootMismatch => Self::RootMismatch,
            RepairRetryReason::HistoryAdvanced => Self::HistoryAdvanced,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub(crate) enum RoomWorkerRepairDisposition {
    Synchronized,
    RetryFresh(RoomWorkerRepairRetryReason),
    AwaitPolicyChange,
}

impl From<RepairDisposition> for RoomWorkerRepairDisposition {
    fn from(value: RepairDisposition) -> Self {
        match value {
            RepairDisposition::Synchronized => Self::Synchronized,
            RepairDisposition::RetryFresh(reason) => Self::RetryFresh(reason.into()),
            RepairDisposition::AwaitPolicyChange => Self::AwaitPolicyChange,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub(crate) struct RoomWorkerRepairOutcome {
    pub admitted: usize,
    pub lifted: usize,
    pub frames_sent: usize,
    pub frames_received: usize,
    pub refused: usize,
    pub policy_divergence: bool,
}

impl From<&SessionOutcome> for RoomWorkerRepairOutcome {
    fn from(value: &SessionOutcome) -> Self {
        Self {
            admitted: value.admitted,
            lifted: value.lifted,
            frames_sent: value.frames_sent,
            frames_received: value.frames_received,
            refused: value.refused,
            policy_divergence: value.policy_divergence,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub(crate) struct RoomWorkerRepairStep {
    pub session: u64,
    pub status: RoomWorkerRepairStatus,
    pub outcome: RoomWorkerRepairOutcome,
    pub disposition: Option<RoomWorkerRepairDisposition>,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub(crate) enum RoomWorkerResponse {
    Opened {
        actor: ActorId,
        projection: RoomWorkerProjection,
    },
    CommandCommitted {
        entry: [u8; 32],
        projection_revision: u64,
    },
    PeerGranted {
        entries: Vec<(RoomLane, [u8; 32])>,
        projection_revision: u64,
    },
    SessionPeerStarted,
    SessionProjectionReset {
        emitted: bool,
    },
    SessionDrained {
        committed: bool,
        projection_revision: u64,
    },
    InboundApplied {
        accepted: bool,
        entry: [u8; 32],
        projection_revision: u64,
    },
    PresenceSigned(Vec<u8>),
    PresenceVerified(RoomPresenceWire),
    Repair(RoomWorkerRepairStep),
    Frontiers(RoomWorkerProjection),
    Closed,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub(crate) struct RoomPresenceWire {
    pub actor: ActorId,
    pub session: u64,
    pub sequence: u64,
    pub pitch: Option<TunedPeriodicPitch>,
}

impl From<RoomPresence> for RoomPresenceWire {
    fn from(value: RoomPresence) -> Self {
        Self {
            actor: value.actor,
            session: value.session,
            sequence: value.sequence,
            pitch: value.pitch,
        }
    }
}

pub(crate) fn encode_open(value: &RoomWorkerOpen) -> Result<Vec<u8>, String> {
    encode(value)
}

pub(crate) fn encode_command(value: &RoomWorkerCommand) -> Result<Vec<u8>, String> {
    encode(value)
}

pub(crate) fn encode_repair(value: &RoomWorkerRepairRequest) -> Result<Vec<u8>, String> {
    encode(value)
}

pub(crate) fn decode_response(bytes: &[u8]) -> Result<RoomWorkerResponse, String> {
    decode(bytes)
}

pub(crate) fn decode_projection(bytes: &[u8]) -> Result<RoomWorkerProjection, String> {
    decode(bytes)
}

struct RepairState {
    lane: RoomLane,
    attempt: StepwiseRepairAttempt<ReplicaRepairSnapshot>,
}

pub(crate) struct RoomDataPlane<D>
where
    D: AsyncTransactionSink,
{
    room: RoomReplicas<MemoryStorage, MemoryStorage>,
    music: DurableLane<D>,
    extension: DurableLane<D>,
    signing_key: hhhs_proof::SigningKey,
    grant_authority: Option<hhhs_proof::SigningKey>,
    local_actor: ActorId,
    repair: BTreeMap<u64, RepairState>,
}

impl<D> RoomDataPlane<D>
where
    D: AsyncTransactionSink,
{
    pub(crate) async fn open(
        request: RoomWorkerOpen,
        music_log: D,
        extension_log: D,
        music_transactions: Vec<StorageTransaction>,
        extension_transactions: Vec<StorageTransaction>,
    ) -> Result<Self, String> {
        let identity = RoomIdentity::from_object(Digest(request.object));
        let room = RoomReplicas::from_transaction_logs(
            identity,
            request.owner,
            music_transactions,
            extension_transactions,
        )
        .map_err(|error| error.to_string())?;
        let signing_key = hhhs_proof::SigningKey::from_bytes(&request.identity_seed);
        let local_actor = ActorId::from_signing_key(&signing_key);
        let mut music = CachedRepairHost::new(room.music_durable_host(music_log));
        let mut extension = CachedRepairHost::new(room.extension_durable_host(extension_log));

        let grant_authority = request
            .authority_seed
            .map(|seed| hhhs_proof::SigningKey::from_bytes(&seed));
        if let Some(authority) = grant_authority.as_ref() {
            if ActorId::from_signing_key(authority) != request.owner {
                return Err("Room worker authority does not match room owner".into());
            }
            if room
                .capabilities_for_lane(local_actor, RoomLane::Music)
                .is_empty()
            {
                let prepared = room
                    .prepare_member_grant(RoomLane::Music, authority, local_actor)
                    .map_err(|error| error.to_string())?;
                music
                    .inner_mut()
                    .commit_prepared(prepared.into_prepared())
                    .await
                    .map_err(|error| error.to_string())?;
            }
            if room
                .capabilities_for_lane(local_actor, RoomLane::Extension)
                .is_empty()
            {
                let prepared = room
                    .prepare_member_grant(RoomLane::Extension, authority, local_actor)
                    .map_err(|error| error.to_string())?;
                extension
                    .inner_mut()
                    .commit_prepared(prepared.into_prepared())
                    .await
                    .map_err(|error| error.to_string())?;
            }
        }

        Ok(Self {
            room,
            music,
            extension,
            signing_key,
            grant_authority,
            local_actor,
            repair: BTreeMap::new(),
        })
    }

    fn lane(&self, lane: RoomLane) -> &DurableLane<D> {
        match lane {
            RoomLane::Music => &self.music,
            RoomLane::Extension => &self.extension,
        }
    }

    fn lane_mut(&mut self, lane: RoomLane) -> &mut DurableLane<D> {
        match lane {
            RoomLane::Music => &mut self.music,
            RoomLane::Extension => &mut self.extension,
        }
    }

    fn projection(&self) -> RoomWorkerProjection {
        let (view, music, extension) = self.room.view_with_frontiers();
        let music_snapshot = self.room.music_snapshot();
        RoomWorkerProjection {
            view,
            music_revision: music_snapshot.sequence,
            music_history_root: *history_root(&music_snapshot.history).as_bytes(),
            music_frontier: *replica_frontier_digest(&music).as_bytes(),
            extension_frontier: *replica_frontier_digest(&extension).as_bytes(),
        }
    }

    async fn commit(&mut self, command: RoomCommand) -> Result<(EntryHash, ReplicaRecord), String> {
        let lane = command.lane();
        let capabilities = self.room.capabilities_for_lane(self.local_actor, lane);
        if capabilities.is_empty() {
            return Err("local actor has no live capability for this Room-v5 lane".into());
        }
        let prepared = self
            .room
            .prepare_author_presenting(&self.signing_key, &capabilities, command)
            .map_err(|error| error.to_string())?;
        let committed = self
            .lane_mut(lane)
            .inner_mut()
            .commit_prepared(prepared.into_prepared())
            .await
            .map_err(|error| error.to_string())?;
        Ok((
            committed.outcome().entry,
            committed.replica_record().clone(),
        ))
    }

    async fn commit_session(
        &mut self,
        plan: &hhhs_session::ReificationPlan,
        command: MusicOp,
    ) -> Result<
        (
            hhhs::Entry,
            hhhs_replica::DurableEntryAdmission,
            ReplicaRecord,
        ),
        String,
    > {
        let capabilities = self
            .room
            .capabilities_for_lane(self.local_actor, RoomLane::Music);
        if capabilities.is_empty() {
            return Err("local actor has no live capability for the music session".into());
        }
        let (prepared, entry) = self
            .room
            .prepare_reified_music(&self.signing_key, &capabilities, plan, command)
            .map_err(|error| error.to_string())?;
        let committed = self
            .music
            .inner_mut()
            .commit_prepared(prepared.into_prepared())
            .await
            .map_err(|error| error.to_string())?;
        if committed.outcome().entry != entry.hash() {
            return Err("session reification committed an unexpected entry identity".into());
        }
        Ok((
            entry,
            committed.outcome().durable_entry_admission(),
            committed.replica_record().clone(),
        ))
    }

    fn session_foundation(&self, peer: ActorId) -> Result<RoomSessionFoundation, String> {
        let local_grants = self
            .room
            .capabilities_for_lane(self.local_actor, RoomLane::Music);
        let peer_grants = self.room.capabilities_for_lane(peer, RoomLane::Music);
        if local_grants.is_empty() || peer_grants.is_empty() {
            return Err(
                "session peer does not yet have a complete admitted music foundation".into(),
            );
        }
        let snapshot = self.room.music_snapshot();
        let root = self
            .room
            .owner_capabilities()
            .music
            .into_iter()
            .next()
            .ok_or("music capability root is absent")?;
        Ok(RoomSessionFoundation {
            identity: self.room.identity().clone(),
            local: self.local_actor,
            peer,
            signing_key: self.signing_key.clone(),
            durable_revision: snapshot.sequence,
            history: snapshot.history,
            music_root: root,
            local_grants,
            peer_grants,
            durable_view: self.room.view().music.shared_pitches,
        })
    }

    async fn grant_peer(
        &mut self,
        peer: ActorId,
    ) -> Result<Vec<(RoomLane, EntryHash, ReplicaRecord)>, String> {
        if peer == self.local_actor {
            return Ok(Vec::new());
        }
        let authority = if self.room.owner() == self.local_actor {
            self.signing_key.clone()
        } else {
            self.grant_authority
                .as_ref()
                .filter(|key| ActorId::from_signing_key(key) == self.room.owner())
                .cloned()
                .ok_or("local worker does not hold this room's grant authority")?
        };
        let mut granted = Vec::new();
        for lane in [RoomLane::Music, RoomLane::Extension] {
            if !self.room.capabilities_for_lane(peer, lane).is_empty() {
                continue;
            }
            let prepared = self
                .room
                .prepare_member_grant(lane, &authority, peer)
                .map_err(|error| error.to_string())?;
            let committed = self
                .lane_mut(lane)
                .inner_mut()
                .commit_prepared(prepared.into_prepared())
                .await
                .map_err(|error| error.to_string())?;
            granted.push((
                lane,
                committed.outcome().entry,
                committed.replica_record().clone(),
            ));
        }
        Ok(granted)
    }

    async fn apply_live(&mut self, live: ReplicaLiveRecord) -> Result<(bool, EntryHash), String> {
        let lane = live.lane;
        let entry = live.record.entry_hash();
        let bytes = live.record.encode();
        let report = hhhs_sync::RepairHost::apply(self.lane_mut(lane), &[(entry, bytes)])
            .await
            .map_err(|error| error.to_string())?;
        Ok((
            report.refused.is_empty() && report.admitted.contains(&entry),
            entry,
        ))
    }

    fn durable_entry_admission(
        &self,
        lane: RoomLane,
        entry: EntryHash,
    ) -> Result<hhhs_replica::DurableEntryAdmission, String> {
        self.lane(lane)
            .inner()
            .replica()
            .durable_entry_admission(entry)
            .ok_or_else(|| format!("durable {lane:?} entry {entry:?} is not retained"))
    }

    fn sign_presence(
        &self,
        session: u64,
        sequence: u64,
        pitch: Option<TunedPeriodicPitch>,
    ) -> Result<Vec<u8>, String> {
        let capabilities = self.room.capabilities_for(self.local_actor);
        self.room
            .sign_presence(&self.signing_key, &capabilities, session, sequence, pitch)
            .map_err(|error| error.to_string())
    }

    fn verify_presence(&self, bytes: &[u8]) -> Result<RoomPresenceWire, String> {
        self.room
            .verify_presence(bytes)
            .map(RoomPresenceWire::from)
            .map_err(|error| error.to_string())
    }

    async fn start_initiator(
        &mut self,
        id: u64,
        lane: RoomLane,
        events: &WorkerEventPort,
    ) -> Result<RoomWorkerRepairStep, String> {
        self.ensure_repair_slot(id)?;
        let sink = events.clone();
        let mut outbound = move |frame| {
            let sink = sink.clone();
            async move {
                sink.send_repair_frame(frame)
                    .await
                    .map_err(|error| error.to_string())
            }
        };
        let attempt = StepwiseRepairAttempt::initiate(
            self.lane(lane),
            &repair_lane(lane),
            hhhs_sync::SessionLimits::default(),
            &mut outbound,
        )
        .await
        .map_err(|error| error.to_string())?;
        let step = repair_step(id, &attempt, None);
        self.repair.insert(id, RepairState { lane, attempt });
        Ok(step)
    }

    async fn start_responder(
        &mut self,
        id: u64,
        lane: RoomLane,
        hello: Vec<u8>,
        events: &WorkerEventPort,
    ) -> Result<RoomWorkerRepairStep, String> {
        self.ensure_repair_slot(id)?;
        let sink = events.clone();
        let mut outbound = move |frame| {
            let sink = sink.clone();
            async move {
                sink.send_repair_frame(frame)
                    .await
                    .map_err(|error| error.to_string())
            }
        };
        let attempt = StepwiseRepairAttempt::accept(
            self.lane(lane),
            &repair_lane(lane),
            hhhs_sync::SessionLimits::default(),
            &hello,
            &mut outbound,
        )
        .await
        .map_err(|error| error.to_string())?;
        let step = repair_step(id, &attempt, None);
        self.repair.insert(id, RepairState { lane, attempt });
        Ok(step)
    }

    async fn advance_repair(
        &mut self,
        id: u64,
        frame: Vec<u8>,
        events: &WorkerEventPort,
    ) -> Result<RoomWorkerRepairStep, String> {
        let mut state = self
            .repair
            .remove(&id)
            .ok_or_else(|| format!("repair session {id} is not active"))?;
        let sink = events.clone();
        let mut outbound = move |frame| {
            let sink = sink.clone();
            async move {
                sink.send_repair_frame(frame)
                    .await
                    .map_err(|error| error.to_string())
            }
        };
        let result = state
            .attempt
            .receive(self.lane_mut(state.lane), &frame, &mut outbound)
            .await
            .map_err(|error| error.to_string());
        let step = repair_step(id, &state.attempt, None);
        self.repair.insert(id, state);
        result?;
        Ok(step)
    }

    fn finish_repair(
        &mut self,
        id: u64,
        close_error: Option<String>,
    ) -> Result<RoomWorkerRepairStep, String> {
        let mut state = self
            .repair
            .remove(&id)
            .ok_or_else(|| format!("repair session {id} is not active"))?;
        if !state.attempt.is_terminal() {
            state.attempt.mark_incomplete();
        }
        let closed = match close_error {
            Some(error) => Err(error),
            None => Ok(()),
        };
        let confirmed = state
            .attempt
            .confirm_close_with_host(self.lane(state.lane), closed)
            .map_err(|error| error.to_string())?;
        let disposition = confirmed.disposition();
        let status = match disposition {
            RepairDisposition::Synchronized => RoomWorkerRepairStatus::Complete,
            RepairDisposition::RetryFresh(reason) => {
                RoomWorkerRepairStatus::RetryFresh(reason.into())
            }
            RepairDisposition::AwaitPolicyChange => RoomWorkerRepairStatus::PolicyDivergence,
        };
        Ok(RoomWorkerRepairStep {
            session: id,
            status,
            outcome: confirmed.outcome().into(),
            disposition: Some(disposition.into()),
        })
    }

    fn repair_lane_for(&self, id: u64) -> Option<RoomLane> {
        self.repair.get(&id).map(|state| state.lane)
    }

    fn ensure_repair_slot(&self, id: u64) -> Result<(), String> {
        if self.repair.contains_key(&id) {
            return Err(format!("repair session {id} already exists"));
        }
        if self.repair.len() >= MAX_ACTIVE_REPAIR_SESSIONS {
            return Err(format!(
                "Room worker already has {} active repair sessions",
                self.repair.len()
            ));
        }
        Ok(())
    }
}

fn repair_step(
    id: u64,
    attempt: &StepwiseRepairAttempt<ReplicaRepairSnapshot>,
    disposition: Option<RoomWorkerRepairDisposition>,
) -> RoomWorkerRepairStep {
    RoomWorkerRepairStep {
        session: id,
        status: attempt.status().into(),
        outcome: attempt.outcome().into(),
        disposition,
    }
}

fn repair_revision_advanced(before: Option<u64>, after: u64) -> bool {
    before.is_some_and(|before| after > before)
}

pub(crate) trait RoomWorkerFactory: 'static {
    type Durability: AsyncTransactionSink + 'static;

    fn open<'a>(
        &'a mut self,
        request: RoomWorkerOpen,
    ) -> RoomWorkerOpenFuture<'a, Self::Durability>;
}

pub(crate) struct RoomReplicaWorkerService<F>
where
    F: RoomWorkerFactory,
{
    factory: F,
    room: Option<RoomDataPlane<F::Durability>>,
    projection: Option<RoomWorkerProjection>,
    revision: u64,
    subscriptions: BTreeSet<SubscriptionId>,
    session: Option<RoomSessionServicePort>,
    session_peers: BTreeSet<ActorId>,
}

impl<F> RoomReplicaWorkerService<F>
where
    F: RoomWorkerFactory,
{
    pub(crate) fn new(factory: F) -> Self {
        Self {
            factory,
            room: None,
            projection: None,
            revision: 0,
            subscriptions: BTreeSet::new(),
            session: None,
            session_peers: BTreeSet::new(),
        }
    }

    pub(crate) fn with_session(factory: F, session: RoomSessionServicePort) -> Self {
        let mut service = Self::new(factory);
        service.session = Some(session);
        service
    }

    fn room(&self) -> Result<&RoomDataPlane<F::Durability>, String> {
        self.room.as_ref().ok_or("Room worker is not open".into())
    }

    fn room_mut(&mut self) -> Result<&mut RoomDataPlane<F::Durability>, String> {
        self.room.as_mut().ok_or("Room worker is not open".into())
    }

    fn current_projection(&self) -> Result<&RoomWorkerProjection, String> {
        self.projection
            .as_ref()
            .ok_or("Room worker has no projection".into())
    }

    fn current_session_foundations(&self) -> Vec<RoomSessionFoundation> {
        let Some(room) = self.room.as_ref() else {
            return Vec::new();
        };
        self.session_peers
            .iter()
            .filter_map(|peer| room.session_foundation(*peer).ok())
            .collect()
    }

    async fn refresh_session_foundations(&mut self) -> Result<(), String> {
        if self.session.is_none() {
            return Ok(());
        }
        let foundations = self.current_session_foundations();
        self.session
            .as_mut()
            .expect("session runtime checked above")
            .task
            .send(RoomSessionTaskInput::RefreshFoundations(foundations))
            .await
            .map_err(|_| "Room worker session task closed during foundation refresh".to_owned())
    }

    fn publish_projection(&mut self, events: &WorkerEventPort) -> Result<u64, String> {
        let next = self.room()?.projection();
        if self.projection.as_ref() == Some(&next) {
            return Ok(self.revision);
        }
        let previous = ProjectionRevision::new(self.revision);
        self.revision = self.revision.saturating_add(1);
        let revision = ProjectionRevision::new(self.revision);
        self.projection = Some(next.clone());
        for subscription in self.subscriptions.iter().copied() {
            let payload = encode(&next)?;
            events
                .emit_revision_or_reset(subscription, previous, revision, payload, || encode(&next))
                .map_err(|error| error.to_string())?;
        }
        Ok(self.revision)
    }

    async fn handle_command(
        &mut self,
        command: RoomWorkerCommand,
        events: &WorkerEventPort,
    ) -> Result<RoomWorkerResponse, String> {
        match command {
            RoomWorkerCommand::Commit(command) => {
                let local = self.room()?.local_actor;
                let lane = command.lane();
                let (entry, record) = self.room_mut()?.commit(command).await?;
                events
                    .emit(
                        WorkerEventKind::OutboundRecord,
                        ReplicaLiveRecord {
                            lane,
                            source: PeerId(local.0),
                            record,
                        }
                        .encode(),
                    )
                    .map_err(|error| error.to_string())?;
                let durable_advance = (lane == RoomLane::Music && self.session.is_some())
                    .then(|| {
                        let snapshot = self.room()?.room.music_snapshot();
                        let durable_revision = snapshot.sequence;
                        let durable_view = self.room()?.room.view().music.shared_pitches;
                        Ok::<_, String>((snapshot.history, durable_view, durable_revision))
                    })
                    .transpose()?;
                if let (Some((history, durable_view, durable_revision)), Some(session)) =
                    (durable_advance, self.session.as_mut())
                {
                    session
                        .task
                        .send(RoomSessionTaskInput::DurableAdvanced {
                            history,
                            durable_view,
                            durable_revision,
                        })
                        .await
                        .map_err(|_| {
                            "Room worker session task closed after music commit".to_owned()
                        })?;
                }
                if lane == RoomLane::Music {
                    self.refresh_session_foundations().await?;
                }
                let projection_revision = self.publish_projection(events)?;
                Ok(RoomWorkerResponse::CommandCommitted {
                    entry: *entry.as_bytes(),
                    projection_revision,
                })
            }
            RoomWorkerCommand::GrantPeer(peer) => {
                let local = self.room()?.local_actor;
                let grants = self.room_mut()?.grant_peer(peer).await?;
                let mut entries = Vec::with_capacity(grants.len());
                for (lane, entry, record) in grants {
                    events
                        .emit(
                            WorkerEventKind::OutboundRecord,
                            ReplicaLiveRecord {
                                lane,
                                source: PeerId(local.0),
                                record,
                            }
                            .encode(),
                        )
                        .map_err(|error| error.to_string())?;
                    entries.push((lane, *entry.as_bytes()));
                }
                let durable_advance = (entries.iter().any(|(lane, _)| *lane == RoomLane::Music)
                    && self.session.is_some())
                .then(|| {
                    let snapshot = self.room()?.room.music_snapshot();
                    let durable_revision = snapshot.sequence;
                    let durable_view = self.room()?.room.view().music.shared_pitches;
                    Ok::<_, String>((snapshot.history, durable_view, durable_revision))
                })
                .transpose()?;
                if let (Some((history, durable_view, durable_revision)), Some(session)) =
                    (durable_advance, self.session.as_mut())
                {
                    session
                        .task
                        .send(RoomSessionTaskInput::DurableAdvanced {
                            history,
                            durable_view,
                            durable_revision,
                        })
                        .await
                        .map_err(|_| {
                            "Room worker session task closed after music grant".to_owned()
                        })?;
                }
                if entries.iter().any(|(lane, _)| *lane == RoomLane::Music) {
                    self.refresh_session_foundations().await?;
                }
                let projection_revision = self.publish_projection(events)?;
                Ok(RoomWorkerResponse::PeerGranted {
                    entries,
                    projection_revision,
                })
            }
            RoomWorkerCommand::StartSessionPeer(peer) => {
                let foundation = self.room()?.session_foundation(peer)?;
                self.session
                    .as_mut()
                    .ok_or("Room worker session runtime is not configured")?
                    .task
                    .send(RoomSessionTaskInput::StartPeer(foundation))
                    .await
                    .map_err(|_| "Room worker session task closed".to_owned())?;
                self.session_peers.insert(peer);
                Ok(RoomWorkerResponse::SessionPeerStarted)
            }
            RoomWorkerCommand::ResetSessionProjection => {
                let snapshot = self.room()?.room.music_snapshot();
                let durable_revision = snapshot.sequence;
                let durable_view = self.room()?.room.view().music.shared_pitches;
                let (reply, response) = futures::channel::oneshot::channel();
                self.session
                    .as_mut()
                    .ok_or("Room worker session runtime is not configured")?
                    .task
                    .send(RoomSessionTaskInput::ResetProjection {
                        history: snapshot.history,
                        durable_view,
                        durable_revision,
                        reply,
                    })
                    .await
                    .map_err(|_| "Room worker session task closed during reset".to_owned())?;
                let emitted = response
                    .await
                    .map_err(|_| "Room worker session reset reply was dropped".to_owned())??;
                Ok(RoomWorkerResponse::SessionProjectionReset { emitted })
            }
            RoomWorkerCommand::DrainSession => {
                let reification = self
                    .session
                    .as_mut()
                    .ok_or("Room worker session runtime is not configured")?
                    .reifications
                    .try_recv()
                    .ok();
                let Some(reification) = reification else {
                    return Ok(RoomWorkerResponse::SessionDrained {
                        committed: false,
                        projection_revision: self.revision,
                    });
                };
                let local = self.room()?.local_actor;
                let (entry, durable_admission, record) = self
                    .room_mut()?
                    .commit_session(&reification.plan, reification.command)
                    .await?;
                events
                    .emit(
                        WorkerEventKind::OutboundRecord,
                        ReplicaLiveRecord {
                            lane: RoomLane::Music,
                            source: PeerId(local.0),
                            record,
                        }
                        .encode(),
                    )
                    .map_err(|error| error.to_string())?;
                let snapshot = self.room()?.room.music_snapshot();
                let durable_revision = snapshot.sequence;
                let durable_view = self.room()?.room.view().music.shared_pitches;
                self.session
                    .as_mut()
                    .expect("session runtime checked above")
                    .task
                    .send(RoomSessionTaskInput::LocalCommitted {
                        peer: reification.peer,
                        plan: reification.plan,
                        entry,
                        durable_admission,
                        history: snapshot.history,
                        durable_view,
                        durable_revision,
                    })
                    .await
                    .map_err(|_| "Room worker session task closed after commit".to_owned())?;
                self.refresh_session_foundations().await?;
                let projection_revision = self.publish_projection(events)?;
                Ok(RoomWorkerResponse::SessionDrained {
                    committed: true,
                    projection_revision,
                })
            }
            RoomWorkerCommand::SignPresence {
                session,
                sequence,
                pitch,
            } => Ok(RoomWorkerResponse::PresenceSigned(
                self.room()?.sign_presence(session, sequence, pitch)?,
            )),
            RoomWorkerCommand::VerifyPresence(bytes) => Ok(RoomWorkerResponse::PresenceVerified(
                self.room()?.verify_presence(&bytes)?,
            )),
        }
    }
}

impl<F> ReplicaWorkerService for RoomReplicaWorkerService<F>
where
    F: RoomWorkerFactory,
{
    fn handle<'a>(
        &'a mut self,
        request: &'a WorkerRequest,
        events: WorkerEventPort,
    ) -> Pin<Box<dyn Future<Output = Result<WorkerReply, String>> + 'a>> {
        Box::pin(async move {
            // The exact eaaade2 worker enum is exhaustive; the next upstream
            // candidate marks it non-exhaustive. Unknown requests must remain
            // explicit refusals, never implicit successful responses.
            #[allow(unreachable_patterns)]
            match request.kind() {
                WorkerRequestKind::Open => {
                    let open: RoomWorkerOpen = decode(request.payload())?;
                    let session_trace = open.session_trace;
                    #[cfg(feature = "browser-acceptance-faults")]
                    let session_renewal_test_cut = open.session_renewal_test_cut;
                    let room = self.factory.open(open).await?;
                    let actor = room.local_actor;
                    let projection = room.projection();
                    self.room = Some(room);
                    self.projection = Some(projection.clone());
                    self.revision = 0;
                    self.subscriptions.clear();
                    self.session_peers.clear();
                    if let Some(session) = self.session.as_mut() {
                        session
                            .task
                            .send(RoomSessionTaskInput::Configure {
                                events: events.clone(),
                                trace_enabled: session_trace,
                                #[cfg(feature = "browser-acceptance-faults")]
                                renewal_test_cut: session_renewal_test_cut,
                            })
                            .await
                            .map_err(|_| {
                                "Room worker session task closed during Open".to_owned()
                            })?;
                    }
                    Ok(WorkerReply::ready(encode(&RoomWorkerResponse::Opened {
                        actor,
                        projection,
                    })?))
                }
                WorkerRequestKind::Subscribe(subscription) => {
                    self.subscriptions.insert(*subscription);
                    Ok(WorkerReply::new(
                        WorkerEventKind::Snapshot {
                            subscription: *subscription,
                            revision: ProjectionRevision::new(self.revision),
                        },
                        encode(self.current_projection()?)?,
                    ))
                }
                WorkerRequestKind::Unsubscribe(subscription) => {
                    self.subscriptions.remove(subscription);
                    Ok(WorkerReply::response(Vec::new()))
                }
                WorkerRequestKind::Command => {
                    let command: RoomWorkerCommand = decode(request.payload())?;
                    let response = self.handle_command(command, &events).await?;
                    Ok(WorkerReply::response(encode(&response)?))
                }
                WorkerRequestKind::InboundRecord => {
                    let live = ReplicaLiveRecord::decode(request.payload())
                        .ok_or("invalid Room-v5 live record")?;
                    let lane = live.lane;
                    let observed = live.record.entry().clone();
                    let (accepted, entry) = self.room_mut()?.apply_live(live).await?;
                    if accepted && lane == RoomLane::Music && self.session.is_some() {
                        let durable_admission =
                            self.room()?.durable_entry_admission(lane, entry)?;
                        let snapshot = self.room()?.room.music_snapshot();
                        let durable_revision = snapshot.sequence;
                        let durable_view = self.room()?.room.view().music.shared_pitches;
                        let input =
                            if hhhs_session::ReifiedSessionCommand::has_domain(&observed.payload) {
                                RoomSessionTaskInput::Observed {
                                    entry: observed,
                                    durable_admission,
                                    history: snapshot.history,
                                    durable_view,
                                    durable_revision,
                                }
                            } else {
                                RoomSessionTaskInput::DurableAdvanced {
                                    history: snapshot.history,
                                    durable_view,
                                    durable_revision,
                                }
                            };
                        self.session
                            .as_mut()
                            .expect("session runtime checked above")
                            .task
                            .send(input)
                            .await
                            .map_err(|_| {
                                "Room worker session task closed after inbound admission".to_owned()
                            })?;
                        self.refresh_session_foundations().await?;
                    }
                    let projection_revision = self.publish_projection(&events)?;
                    Ok(WorkerReply::response(encode(
                        &RoomWorkerResponse::InboundApplied {
                            accepted,
                            entry: *entry.as_bytes(),
                            projection_revision,
                        },
                    )?))
                }
                WorkerRequestKind::RepairFrame => {
                    let operation: RoomWorkerRepairRequest = decode(request.payload())?;
                    let repaired_music = match &operation {
                        RoomWorkerRepairRequest::Frame { session, .. } => self
                            .room()?
                            .repair_lane_for(*session)
                            .is_some_and(|lane| lane == RoomLane::Music),
                        _ => false,
                    };
                    let music_revision_before = repaired_music
                        .then(|| self.room().map(|room| room.room.music_snapshot().sequence))
                        .transpose()?;
                    let step = match operation {
                        RoomWorkerRepairRequest::StartInitiator { session, lane } => {
                            self.room_mut()?
                                .start_initiator(session, lane, &events)
                                .await?
                        }
                        RoomWorkerRepairRequest::StartResponder {
                            session,
                            lane,
                            hello,
                        } => {
                            self.room_mut()?
                                .start_responder(session, lane, hello, &events)
                                .await?
                        }
                        RoomWorkerRepairRequest::Frame { session, frame } => {
                            self.room_mut()?
                                .advance_repair(session, frame, &events)
                                .await?
                        }
                        RoomWorkerRepairRequest::Finish {
                            session,
                            close_error,
                        } => self.room_mut()?.finish_repair(session, close_error)?,
                    };
                    let music_revision_after = self.room()?.room.music_snapshot().sequence;
                    let music_revision_advanced =
                        repair_revision_advanced(music_revision_before, music_revision_after);
                    if music_revision_advanced && self.session.is_some() {
                        let snapshot = self.room()?.room.music_snapshot();
                        let durable_revision = snapshot.sequence;
                        let durable_view = self.room()?.room.view().music.shared_pitches;
                        self.session
                            .as_mut()
                            .expect("session runtime checked above")
                            .task
                            .send(RoomSessionTaskInput::RepairResynchronized {
                                history: snapshot.history,
                                durable_view,
                                durable_revision,
                            })
                            .await
                            .map_err(|_| {
                                "Room worker session task closed after music repair".to_owned()
                            })?;
                        self.refresh_session_foundations().await?;
                    }
                    let _ = self.publish_projection(&events)?;
                    Ok(WorkerReply::response(encode(&RoomWorkerResponse::Repair(
                        step,
                    ))?))
                }
                WorkerRequestKind::Ping => Ok(WorkerReply::new(
                    WorkerEventKind::Pong,
                    encode(&RoomWorkerResponse::Frontiers(
                        self.current_projection()?.clone(),
                    ))?,
                )),
                WorkerRequestKind::Close => {
                    self.room = None;
                    self.projection = None;
                    self.subscriptions.clear();
                    self.session_peers.clear();
                    Ok(WorkerReply::new(
                        WorkerEventKind::Closed,
                        encode(&RoomWorkerResponse::Closed)?,
                    ))
                }
                WorkerRequestKind::AcknowledgeRepairFrame(_) => Err(
                    "repair-frame acknowledgements must be intercepted by the worker host".into(),
                ),
                unknown => Err(format!(
                    "unsupported Replica worker request kind: {unknown:?}"
                )),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use futures::executor::block_on;
    use hhhs_web_browser::{InProcessWorkerHost, WorkerEventKind, WorkerRequestKind};

    use super::*;
    use crate::{
        room::v5::open_room_authority,
        tuning::{TunedDegree, Tuning},
    };

    #[derive(Clone, Default)]
    struct MemoryLog(Rc<RefCell<Vec<StorageTransaction>>>);

    impl AsyncTransactionSink for MemoryLog {
        type Error = std::convert::Infallible;

        async fn persist(&mut self, transaction: &StorageTransaction) -> Result<(), Self::Error> {
            self.0.borrow_mut().push(transaction.clone());
            Ok(())
        }
    }

    #[derive(Default)]
    struct MemoryFactory;

    impl RoomWorkerFactory for MemoryFactory {
        type Durability = MemoryLog;

        fn open<'a>(&'a mut self, request: RoomWorkerOpen) -> RoomWorkerOpenFuture<'a, MemoryLog> {
            Box::pin(async move {
                RoomDataPlane::open(
                    request,
                    MemoryLog::default(),
                    MemoryLog::default(),
                    Vec::new(),
                    Vec::new(),
                )
                .await
            })
        }
    }

    fn open_request(room_name: &str, seed: [u8; 32]) -> RoomWorkerOpen {
        let authority = open_room_authority(room_name);
        RoomWorkerOpen::new(
            &RoomIdentity::from_name(room_name),
            ActorId::from_signing_key(&authority),
            seed,
            Some(authority.to_bytes()),
        )
    }

    #[test]
    fn open_request_disables_session_diagnostics_and_fault_injection_by_default() {
        let open = open_request("worker-open-defaults", [0x11; 32]);
        assert!(!open.session_trace);
        #[cfg(feature = "browser-acceptance-faults")]
        assert!(!open.session_renewal_test_cut);
    }

    #[test]
    fn repair_projection_reset_requires_actual_music_growth() {
        assert!(!repair_revision_advanced(None, 4));
        assert!(!repair_revision_advanced(Some(4), 4));
        assert!(!repair_revision_advanced(Some(5), 4));
        assert!(repair_revision_advanced(Some(4), 5));
        assert!(repair_revision_advanced(Some(4), 7));
    }

    async fn open_and_subscribe(
        host: &mut InProcessWorkerHost<RoomReplicaWorkerService<MemoryFactory>>,
        open: RoomWorkerOpen,
    ) {
        let opened = host
            .dispatch(WorkerRequestKind::Open, encode_open(&open).unwrap())
            .await
            .unwrap();
        assert_eq!(opened.reply.kind(), &WorkerEventKind::Ready);
        let response = decode_response(opened.reply.payload()).unwrap();
        assert!(matches!(response, RoomWorkerResponse::Opened { .. }));
        let subscribed = host
            .dispatch(
                WorkerRequestKind::Subscribe(SubscriptionId::new(1)),
                Vec::new(),
            )
            .await
            .unwrap();
        assert!(matches!(
            subscribed.reply.kind(),
            WorkerEventKind::Snapshot { .. }
        ));
        decode_projection(subscribed.reply.payload()).unwrap();
    }

    async fn commit_degree(
        host: &mut InProcessWorkerHost<RoomReplicaWorkerService<MemoryFactory>>,
        degree: u16,
    ) {
        commit_degree_state(host, degree, true).await;
    }

    async fn commit_degree_state(
        host: &mut InProcessWorkerHost<RoomReplicaWorkerService<MemoryFactory>>,
        degree: u16,
        active: bool,
    ) {
        let tuning = Tuning::twelve_tet();
        let degree = TunedDegree::new(&tuning, degree).unwrap();
        let operation = if active {
            super::super::v5::MusicOp::AddDegree { degree }
        } else {
            super::super::v5::MusicOp::RemoveDegree { degree }
        };
        let command = RoomWorkerCommand::Commit(operation.into());
        let dispatched = host
            .dispatch(
                WorkerRequestKind::Command,
                encode_command(&command).unwrap(),
            )
            .await
            .unwrap();
        let response_revision = match decode_response(dispatched.reply.payload()).unwrap() {
            RoomWorkerResponse::CommandCommitted {
                projection_revision,
                ..
            } => projection_revision,
            other => panic!("unexpected command response: {other:?}"),
        };
        assert!(
            dispatched
                .events
                .iter()
                .any(|event| matches!(event.kind(), WorkerEventKind::OutboundRecord))
        );
        let event_revision = dispatched
            .events
            .iter()
            .find_map(|event| match event.kind() {
                WorkerEventKind::Revision { revision, .. } => Some(revision.get()),
                _ => None,
            })
            .expect("command publishes an exact projection revision");
        assert_eq!(response_revision, event_revision);
    }

    async fn repair_request(
        host: &mut InProcessWorkerHost<RoomReplicaWorkerService<MemoryFactory>>,
        request: RoomWorkerRepairRequest,
    ) -> (RoomWorkerRepairStep, Vec<Vec<u8>>) {
        let dispatched = host
            .dispatch(
                WorkerRequestKind::RepairFrame,
                encode_repair(&request).unwrap(),
            )
            .await
            .unwrap();
        let frames = dispatched
            .events
            .iter()
            .filter_map(|event| {
                matches!(event.kind(), WorkerEventKind::RepairFrame { .. })
                    .then(|| event.payload().to_vec())
            })
            .collect();
        match decode_response(dispatched.reply.payload()).unwrap() {
            RoomWorkerResponse::Repair(step) => (step, frames),
            other => panic!("unexpected repair response: {other:?}"),
        }
    }

    async fn repair_lane_between(
        alice: &mut InProcessWorkerHost<RoomReplicaWorkerService<MemoryFactory>>,
        bob: &mut InProcessWorkerHost<RoomReplicaWorkerService<MemoryFactory>>,
        session: u64,
        lane: RoomLane,
    ) -> (RoomWorkerRepairOutcome, RoomWorkerRepairOutcome) {
        let (mut alice_step, mut to_bob) = repair_request(
            alice,
            RoomWorkerRepairRequest::StartInitiator { session, lane },
        )
        .await;
        let hello = to_bob.remove(0);
        let (mut bob_step, mut to_alice) = repair_request(
            bob,
            RoomWorkerRepairRequest::StartResponder {
                session,
                lane,
                hello,
            },
        )
        .await;
        for _ in 0..1_024 {
            if let Some(frame) = to_bob.pop() {
                let (step, frames) =
                    repair_request(bob, RoomWorkerRepairRequest::Frame { session, frame }).await;
                bob_step = step;
                to_alice.extend(frames.into_iter().rev());
            } else if let Some(frame) = to_alice.pop() {
                let (step, frames) =
                    repair_request(alice, RoomWorkerRepairRequest::Frame { session, frame }).await;
                alice_step = step;
                to_bob.extend(frames.into_iter().rev());
            } else {
                break;
            }
        }
        assert_eq!(alice_step.status, RoomWorkerRepairStatus::Complete);
        assert_eq!(bob_step.status, RoomWorkerRepairStatus::Complete);
        let (alice_finished, _) = repair_request(
            alice,
            RoomWorkerRepairRequest::Finish {
                session,
                close_error: None,
            },
        )
        .await;
        let (bob_finished, _) = repair_request(
            bob,
            RoomWorkerRepairRequest::Finish {
                session,
                close_error: None,
            },
        )
        .await;
        assert_eq!(
            alice_finished.disposition,
            Some(RoomWorkerRepairDisposition::Synchronized)
        );
        assert_eq!(
            bob_finished.disposition,
            Some(RoomWorkerRepairDisposition::Synchronized)
        );
        (alice_finished.outcome, bob_finished.outcome)
    }

    #[test]
    fn worker_local_advance_requires_a_fresh_repair_cut() {
        block_on(async {
            let room_name = "worker-stale-cut-song";
            let mut alice =
                InProcessWorkerHost::with_defaults(RoomReplicaWorkerService::new(MemoryFactory));
            let mut bob =
                InProcessWorkerHost::with_defaults(RoomReplicaWorkerService::new(MemoryFactory));
            open_and_subscribe(&mut alice, open_request(room_name, [0x51; 32])).await;
            open_and_subscribe(&mut bob, open_request(room_name, [0x62; 32])).await;
            commit_degree(&mut alice, 3).await;

            let session = 41;
            let (mut alice_step, mut to_bob) = repair_request(
                &mut alice,
                RoomWorkerRepairRequest::StartInitiator {
                    session,
                    lane: RoomLane::Music,
                },
            )
            .await;
            let hello = to_bob.remove(0);

            let (mut bob_step, mut to_alice) = repair_request(
                &mut bob,
                RoomWorkerRepairRequest::StartResponder {
                    session,
                    lane: RoomLane::Music,
                    hello,
                },
            )
            .await;
            for _ in 0..1_024 {
                if let Some(frame) = to_bob.pop() {
                    let (step, frames) =
                        repair_request(&mut bob, RoomWorkerRepairRequest::Frame { session, frame })
                            .await;
                    bob_step = step;
                    to_alice.extend(frames.into_iter().rev());
                } else if let Some(frame) = to_alice.pop() {
                    let (step, frames) = repair_request(
                        &mut alice,
                        RoomWorkerRepairRequest::Frame { session, frame },
                    )
                    .await;
                    alice_step = step;
                    to_bob.extend(frames.into_iter().rev());
                } else {
                    break;
                }
            }
            assert_eq!(alice_step.status, RoomWorkerRepairStatus::Complete);
            assert_eq!(bob_step.status, RoomWorkerRepairStatus::Complete);

            // A command before terminal repair can be folded into HHHS's
            // incremental recapture and reconciled in the same attempt. This
            // one lands after Done/Ack but before carrier-close confirmation,
            // so the completed cut is truthful yet no longer current.
            commit_degree(&mut alice, 9).await;

            let (alice_finished, _) = repair_request(
                &mut alice,
                RoomWorkerRepairRequest::Finish {
                    session,
                    close_error: None,
                },
            )
            .await;
            let (bob_finished, _) = repair_request(
                &mut bob,
                RoomWorkerRepairRequest::Finish {
                    session,
                    close_error: None,
                },
            )
            .await;
            assert_eq!(
                alice_finished.disposition,
                Some(RoomWorkerRepairDisposition::RetryFresh(
                    RoomWorkerRepairRetryReason::HistoryAdvanced
                ))
            );
            assert_eq!(
                bob_finished.disposition,
                Some(RoomWorkerRepairDisposition::Synchronized)
            );

            repair_lane_between(&mut alice, &mut bob, 42, RoomLane::Music).await;
            let alice_projection = alice.service().room().unwrap().projection();
            let bob_projection = bob.service().room().unwrap().projection();
            assert_eq!(alice_projection.view.music, bob_projection.view.music);
            assert_eq!(
                alice_projection.music_frontier,
                bob_projection.music_frontier
            );
            assert_eq!(alice_projection.view.music.live.len(), 2);
        });
    }

    #[test]
    fn exact_service_commits_and_repairs_two_worker_placements() {
        block_on(async {
            let room_name = "worker-repair-song";
            let mut alice =
                InProcessWorkerHost::with_defaults(RoomReplicaWorkerService::new(MemoryFactory));
            let mut bob =
                InProcessWorkerHost::with_defaults(RoomReplicaWorkerService::new(MemoryFactory));
            open_and_subscribe(&mut alice, open_request(room_name, [0x31; 32])).await;
            open_and_subscribe(&mut bob, open_request(room_name, [0x42; 32])).await;
            commit_degree(&mut alice, 3).await;
            commit_degree(&mut bob, 8).await;

            repair_lane_between(&mut alice, &mut bob, 7, RoomLane::Music).await;
            repair_lane_between(&mut alice, &mut bob, 8, RoomLane::Extension).await;

            let alice_projection = alice.service().room().unwrap().projection();
            let bob_projection = bob.service().room().unwrap().projection();
            assert_eq!(alice_projection, bob_projection);
            assert_eq!(alice_projection.view.music.live.len(), 2);

            let (alice_equal, bob_equal) =
                repair_lane_between(&mut alice, &mut bob, 9, RoomLane::Music).await;
            assert_eq!(
                alice_equal.frames_sent + bob_equal.frames_sent,
                4,
                "an equal-root reconnect is Hello/CutRoot/Done/Done only",
            );
            assert_eq!(alice_equal.frames_received + bob_equal.frames_received, 4);
            for host in [&alice, &bob] {
                let room = host.service().room().unwrap();
                assert!(room.lane(RoomLane::Music).has_cached_snapshot());
                assert!(room.lane(RoomLane::Extension).has_cached_snapshot());
            }

            alice
                .dispatch(WorkerRequestKind::Close, Vec::new())
                .await
                .unwrap();
            let mut restarted =
                InProcessWorkerHost::with_defaults(RoomReplicaWorkerService::new(MemoryFactory));
            open_and_subscribe(&mut restarted, open_request(room_name, [0x31; 32])).await;
            let reopened = restarted.service().room().unwrap();
            assert!(!reopened.lane(RoomLane::Music).has_cached_snapshot());
            assert!(!reopened.lane(RoomLane::Extension).has_cached_snapshot());
        });
    }

    #[test]
    fn browser_worker_shared_degree_can_be_removed_by_the_other_peer() {
        block_on(async {
            let room_name = "worker-shared-remove-song";
            let tuning = Tuning::twelve_tet();
            let degree = TunedDegree::new(&tuning, 5).unwrap();
            let mut alice =
                InProcessWorkerHost::with_defaults(RoomReplicaWorkerService::new(MemoryFactory));
            let mut bob =
                InProcessWorkerHost::with_defaults(RoomReplicaWorkerService::new(MemoryFactory));
            open_and_subscribe(&mut alice, open_request(room_name, [0x51; 32])).await;
            open_and_subscribe(&mut bob, open_request(room_name, [0x62; 32])).await;

            commit_degree_state(&mut alice, degree.degree.index(), true).await;
            repair_lane_between(&mut alice, &mut bob, 21, RoomLane::Music).await;
            assert!(
                bob.service()
                    .room()
                    .unwrap()
                    .projection()
                    .view
                    .music
                    .shared_pitches
                    .pitch_classes
                    .contains(&degree)
            );

            commit_degree_state(&mut bob, degree.degree.index(), false).await;
            assert!(
                !bob.service()
                    .room()
                    .unwrap()
                    .projection()
                    .view
                    .music
                    .shared_pitches
                    .pitch_classes
                    .contains(&degree),
                "the source worker must materialize its own removal before repair",
            );
            repair_lane_between(&mut bob, &mut alice, 22, RoomLane::Music).await;
            for host in [&alice, &bob] {
                assert!(
                    !host
                        .service()
                        .room()
                        .unwrap()
                        .projection()
                        .view
                        .music
                        .shared_pitches
                        .pitch_classes
                        .contains(&degree),
                    "cross-peer observed removal must clear the shared degree"
                );
            }
        });
    }
}
