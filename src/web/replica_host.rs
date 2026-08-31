//! Capability-native in-page Room-v5 host.
//!
//! The window owns iroh/WebRTC, rendezvous, tasks, audio, and UI events. One
//! dedicated worker owns both HHHS lanes, materialization, and their existing
//! Room-v5 IndexedDB logs.

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
    time::Duration,
};

use futures::{
    SinkExt, StreamExt,
    channel::{mpsc, oneshot},
};
use wasm_bindgen_futures::spawn_local;

use crate::{
    client::{
        AppError, AppErrorCode, AppEvent, AppEventEnvelope, AppSnapshot, CLIENT_PROTOCOL_VERSION,
        Capabilities, ClientCommand, CommandAck, DiscoverySource, PeerPath, PeerSnapshot,
        PieceSnapshot, RealtimeMidiKind, RealtimeMidiSnapshot, VoiceSnapshot,
    },
    is_valid_room_name,
    net::{
        BrowserNetHandle, BrowserRoomInbound, BrowserRoomNetwork, IrohSyncStream,
        NativeNetworkEvent, NativeRoomTicketV5, ReplicaLiveRecord, ReplicaProtocol,
        ReplicaRepairHint, ReplicaRepairProbe, ReplicaRoomNetworkConfig, RoomRealtime, SyncStream,
        WalkieIdentity, is_routine_repair_initiator, spawn_rendezvous_v5,
    },
    room::session::{
        RoomSessionCompactTrace, RoomSessionEgress, RoomSessionIngress, RoomSessionProjectionGate,
        RoomSessionProjectionKind, RoomSessionRealtimeEgress, RoomSessionRenewalTrace,
        is_session_carrier,
    },
    room::v5::{
        ActorId, ExtensionCommand, MusicOp, ProtocolSupport, RoomCommand, RoomLane, RoomView,
        open_room_authority,
    },
    room::worker::{
        RoomWorkerOpen, RoomWorkerRepairOutcome, RoomWorkerRepairRequest, RoomWorkerRepairStatus,
        RoomWorkerResponse,
    },
    tuning::{TunedDegree, TunedPeriodicPitch},
};

use super::replica_worker::BrowserReplicaHandle;

/// End the current browser task before accepting more already-buffered work.
///
/// IndexedDB completion and WebRTC callbacks resume Rust futures on the main
/// thread. Without an explicit task boundary, the tail of one durable commit
/// can immediately drain the next queued command (or several inbound records),
/// and Chrome charges the combined synchronous work to one `complete` or timer
/// handler. Every command remains ordered and durable; this only gives input,
/// rendering, and audio callbacks a fair chance to run between records.
async fn yield_browser_task() {
    gloo_timers::future::TimeoutFuture::new(0).await;
}

fn browser_time_micros() -> Option<u64> {
    let performance = web_sys::window()?.performance()?;
    let milliseconds = performance.time_origin() + performance.now();
    milliseconds
        .is_finite()
        .then_some((milliseconds.max(0.0) * 1_000.0).round() as u64)
}

fn session_tracing_enabled() -> bool {
    browser_query_flag("sessionTrace")
}

#[cfg(feature = "browser-acceptance-faults")]
fn session_renewal_test_cut_enabled() -> bool {
    session_tracing_enabled() && browser_query_flag("renewalCut")
}

fn browser_query_flag(name: &str) -> bool {
    web_sys::window()
        .and_then(|window| window.location().search().ok())
        .is_some_and(|search| {
            search
                .trim_start_matches('?')
                .split('&')
                .any(|part| part == format!("{name}=1"))
        })
}

fn log_session_trace(stage: &str, trace: &RoomSessionCompactTrace) {
    if !session_tracing_enabled() {
        return;
    }
    let Ok(stage) = serde_json::to_string(stage) else {
        return;
    };
    let Ok(trace) = serde_json::to_string(trace) else {
        return;
    };
    web_sys::console::info_1(
        &format!(
            "[session_trace] {{\"stage\":{stage},\"atMicros\":{},\"trace\":{trace}}}",
            browser_time_micros().map_or_else(|| "null".to_owned(), |at| at.to_string())
        )
        .into(),
    );
}

fn log_session_renewal_trace(trace: &RoomSessionRenewalTrace) {
    if !session_tracing_enabled() {
        return;
    }
    let Ok(trace) = serde_json::to_string(trace) else {
        return;
    };
    web_sys::console::info_1(&format!("[session_renewal_trace] {trace}").into());
}

enum RoomControl {
    Commit {
        command: RoomCommand,
        response: oneshot::Sender<Result<CommandAck, AppError>>,
    },
    Presence {
        session: u64,
        pitch: Option<TunedPeriodicPitch>,
        response: oneshot::Sender<Result<CommandAck, AppError>>,
    },
    OutboundRecord(Vec<u8>),
    ResetSessionProjection,
    Shutdown {
        response: oneshot::Sender<()>,
    },
}

struct ActiveRoom {
    control: mpsc::Sender<RoomControl>,
    alive: Rc<Cell<bool>>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct RepairKey {
    peer: [u8; 32],
    lane: RoomLane,
}

#[derive(Default)]
struct RepairCoordinatorState {
    /// One running initiator per peer/lane. A concurrent trigger records one
    /// follow-up instead of allocating another HHHS worker session.
    active: BTreeMap<RepairKey, bool>,
    /// An inbound duplicate must not allocate another worker session for the
    /// same peer/lane. Initiator and responder ownership share the same key
    /// space: deterministic endpoint ordering means a healthy pair never needs
    /// both roles at once.
    responders: BTreeSet<RepairKey>,
}

#[derive(Clone, Default)]
struct RepairCoordinator(Rc<RefCell<RepairCoordinatorState>>);

impl RepairCoordinator {
    fn schedule(&self, peer: iroh::EndpointId, lane: RoomLane) -> bool {
        let key = RepairKey {
            peer: *peer.as_bytes(),
            lane,
        };
        let mut state = self.0.borrow_mut();
        if state.responders.contains(&key) {
            return false;
        }
        if let Some(pending) = state.active.get_mut(&key) {
            *pending = true;
            false
        } else {
            state.active.insert(key, false);
            true
        }
    }

    /// Finish the current batch. `true` consumes one coalesced trigger and
    /// keeps ownership of the slot for an immediate fresh attempt.
    fn continue_pending(&self, peer: iroh::EndpointId, lane: RoomLane) -> bool {
        let key = RepairKey {
            peer: *peer.as_bytes(),
            lane,
        };
        let mut state = self.0.borrow_mut();
        match state.active.get_mut(&key) {
            Some(pending) if *pending => {
                *pending = false;
                true
            }
            Some(_) => {
                state.active.remove(&key);
                false
            }
            None => false,
        }
    }

    fn finish(&self, peer: iroh::EndpointId, lane: RoomLane) {
        self.0.borrow_mut().active.remove(&RepairKey {
            peer: *peer.as_bytes(),
            lane,
        });
    }

    fn begin_responder(&self, peer: iroh::EndpointId, lane: RoomLane) -> bool {
        let key = RepairKey {
            peer: *peer.as_bytes(),
            lane,
        };
        let mut state = self.0.borrow_mut();
        if state.active.contains_key(&key) || state.responders.contains(&key) {
            return false;
        }
        state.responders.insert(key)
    }

    fn finish_responder(&self, peer: iroh::EndpointId, lane: RoomLane) {
        self.0.borrow_mut().responders.remove(&RepairKey {
            peer: *peer.as_bytes(),
            lane,
        });
    }
}

struct HostState {
    sequence: u64,
    snapshot: AppSnapshot,
    peer_sync: BTreeMap<ActorId, PeerSyncState>,
    subscribers: Vec<Rc<dyn Fn(AppEventEnvelope)>>,
    active_room: Option<ActiveRoom>,
}

#[derive(Clone, Copy, Default)]
struct PeerSyncState {
    required: u8,
    complete: u8,
}

pub struct BrowserHost {
    state: Rc<RefCell<HostState>>,
    identity: WalkieIdentity,
}

struct QueuedCommand {
    command: ClientCommand,
    on_error: Box<dyn FnOnce(String)>,
}

thread_local! {
    static COMMANDS: RefCell<Option<mpsc::UnboundedSender<QueuedCommand>>> = const { RefCell::new(None) };
}

fn browser_capabilities() -> Capabilities {
    Capabilities {
        protocol_version: CLIENT_PROTOCOL_VERSION,
        native_iroh: true,
        mdns: false,
        relay: true,
        native_midi: false,
        durable_storage: true,
    }
}

fn peer_override_seed() -> Option<[u8; 32]> {
    let search = web_sys::window()?.location().search().ok()?;
    let query = search.strip_prefix('?').unwrap_or(&search);
    query.split('&').find_map(|pair| {
        let value = pair.strip_prefix("peer=")?;
        (!value.is_empty()).then(|| *blake3::hash(value.as_bytes()).as_bytes())
    })
}

pub async fn init(on_event: impl Fn(AppEventEnvelope) + 'static) -> Result<(), String> {
    let seed = match peer_override_seed() {
        Some(seed) => seed,
        None => super::storage::get_or_create_identity_seed().await,
    };
    let identity = WalkieIdentity::from_seed(seed);
    let mut snapshot = AppSnapshot::empty(browser_capabilities());
    snapshot.local_actor = Some(identity.capability_actor_id());
    let host = Rc::new(BrowserHost {
        state: Rc::new(RefCell::new(HostState {
            sequence: 0,
            snapshot,
            peer_sync: BTreeMap::new(),
            subscribers: Vec::new(),
            active_room: None,
        })),
        identity,
    });
    host.register(Rc::new(on_event));
    let (commands, mut command_rx) = mpsc::unbounded::<QueuedCommand>();
    COMMANDS.with(|slot| *slot.borrow_mut() = Some(commands));
    spawn_local(async move {
        while let Some(queued) = command_rx.next().await {
            if let Err(error) = host.dispatch(queued.command).await {
                let detail = error
                    .detail
                    .as_deref()
                    .map(|detail| format!(" ({detail})"))
                    .unwrap_or_default();
                (queued.on_error)(format!("{}{detail}", error.message));
            }
            yield_browser_task().await;
        }
    });
    Ok(())
}

pub fn dispatch(command: ClientCommand, on_error: impl Fn(String) + 'static) {
    let queued = QueuedCommand {
        command,
        on_error: Box::new(on_error),
    };
    let result = COMMANDS.with(|slot| match slot.borrow().as_ref() {
        Some(commands) => commands
            .unbounded_send(queued)
            .map_err(|error| error.into_inner()),
        None => Err(queued),
    });
    if let Err(queued) = result {
        (queued.on_error)("browser networking is not initialized".to_owned());
    }
}

impl BrowserHost {
    fn register(&self, subscriber: Rc<dyn Fn(AppEventEnvelope)>) {
        let envelope = {
            let mut state = self.state.borrow_mut();
            state.sequence = state.sequence.saturating_add(1);
            let envelope = AppEventEnvelope {
                sequence: state.sequence,
                event: AppEvent::Snapshot {
                    snapshot: Box::new(state.snapshot.clone()),
                },
            };
            state.subscribers.push(subscriber.clone());
            envelope
        };
        subscriber(envelope);
    }

    fn emit(&self, event: AppEvent) {
        let (envelope, subscribers) = {
            let mut state = self.state.borrow_mut();
            state.sequence = state.sequence.saturating_add(1);
            (
                AppEventEnvelope {
                    sequence: state.sequence,
                    event,
                },
                state.subscribers.clone(),
            )
        };
        for subscriber in subscribers {
            subscriber(envelope.clone());
        }
    }

    fn emit_diagnostic(&self, code: &str, message: &str) {
        self.emit(AppEvent::Diagnostic {
            code: code.to_owned(),
            message: message.to_owned(),
        });
    }

    fn sequence(&self) -> u64 {
        self.state.borrow().sequence
    }

    async fn dispatch(self: &Rc<Self>, command: ClientCommand) -> Result<CommandAck, AppError> {
        match command {
            ClientCommand::EnterRoom { room_name } => self.enter_room(room_name).await,
            ClientCommand::JoinTicket { ticket } => self.join_ticket(ticket).await,
            ClientCommand::LeaveRoom => self.leave_room().await,
            ClientCommand::SetTuning { definition } => {
                definition.validate("room tuning").map_err(invalid_tuning)?;
                self.submit(MusicOp::SetTuning { definition }.into()).await
            }
            ClientCommand::AddDegree { pitch } => {
                self.validate_degree(pitch)?;
                self.submit(MusicOp::AddDegree { degree: pitch }.into())
                    .await
            }
            ClientCommand::RemoveDegree { pitch } => {
                self.validate_degree(pitch)?;
                self.submit(MusicOp::RemoveDegree { degree: pitch }.into())
                    .await
            }
            ClientCommand::SetRoundTable { config } => {
                let config = config.validate().map_err(|error| {
                    AppError::new(AppErrorCode::InvalidCommand, "invalid round-table config")
                        .with_detail(error.to_string())
                })?;
                self.submit(MusicOp::SetRoundTable { config }.into()).await
            }
            ClientCommand::AddPitch { pitch } => {
                self.validate_pitch(pitch)?;
                self.submit(MusicOp::AddPitch { pitch }.into()).await
            }
            ClientCommand::RemovePitch { pitch } => {
                self.validate_pitch(pitch)?;
                self.submit(MusicOp::RemovePitch { pitch }.into()).await
            }
            ClientCommand::PutPiece { emoji, pitch } => {
                self.validate_pitch(pitch)?;
                self.submit(ExtensionCommand::PutPiece { emoji, pitch }.into())
                    .await
            }
            ClientCommand::MovePiece { piece, pitch } => {
                self.validate_pitch(pitch)?;
                self.submit(ExtensionCommand::MovePiece { piece, pitch }.into())
                    .await
            }
            ClientCommand::RemovePiece { piece } => {
                self.submit(ExtensionCommand::RemovePiece { piece }.into())
                    .await
            }
            ClientCommand::SetRoomConfig {
                pieces_locked,
                available_emojis,
            } => {
                self.submit(
                    ExtensionCommand::SetConfig {
                        pieces_locked,
                        available_emojis,
                    }
                    .into(),
                )
                .await
            }
            ClientCommand::SetVoicePreview { session, pitch } => {
                if session == 0 {
                    return Err(AppError::new(
                        AppErrorCode::InvalidCommand,
                        "voice presence session must be non-zero",
                    ));
                }
                if let Some(pitch) = pitch {
                    self.validate_pitch(pitch)?;
                }
                self.submit_presence(session, pitch).await
            }
            ClientCommand::ListMidiPorts
            | ClientCommand::SelectMidiInput { .. }
            | ClientCommand::SelectMidiOutput { .. }
            | ClientCommand::PanicMidi => Ok(CommandAck {
                accepted_sequence: self.sequence(),
            }),
        }
    }

    async fn enter_room(self: &Rc<Self>, room_name: String) -> Result<CommandAck, AppError> {
        if !is_valid_room_name(&room_name) {
            return Err(AppError::new(
                AppErrorCode::InvalidRoom,
                "room names use the form adjective-noun-noun",
            ));
        }
        let authority = open_room_authority(&room_name);
        let owner = ActorId::from_signing_key(&authority);
        let config = ReplicaRoomNetworkConfig::create(&room_name, owner);
        self.start_room(
            Some(room_name),
            config,
            DiscoverySource::Gossip,
            Some(authority),
        )
        .await
    }

    async fn join_ticket(self: &Rc<Self>, encoded: String) -> Result<CommandAck, AppError> {
        let ticket = encoded.parse::<NativeRoomTicketV5>().map_err(|error| {
            AppError::new(AppErrorCode::InvalidTicket, "invalid Room-v5 ticket")
                .with_detail(error.to_string())
        })?;
        self.start_room(
            None,
            ReplicaRoomNetworkConfig::join(&ticket),
            DiscoverySource::Ticket,
            None,
        )
        .await
    }

    async fn start_room(
        self: &Rc<Self>,
        room_name: Option<String>,
        config: ReplicaRoomNetworkConfig,
        bootstrap_source: DiscoverySource,
        room_authority: Option<hhhs_proof::SigningKey>,
    ) -> Result<CommandAck, AppError> {
        self.stop_active_room().await;
        let local_actor = self.identity.capability_actor_id();
        if let Some(authority) = room_authority.as_ref() {
            if ActorId::from_signing_key(authority) != config.owner {
                return Err(AppError::new(
                    AppErrorCode::InvalidRoom,
                    "open-room authority does not match the room owner",
                ));
            }
        }

        let (control, control_rx) = mpsc::channel(64);
        let worker_control = Rc::new(RefCell::new(control.clone()));
        let session_gate = Rc::new(RefCell::new(RoomSessionProjectionGate::default()));
        let session_reset_outstanding = Rc::new(Cell::new(false));
        let projection_host = self.clone();
        let projection_gate = Rc::clone(&session_gate);
        let projection_reset_outstanding = Rc::clone(&session_reset_outstanding);
        let projection_reset_control = Rc::clone(&worker_control);
        let outbound_host = self.clone();
        let outbound = Rc::clone(&worker_control);
        let diagnostic_host = self.clone();
        let worker_open = RoomWorkerOpen::new(
            &config.room,
            config.owner,
            *self.identity.seed(),
            room_authority
                .as_ref()
                .map(|authority| authority.to_bytes()),
        )
        .with_session_trace(session_tracing_enabled());
        #[cfg(feature = "browser-acceptance-faults")]
        let worker_open =
            worker_open.with_session_renewal_test_cut(session_renewal_test_cut_enabled());
        let (worker, opened) = BrowserReplicaHandle::open(
            worker_open,
            move |projection| match projection_gate
                .borrow_mut()
                .canonical(projection.music_revision, projection.music_history_root)
            {
                Ok(true) => {
                    projection_host.apply_room_view(projection.view);
                }
                Ok(false) => {
                    projection_host.apply_non_music_room_view(projection.view);
                }
                Err(error) => {
                    projection_host.emit_diagnostic("canonical_projection_continuity", &error);
                    if !projection_reset_outstanding.replace(true)
                        && projection_reset_control
                            .borrow_mut()
                            .try_send(RoomControl::ResetSessionProjection)
                            .is_err()
                    {
                        projection_reset_outstanding.set(false);
                        projection_host.emit_diagnostic(
                            "session_projection_reset",
                            "worker control queue is full",
                        );
                    }
                }
            },
            move |record| {
                if outbound
                    .borrow_mut()
                    .try_send(RoomControl::OutboundRecord(record))
                    .is_err()
                {
                    outbound_host.emit_diagnostic(
                        "replica_worker_outbound",
                        "worker outbound-record queue is full",
                    );
                }
            },
            move |message| {
                diagnostic_host.emit_diagnostic("replica_worker", &message);
            },
        )
        .await
        .map_err(persistence_error)?;
        match opened {
            RoomWorkerResponse::Opened { actor, projection }
                if actor == local_actor
                    && worker
                        .projections()
                        .get_cloned()
                        .is_some_and(|current| current.value == projection)
                    && matches!(
                        worker.lifecycle().get_cloned(),
                        super::replica_worker::BrowserReplicaLifecycle::Ready { .. }
                    ) => {}
            RoomWorkerResponse::Opened { .. } => {
                worker.terminate();
                return Err(AppError::new(
                    AppErrorCode::Internal,
                    "Replica worker opened with the wrong local actor",
                ));
            }
            _ => unreachable!("BrowserReplicaHandle validates its Open response"),
        }

        let room_identity = config.room.clone();
        let topic = config.topic();
        let topic_string = topic.to_string();
        let bootstrap = config.bootstrap.as_ref().map(|address| address.id);
        let bootstrap_support = config.bootstrap_support;
        let network = BrowserRoomNetwork::bind(self.identity.iroh_secret(), config)
            .await
            .map_err(network_error)?;
        let handle = network.handle();
        let ticket = handle.settle_ticket(Duration::from_secs(5)).await;
        let ticket_string = ticket.to_string();

        let (rendezvous_guard, rendezvous_rx) = if bootstrap.is_none() {
            let (tx, rx) = mpsc::channel(64);
            let tx = Rc::new(RefCell::new(tx));
            let guard = spawn_rendezvous_v5(
                handle.rendezvous_peering(),
                topic,
                ProtocolSupport::WALKIE,
                move |peer, support| {
                    let _ = tx.borrow_mut().try_send((peer, support));
                },
            );
            (Some(guard), Some(rx))
        } else {
            (None, None)
        };

        let alive = Rc::new(Cell::new(true));
        let repairs = RepairCoordinator::default();
        let peers = Rc::new(RefCell::new(BTreeMap::<
            iroh::EndpointId,
            (DiscoverySource, PeerPath, ProtocolSupport),
        >::new()));
        if let Some(peer) = bootstrap {
            peers.borrow_mut().insert(
                peer,
                (
                    bootstrap_source,
                    PeerPath::Connecting,
                    bootstrap_support.unwrap_or(ProtocolSupport::WALKIE),
                ),
            );
            self.update_peer(
                peer,
                bootstrap_source,
                PeerPath::Connecting,
                bootstrap_support.unwrap_or(ProtocolSupport::WALKIE),
            );
        }
        spawn_room_loop(
            self.clone(),
            worker.clone(),
            network,
            handle.clone(),
            control_rx,
            rendezvous_rx,
            rendezvous_guard,
            peers.clone(),
            alive.clone(),
            room_identity,
            repairs.clone(),
            session_gate.clone(),
            session_reset_outstanding.clone(),
        );
        spawn_session_loop(
            self.clone(),
            worker.clone(),
            handle.clone(),
            alive.clone(),
            session_gate,
            session_reset_outstanding,
        );
        spawn_periodic_repair(self.clone(), worker, handle, peers, alive.clone());

        {
            let mut state = self.state.borrow_mut();
            state.active_room = Some(ActiveRoom { control, alive });
            state.snapshot.room_name = room_name.clone();
            state.snapshot.room_topic = Some(topic_string.clone());
            state.snapshot.room_ticket = Some(ticket_string.clone());
            state.snapshot.peers.clear();
            state.peer_sync.clear();
            state.snapshot.voices.clear();
        }
        self.emit(AppEvent::RoomChanged {
            room_name,
            room_topic: Some(topic_string),
            ticket: Some(ticket_string),
        });
        Ok(CommandAck {
            accepted_sequence: self.sequence(),
        })
    }

    async fn leave_room(self: &Rc<Self>) -> Result<CommandAck, AppError> {
        self.stop_active_room().await;
        {
            let mut state = self.state.borrow_mut();
            state.snapshot.room_name = None;
            state.snapshot.room_topic = None;
            state.snapshot.room_ticket = None;
            state.snapshot.shared_pitches = Default::default();
            state.snapshot.pieces.clear();
            state.snapshot.voices.clear();
            state.snapshot.peers.clear();
            state.peer_sync.clear();
        }
        self.emit(AppEvent::RoomChanged {
            room_name: None,
            room_topic: None,
            ticket: None,
        });
        Ok(CommandAck {
            accepted_sequence: self.sequence(),
        })
    }

    async fn stop_active_room(&self) {
        let active = self.state.borrow_mut().active_room.take();
        if let Some(mut active) = active {
            active.alive.set(false);
            let (response, closed) = oneshot::channel();
            if active
                .control
                .send(RoomControl::Shutdown { response })
                .await
                .is_ok()
            {
                let _ = closed.await;
            }
        }
    }

    async fn submit(&self, command: RoomCommand) -> Result<CommandAck, AppError> {
        let mut control = self
            .state
            .borrow()
            .active_room
            .as_ref()
            .map(|active| active.control.clone())
            .ok_or_else(not_in_room)?;
        let (tx, rx) = oneshot::channel();
        control
            .send(RoomControl::Commit {
                command,
                response: tx,
            })
            .await
            .map_err(|_| shutting_down())?;
        rx.await.map_err(|_| shutting_down())?
    }

    async fn submit_presence(
        &self,
        session: u64,
        pitch: Option<TunedPeriodicPitch>,
    ) -> Result<CommandAck, AppError> {
        let mut control = self
            .state
            .borrow()
            .active_room
            .as_ref()
            .map(|active| active.control.clone())
            .ok_or_else(not_in_room)?;
        let (tx, rx) = oneshot::channel();
        control
            .send(RoomControl::Presence {
                session,
                pitch,
                response: tx,
            })
            .await
            .map_err(|_| shutting_down())?;
        rx.await.map_err(|_| shutting_down())?
    }

    fn validate_degree(&self, degree: TunedDegree) -> Result<(), AppError> {
        let tuning = self
            .state
            .borrow()
            .snapshot
            .tuning
            .clone()
            .ok_or_else(not_in_room)?
            .validate("active tuning")
            .map_err(invalid_tuning)?;
        degree.validate(&tuning).map(|_| ()).map_err(invalid_tuning)
    }

    fn validate_pitch(&self, pitch: TunedPeriodicPitch) -> Result<(), AppError> {
        let tuning = self
            .state
            .borrow()
            .snapshot
            .tuning
            .clone()
            .ok_or_else(not_in_room)?
            .validate("active tuning")
            .map_err(invalid_tuning)?;
        pitch.validate(&tuning).map(|_| ()).map_err(invalid_tuning)
    }

    fn apply_room_view(&self, view: RoomView) -> u64 {
        self.apply_room_view_inner(view, true)
    }

    fn apply_non_music_room_view(&self, view: RoomView) -> u64 {
        self.apply_room_view_inner(view, false)
    }

    fn apply_room_view_inner(&self, view: RoomView, apply_shared_pitches: bool) -> u64 {
        let mut events = Vec::new();
        {
            let mut state = self.state.borrow_mut();
            if state.snapshot.tuning.as_ref() != Some(&view.music.tuning) {
                state.snapshot.tuning = Some(view.music.tuning.clone());
                state.snapshot.tuning_id = Some(view.music.tuning.id);
                events.push(AppEvent::TuningChanged {
                    definition: view.music.tuning.clone(),
                });
            }
            if state.snapshot.round_table != view.music.round_table {
                state.snapshot.round_table = view.music.round_table;
                events.push(AppEvent::RoundTableChanged {
                    config: view.music.round_table,
                });
            }
            if apply_shared_pitches && state.snapshot.shared_pitches != view.music.shared_pitches {
                state.snapshot.shared_pitches = view.music.shared_pitches.clone();
                events.push(AppEvent::PitchSetChanged {
                    shared: view.music.shared_pitches.clone(),
                });
            }

            let new_pieces: Vec<_> = view
                .pieces
                .iter()
                .map(|(id, piece)| PieceSnapshot {
                    id: *id,
                    owner: piece.owner,
                    emoji: piece.emoji.clone(),
                    pitch: piece.pitch,
                })
                .collect();
            for piece in &state.snapshot.pieces {
                if !view.pieces.contains_key(&piece.id) {
                    events.push(AppEvent::PieceRemoved { piece: piece.id });
                }
            }
            for piece in &new_pieces {
                if state.snapshot.pieces.iter().find(|old| old.id == piece.id) != Some(piece) {
                    events.push(AppEvent::PieceUpserted {
                        piece: piece.clone(),
                    });
                }
            }
            state.snapshot.pieces = new_pieces;
            if state.snapshot.pieces_locked != view.pieces_locked
                || state.snapshot.available_emojis != view.available_emojis
            {
                state.snapshot.pieces_locked = view.pieces_locked;
                state.snapshot.available_emojis = view.available_emojis.clone();
                events.push(AppEvent::RoomConfigChanged {
                    pieces_locked: view.pieces_locked,
                    available_emojis: view.available_emojis,
                });
            }
        }
        for event in events {
            self.emit(event);
        }
        self.sequence()
    }

    fn apply_session_pitch_view(&self, shared: tutti_music::SharedPitchSet) -> u64 {
        let changed = {
            let mut state = self.state.borrow_mut();
            if state.snapshot.shared_pitches == shared {
                false
            } else {
                state.snapshot.shared_pitches = shared.clone();
                true
            }
        };
        if changed {
            self.emit(AppEvent::PitchSetChanged { shared });
        }
        self.sequence()
    }

    fn apply_presence(
        &self,
        actor: ActorId,
        session: u64,
        sequence: u64,
        pitch: Option<TunedPeriodicPitch>,
    ) -> u64 {
        let voice = VoiceSnapshot {
            author: actor,
            session,
            sequence,
            pitch,
            expires_at_ms: now_ms().saturating_add(1_500),
        };
        {
            let mut state = self.state.borrow_mut();
            if let Some(existing) = state
                .snapshot
                .voices
                .iter_mut()
                .find(|voice| voice.author == actor && voice.session == session)
            {
                if sequence <= existing.sequence {
                    return state.sequence;
                }
                *existing = voice.clone();
            } else {
                state.snapshot.voices.push(voice.clone());
            }
        }
        self.emit(AppEvent::VoiceUpdated { voice });
        self.sequence()
    }

    fn update_peer(
        &self,
        endpoint: iroh::EndpointId,
        discovery: DiscoverySource,
        path: PeerPath,
        support: ProtocolSupport,
    ) {
        let actor = ActorId(*endpoint.as_bytes());
        let synchronized;
        {
            let mut state = self.state.borrow_mut();
            let lane_sync = state.peer_sync.entry(actor).or_default();
            lane_sync.required = support.bits();
            if path == PeerPath::Disconnected {
                lane_sync.complete = 0;
            }
            synchronized = lane_sync.required != 0
                && lane_sync.complete & lane_sync.required == lane_sync.required;
        }
        let peer = PeerSnapshot {
            author: actor,
            endpoint_id: endpoint.to_string(),
            path,
            discovery,
            round_trip_ms: None,
            synchronized,
        };
        {
            let mut state = self.state.borrow_mut();
            if let Some(existing) = state
                .snapshot
                .peers
                .iter_mut()
                .find(|existing| existing.author == actor)
            {
                *existing = peer.clone();
            } else {
                state.snapshot.peers.push(peer.clone());
                state.snapshot.peers.sort_by_key(|peer| peer.author);
            }
        }
        self.emit(AppEvent::PeerUpdated { peer });
    }

    fn mark_lane_synchronized(&self, endpoint: iroh::EndpointId, lane: RoomLane) {
        let actor = ActorId(*endpoint.as_bytes());
        let event = {
            let mut state = self.state.borrow_mut();
            let synchronized = {
                let lane_sync = state.peer_sync.entry(actor).or_insert(PeerSyncState {
                    required: lane.tag(),
                    complete: 0,
                });
                lane_sync.complete |= lane.tag();
                lane_sync.required != 0
                    && lane_sync.complete & lane_sync.required == lane_sync.required
            };
            let Some(peer) = state
                .snapshot
                .peers
                .iter_mut()
                .find(|peer| peer.author == actor)
            else {
                return;
            };
            peer.synchronized = synchronized;
            peer.clone()
        };
        self.emit(AppEvent::PeerUpdated { peer: event });
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_room_loop(
    host: Rc<BrowserHost>,
    worker: BrowserReplicaHandle,
    mut network: BrowserRoomNetwork,
    handle: BrowserNetHandle,
    mut control: mpsc::Receiver<RoomControl>,
    mut rendezvous: Option<mpsc::Receiver<(iroh::EndpointId, ProtocolSupport)>>,
    rendezvous_guard: Option<crate::net::RendezvousHandle>,
    peers: Rc<RefCell<BTreeMap<iroh::EndpointId, (DiscoverySource, PeerPath, ProtocolSupport)>>>,
    alive: Rc<Cell<bool>>,
    room_identity: crate::room::v5::RoomIdentity,
    repairs: RepairCoordinator,
    _session_gate: Rc<RefCell<RoomSessionProjectionGate>>,
    session_reset_outstanding: Rc<Cell<bool>>,
) {
    let local_actor = host.identity.capability_actor_id();
    spawn_local(async move {
        let _rendezvous_guard = rendezvous_guard;
        let mut presence_session = 0_u64;
        let mut presence_sequence = 0_u64;
        let mut realtime_replay = BTreeMap::<(crate::net::PeerId, u64), u64>::new();
        let mut shutdown_response = None;
        while alive.get() {
            let control_next = control.next();
            let inbound_next = network.next_inbound();
            let rendezvous_next = async {
                match rendezvous.as_mut() {
                    Some(rx) => rx.next().await,
                    None => std::future::pending().await,
                }
            };
            futures::pin_mut!(control_next, inbound_next, rendezvous_next);
            match futures::future::select(
                futures::future::select(control_next, inbound_next),
                rendezvous_next,
            )
            .await
            {
                futures::future::Either::Left((futures::future::Either::Left((control, _)), _)) => {
                    match control {
                        Some(RoomControl::Commit { command, response }) => {
                            let result = match command {
                                RoomCommand::Music(command) if is_session_pitch_edit(&command) => {
                                    worker
                                        .send_session(RoomSessionIngress::LocalPitchEdit {
                                            command,
                                            trace_token: None,
                                        })
                                        .await
                                        .map(|()| host.sequence())
                                }
                                command => worker.commit(command).await.map(|receipt| {
                                    let _ = (receipt.entry, receipt.projection_revision);
                                    host.sequence()
                                }),
                            }
                            .map_err(persistence_error);
                            let _ = response.send(
                                result.map(|accepted_sequence| CommandAck { accepted_sequence }),
                            );
                        }
                        Some(RoomControl::Presence {
                            session,
                            pitch,
                            response,
                        }) => {
                            if session == presence_session {
                                presence_sequence = presence_sequence.saturating_add(1);
                            } else {
                                presence_session = session;
                                presence_sequence = 0;
                            }
                            let wire = worker
                                .sign_presence(session, presence_sequence, pitch)
                                .await;
                            match wire {
                                Ok(wire) => {
                                    let accepted_sequence = host.apply_presence(
                                        local_actor,
                                        session,
                                        presence_sequence,
                                        pitch,
                                    );
                                    if let Err(error) = handle.broadcast(wire).await {
                                        host.emit_diagnostic(
                                            "presence_broadcast",
                                            &format!(
                                                "local presence applied; broadcast failed: {error}"
                                            ),
                                        );
                                    }
                                    let _ = response.send(Ok(CommandAck { accepted_sequence }));
                                }
                                Err(error) => {
                                    let _ = response.send(Err(persistence_error(error)));
                                }
                            }
                        }
                        Some(RoomControl::OutboundRecord(bytes)) => {
                            let live = ReplicaLiveRecord::decode(&bytes);
                            let repair = live.as_ref().map(|live| ReplicaRepairHint {
                                lane: live.lane,
                                source: live.source,
                                entry: live.record.entry_hash(),
                            });
                            if let Some(live) = live.as_ref() {
                                web_sys::console::debug_1(
                                    &format!(
                                        "[replica_live] outbound lane={:?} entry={:?}",
                                        live.lane,
                                        live.record.entry_hash()
                                    )
                                    .into(),
                                );
                            }
                            if let Err(error) = handle.broadcast(bytes).await {
                                host.emit_diagnostic(
                                    "live_record_broadcast",
                                    &format!("durable record broadcast failed: {error}"),
                                );
                                if let Some(hint) = repair
                                    && let Err(error) = handle.broadcast(hint.encode()).await
                                {
                                    host.emit_diagnostic(
                                        "repair_hint_broadcast",
                                        &format!(
                                            "durable record broadcast failed; repair hint also failed: {error}"
                                        ),
                                    );
                                }
                            }
                        }
                        Some(RoomControl::ResetSessionProjection) => {
                            match worker.reset_session_projection().await {
                                Ok(true) => {
                                    // Keep coalescing until the Reset event is accepted.
                                }
                                Ok(false) => {
                                    session_reset_outstanding.set(false);
                                    host.emit_diagnostic(
                                        "session_projection_reset",
                                        "no active presentation session was available to reset",
                                    );
                                }
                                Err(error) => {
                                    session_reset_outstanding.set(false);
                                    host.emit_diagnostic("session_projection_reset", &error);
                                }
                            }
                        }
                        Some(RoomControl::Shutdown { response }) => {
                            shutdown_response = Some(response);
                            break;
                        }
                        None => break,
                    }
                }
                futures::future::Either::Left((
                    futures::future::Either::Right((inbound, _)),
                    _,
                )) => {
                    let Some(inbound) = inbound else { break };
                    match inbound {
                        BrowserRoomInbound::Repair(repair) => {
                            let Some(ReplicaProtocol::Repair(lane)) =
                                ReplicaProtocol::from_alpn(repair.alpn)
                            else {
                                repair
                                    .connection
                                    .close(4u32.into(), b"unsupported Room-v5 ALPN");
                                continue;
                            };
                            spawn_repair_responder(
                                host.clone(),
                                worker.clone(),
                                alive.clone(),
                                repair.endpoint_id,
                                lane,
                                repair.stream.owning(repair.connection),
                                repairs.clone(),
                            );
                        }
                        BrowserRoomInbound::Event(event) => match event {
                            NativeNetworkEvent::NeighborUp {
                                endpoint_id,
                                discovery,
                            } => {
                                let local = crate::net::PeerId(*handle.endpoint_id().as_bytes());
                                let remote = crate::net::PeerId(*endpoint_id.as_bytes());
                                let support = peers
                                    .borrow()
                                    .get(&endpoint_id)
                                    .map(|(_, _, support)| *support)
                                    .unwrap_or(ProtocolSupport::WALKIE);
                                let path = map_path(handle.peer_path(endpoint_id).await);
                                peers
                                    .borrow_mut()
                                    .insert(endpoint_id, (discovery, path, support));
                                host.update_peer(endpoint_id, discovery, path, support);
                                if let Err(error) =
                                    worker.grant_peer(ActorId(*endpoint_id.as_bytes())).await
                                {
                                    host.emit_diagnostic("capability_grant", &error);
                                }
                                if is_routine_repair_initiator(local, remote) {
                                    for lane in [RoomLane::Music, RoomLane::Extension] {
                                        if support.supports(lane) {
                                            spawn_repair_initiator(
                                                host.clone(),
                                                worker.clone(),
                                                handle.clone(),
                                                alive.clone(),
                                                endpoint_id,
                                                lane,
                                                repairs.clone(),
                                            );
                                        }
                                    }
                                }
                            }
                            NativeNetworkEvent::NeighborDown { endpoint_id } => {
                                if let Some((source, path, support)) =
                                    peers.borrow_mut().get_mut(&endpoint_id)
                                {
                                    *path = PeerPath::Disconnected;
                                    host.update_peer(
                                        endpoint_id,
                                        *source,
                                        PeerPath::Disconnected,
                                        *support,
                                    );
                                }
                            }
                            NativeNetworkEvent::Message { bytes, .. } => {
                                if is_session_carrier(&bytes) {
                                    if let Err(error) = worker
                                        .send_session(RoomSessionIngress::Carrier {
                                            bytes,
                                            received_at_micros: session_tracing_enabled()
                                                .then(browser_time_micros)
                                                .flatten(),
                                        })
                                        .await
                                    {
                                        host.emit_diagnostic("session_carrier_ingress", &error);
                                    }
                                } else if let Ok(realtime) =
                                    RoomRealtime::decode(&room_identity, &bytes)
                                {
                                    let local =
                                        crate::net::PeerId(*handle.endpoint_id().as_bytes());
                                    let replay_key = (realtime.source, realtime.session);
                                    let fresh = realtime.source != local
                                        && realtime_replay
                                            .get(&replay_key)
                                            .map_or(true, |last| realtime.sequence > *last);
                                    if fresh {
                                        const MAX_REALTIME_SESSIONS: usize = 128;
                                        if realtime_replay.len() == MAX_REALTIME_SESSIONS
                                            && !realtime_replay.contains_key(&replay_key)
                                            && let Some(oldest) =
                                                realtime_replay.keys().next().copied()
                                        {
                                            realtime_replay.remove(&oldest);
                                        }
                                        realtime_replay.insert(replay_key, realtime.sequence);
                                        if let tutti_realtime::Frame::Midi(frame) = realtime.frame {
                                            let kind = match frame.kind {
                                                tutti_realtime::MidiKind::NoteOn => {
                                                    RealtimeMidiKind::NoteOn
                                                }
                                                tutti_realtime::MidiKind::NoteOff => {
                                                    RealtimeMidiKind::NoteOff
                                                }
                                                tutti_realtime::MidiKind::Choke => {
                                                    RealtimeMidiKind::Choke
                                                }
                                                tutti_realtime::MidiKind::PolyPressure => {
                                                    RealtimeMidiKind::PolyPressure
                                                }
                                                tutti_realtime::MidiKind::PitchBend => {
                                                    RealtimeMidiKind::PitchBend
                                                }
                                                tutti_realtime::MidiKind::ChannelPressure => {
                                                    RealtimeMidiKind::ChannelPressure
                                                }
                                            };
                                            host.emit(AppEvent::RealtimeMidi {
                                                midi: RealtimeMidiSnapshot {
                                                    source: ActorId(*realtime.source.as_bytes()),
                                                    session: realtime.session,
                                                    sequence: realtime.sequence,
                                                    voice_id: frame.voice_id,
                                                    channel: frame.channel,
                                                    note: frame.note,
                                                    kind,
                                                    value: frame.value,
                                                },
                                            });
                                        }
                                    }
                                } else if let Some(live) = ReplicaLiveRecord::decode(&bytes) {
                                    let local =
                                        crate::net::PeerId(*handle.endpoint_id().as_bytes());
                                    if live.source != local {
                                        let source = live.source;
                                        let lane = live.lane;
                                        let entry = live.record.entry_hash();
                                        let accepted =
                                            match worker.inbound_record(bytes.clone()).await {
                                                Ok(accepted) => accepted,
                                                Err(error) => {
                                                    host.emit_diagnostic(
                                                        "live_record_admission",
                                                        &error,
                                                    );
                                                    false
                                                }
                                            };
                                        web_sys::console::debug_1(
                                            &format!(
                                                "[replica_live] inbound lane={lane:?} entry={entry:?} accepted={accepted}"
                                            )
                                            .into(),
                                        );
                                        if !accepted {
                                            request_repair_after_live_failure(
                                                host.clone(),
                                                worker.clone(),
                                                handle.clone(),
                                                alive.clone(),
                                                source,
                                                lane,
                                                entry,
                                                repairs.clone(),
                                            );
                                        }
                                    }
                                } else if let Some(hint) = ReplicaRepairHint::decode(&bytes)
                                    && is_routine_repair_initiator(
                                        crate::net::PeerId(*handle.endpoint_id().as_bytes()),
                                        hint.source,
                                    )
                                    && let Ok(source) =
                                        iroh::EndpointId::from_bytes(hint.source.as_bytes())
                                {
                                    spawn_repair_initiator(
                                        host.clone(),
                                        worker.clone(),
                                        handle.clone(),
                                        alive.clone(),
                                        source,
                                        hint.lane,
                                        repairs.clone(),
                                    );
                                } else if let Some(probe) = ReplicaRepairProbe::decode(&bytes) {
                                    let local =
                                        crate::net::PeerId(*handle.endpoint_id().as_bytes());
                                    let frontier =
                                        worker.projection().map(|projection| match probe.lane {
                                            RoomLane::Music => projection.music_frontier,
                                            RoomLane::Extension => projection.extension_frontier,
                                        });
                                    if probe.source != local
                                        && is_routine_repair_initiator(local, probe.source)
                                        && frontier != Some(*probe.frontier.as_bytes())
                                        && let Ok(source) =
                                            iroh::EndpointId::from_bytes(probe.source.as_bytes())
                                    {
                                        spawn_repair_initiator(
                                            host.clone(),
                                            worker.clone(),
                                            handle.clone(),
                                            alive.clone(),
                                            source,
                                            probe.lane,
                                            repairs.clone(),
                                        );
                                    }
                                } else if let Ok(presence) = worker.verify_presence(bytes).await {
                                    host.apply_presence(
                                        presence.actor,
                                        presence.session,
                                        presence.sequence,
                                        presence.pitch,
                                    );
                                }
                            }
                            NativeNetworkEvent::Lagged => {
                                let local = crate::net::PeerId(*handle.endpoint_id().as_bytes());
                                for (peer, (_, path, support)) in peers.borrow().iter() {
                                    let remote = crate::net::PeerId(*peer.as_bytes());
                                    if *path == PeerPath::Disconnected
                                        || !is_routine_repair_initiator(local, remote)
                                    {
                                        continue;
                                    }
                                    for lane in [RoomLane::Music, RoomLane::Extension] {
                                        if support.supports(lane) {
                                            spawn_repair_initiator(
                                                host.clone(),
                                                worker.clone(),
                                                handle.clone(),
                                                alive.clone(),
                                                *peer,
                                                lane,
                                                repairs.clone(),
                                            );
                                        }
                                    }
                                }
                            }
                            NativeNetworkEvent::Diagnostic(message) => {
                                host.emit_diagnostic("browser_network", &message)
                            }
                            NativeNetworkEvent::Closed => break,
                            NativeNetworkEvent::MdnsDiscovered { .. }
                            | NativeNetworkEvent::MdnsExpired { .. } => {}
                        },
                    }
                }
                futures::future::Either::Right((discovered, _)) => match discovered {
                    Some((peer, support)) => {
                        peers.borrow_mut().entry(peer).or_insert((
                            DiscoverySource::AddressLookup,
                            PeerPath::Connecting,
                            support,
                        ));
                        host.update_peer(
                            peer,
                            DiscoverySource::AddressLookup,
                            PeerPath::Connecting,
                            support,
                        );
                    }
                    None => rendezvous = None,
                },
            }
            yield_browser_task().await;
        }
        alive.set(false);
        let _ = network.shutdown().await;
        if let Err(error) = worker.close().await {
            host.emit_diagnostic("replica_worker_close", &error);
        }
        if let Some(response) = shutdown_response {
            let _ = response.send(());
        }
    });
}

fn is_session_pitch_edit(command: &MusicOp) -> bool {
    matches!(
        command,
        MusicOp::AddDegree { .. }
            | MusicOp::RemoveDegree { .. }
            | MusicOp::AddPitch { .. }
            | MusicOp::RemovePitch { .. }
    )
}

fn spawn_session_loop(
    host: Rc<BrowserHost>,
    worker: BrowserReplicaHandle,
    handle: BrowserNetHandle,
    alive: Rc<Cell<bool>>,
    gate: Rc<RefCell<RoomSessionProjectionGate>>,
    reset_outstanding: Rc<Cell<bool>>,
) {
    let draining = Rc::new(Cell::new(false));
    spawn_local(async move {
        while alive.get() {
            let Some(event) = worker.next_session_event().await else {
                break;
            };
            match event {
                RoomSessionEgress::Carrier(carrier) => {
                    let host = host.clone();
                    let handle = handle.clone();
                    spawn_local(async move {
                        if let Err(error) = handle.broadcast(carrier).await {
                            host.emit_diagnostic(
                                "session_carrier_broadcast",
                                &format!("session establishment broadcast failed: {error}"),
                            );
                        }
                    });
                }
                RoomSessionEgress::Realtime(RoomSessionRealtimeEgress {
                    projection,
                    carrier,
                    durable,
                    trace,
                }) => {
                    match gate.borrow_mut().accept(&projection) {
                        Ok(true) => {
                            if let Some(trace) = trace.as_ref() {
                                log_session_trace("projection_gate_accepted", trace);
                            }
                            if projection.kind == RoomSessionProjectionKind::Reset {
                                reset_outstanding.set(false);
                            }
                            host.apply_session_pitch_view(projection.view);
                            if let Some(trace) = trace.as_ref() {
                                log_session_trace("signal_applied", trace);
                            }
                        }
                        Ok(false) => {}
                        Err(error) => {
                            host.emit_diagnostic("session_projection_continuity", &error);
                            if !reset_outstanding.replace(true) {
                                let host = host.clone();
                                let worker = worker.clone();
                                let reset_outstanding = reset_outstanding.clone();
                                spawn_local(async move {
                                    match worker.reset_session_projection().await {
                                        Ok(true) => {
                                            // Keep coalescing requests until the accepted Reset
                                            // snapshot itself crosses the sideband.
                                        }
                                        Ok(false) => {
                                            reset_outstanding.set(false);
                                            host.emit_diagnostic(
                                                "session_projection_reset",
                                                "no active presentation session was available to reset",
                                            );
                                        }
                                        Err(error) => {
                                            reset_outstanding.set(false);
                                            host.emit_diagnostic(
                                                "session_projection_reset",
                                                &error,
                                            );
                                        }
                                    }
                                });
                            }
                        }
                    }

                    if let Some(carrier) = carrier {
                        let host = host.clone();
                        let handle = handle.clone();
                        let trace = trace.clone();
                        spawn_local(async move {
                            if let Some(trace) = trace.as_ref() {
                                log_session_trace("carrier_broadcast_call_started", trace);
                            }
                            if let Err(error) = handle.broadcast(carrier).await {
                                host.emit_diagnostic(
                                    "session_carrier_broadcast",
                                    &format!("session event broadcast failed: {error}"),
                                );
                            } else if let Some(trace) = trace.as_ref() {
                                log_session_trace("carrier_broadcast_call_completed", trace);
                            }
                        });
                    }

                    if !durable.is_empty() && !draining.replace(true) {
                        let host = host.clone();
                        let worker = worker.clone();
                        let draining = draining.clone();
                        spawn_local(async move {
                            loop {
                                match worker.drain_session().await {
                                    Ok(true) => {}
                                    Ok(false) => break,
                                    Err(error) => {
                                        host.emit_diagnostic("session_reification", &error);
                                        break;
                                    }
                                }
                            }
                            draining.set(false);
                        });
                    }
                }
                RoomSessionEgress::FallbackDurable(command) => {
                    let host = host.clone();
                    let worker = worker.clone();
                    spawn_local(async move {
                        if let Err(error) = worker.commit(RoomCommand::Music(command)).await {
                            host.emit_diagnostic("session_durable_fallback", &error);
                        }
                    });
                }
                RoomSessionEgress::Diagnostic(message) => {
                    host.emit_diagnostic("hhhs_session", &message);
                }
                RoomSessionEgress::RenewalTrace(trace) => {
                    log_session_renewal_trace(&trace);
                }
            }
        }
    });
}

fn spawn_periodic_repair(
    host: Rc<BrowserHost>,
    worker: BrowserReplicaHandle,
    handle: BrowserNetHandle,
    peers: Rc<RefCell<BTreeMap<iroh::EndpointId, (DiscoverySource, PeerPath, ProtocolSupport)>>>,
    alive: Rc<Cell<bool>>,
) {
    spawn_local(async move {
        let local = crate::net::PeerId(*handle.endpoint_id().as_bytes());
        while alive.get() {
            n0_future::time::sleep(Duration::from_secs(27)).await;
            if !alive.get() {
                break;
            }
            let supported = {
                let peers = peers.borrow();
                [RoomLane::Music, RoomLane::Extension].map(|lane| {
                    peers.values().any(|(_, path, support)| {
                        *path != PeerPath::Disconnected && support.supports(lane)
                    })
                })
            };
            for (lane, supported) in [RoomLane::Music, RoomLane::Extension]
                .into_iter()
                .zip(supported)
            {
                if !supported {
                    continue;
                }
                let Some(frontier) = worker.projection().map(|projection| match lane {
                    RoomLane::Music => hhhs::Digest(projection.music_frontier),
                    RoomLane::Extension => hhhs::Digest(projection.extension_frontier),
                }) else {
                    host.emit_diagnostic(
                        "replica_repair_probe",
                        "Replica worker has no current projection frontier",
                    );
                    continue;
                };
                if let Err(error) = handle
                    .broadcast(
                        ReplicaRepairProbe {
                            lane,
                            source: local,
                            frontier,
                        }
                        .encode(),
                    )
                    .await
                {
                    host.emit_diagnostic(
                        "replica_repair_probe",
                        &format!("periodic {lane:?} probe failed: {error}"),
                    );
                }
            }
        }
    });
}

fn spawn_repair_initiator(
    host: Rc<BrowserHost>,
    worker: BrowserReplicaHandle,
    handle: BrowserNetHandle,
    alive: Rc<Cell<bool>>,
    peer: iroh::EndpointId,
    lane: RoomLane,
    repairs: RepairCoordinator,
) {
    if !repairs.schedule(peer, lane) {
        return;
    }
    spawn_local(async move {
        const DIVERGENT_BACKOFF_MS: [u64; 3] = [100, 300, 900];
        loop {
            let mut retry = 0;
            let mut completed = false;
            while alive.get() {
                let (session, result) =
                    run_initiator_repair_attempt(&worker, &handle, &alive, peer, lane).await;
                let retry_fresh = matches!(
                    &result,
                    Ok((
                        RoomWorkerRepairStatus::Divergent | RoomWorkerRepairStatus::RetryFresh(_),
                        _
                    ))
                );
                completed = matches!(&result, Ok((RoomWorkerRepairStatus::Complete, _)));
                if completed
                    && lane == RoomLane::Music
                    && let Err(error) = worker.start_session_peer(ActorId(*peer.as_bytes())).await
                {
                    host.emit_diagnostic("session_establishment", &error);
                }
                report_repair(
                    host.clone(),
                    &alive,
                    peer,
                    lane,
                    "initiator",
                    session,
                    result,
                );
                if !retry_fresh || retry == DIVERGENT_BACKOFF_MS.len() {
                    break;
                }
                let delay_ms = DIVERGENT_BACKOFF_MS[retry];
                host.emit_diagnostic(
                    "replica_repair_retry",
                    &format!(
                        "{lane:?} repair with {peer} attempt={session} requires a fresh cut; scheduling another attempt after {delay_ms}ms"
                    ),
                );
                n0_future::time::sleep(Duration::from_millis(delay_ms)).await;
                retry += 1;
            }

            if !alive.get() || !completed {
                repairs.finish(peer, lane);
                break;
            }
            if !repairs.continue_pending(peer, lane) {
                break;
            }
        }
    });
}

async fn run_initiator_repair_attempt(
    worker: &BrowserReplicaHandle,
    handle: &BrowserNetHandle,
    alive: &Cell<bool>,
    peer: iroh::EndpointId,
    lane: RoomLane,
) -> (
    u64,
    Result<(RoomWorkerRepairStatus, RoomWorkerRepairOutcome), String>,
) {
    let connection = match handle.begin_replica(peer, lane).await {
        Ok(connection) => connection,
        Err(error) => {
            return (0, Err(format!("dial: {error}")));
        }
    };
    if !alive.get() {
        connection.close(0u32.into(), b"room closed");
        return (0, Err("cancelled because the room closed".into()));
    }
    let stream = match IrohSyncStream::open(&connection).await {
        Ok(stream) => stream.owning(connection),
        Err(error) => {
            return (0, Err(format!("stream: {error}")));
        }
    };
    drive_worker_repair(stream, worker, lane, true).await
}

fn spawn_repair_responder(
    host: Rc<BrowserHost>,
    worker: BrowserReplicaHandle,
    alive: Rc<Cell<bool>>,
    peer: iroh::EndpointId,
    lane: RoomLane,
    stream: IrohSyncStream,
    repairs: RepairCoordinator,
) {
    if !repairs.begin_responder(peer, lane) {
        let close_host = host.clone();
        spawn_local(async move {
            if let Err(error) = stream.close().await {
                close_host.emit_diagnostic(
                    "replica_repair_duplicate_close",
                    &format!(
                        "{lane:?} duplicate responder stream with {peer} failed to close: {error}"
                    ),
                );
            }
        });
        return;
    }
    spawn_local(async move {
        if !alive.get() {
            repairs.finish_responder(peer, lane);
            if let Err(error) = stream.close().await {
                host.emit_diagnostic(
                    "replica_repair_cancel_close",
                    &format!("{lane:?} responder stream with {peer} failed to close: {error}"),
                );
            }
            return;
        }
        run_repair(host, worker, &alive, peer, lane, stream, false).await;
        repairs.finish_responder(peer, lane);
    });
}

fn request_repair_after_live_failure(
    host: Rc<BrowserHost>,
    worker: BrowserReplicaHandle,
    handle: BrowserNetHandle,
    alive: Rc<Cell<bool>>,
    source: crate::net::PeerId,
    lane: RoomLane,
    entry: hhhs::EntryHash,
    repairs: RepairCoordinator,
) {
    let local = crate::net::PeerId(*handle.endpoint_id().as_bytes());
    let Ok(source_endpoint) = iroh::EndpointId::from_bytes(source.as_bytes()) else {
        return;
    };
    if is_routine_repair_initiator(local, source) {
        spawn_repair_initiator(host, worker, handle, alive, source_endpoint, lane, repairs);
        return;
    }
    spawn_local(async move {
        if !alive.get() {
            return;
        }
        if let Err(error) = handle
            .broadcast(
                ReplicaRepairHint {
                    lane,
                    source: local,
                    entry,
                }
                .encode(),
            )
            .await
        {
            host.emit_diagnostic(
                "repair_hint_broadcast",
                &format!("live delivery needs repair; hint failed: {error}"),
            );
        }
    });
}

async fn run_repair(
    host: Rc<BrowserHost>,
    worker: BrowserReplicaHandle,
    alive: &Cell<bool>,
    peer: iroh::EndpointId,
    lane: RoomLane,
    stream: IrohSyncStream,
    initiator: bool,
) {
    let (session, result) = drive_worker_repair(stream, &worker, lane, initiator).await;
    if lane == RoomLane::Music
        && matches!(&result, Ok((RoomWorkerRepairStatus::Complete, _)))
        && let Err(error) = worker.start_session_peer(ActorId(*peer.as_bytes())).await
    {
        host.emit_diagnostic("session_establishment", &error);
    }
    report_repair(
        host,
        alive,
        peer,
        lane,
        if initiator { "initiator" } else { "responder" },
        session,
        result,
    );
}

fn report_repair(
    host: Rc<BrowserHost>,
    alive: &Cell<bool>,
    peer: iroh::EndpointId,
    lane: RoomLane,
    role: &'static str,
    session: u64,
    result: Result<(RoomWorkerRepairStatus, RoomWorkerRepairOutcome), String>,
) {
    if !alive.get() {
        return;
    }
    match result {
        Ok((RoomWorkerRepairStatus::Complete, outcome)) => {
            web_sys::console::info_1(
                &format!(
                    "[replica_repair_complete] lane={lane:?} peer={peer} attempt={session} role={role} outcome={outcome:?}"
                )
                .into(),
            );
            host.mark_lane_synchronized(peer, lane);
        }
        Ok((status, outcome)) => {
            host.emit_diagnostic(
                "replica_repair_incomplete",
                &format!(
                    "{lane:?} repair with {peer} attempt={session} role={role} ended {status:?}: {outcome:?}"
                ),
            );
        }
        Err(error) => host.emit_diagnostic(
            "replica_repair",
            &format!("{lane:?} repair with {peer} attempt={session} role={role} failed: {error}"),
        ),
    }
}

async fn drive_worker_repair(
    mut stream: IrohSyncStream,
    worker: &BrowserReplicaHandle,
    lane: RoomLane,
    initiator: bool,
) -> (
    u64,
    Result<(RoomWorkerRepairStatus, RoomWorkerRepairOutcome), String>,
) {
    let _permit = worker.repair_permit().await;
    let session = worker.next_repair_session();
    let result = drive_worker_repair_inner(&mut stream, worker, session, lane, initiator).await;
    let close = stream.close().await.map_err(|error| error.to_string());
    let close_error = close.as_ref().err().cloned();
    let finish = worker
        .repair(RoomWorkerRepairRequest::Finish {
            session,
            close_error,
        })
        .await;
    let result = match (result, close, finish) {
        (Ok(()), Ok(()), Ok(step)) => Ok((step.status, step.outcome)),
        (Err(repair), Err(close), _) => Err(format!(
            "repair failed ({repair}) and its stream also failed to close ({close})"
        )),
        (Err(repair), _, _) => Err(repair),
        (_, Err(close), _) => Err(format!("repair stream failed to close: {close}")),
        (_, _, Err(finish)) => Err(format!("repair close confirmation failed: {finish}")),
    };
    (session, result)
}

async fn drive_worker_repair_inner(
    stream: &mut IrohSyncStream,
    worker: &BrowserReplicaHandle,
    session: u64,
    lane: RoomLane,
    initiator: bool,
) -> Result<(), String> {
    let step = if initiator {
        worker
            .repair_with_stream(
                RoomWorkerRepairRequest::StartInitiator { session, lane },
                stream,
            )
            .await?
    } else {
        let hello = receive_repair_frame(stream).await?;
        worker
            .repair_with_stream(
                RoomWorkerRepairRequest::StartResponder {
                    session,
                    lane,
                    hello,
                },
                stream,
            )
            .await?
    };

    pump_worker_repair(stream, worker, session, step).await
}

async fn pump_worker_repair(
    stream: &mut IrohSyncStream,
    worker: &BrowserReplicaHandle,
    session: u64,
    mut step: crate::room::worker::RoomWorkerRepairStep,
) -> Result<(), String> {
    loop {
        if step.status != RoomWorkerRepairStatus::Exchanging {
            return Ok(());
        }
        let frame = receive_repair_frame(stream).await?;
        step = worker
            .repair_with_stream(RoomWorkerRepairRequest::Frame { session, frame }, stream)
            .await?;
    }
}

async fn receive_repair_frame(stream: &mut IrohSyncStream) -> Result<Vec<u8>, String> {
    let limits = hhhs_sync::SessionLimits::default();
    let receive = stream.recv_frame();
    let timeout = n0_future::time::sleep(limits.recv_timeout);
    futures::pin_mut!(receive, timeout);
    match futures::future::select(receive, timeout).await {
        futures::future::Either::Left((frame, _)) => {
            let frame = frame.map_err(|error| error.to_string())?.ok_or_else(|| {
                "repair stream closed before the worker session finished".to_owned()
            })?;
            if frame.len() > limits.max_frame_bytes {
                return Err(format!(
                    "repair frame is {} bytes; maximum is {}",
                    frame.len(),
                    limits.max_frame_bytes
                ));
            }
            Ok(frame)
        }
        futures::future::Either::Right(((), _)) => {
            Err("repair stream timed out waiting for the next frame".into())
        }
    }
}

fn map_path(path: crate::net::PeerTransportPath) -> PeerPath {
    match path {
        crate::net::PeerTransportPath::Connecting => PeerPath::Connecting,
        crate::net::PeerTransportPath::Direct => PeerPath::Direct,
        crate::net::PeerTransportPath::Relayed => PeerPath::Relayed,
        crate::net::PeerTransportPath::Disconnected => PeerPath::Disconnected,
    }
}

fn invalid_tuning(error: impl std::fmt::Display) -> AppError {
    AppError::new(AppErrorCode::InvalidTuning, "invalid tuning").with_detail(error.to_string())
}

fn persistence_error(error: impl std::fmt::Display) -> AppError {
    AppError::new(AppErrorCode::Persistence, "durable Room-v5 storage failed")
        .with_detail(error.to_string())
}

fn network_error(error: impl std::fmt::Display) -> AppError {
    AppError::new(
        AppErrorCode::NetworkUnavailable,
        "could not start browser Iroh room",
    )
    .with_detail(error.to_string())
}

fn not_in_room() -> AppError {
    AppError::new(
        AppErrorCode::NetworkUnavailable,
        "enter a room before changing shared state",
    )
}

fn shutting_down() -> AppError {
    AppError::new(
        AppErrorCode::ShuttingDown,
        "the active Room-v5 task is shutting down",
    )
}

fn now_ms() -> u64 {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}
