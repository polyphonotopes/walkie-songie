//! Native Iroh/Room-v5 leg for the plugin bridge.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel},
    },
    thread::{self, JoinHandle},
};

use ed25519_dalek::SigningKey as RealtimeSigningKey;
use hhhs::DagRead;
use hhhs_replica::ReplicaRepairHost;
use hhhs_store::MemoryStorage;
use hhhs_sync::{CachedRepairHost, RepairHost, SessionLimits};
use tokio::{sync::mpsc, task::JoinSet};
use tutti_music::{
    MusicOp, SharedPitchSet, TunedDegree, TunedPeriodicPitch, roundtable::RoundTableConfig,
};
use tutti_realtime::{Frame as RealtimeFrame, MidiFrame, MidiKind};
use tutti_roundtable::{ConfigState, Frame as RoundTableFrame};

use super::{
    BoardSessionBinding, BridgeCommand, BridgeError, BridgeTransport, LinkState,
    PitchIntentOutcome, RealtimeMidi, RealtimeMidiKind, TransportEvent,
};
use crate::{
    net::{
        IrohSyncStream, NativeNetworkEvent, NativeRoomNetwork, PeerId, ReplicaLiveRecord,
        ReplicaProtocol, ReplicaRepairHint, ReplicaRepairProbe, ReplicaRoomNetworkConfig,
        RoomInbound, RoomRealtime, SyncStream, TokioTimer, WalkieIdentity, drive_replica_initiator,
        drive_replica_responder, is_routine_repair_initiator, replica_frontier_digest,
        spawn_rendezvous_v5,
    },
    room::v5::{
        ActorId, MemberCapabilities, ProtocolSupport, RoomIdentity, RoomLane, RoomReplicas,
        open_room_authority,
    },
};

const COMMAND_CAPACITY: usize = 64;
const EVENT_CAPACITY: usize = 256;
const MAX_REPLAY_SESSIONS: usize = 128;

type SharedRoom = Arc<RoomReplicas<MemoryStorage, MemoryStorage>>;
type NativeRepairHost =
    CachedRepairHost<ReplicaRepairHost<MemoryStorage, crate::room::v5::RoomAdmissionPolicy>>;
type SharedRepairHost = Arc<tokio::sync::Mutex<NativeRepairHost>>;
type InFlight = Arc<tokio::sync::Mutex<BTreeSet<(iroh::EndpointId, RoomLane)>>>;

fn round_table_settings(mut config: RoundTableConfig) -> RoundTableConfig {
    config.pattern = tutti_music::roundtable::RoundTablePattern::default().cleared();
    config
}

#[derive(Clone)]
struct RepairHosts {
    music: SharedRepairHost,
    extension: SharedRepairHost,
}

impl RepairHosts {
    fn new(room: &SharedRoom) -> Self {
        Self {
            music: Arc::new(tokio::sync::Mutex::new(CachedRepairHost::new(
                room.music_repair_host(),
            ))),
            extension: Arc::new(tokio::sync::Mutex::new(CachedRepairHost::new(
                room.extension_repair_host(),
            ))),
        }
    }

    fn get(&self, lane: RoomLane) -> SharedRepairHost {
        match lane {
            RoomLane::Music => Arc::clone(&self.music),
            RoomLane::Extension => Arc::clone(&self.extension),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct NativeRoomConfig {
    pub identity_seed: [u8; 32],
}

impl NativeRoomConfig {
    pub const fn new(identity_seed: [u8; 32]) -> Self {
        Self { identity_seed }
    }
}

enum DriverCommand {
    ConfigureIdentity([u8; 32]),
    Join(String),
    Leave,
    PrepareBoardProvisioning(BoardSessionBinding),
    Send(RealtimeFrame),
    RoundTable(RoundTableFrame),
    BoardEdit {
        token: u64,
        frame: RoundTableFrame,
        settings: Option<RoundTableConfig>,
        pitch_edits: Vec<(TunedPeriodicPitch, bool)>,
    },
    PitchEdit {
        token: u64,
        pitch: TunedPeriodicPitch,
        active: bool,
    },
    Shutdown,
}

/// Non-blocking facade for a private native Iroh runtime.
pub struct NativeRoomTransport {
    commands: mpsc::Sender<DriverCommand>,
    events: Receiver<TransportEvent>,
    dropped_events: Arc<AtomicU64>,
    abandoned_pitch_intents_through: Arc<AtomicU64>,
    highest_queued_pitch_intent: Arc<AtomicU64>,
    worker_disconnect_reported: bool,
    stopping: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl NativeRoomTransport {
    pub fn spawn(config: NativeRoomConfig) -> Result<Self, BridgeError> {
        let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (event_tx, events) = sync_channel(EVENT_CAPACITY);
        let dropped_events = Arc::new(AtomicU64::new(0));
        let abandoned_pitch_intents_through = Arc::new(AtomicU64::new(0));
        let sink = EventSink {
            sender: event_tx,
            dropped: Arc::clone(&dropped_events),
            abandoned_pitch_intents_through: Arc::clone(&abandoned_pitch_intents_through),
        };
        let stopping = Arc::new(AtomicBool::new(false));
        let worker_stopping = Arc::clone(&stopping);
        let worker = thread::Builder::new()
            .name("walkie-iroh-room".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        sink.send(TransportEvent::Diagnostic(format!(
                            "could not create native room runtime: {error}"
                        )));
                        sink.send(TransportEvent::RoomLink(LinkState::Failed));
                        return;
                    }
                };
                runtime.block_on(driver_loop(command_rx, sink, worker_stopping, config));
            })
            .map_err(|error| {
                BridgeError::Unavailable(format!("could not start native room worker: {error}"))
            })?;
        Ok(Self {
            commands,
            events,
            dropped_events,
            abandoned_pitch_intents_through,
            highest_queued_pitch_intent: Arc::new(AtomicU64::new(0)),
            worker_disconnect_reported: false,
            stopping,
            worker: Some(worker),
        })
    }

    fn command(&self, command: DriverCommand) -> Result<(), BridgeError> {
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => BridgeError::QueueFull {
                    queue: "native room command",
                },
                mpsc::error::TrySendError::Closed(_) => {
                    BridgeError::Unavailable("native room worker has stopped".into())
                }
            })
    }
}

impl BridgeTransport for NativeRoomTransport {
    fn start(&mut self) -> Result<(), BridgeError> {
        Ok(())
    }

    fn handle_command(&mut self, command: BridgeCommand) -> Result<(), BridgeError> {
        match command {
            BridgeCommand::ConfigureRoomIdentity { identity_seed } => {
                self.command(DriverCommand::ConfigureIdentity(identity_seed))
            }
            BridgeCommand::SelectRoom(room) => self.command(DriverCommand::Join(room)),
            BridgeCommand::LeaveRoom => self.command(DriverCommand::Leave),
            BridgeCommand::PrepareBoardProvisioning(binding) => {
                self.command(DriverCommand::PrepareBoardProvisioning(binding))
            }
            BridgeCommand::PublishRoundTable(frame) => {
                self.command(DriverCommand::RoundTable(frame))
            }
            BridgeCommand::PublishBoardEdit {
                token,
                frame,
                settings,
                pitch_edits,
            } => {
                self.command(DriverCommand::BoardEdit {
                    token,
                    frame,
                    settings,
                    pitch_edits,
                })?;
                self.highest_queued_pitch_intent
                    .fetch_max(token, Ordering::AcqRel);
                Ok(())
            }
            BridgeCommand::SetSharedPitch {
                token,
                pitch,
                active,
            } => {
                self.command(DriverCommand::PitchEdit {
                    token,
                    pitch,
                    active,
                })?;
                self.highest_queued_pitch_intent
                    .fetch_max(token, Ordering::AcqRel);
                Ok(())
            }
            _ => Err(BridgeError::Unavailable(
                "command does not belong to the native room leg".into(),
            )),
        }
    }

    fn send_realtime(&mut self, event: RealtimeMidi) -> Result<(), BridgeError> {
        self.command(DriverCommand::Send(RealtimeFrame::Midi(to_midi(event)?)))
    }

    fn poll_event(&mut self) -> Option<TransportEvent> {
        let through_token = self
            .abandoned_pitch_intents_through
            .swap(0, Ordering::AcqRel);
        if through_token != 0 {
            return Some(TransportEvent::PitchIntentReset { through_token });
        }
        match self.events.try_recv() {
            Ok(event) => Some(event),
            Err(TryRecvError::Empty) => {
                let dropped = self.dropped_events.swap(0, Ordering::AcqRel);
                (dropped != 0).then_some(TransportEvent::Diagnostic(format!(
                    "native room event queue dropped {dropped} values"
                )))
            }
            Err(TryRecvError::Disconnected) if !self.worker_disconnect_reported => {
                self.worker_disconnect_reported = true;
                Some(TransportEvent::PitchIntentReset {
                    through_token: self.highest_queued_pitch_intent.load(Ordering::Acquire),
                })
            }
            Err(TryRecvError::Disconnected) => None,
        }
    }

    fn shutdown(&mut self) {
        self.stopping.store(true, Ordering::Release);
        let _ = self.commands.try_send(DriverCommand::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for NativeRoomTransport {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        let _ = self.commands.try_send(DriverCommand::Shutdown);
        let _ = self.worker.take();
    }
}

#[derive(Clone)]
struct EventSink {
    sender: SyncSender<TransportEvent>,
    dropped: Arc<AtomicU64>,
    abandoned_pitch_intents_through: Arc<AtomicU64>,
}

impl EventSink {
    fn send(&self, event: TransportEvent) {
        match self.sender.try_send(event) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => {}
            Err(TrySendError::Full(TransportEvent::PitchIntentOutcome { token, .. })) => {
                self.abandoned_pitch_intents_through
                    .fetch_max(token, Ordering::AcqRel);
            }
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

struct ActiveRoom {
    name: String,
    identity: RoomIdentity,
    signing_key: RealtimeSigningKey,
    capability_key: hhhs_proof::SigningKey,
    local_capabilities: MemberCapabilities,
    board_music_grants: BTreeMap<ActorId, hhhs::EntryHash>,
    session: u64,
    sequence: u64,
    forwarded_round_table: RoundTableConfig,
    forwarded_pitch_set: SharedPitchSet,
    network: NativeRoomNetwork,
    replica: SharedRoom,
    repair_hosts: RepairHosts,
    authority: hhhs_proof::SigningKey,
    rendezvous: crate::net::RendezvousHandle,
    rendezvous_rx: mpsc::UnboundedReceiver<(iroh::EndpointId, ProtocolSupport)>,
    rendezvous_open: bool,
    peers: BTreeMap<iroh::EndpointId, ProtocolSupport>,
    connected: BTreeSet<iroh::EndpointId>,
    replay: BTreeMap<(PeerId, u64), u64>,
    in_flight: InFlight,
    repairs: JoinSet<()>,
}

trait BoardEditAdmissionHost {
    async fn admit_board_settings(
        &mut self,
        settings: RoundTableConfig,
        events: &EventSink,
    ) -> Result<(), String>;

    async fn admit_board_pitch(
        &mut self,
        pitch: TunedPeriodicPitch,
        active: bool,
    ) -> Result<(), String>;
}

async fn admit_board_intents<H: BoardEditAdmissionHost>(
    host: &mut H,
    settings: Option<RoundTableConfig>,
    pitch_edits: Vec<(TunedPeriodicPitch, bool)>,
    events: &EventSink,
) -> Result<(), String> {
    if let Some(settings) = settings {
        host.admit_board_settings(settings, events).await?;
    }
    for (pitch, active) in pitch_edits {
        host.admit_board_pitch(pitch, active).await?;
    }
    Ok(())
}

fn pitch_intent_outcome(result: Result<(), String>) -> PitchIntentOutcome {
    match result {
        Ok(()) => PitchIntentOutcome::Applied,
        Err(error) => PitchIntentOutcome::Rejected(error),
    }
}

impl ActiveRoom {
    async fn open(
        name: String,
        config: NativeRoomConfig,
        events: &EventSink,
    ) -> Result<Self, String> {
        if !crate::is_valid_room_name(&name) {
            return Err(format!("invalid Walkie room name {name:?}"));
        }
        let authority = open_room_authority(&name);
        let owner = ActorId::from_signing_key(&authority);
        let identity = WalkieIdentity::from_seed(config.identity_seed);
        let local_actor = identity.capability_actor_id();
        let replica =
            Arc::new(RoomReplicas::memory(&name, owner).map_err(|error| error.to_string())?);
        let invitation = replica
            .grant_member(&authority, local_actor)
            .map_err(|error| error.to_string())?;
        let repair_hosts = RepairHosts::new(&replica);
        let network = NativeRoomNetwork::bind(
            identity.iroh_secret(),
            ReplicaRoomNetworkConfig::create(&name, owner),
        )
        .await
        .map_err(|error| error.to_string())?;
        let endpoint = network.endpoint_id();
        let (rendezvous_tx, rendezvous_rx) = mpsc::unbounded_channel();
        let rendezvous = spawn_rendezvous_v5(
            network.rendezvous_peering(),
            network.topic(),
            ProtocolSupport::WALKIE,
            move |peer, support| {
                let _ = rendezvous_tx.send((peer, support));
            },
        );
        events.send(TransportEvent::Diagnostic(format!(
            "Iroh endpoint {endpoint} joined room {name}"
        )));
        let room_identity = RoomIdentity::from_name(&name);
        let initial_view = replica.view();
        let forwarded_round_table = round_table_settings(initial_view.music.round_table);
        let forwarded_pitch_set = initial_view.sounding_pitch_set();
        Ok(Self {
            name,
            identity: room_identity,
            signing_key: RealtimeSigningKey::from_bytes(&config.identity_seed),
            capability_key: identity.capability_signing_key(),
            local_capabilities: invitation.capabilities,
            board_music_grants: BTreeMap::new(),
            session: rand::random(),
            sequence: 0,
            forwarded_round_table,
            forwarded_pitch_set,
            network,
            replica,
            repair_hosts,
            authority,
            rendezvous,
            rendezvous_rx,
            rendezvous_open: true,
            peers: BTreeMap::new(),
            connected: BTreeSet::new(),
            replay: BTreeMap::new(),
            in_flight: Arc::new(tokio::sync::Mutex::new(BTreeSet::new())),
            repairs: JoinSet::new(),
        })
    }

    fn prepare_board_capability_bundle(
        &mut self,
        binding: BoardSessionBinding,
    ) -> Result<Vec<u8>, String> {
        let board = ActorId::from_bytes(binding.identity);
        let grant = if let Some(grant) = self.board_music_grants.get(&board) {
            *grant
        } else {
            let prepared = self
                .replica
                .prepare_member_grant(RoomLane::Music, &self.authority, board)
                .map_err(|error| error.to_string())?;
            let grant = self
                .replica
                .commit_prepared_member_grant(prepared)
                .map_err(|error| error.to_string())?;
            self.board_music_grants.insert(board, grant);
            grant
        };
        let bundle = self
            .replica
            .export_music_capability_bundle(board, [grant])
            .map_err(|error| error.to_string())?;
        tutti_music_hhhs::encode_embedded_capability_bundle(&bundle)
            .map_err(|error| error.to_string())
    }

    async fn send_realtime(&mut self, frame: RealtimeFrame) -> Result<(), String> {
        let bytes = RoomRealtime::encode(
            &self.identity,
            &self.signing_key,
            self.session,
            self.sequence,
            frame,
        )
        .map_err(|error| error.to_string())?;
        self.sequence = self.sequence.wrapping_add(1);
        self.network
            .broadcast(bytes)
            .await
            .map_err(|error| error.to_string())
    }

    async fn admit_round_table_settings(
        &mut self,
        config: Option<RoundTableConfig>,
        events: &EventSink,
    ) -> Result<(), String> {
        if let Some(config) = config {
            // Pitch membership has one durable authority: SharedPitchSet.
            // Round-table records retain settings only, even though the
            // carrier-facing value type also contains a projected pattern.
            let config = round_table_settings(config);
            if self.replica.view().music.round_table == config {
                return Ok(());
            }
            let prepared = self
                .replica
                .prepare_author(
                    &self.capability_key,
                    &self.local_capabilities,
                    MusicOp::SetRoundTable { config }.into(),
                )
                .map_err(|error| error.to_string())?;
            let record = prepared.replica_record();
            self.replica
                .commit_prepared(prepared)
                .map_err(|error| error.to_string())?;
            self.network
                .broadcast(
                    ReplicaLiveRecord {
                        lane: RoomLane::Music,
                        source: PeerId(*self.network.endpoint_id().as_bytes()),
                        record,
                    }
                    .encode(),
                )
                .await
                .map_err(|error| error.to_string())?;
            events.send(TransportEvent::Diagnostic(
                "board arpeggiator settings committed to HHHS".into(),
            ));
        }
        Ok(())
    }

    async fn admit_pitch_edit(
        &mut self,
        pitch: TunedPeriodicPitch,
        active: bool,
    ) -> Result<(), String> {
        let view = self.replica.view().music;
        let mut commands = Vec::new();
        let degree = TunedDegree {
            tuning_id: pitch.tuning_id,
            degree: pitch.pitch.degree(),
        };
        if active && !view.live.contains(&degree) {
            commands.push(MusicOp::AddDegree { degree });
        } else if !active && view.live.contains(&degree) {
            commands.push(MusicOp::RemoveDegree { degree });
        }
        // MIDI/board input is pitch-class mode by default. Canonicalize any
        // older absolute members of the same class so another octave cannot
        // appear as a duplicate or survive a cross-peer removal.
        for candidate in view.live_pitches {
            if candidate.tuning_id == degree.tuning_id && candidate.pitch.degree() == degree.degree
            {
                commands.push(MusicOp::RemovePitch { pitch: candidate });
            }
        }
        if commands.is_empty() {
            return Ok(());
        }
        for command in commands {
            let prepared = self
                .replica
                .prepare_author(
                    &self.capability_key,
                    &self.local_capabilities,
                    command.into(),
                )
                .map_err(|error| error.to_string())?;
            let record = prepared.replica_record();
            self.replica
                .commit_prepared(prepared)
                .map_err(|error| error.to_string())?;
            self.network
                .broadcast(
                    ReplicaLiveRecord {
                        lane: RoomLane::Music,
                        source: PeerId(*self.network.endpoint_id().as_bytes()),
                        record,
                    }
                    .encode(),
                )
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    async fn accept_round_table(
        &mut self,
        frame: RoundTableFrame,
        events: &EventSink,
    ) -> Result<(), String> {
        let settings = match frame {
            RoundTableFrame::Run(run) => Some(run.config),
            RoundTableFrame::Config(config) => Some(config.config),
            RoundTableFrame::Pulse(_) | RoundTableFrame::ConfigSnapshot(_) => None,
        };
        self.admit_round_table_settings(settings, events).await?;
        self.publish_round_table_if_changed(events, false);
        self.send_realtime(RealtimeFrame::RoundTable(frame)).await
    }

    async fn accept_pitch_edit(
        &mut self,
        pitch: TunedPeriodicPitch,
        active: bool,
        events: &EventSink,
    ) -> Result<(), String> {
        self.admit_pitch_edit(pitch, active).await?;
        self.publish_pitch_set_if_changed(events, false);
        Ok(())
    }

    async fn accept_board_edit(
        &mut self,
        frame: RoundTableFrame,
        settings: Option<RoundTableConfig>,
        pitch_edits: Vec<(TunedPeriodicPitch, bool)>,
        events: &EventSink,
    ) -> Result<(), String> {
        // This is deliberately a bounded envelope of separate application
        // intents, not a claim of one atomic HHHS admission. If a later pitch
        // admission fails, the already-admitted settings revision remains
        // canonical and the caller receives Rejected for the unfinished
        // envelope.
        admit_board_intents(self, settings, pitch_edits, events).await?;
        // Publish only after all requested pitch operations have admitted.
        // On failure the driver publishes the actual admitted prefix before
        // emitting Rejected; presentation continuity never masquerades as
        // durable transactionality.
        self.publish_pitch_set_if_changed(events, false);
        self.publish_round_table_if_changed(events, false);
        self.send_realtime(RealtimeFrame::RoundTable(frame)).await
    }

    fn publish_round_table_if_changed(&mut self, events: &EventSink, force: bool) {
        let config = round_table_settings(self.replica.view().music.round_table);
        if !force && config == self.forwarded_round_table {
            return;
        }
        self.forwarded_round_table = config;
        events.send(TransportEvent::RoundTable(RoundTableFrame::Config(
            ConfigState { config },
        )));
    }

    fn publish_pitch_set_if_changed(&mut self, events: &EventSink, force: bool) {
        let shared = self.replica.view().sounding_pitch_set();
        if !force && shared == self.forwarded_pitch_set {
            return;
        }
        self.forwarded_pitch_set = shared.clone();
        events.send(TransportEvent::RoomPitchSet(shared));
    }

    async fn handle_discovered(
        &mut self,
        peer: iroh::EndpointId,
        support: ProtocolSupport,
        events: &EventSink,
    ) {
        if peer != self.network.endpoint_id() && self.peers.insert(peer, support).is_none() {
            events.send(TransportEvent::Diagnostic(format!(
                "discovered room peer {peer}; awaiting Iroh gossip link"
            )));
        }
    }

    async fn handle_inbound(&mut self, inbound: RoomInbound, events: &EventSink) {
        match inbound {
            RoomInbound::Repair(repair) => {
                let Some(protocol) = ReplicaProtocol::from_alpn(repair.alpn) else {
                    repair
                        .connection
                        .close(4u32.into(), b"unsupported Room-v5 ALPN");
                    return;
                };
                spawn_responder(
                    &mut self.repairs,
                    *repair,
                    self.repair_hosts.get(protocol.lane()),
                    protocol.lane(),
                    Arc::clone(&self.in_flight),
                    events.clone(),
                );
            }
            RoomInbound::Event(event) => self.handle_network_event(event, events).await,
        }
    }

    async fn handle_network_event(&mut self, event: NativeNetworkEvent, events: &EventSink) {
        match event {
            NativeNetworkEvent::NeighborUp { endpoint_id, .. } => {
                let inserted = self.connected.insert(endpoint_id);
                let support = *self
                    .peers
                    .entry(endpoint_id)
                    .or_insert(ProtocolSupport::WALKIE);
                if inserted {
                    events.send(TransportEvent::RoomPeers(
                        u32::try_from(self.connected.len()).unwrap_or(u32::MAX),
                    ));
                    events.send(TransportEvent::Diagnostic(format!(
                        "Iroh gossip linked room peer {endpoint_id}"
                    )));
                }
                if let Err(error) = self.grant_peer(endpoint_id).await {
                    events.send(TransportEvent::Diagnostic(format!(
                        "could not grant room peer {endpoint_id}: {error}"
                    )));
                }
                let local = PeerId(*self.network.endpoint_id().as_bytes());
                let remote = PeerId(*endpoint_id.as_bytes());
                if is_routine_repair_initiator(local, remote) {
                    for lane in [RoomLane::Music, RoomLane::Extension] {
                        if support.supports(lane) {
                            self.spawn_initiator(endpoint_id, lane, events);
                        }
                    }
                }
            }
            NativeNetworkEvent::NeighborDown { endpoint_id } => {
                if self.connected.remove(&endpoint_id) {
                    events.send(TransportEvent::RoomPeers(
                        u32::try_from(self.connected.len()).unwrap_or(u32::MAX),
                    ));
                }
            }
            NativeNetworkEvent::Message { bytes, .. } => {
                if let Ok(realtime) = RoomRealtime::decode(&self.identity, &bytes) {
                    if realtime.source == PeerId(*self.network.endpoint_id().as_bytes()) {
                        return;
                    }
                    let key = (realtime.source, realtime.session);
                    if self
                        .replay
                        .get(&key)
                        .is_some_and(|last| realtime.sequence <= *last)
                    {
                        return;
                    }
                    if self.replay.len() == MAX_REPLAY_SESSIONS
                        && !self.replay.contains_key(&key)
                        && let Some(oldest) = self.replay.keys().next().copied()
                    {
                        self.replay.remove(&oldest);
                    }
                    self.replay.insert(key, realtime.sequence);
                    events.send(frame_event(realtime.frame));
                } else if let Some(live) = ReplicaLiveRecord::decode(&bytes) {
                    self.apply_live(live, events).await;
                } else if let Some(hint) = ReplicaRepairHint::decode(&bytes) {
                    self.repair_from_peer(hint.source, hint.lane, events);
                } else if let Some(probe) = ReplicaRepairProbe::decode(&bytes) {
                    let frontier = match probe.lane {
                        RoomLane::Music => self.replica.music_snapshot().history.frontier(),
                        RoomLane::Extension => self.replica.extension_snapshot().history.frontier(),
                    };
                    if replica_frontier_digest(&frontier) != probe.frontier {
                        self.repair_from_peer(probe.source, probe.lane, events);
                    }
                }
            }
            NativeNetworkEvent::Lagged => {
                events.send(TransportEvent::Diagnostic(
                    "Iroh room gossip lagged; scheduling HHHS repair".into(),
                ));
                let peers = self
                    .peers
                    .iter()
                    .map(|(peer, support)| (*peer, *support))
                    .collect::<Vec<_>>();
                for (peer, support) in peers {
                    let local = PeerId(*self.network.endpoint_id().as_bytes());
                    let remote = PeerId(*peer.as_bytes());
                    if !self.connected.contains(&peer)
                        || !is_routine_repair_initiator(local, remote)
                    {
                        continue;
                    }
                    for lane in [RoomLane::Music, RoomLane::Extension] {
                        if support.supports(lane) {
                            self.spawn_initiator(peer, lane, events);
                        }
                    }
                }
            }
            NativeNetworkEvent::Diagnostic(message) => {
                events.send(TransportEvent::Diagnostic(format!("Iroh room: {message}")));
            }
            NativeNetworkEvent::Closed => {
                events.send(TransportEvent::RoomLink(LinkState::Failed));
                events.send(TransportEvent::Diagnostic(
                    "Iroh room event stream closed".into(),
                ));
            }
            NativeNetworkEvent::MdnsDiscovered { .. } | NativeNetworkEvent::MdnsExpired { .. } => {}
        }
    }

    async fn apply_live(&mut self, live: ReplicaLiveRecord, events: &EventSink) {
        let entry = live.record.entry_hash();
        let host = self.repair_hosts.get(live.lane);
        let mut host = host.lock_owned().await;
        match RepairHost::apply(&mut *host, &[(entry, live.record.encode())]).await {
            Ok(report) if report.refused.is_empty() => {
                self.publish_round_table_if_changed(events, false);
                self.publish_pitch_set_if_changed(events, false);
            }
            Ok(report) => events.send(TransportEvent::Diagnostic(format!(
                "Room-v5 live record was refused: {:?}",
                report.refused
            ))),
            Err(error) => events.send(TransportEvent::Diagnostic(format!(
                "Room-v5 live admission failed: {error}"
            ))),
        }
    }

    async fn grant_peer(&self, peer: iroh::EndpointId) -> Result<(), String> {
        let actor = ActorId(*peer.as_bytes());
        let existing = self.replica.capabilities_for(actor);
        if !existing.music.is_empty() && !existing.extension.is_empty() {
            return Ok(());
        }
        let invitation = self
            .replica
            .grant_member(&self.authority, actor)
            .map_err(|error| error.to_string())?;
        for (lane, entries) in [
            (RoomLane::Music, invitation.capabilities.music),
            (RoomLane::Extension, invitation.capabilities.extension),
        ] {
            for entry in entries {
                self.network
                    .broadcast(
                        ReplicaRepairHint {
                            lane,
                            source: PeerId(*self.network.endpoint_id().as_bytes()),
                            entry,
                        }
                        .encode(),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    }

    fn repair_from_peer(&mut self, source: PeerId, lane: RoomLane, events: &EventSink) {
        let Ok(peer) = iroh::EndpointId::from_bytes(&source.0) else {
            return;
        };
        if peer == self.network.endpoint_id()
            || !self.connected.contains(&peer)
            || !self
                .peers
                .get(&peer)
                .is_some_and(|support| support.supports(lane))
        {
            return;
        }
        let local = PeerId(*self.network.endpoint_id().as_bytes());
        if !is_routine_repair_initiator(local, source) {
            return;
        }
        self.spawn_initiator(peer, lane, events);
    }

    fn spawn_initiator(&mut self, peer: iroh::EndpointId, lane: RoomLane, events: &EventSink) {
        spawn_initiator(
            &mut self.repairs,
            self.network.endpoint().clone(),
            peer,
            self.repair_hosts.get(lane),
            lane,
            Arc::clone(&self.in_flight),
            events.clone(),
        );
    }

    async fn shutdown(mut self) {
        self.rendezvous.stop();
        self.repairs.abort_all();
        while self.repairs.join_next().await.is_some() {}
        let _ = self.network.shutdown().await;
    }
}

impl BoardEditAdmissionHost for ActiveRoom {
    async fn admit_board_settings(
        &mut self,
        settings: RoundTableConfig,
        events: &EventSink,
    ) -> Result<(), String> {
        self.admit_round_table_settings(Some(settings), events)
            .await
    }

    async fn admit_board_pitch(
        &mut self,
        pitch: TunedPeriodicPitch,
        active: bool,
    ) -> Result<(), String> {
        self.admit_pitch_edit(pitch, active).await
    }
}

enum LoopEvent {
    Command(Option<DriverCommand>),
    Discovered(Option<(iroh::EndpointId, ProtocolSupport)>),
    Inbound(Option<RoomInbound>),
    Repair(Option<Result<(), tokio::task::JoinError>>),
}

async fn driver_loop(
    mut commands: mpsc::Receiver<DriverCommand>,
    events: EventSink,
    stopping: Arc<AtomicBool>,
    mut config: NativeRoomConfig,
) {
    let mut active: Option<ActiveRoom> = None;
    loop {
        if stopping.load(Ordering::Acquire) {
            break;
        }
        let event = if let Some(room) = active.as_mut() {
            tokio::select! {
                command = commands.recv() => LoopEvent::Command(command),
                discovered = room.rendezvous_rx.recv(), if room.rendezvous_open => LoopEvent::Discovered(discovered),
                inbound = room.network.next_inbound() => LoopEvent::Inbound(inbound),
                repair = room.repairs.join_next(), if !room.repairs.is_empty() => LoopEvent::Repair(repair),
            }
        } else {
            LoopEvent::Command(commands.recv().await)
        };
        match event {
            LoopEvent::Command(Some(DriverCommand::ConfigureIdentity(identity_seed))) => {
                if active.is_some() {
                    events.send(TransportEvent::Diagnostic(
                        "native room identity can only change before joining a room".into(),
                    ));
                } else {
                    config.identity_seed = identity_seed;
                }
            }
            LoopEvent::Command(Some(DriverCommand::Join(name))) => {
                if active.as_ref().is_some_and(|room| room.name == name) {
                    events.send(TransportEvent::RoomSelected(name));
                    events.send(TransportEvent::RoomLink(LinkState::Ready));
                    continue;
                }
                if let Some(room) = active.take() {
                    room.shutdown().await;
                }
                // A room switch is an endpoint replacement, not a replay of
                // edges from the previous room. Retract the old authoritative
                // projection before the new replica can publish its initial
                // materialization (or fail to open).
                events.send(TransportEvent::RoomPitchSet(SharedPitchSet::default()));
                events.send(TransportEvent::RoomPeers(0));
                events.send(TransportEvent::RoomLink(LinkState::Connecting));
                match ActiveRoom::open(name.clone(), config, &events).await {
                    Ok(room) => {
                        let mut room = room;
                        room.publish_round_table_if_changed(&events, true);
                        room.publish_pitch_set_if_changed(&events, true);
                        events.send(TransportEvent::RoomSelected(name));
                        events.send(TransportEvent::RoomLink(LinkState::Ready));
                        active = Some(room);
                    }
                    Err(error) => {
                        events.send(TransportEvent::RoomLink(LinkState::Failed));
                        events.send(TransportEvent::Diagnostic(format!(
                            "could not join Iroh room: {error}"
                        )));
                    }
                }
            }
            LoopEvent::Command(Some(DriverCommand::Leave)) => {
                if let Some(room) = active.take() {
                    events.send(TransportEvent::Diagnostic(format!(
                        "left Iroh room {}",
                        room.name
                    )));
                    room.shutdown().await;
                }
                events.send(TransportEvent::RoomPitchSet(SharedPitchSet::default()));
                events.send(TransportEvent::RoomPeers(0));
                events.send(TransportEvent::RoomLink(LinkState::Offline));
            }
            LoopEvent::Command(Some(DriverCommand::PrepareBoardProvisioning(binding))) => {
                let result = active
                    .as_mut()
                    .ok_or_else(|| "no active room is available for board provisioning".to_owned())
                    .and_then(|room| room.prepare_board_capability_bundle(binding));
                match result {
                    Ok(bundle) => events
                        .send(TransportEvent::BoardCapabilityBundlePrepared { binding, bundle }),
                    Err(reason) => {
                        events.send(TransportEvent::BoardProvisioningFailed { binding, reason })
                    }
                }
            }
            LoopEvent::Command(Some(DriverCommand::Send(frame))) => {
                if let Some(room) = active.as_mut()
                    && let Err(error) = room.send_realtime(frame).await
                {
                    events.send(TransportEvent::Diagnostic(format!(
                        "Iroh realtime send failed: {error}"
                    )));
                }
            }
            LoopEvent::Command(Some(DriverCommand::RoundTable(frame))) => {
                if let Some(room) = active.as_mut()
                    && let Err(error) = room.accept_round_table(frame, &events).await
                {
                    events.send(TransportEvent::Diagnostic(format!(
                        "round-table bridge failed: {error}"
                    )));
                }
            }
            LoopEvent::Command(Some(DriverCommand::BoardEdit {
                token,
                frame,
                settings,
                pitch_edits,
            })) => {
                let result = if let Some(room) = active.as_mut() {
                    let result = room
                        .accept_board_edit(frame, settings, pitch_edits, &events)
                        .await;
                    // Even a rejected envelope may have admitted a truthful
                    // prefix because settings and pitches are explicit
                    // separate intents. Publish that canonical fact before
                    // the correlated outcome clears presentation fencing.
                    room.publish_pitch_set_if_changed(&events, false);
                    room.publish_round_table_if_changed(&events, false);
                    result
                } else {
                    Err("no active room is available for the board edit".into())
                };
                let outcome = pitch_intent_outcome(result);
                events.send(TransportEvent::PitchIntentOutcome { token, outcome });
            }
            LoopEvent::Command(Some(DriverCommand::PitchEdit {
                token,
                pitch,
                active: enabled,
            })) => {
                let result = if let Some(room) = active.as_mut() {
                    let result = room.accept_pitch_edit(pitch, enabled, &events).await;
                    room.publish_pitch_set_if_changed(&events, false);
                    result
                } else {
                    Err("no active room is available for the shared pitch edit".into())
                };
                let outcome = pitch_intent_outcome(result);
                events.send(TransportEvent::PitchIntentOutcome { token, outcome });
            }
            LoopEvent::Command(Some(DriverCommand::Shutdown) | None) => break,
            LoopEvent::Discovered(Some((peer, support))) => {
                if let Some(room) = active.as_mut() {
                    room.handle_discovered(peer, support, &events).await;
                }
            }
            LoopEvent::Discovered(None) => {
                events.send(TransportEvent::Diagnostic(
                    "room rendezvous task stopped".into(),
                ));
                if let Some(room) = active.as_mut() {
                    room.rendezvous_open = false;
                }
            }
            LoopEvent::Inbound(Some(inbound)) => {
                if let Some(room) = active.as_mut() {
                    room.handle_inbound(inbound, &events).await;
                    room.publish_round_table_if_changed(&events, false);
                    room.publish_pitch_set_if_changed(&events, false);
                }
            }
            LoopEvent::Inbound(None) => {
                events.send(TransportEvent::RoomLink(LinkState::Failed));
                events.send(TransportEvent::Diagnostic(
                    "Iroh room transport stopped".into(),
                ));
                if let Some(room) = active.take() {
                    room.shutdown().await;
                }
                events.send(TransportEvent::RoomPitchSet(SharedPitchSet::default()));
                events.send(TransportEvent::RoomPeers(0));
            }
            LoopEvent::Repair(Some(result)) => {
                if let Err(error) = result {
                    events.send(TransportEvent::Diagnostic(format!(
                        "Iroh room repair task failed: {error}"
                    )));
                }
                if let Some(room) = active.as_mut() {
                    // A repair task mutates the shared Replica directly. Wake
                    // the bridge projection as part of task completion rather
                    // than waiting for an unrelated network message.
                    room.publish_round_table_if_changed(&events, false);
                    room.publish_pitch_set_if_changed(&events, false);
                }
            }
            LoopEvent::Repair(None) => {}
        }
    }
    if let Some(room) = active.take() {
        room.shutdown().await;
    }
    events.send(TransportEvent::RoomPitchSet(SharedPitchSet::default()));
    events.send(TransportEvent::RoomPeers(0));
    events.send(TransportEvent::RoomLink(LinkState::Offline));
}

fn spawn_responder(
    repairs: &mut JoinSet<()>,
    repair: crate::net::IncomingRepair,
    repair_host: SharedRepairHost,
    lane: RoomLane,
    in_flight: InFlight,
    events: EventSink,
) {
    repairs.spawn(async move {
        let stream = repair.stream.owning(repair.connection);
        if !in_flight.lock().await.insert((repair.endpoint_id, lane)) {
            if let Err(error) = stream.close().await {
                events.send(TransportEvent::Diagnostic(format!(
                    "duplicate {lane:?} repair stream close failed: {error}"
                )));
            }
            return;
        }
        let mut host = repair_host.lock_owned().await;
        let result = drive_replica_responder(
            stream,
            &TokioTimer,
            &mut *host,
            lane,
            SessionLimits::default(),
        )
        .await;
        in_flight.lock().await.remove(&(repair.endpoint_id, lane));
        match result {
            Ok(confirmed)
                if confirmed.disposition() == hhhs_sync::RepairDisposition::Synchronized => {}
            Ok(confirmed) => events.send(TransportEvent::Diagnostic(format!(
                "Room-v5 responder repair on {lane:?} requires {:?}: {:?}",
                confirmed.disposition(),
                confirmed.outcome()
            ))),
            Err(error) => events.send(TransportEvent::Diagnostic(format!(
                "Room-v5 responder repair failed on {lane:?}: {error}"
            ))),
        }
    });
}

fn spawn_initiator(
    repairs: &mut JoinSet<()>,
    endpoint: iroh::Endpoint,
    peer: iroh::EndpointId,
    repair_host: SharedRepairHost,
    lane: RoomLane,
    in_flight: InFlight,
    events: EventSink,
) {
    repairs.spawn(async move {
        if !in_flight.lock().await.insert((peer, lane)) {
            return;
        }
        let result = async {
            let connection = endpoint
                .connect(peer, lane.repair_alpn())
                .await
                .map_err(|error| error.to_string())?;
            let stream = IrohSyncStream::open(&connection)
                .await
                .map_err(|error| error.to_string())?
                .owning(connection);
            let mut host = repair_host.lock_owned().await;
            drive_replica_initiator(
                stream,
                &TokioTimer,
                &mut *host,
                lane,
                SessionLimits::default(),
            )
            .await
            .map_err(|error| error.to_string())
        }
        .await;
        match result {
            Ok(confirmed)
                if confirmed.disposition() == hhhs_sync::RepairDisposition::Synchronized => {}
            Ok(confirmed) => events.send(TransportEvent::Diagnostic(format!(
                "Room-v5 initiator repair for {peer} on {lane:?} requires {:?}: {:?}",
                confirmed.disposition(),
                confirmed.outcome()
            ))),
            Err(error) => events.send(TransportEvent::Diagnostic(format!(
                "Room-v5 initiator repair failed for {peer} on {lane:?}: {error}"
            ))),
        }
        in_flight.lock().await.remove(&(peer, lane));
    });
}

fn to_midi(event: RealtimeMidi) -> Result<MidiFrame, BridgeError> {
    let kind = match event.kind {
        RealtimeMidiKind::NoteOn => MidiKind::NoteOn,
        RealtimeMidiKind::NoteOff => MidiKind::NoteOff,
        RealtimeMidiKind::Choke => MidiKind::Choke,
        RealtimeMidiKind::PolyPressure => MidiKind::PolyPressure,
        RealtimeMidiKind::PitchBend => MidiKind::PitchBend,
        RealtimeMidiKind::ChannelPressure => MidiKind::ChannelPressure,
    };
    MidiFrame::from_normalized(event.voice_id, event.channel, event.note, kind, event.value)
        .map_err(|error| BridgeError::Transport(error.to_string()))
}

fn frame_event(frame: RealtimeFrame) -> TransportEvent {
    match frame {
        RealtimeFrame::Midi(event) => {
            let kind = match event.kind {
                MidiKind::NoteOn => RealtimeMidiKind::NoteOn,
                MidiKind::NoteOff => RealtimeMidiKind::NoteOff,
                MidiKind::Choke => RealtimeMidiKind::Choke,
                MidiKind::PolyPressure => RealtimeMidiKind::PolyPressure,
                MidiKind::PitchBend => RealtimeMidiKind::PitchBend,
                MidiKind::ChannelPressure => RealtimeMidiKind::ChannelPressure,
            };
            TransportEvent::Midi(RealtimeMidi {
                timing: 0,
                voice_id: event.voice_id,
                channel: event.channel,
                note: event.note,
                kind,
                value: event.normalized_value(),
            })
        }
        RealtimeFrame::RoundTable(frame) => TransportEvent::RoundTable(frame),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::bridge::BridgePitchState;

    #[derive(Default)]
    struct RefusingBoardAdmission {
        settings_admitted: usize,
        pitches_attempted: usize,
        refuse_pitch: bool,
    }

    impl BoardEditAdmissionHost for RefusingBoardAdmission {
        async fn admit_board_settings(
            &mut self,
            _settings: RoundTableConfig,
            _events: &EventSink,
        ) -> Result<(), String> {
            self.settings_admitted += 1;
            Ok(())
        }

        async fn admit_board_pitch(
            &mut self,
            _pitch: TunedPeriodicPitch,
            _active: bool,
        ) -> Result<(), String> {
            self.pitches_attempted += 1;
            if self.refuse_pitch {
                Err("injected pitch admission refusal".into())
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn mixed_board_edit_reports_an_admitted_prefix_without_atomicity_claim() {
        let (sender, _events) = sync_channel(1);
        let sink = EventSink {
            sender,
            dropped: Arc::new(AtomicU64::new(0)),
            abandoned_pitch_intents_through: Arc::new(AtomicU64::new(0)),
        };
        let mut host = RefusingBoardAdmission {
            refuse_pitch: true,
            ..RefusingBoardAdmission::default()
        };
        let mut settings = RoundTableConfig::default();
        settings.pulse_ms = settings.pulse_ms.saturating_add(1);
        settings.pattern = tutti_music::roundtable::RoundTablePattern::default().cleared();
        let pitch = BridgePitchState::pitch_for_midi(52).unwrap();

        let result = admit_board_intents(
            &mut host,
            Some(settings),
            vec![(pitch, true), (pitch, false)],
            &sink,
        )
        .await;

        assert_eq!(host.settings_admitted, 1);
        assert_eq!(host.pitches_attempted, 1);
        assert!(matches!(
            pitch_intent_outcome(result),
            PitchIntentOutcome::Rejected(reason)
                if reason == "injected pitch admission refusal"
        ));
    }

    #[tokio::test]
    async fn pattern_only_envelope_executes_only_shared_pitch_admission() {
        let (sender, _events) = sync_channel(1);
        let sink = EventSink {
            sender,
            dropped: Arc::new(AtomicU64::new(0)),
            abandoned_pitch_intents_through: Arc::new(AtomicU64::new(0)),
        };
        let mut host = RefusingBoardAdmission::default();
        let pitch = BridgePitchState::pitch_for_midi(52).unwrap();

        admit_board_intents(&mut host, None, vec![(pitch, true)], &sink)
            .await
            .unwrap();

        assert_eq!(host.settings_admitted, 0);
        assert_eq!(host.pitches_attempted, 1);
    }

    #[test]
    fn saturated_event_queue_requests_a_token_bounded_board_reset() {
        let (sender, _events) = sync_channel(1);
        let abandoned = Arc::new(AtomicU64::new(0));
        let sink = EventSink {
            sender,
            dropped: Arc::new(AtomicU64::new(0)),
            abandoned_pitch_intents_through: Arc::clone(&abandoned),
        };
        sink.send(TransportEvent::Diagnostic("occupy the queue".into()));

        sink.send(TransportEvent::PitchIntentOutcome {
            token: 37,
            outcome: PitchIntentOutcome::Rejected("injected refusal".into()),
        });

        assert_eq!(abandoned.load(Ordering::Acquire), 37);
    }

    #[test]
    fn board_edit_without_active_room_is_explicitly_rejected() {
        let mut room = NativeRoomTransport::spawn(NativeRoomConfig::new([41; 32])).unwrap();
        let frame = RoundTableFrame::Config(ConfigState {
            config: RoundTableConfig::default(),
        });
        room.handle_command(BridgeCommand::PublishBoardEdit {
            token: 23,
            frame,
            settings: None,
            pitch_edits: Vec::new(),
        })
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut outcome = None;
        while Instant::now() < deadline && outcome.is_none() {
            if let Some(TransportEvent::PitchIntentOutcome {
                token,
                outcome: result,
            }) = room.poll_event()
            {
                outcome = Some((token, result));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let (token, result) = outcome.expect("inactive room must answer the board edit");
        assert_eq!(token, 23);
        assert!(matches!(
            result,
            PitchIntentOutcome::Rejected(reason) if reason.contains("no active room")
        ));
        room.shutdown();
    }

    #[test]
    fn host_pitch_without_active_room_is_explicitly_rejected() {
        let mut room = NativeRoomTransport::spawn(NativeRoomConfig::new([42; 32])).unwrap();
        room.handle_command(BridgeCommand::SetSharedPitch {
            token: 24,
            pitch: BridgePitchState::pitch_for_midi(60).unwrap(),
            active: true,
        })
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut outcome = None;
        while Instant::now() < deadline && outcome.is_none() {
            if let Some(TransportEvent::PitchIntentOutcome {
                token,
                outcome: result,
            }) = room.poll_event()
            {
                outcome = Some((token, result));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let (token, result) = outcome.expect("inactive room must answer the host pitch edit");
        assert_eq!(token, 24);
        assert!(matches!(
            result,
            PitchIntentOutcome::Rejected(reason) if reason.contains("no active room")
        ));
        room.shutdown();
    }

    #[test]
    #[ignore = "requires live native Iroh networking"]
    fn two_native_room_legs_repair_and_exchange_shared_pitches_and_realtime() {
        let room = crate::generate_room_name();
        let mut left = NativeRoomTransport::spawn(NativeRoomConfig::new([31; 32])).unwrap();
        let mut right = NativeRoomTransport::spawn(NativeRoomConfig::new([32; 32])).unwrap();
        left.handle_command(BridgeCommand::SelectRoom(room.clone()))
            .unwrap();
        right
            .handle_command(BridgeCommand::SelectRoom(room))
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(30);
        let mut left_ready = false;
        let mut right_ready = false;
        let mut left_peer = false;
        let mut right_peer = false;
        let mut diagnostics = Vec::new();
        while Instant::now() < deadline && !(left_ready && right_ready && left_peer && right_peer) {
            while let Some(event) = left.poll_event() {
                left_ready |= event == TransportEvent::RoomLink(LinkState::Ready);
                left_peer |= matches!(event, TransportEvent::RoomPeers(peers) if peers > 0);
                if let TransportEvent::Diagnostic(message) = event {
                    diagnostics.push(format!("left: {message}"));
                }
            }
            while let Some(event) = right.poll_event() {
                right_ready |= event == TransportEvent::RoomLink(LinkState::Ready);
                right_peer |= matches!(event, TransportEvent::RoomPeers(peers) if peers > 0);
                if let TransportEvent::Diagnostic(message) = event {
                    diagnostics.push(format!("right: {message}"));
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            left_ready && right_ready,
            "both Iroh room legs must bind: {diagnostics:#?}"
        );
        assert!(
            left_peer && right_peer,
            "both room legs must observe a peer: {diagnostics:#?}"
        );

        // Let the routine Music and Extension repairs finish, then prove that
        // either peer can edit and remove the same durable shared member.
        let repair_deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < repair_deadline {
            while let Some(event) = left.poll_event() {
                if let TransportEvent::Diagnostic(message) = event {
                    diagnostics.push(format!("left: {message}"));
                }
            }
            while let Some(event) = right.poll_event() {
                if let TransportEvent::Diagnostic(message) = event {
                    diagnostics.push(format!("right: {message}"));
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !diagnostics.iter().any(|message| {
                message.contains("repair failed")
                    || message.contains("repair incomplete")
                    || message.contains("record was refused")
                    || message.contains("admission failed")
            }),
            "initial HHHS repair must complete cleanly: {diagnostics:#?}"
        );

        let pitch = BridgePitchState::pitch_for_midi(69).unwrap();
        let degree = BridgePitchState::shared_degree(pitch);
        left.handle_command(BridgeCommand::SetSharedPitch {
            token: 1,
            pitch,
            active: true,
        })
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut right_observed_add = false;
        while Instant::now() < deadline && !right_observed_add {
            while let Some(event) = left.poll_event() {
                if let TransportEvent::Diagnostic(message) = event {
                    diagnostics.push(format!("left: {message}"));
                }
            }
            while let Some(event) = right.poll_event() {
                match event {
                    TransportEvent::RoomPitchSet(shared) => {
                        right_observed_add = shared.pitch_classes.contains(&degree);
                    }
                    TransportEvent::Diagnostic(message) => {
                        diagnostics.push(format!("right: {message}"));
                    }
                    _ => {}
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            right_observed_add,
            "right room leg did not materialize the shared pitch: {diagnostics:#?}"
        );

        right
            .handle_command(BridgeCommand::SetSharedPitch {
                token: 1,
                pitch,
                active: false,
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut left_observed_remove = false;
        while Instant::now() < deadline && !left_observed_remove {
            while let Some(event) = left.poll_event() {
                match event {
                    TransportEvent::RoomPitchSet(shared) => {
                        left_observed_remove = !shared.pitch_classes.contains(&degree);
                    }
                    TransportEvent::Diagnostic(message) => {
                        diagnostics.push(format!("left: {message}"));
                    }
                    _ => {}
                }
            }
            while let Some(event) = right.poll_event() {
                if let TransportEvent::Diagnostic(message) = event {
                    diagnostics.push(format!("right: {message}"));
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            left_observed_remove,
            "left room leg did not materialize the cross-peer removal: {diagnostics:#?}"
        );

        let note = RealtimeMidi {
            timing: 0,
            voice_id: 11,
            channel: 2,
            note: 69,
            kind: RealtimeMidiKind::NoteOn,
            value: 0.8,
        };
        left.send_realtime(note).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut received = None;
        while Instant::now() < deadline && received.is_none() {
            while let Some(event) = right.poll_event() {
                if let TransportEvent::Midi(event) = event {
                    received = Some(event);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let received = received.expect("right room leg did not receive signed realtime");
        assert_eq!(received.voice_id, note.voice_id);
        assert_eq!(received.channel, note.channel);
        assert_eq!(received.note, note.note);
        assert_eq!(received.kind, note.kind);
        assert!((received.value - note.value).abs() < 0.001);

        // Leaving retracts this endpoint's room projection but does not tear
        // down any other carrier owned by the composite bridge.
        left.handle_command(BridgeCommand::LeaveRoom).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut left_offline = false;
        let mut left_cleared = false;
        while Instant::now() < deadline && !(left_offline && left_cleared) {
            while let Some(event) = left.poll_event() {
                left_offline |= event == TransportEvent::RoomLink(LinkState::Offline);
                left_cleared |= matches!(
                    event,
                    TransportEvent::RoomPitchSet(ref shared) if shared.is_empty()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(left_offline, "leaving must make the room leg offline");
        assert!(left_cleared, "leaving must retract the old room projection");

        left.shutdown();
        right.shutdown();
    }
}
