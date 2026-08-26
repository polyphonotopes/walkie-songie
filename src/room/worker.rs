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

use hhhs::{Digest, EntryHash};
use hhhs_replica::{
    AsyncTransactionSink, DurableReplicaHost, ReplicaRecord, ReplicaRepairSnapshot,
};
use hhhs_store::{MemoryStorage, StorageTransaction};
use hhhs_sync::sync_session::SessionStatus;
use hhhs_sync::{Refusal, Snapshot as _, SyncMessage, SyncSession};
use hhhs_web_browser::{
    ProjectionRevision, ReplicaWorkerService, SubscriptionId, WorkerEventKind, WorkerEventPort,
    WorkerReply, WorkerRequest, WorkerRequestKind,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    net::{PeerId, ReplicaLiveRecord, repair_lane, replica_frontier_digest},
    room::v5::{
        ActorId, RoomCommand, RoomIdentity, RoomLane, RoomPresence, RoomReplicas, RoomView,
    },
    tuning::TunedPeriodicPitch,
};

const ROOM_WORKER_PAYLOAD_VERSION: u16 = 1;
const MAX_ACTIVE_REPAIR_SESSIONS: usize = 8;
const APPLICATION_REFUSAL_ABORT_PREFIX: &str = "application refused repair entries:";

type DurableLane<D> = DurableReplicaHost<MemoryStorage, super::v5::RoomAdmissionPolicy, D>;
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
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub(crate) struct RoomWorkerProjection {
    pub view: RoomView,
    pub music_frontier: [u8; 32],
    pub extension_frontier: [u8; 32],
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub(crate) enum RoomWorkerCommand {
    Commit(RoomCommand),
    GrantPeer(ActorId),
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
    Close {
        session: u64,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub(crate) enum RoomWorkerRepairStatus {
    Exchanging,
    Aborted,
    Complete,
    Divergent,
    Closed,
}

impl From<SessionStatus> for RoomWorkerRepairStatus {
    fn from(value: SessionStatus) -> Self {
        match value {
            SessionStatus::Exchanging => Self::Exchanging,
            SessionStatus::Aborted => Self::Aborted,
            SessionStatus::Complete => Self::Complete,
            SessionStatus::Divergent => Self::Divergent,
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

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub(crate) struct RoomWorkerRepairStep {
    pub session: u64,
    pub frames: Vec<Vec<u8>>,
    pub status: RoomWorkerRepairStatus,
    pub outcome: RoomWorkerRepairOutcome,
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
    session: SyncSession,
    source: ReplicaRepairSnapshot,
    outcome: RoomWorkerRepairOutcome,
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
        let mut music = room.music_durable_host(music_log);
        let mut extension = room.extension_durable_host(extension_log);

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
        RoomWorkerProjection {
            view,
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
            .commit_prepared(prepared.into_prepared())
            .await
            .map_err(|error| error.to_string())?;
        Ok((
            committed.outcome().entry,
            committed.replica_record().clone(),
        ))
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

    fn start_initiator(&mut self, id: u64, lane: RoomLane) -> Result<RoomWorkerRepairStep, String> {
        self.ensure_repair_slot(id)?;
        let salt: [u8; 16] = rand::random();
        let source = hhhs_sync::RepairHost::capture(self.lane(lane), salt)
            .map_err(|error| error.to_string())?;
        let repair_lane = repair_lane(lane);
        let (session, opening) = SyncSession::initiate(
            repair_lane.strategy().clone(),
            source.index(),
            hhhs_sync::reconciliation::Config::default(),
            salt,
        );
        let mut session = session.with_budget(hhhs_sync::SessionBudget::default());
        session.set_root(Some(source.root()));
        let frames = encode_frames(opening)?;
        let outcome = RoomWorkerRepairOutcome {
            frames_sent: frames.len(),
            ..RoomWorkerRepairOutcome::default()
        };
        self.repair.insert(
            id,
            RepairState {
                lane,
                session,
                source,
                outcome: outcome.clone(),
            },
        );
        Ok(RoomWorkerRepairStep {
            session: id,
            frames,
            status: RoomWorkerRepairStatus::Exchanging,
            outcome,
        })
    }

    fn start_responder(
        &mut self,
        id: u64,
        lane: RoomLane,
        hello: Vec<u8>,
    ) -> Result<RoomWorkerRepairStep, String> {
        self.ensure_repair_slot(id)?;
        let message = SyncMessage::decode(&hello).map_err(|error| error.to_string())?;
        let SyncMessage::Hello(hello) = message else {
            return Err("first repair frame was not a Hello".into());
        };
        let source = hhhs_sync::RepairHost::capture(self.lane(lane), hello.session_salt)
            .map_err(|error| error.to_string())?;
        let repair_lane = repair_lane(lane);
        let session = SyncSession::accept(
            &hello,
            repair_lane.strategy().clone(),
            source.index(),
            hhhs_sync::reconciliation::Config::default(),
        )
        .map_err(|error| format!("repair strategy rejected: {error}"))?;
        let mut session = session.with_budget(hhhs_sync::SessionBudget::default());
        session.set_root(Some(source.root()));
        let outcome = RoomWorkerRepairOutcome {
            frames_received: 1,
            ..RoomWorkerRepairOutcome::default()
        };
        self.repair.insert(
            id,
            RepairState {
                lane,
                session,
                source,
                outcome: outcome.clone(),
            },
        );
        Ok(RoomWorkerRepairStep {
            session: id,
            frames: Vec::new(),
            status: RoomWorkerRepairStatus::Exchanging,
            outcome,
        })
    }

    async fn advance_repair(
        &mut self,
        id: u64,
        frame: Vec<u8>,
    ) -> Result<RoomWorkerRepairStep, String> {
        let mut state = self
            .repair
            .remove(&id)
            .ok_or_else(|| format!("repair session {id} is not active"))?;
        state.outcome.frames_received = state.outcome.frames_received.saturating_add(1);
        let message = SyncMessage::decode(&frame).map_err(|error| error.to_string())?;
        let answered_fetch = matches!(message, SyncMessage::Entries { .. });
        let output = state
            .session
            .on_message(message, &state.source)
            .map_err(|error| format!("HHHS repair session {id} failed: {error}"))?;
        let mut frames = encode_frames(output.send)?;

        if answered_fetch {
            let report = hhhs_sync::RepairHost::apply(self.lane_mut(state.lane), &output.ingest)
                .await
                .map_err(|error| error.to_string())?;
            state.outcome.admitted = state.outcome.admitted.saturating_add(report.admitted.len());
            state.outcome.lifted = state.outcome.lifted.saturating_add(report.lifted);
            state.outcome.refused = state.outcome.refused.saturating_add(report.refused.len());
            let policy_refusals = report
                .refused
                .iter()
                .filter(|(_, refusal)| matches!(refusal, Refusal::Unauthorized | Refusal::Declined))
                .count();
            if policy_refusals > 0 {
                state.outcome.policy_divergence = true;
                frames.push(
                    SyncMessage::Abort {
                        reason: format!("{APPLICATION_REFUSAL_ABORT_PREFIX} {policy_refusals}"),
                    }
                    .encode(),
                );
                state.outcome.frames_sent = state.outcome.frames_sent.saturating_add(frames.len());
                return Ok(RoomWorkerRepairStep {
                    session: id,
                    frames,
                    status: RoomWorkerRepairStatus::Aborted,
                    outcome: state.outcome,
                });
            }
            state.source = hhhs_sync::RepairHost::recapture(
                self.lane(state.lane),
                &state.source,
                &report.admitted,
                state.session.salt(),
            )
            .map_err(|error| error.to_string())?;
            let follow_up = state
                .session
                .resume_admitted(
                    state.source.index(),
                    &report.admitted,
                    Some(state.source.root()),
                )
                .map_err(|error| error.to_string())?;
            frames.extend(encode_frames(follow_up)?);
        }

        state.outcome.frames_sent = state.outcome.frames_sent.saturating_add(frames.len());
        let status = RoomWorkerRepairStatus::from(state.session.status());
        let outcome = state.outcome.clone();
        if status == RoomWorkerRepairStatus::Exchanging {
            self.repair.insert(id, state);
        }
        Ok(RoomWorkerRepairStep {
            session: id,
            frames,
            status,
            outcome,
        })
    }

    fn close_repair(&mut self, id: u64) -> RoomWorkerRepairStep {
        let outcome = self
            .repair
            .remove(&id)
            .map(|state| state.outcome)
            .unwrap_or_default();
        RoomWorkerRepairStep {
            session: id,
            frames: Vec::new(),
            status: RoomWorkerRepairStatus::Closed,
            outcome,
        }
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

fn encode_frames(messages: Vec<SyncMessage>) -> Result<Vec<Vec<u8>>, String> {
    let max = hhhs_sync::SessionLimits::default().max_frame_bytes;
    messages
        .into_iter()
        .map(|message| {
            let frame = message.encode();
            if frame.len() > max {
                Err(format!(
                    "repair frame is {} bytes; maximum is {max}",
                    frame.len()
                ))
            } else {
                Ok(frame)
            }
        })
        .collect()
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
        }
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
                let projection_revision = self.publish_projection(events)?;
                Ok(RoomWorkerResponse::PeerGranted {
                    entries,
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
            match request.kind() {
                WorkerRequestKind::Open => {
                    let open: RoomWorkerOpen = decode(request.payload())?;
                    let room = self.factory.open(open).await?;
                    let actor = room.local_actor;
                    let projection = room.projection();
                    self.room = Some(room);
                    self.projection = Some(projection.clone());
                    self.revision = 0;
                    self.subscriptions.clear();
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
                    let (accepted, entry) = self.room_mut()?.apply_live(live).await?;
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
                    let step = match operation {
                        RoomWorkerRepairRequest::StartInitiator { session, lane } => {
                            self.room_mut()?.start_initiator(session, lane)?
                        }
                        RoomWorkerRepairRequest::StartResponder {
                            session,
                            lane,
                            hello,
                        } => self.room_mut()?.start_responder(session, lane, hello)?,
                        RoomWorkerRepairRequest::Frame { session, frame } => {
                            self.room_mut()?.advance_repair(session, frame).await?
                        }
                        RoomWorkerRepairRequest::Close { session } => {
                            self.room_mut()?.close_repair(session)
                        }
                    };
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
                    Ok(WorkerReply::new(
                        WorkerEventKind::Closed,
                        encode(&RoomWorkerResponse::Closed)?,
                    ))
                }
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
        let tuning = Tuning::twelve_tet();
        let command = RoomWorkerCommand::Commit(
            super::super::v5::MusicOp::AddDegree {
                degree: TunedDegree::new(&tuning, degree).unwrap(),
            }
            .into(),
        );
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
    ) -> RoomWorkerRepairStep {
        let dispatched = host
            .dispatch(
                WorkerRequestKind::RepairFrame,
                encode_repair(&request).unwrap(),
            )
            .await
            .unwrap();
        match decode_response(dispatched.reply.payload()).unwrap() {
            RoomWorkerResponse::Repair(step) => step,
            other => panic!("unexpected repair response: {other:?}"),
        }
    }

    async fn repair_lane_between(
        alice: &mut InProcessWorkerHost<RoomReplicaWorkerService<MemoryFactory>>,
        bob: &mut InProcessWorkerHost<RoomReplicaWorkerService<MemoryFactory>>,
        session: u64,
        lane: RoomLane,
    ) {
        let mut alice_step = repair_request(
            alice,
            RoomWorkerRepairRequest::StartInitiator { session, lane },
        )
        .await;
        let hello = alice_step.frames.remove(0);
        let mut bob_step = repair_request(
            bob,
            RoomWorkerRepairRequest::StartResponder {
                session,
                lane,
                hello,
            },
        )
        .await;
        let mut to_bob = alice_step.frames;
        let mut to_alice = bob_step.frames;
        for _ in 0..1_024 {
            if let Some(frame) = to_bob.pop() {
                bob_step =
                    repair_request(bob, RoomWorkerRepairRequest::Frame { session, frame }).await;
                to_alice.extend(bob_step.frames.clone().into_iter().rev());
            } else if let Some(frame) = to_alice.pop() {
                alice_step =
                    repair_request(alice, RoomWorkerRepairRequest::Frame { session, frame }).await;
                to_bob.extend(alice_step.frames.clone().into_iter().rev());
            } else {
                break;
            }
        }
        assert_eq!(alice_step.status, RoomWorkerRepairStatus::Complete);
        assert_eq!(bob_step.status, RoomWorkerRepairStatus::Complete);
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
        });
    }
}
