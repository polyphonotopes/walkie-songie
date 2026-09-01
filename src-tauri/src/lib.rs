use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use hhhs_store::JournalStorage;
use tauri::{Manager, ipc::Channel};
use tokio::sync::{mpsc, oneshot};
use walkie_songie::{
    client::{
        AppError, AppErrorCode, AppEvent, AppEventEnvelope, AppSnapshot, Capabilities,
        ClientCommand, CommandAck, DiscoverySource, MidiPortSnapshot, PeerPath, PeerSnapshot,
    },
    is_valid_room_name,
    midi::{
        HeldInputAction, MidiDeviceDirection, MidiInputTracker, MidiLedger, MidiOutputConfig,
        MidiSource, NativeMidiService, PhysicalMidiKey,
    },
    net::{
        FileSeedStore, IrohSyncStream, NativeNetworkEvent, NativeRoomNetwork, NativeRoomTicketV5,
        PeerTransportPath, RelayPolicy, ReplicaLiveRecord, ReplicaProtocol, ReplicaRepairHint,
        ReplicaRoomNetworkConfig, RoomInbound, TokioTimer, WalkieIdentity, drive_replica_initiator,
        drive_replica_responder, is_routine_repair_initiator, spawn_rendezvous_v5,
    },
    room::v5::{
        ActorId, ExtensionCommand, MemberCapabilities, MusicOp, ProtocolSupport, RoomCommand,
        RoomLane, RoomReplicas, RoomView, open_room_authority,
    },
};

enum RoomControl {
    Commit {
        command: RoomCommand,
        response: oneshot::Sender<Result<CommandAck, AppError>>,
    },
    Presence {
        session: u64,
        pitch: Option<walkie_songie::TunedPeriodicPitch>,
        response: oneshot::Sender<Result<CommandAck, AppError>>,
    },
    Shutdown,
}

struct ActiveRoom {
    control: mpsc::Sender<RoomControl>,
    task: tauri::async_runtime::JoinHandle<()>,
}

struct RuntimeState {
    sequence: u64,
    snapshot: AppSnapshot,
    peer_sync: BTreeMap<ActorId, PeerSyncState>,
    subscribers: Vec<Channel<AppEventEnvelope>>,
    active_room: Option<ActiveRoom>,
}

#[derive(Clone, Copy, Default)]
struct PeerSyncState {
    required: u8,
    complete: u8,
}

impl PeerSyncState {
    fn update_requirements(&mut self, support: ProtocolSupport, path: PeerPath) -> bool {
        self.required = support.bits();
        self.complete &= self.required;
        if path == PeerPath::Disconnected {
            self.complete = 0;
        }
        self.synchronized()
    }

    fn mark_lane(&mut self, lane: RoomLane, synchronized: bool) -> bool {
        if synchronized {
            self.complete |= lane.tag();
        } else {
            self.complete &= !lane.tag();
        }
        self.synchronized()
    }

    const fn synchronized(self) -> bool {
        self.required != 0 && self.complete & self.required == self.required
    }
}

struct MidiRuntime {
    service: NativeMidiService,
    ledger: MidiLedger,
    input: MidiInputTracker,
}

struct DurableRoom {
    room: RoomReplicas<JournalStorage, JournalStorage>,
    capabilities: MemberCapabilities,
}

type SharedDurableRoom = Arc<tokio::sync::Mutex<DurableRoom>>;

#[derive(Clone)]
pub struct AppRuntime {
    state: Arc<Mutex<RuntimeState>>,
    identity: WalkieIdentity,
    midi: Arc<Mutex<MidiRuntime>>,
    data_dir: PathBuf,
}

impl AppRuntime {
    fn new(identity: WalkieIdentity, data_dir: PathBuf) -> Self {
        let tuning = walkie_songie::Tuning::twelve_tet();
        Self {
            state: Arc::new(Mutex::new(RuntimeState {
                sequence: 0,
                snapshot: AppSnapshot::empty(Capabilities::tauri_desktop()),
                peer_sync: BTreeMap::new(),
                subscribers: Vec::new(),
                active_room: None,
            })),
            identity,
            data_dir,
            midi: Arc::new(Mutex::new(MidiRuntime {
                service: NativeMidiService::new(),
                ledger: MidiLedger::new(&tuning, MidiOutputConfig::exact_twelve_tet())
                    .expect("built-in MIDI configuration is valid"),
                input: MidiInputTracker::new(),
            })),
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, RuntimeState>, AppError> {
        self.state.lock().map_err(|_| {
            AppError::new(
                AppErrorCode::Internal,
                "native runtime state lock was poisoned",
            )
        })
    }

    fn lock_midi(&self) -> Result<MutexGuard<'_, MidiRuntime>, AppError> {
        self.midi.lock().map_err(|_| {
            AppError::new(
                AppErrorCode::Internal,
                "native MIDI state lock was poisoned",
            )
        })
    }

    fn register(&self, subscriber: Channel<AppEventEnvelope>) -> Result<CommandAck, AppError> {
        let mut state = self.lock()?;
        state.sequence = state.sequence.saturating_add(1);
        let envelope = AppEventEnvelope {
            sequence: state.sequence,
            event: AppEvent::Snapshot {
                snapshot: Box::new(state.snapshot.clone()),
            },
        };
        subscriber.send(envelope).map_err(|error| {
            AppError::new(AppErrorCode::Internal, "could not send initial snapshot")
                .with_detail(error.to_string())
        })?;
        state.subscribers.push(subscriber);
        Ok(CommandAck {
            accepted_sequence: state.sequence,
        })
    }

    async fn dispatch(&self, command: ClientCommand) -> Result<CommandAck, AppError> {
        match command {
            ClientCommand::EnterRoom { room_name } => self.enter_room_v5(room_name).await,
            ClientCommand::JoinTicket { ticket } => self.join_ticket_v5(ticket).await,
            ClientCommand::LeaveRoom => self.leave_room().await,
            ClientCommand::SetTuning { definition } => {
                definition.validate("room tuning").map_err(|error| {
                    AppError::new(AppErrorCode::InvalidTuning, "invalid tuning definition")
                        .with_detail(error.to_string())
                })?;
                self.submit_durable(MusicOp::SetTuning { definition }.into())
                    .await
            }
            ClientCommand::AddDegree { pitch } => {
                self.validate_degree(pitch)?;
                self.submit_durable(MusicOp::AddDegree { degree: pitch }.into())
                    .await
            }
            ClientCommand::RemoveDegree { pitch } => {
                self.validate_degree(pitch)?;
                self.submit_durable(MusicOp::RemoveDegree { degree: pitch }.into())
                    .await
            }
            ClientCommand::PutPiece { emoji, pitch } => {
                self.validate_periodic_pitch(pitch)?;
                self.submit_durable(ExtensionCommand::PutPiece { emoji, pitch }.into())
                    .await
            }
            ClientCommand::MovePiece { piece, pitch } => {
                self.validate_periodic_pitch(pitch)?;
                self.submit_durable(ExtensionCommand::MovePiece { piece, pitch }.into())
                    .await
            }
            ClientCommand::RemovePiece { piece } => {
                self.submit_durable(ExtensionCommand::RemovePiece { piece }.into())
                    .await
            }
            ClientCommand::SetRoomConfig {
                pieces_locked,
                available_emojis,
            } => {
                self.submit_durable(
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
                    self.validate_periodic_pitch(pitch)?;
                }
                self.submit_presence(session, pitch).await
            }
            command => self.dispatch_nondurable(command).await,
        }
    }

    async fn enter_room_v5(&self, room_name: String) -> Result<CommandAck, AppError> {
        if !is_valid_room_name(&room_name) {
            return Err(AppError::new(
                AppErrorCode::InvalidRoom,
                "room names use the form adjective-noun-noun",
            ));
        }
        let authority = open_room_authority(&room_name);
        let owner = ActorId::from_signing_key(&authority);
        let mut config = ReplicaRoomNetworkConfig::create(&room_name, owner);
        config.relay = relay_policy_from_environment()?;
        self.start_room_v5(
            Some(room_name),
            config,
            DiscoverySource::Mdns,
            Some(authority),
        )
        .await
    }

    async fn join_ticket_v5(&self, encoded: String) -> Result<CommandAck, AppError> {
        let ticket = encoded.parse::<NativeRoomTicketV5>().map_err(|error| {
            AppError::new(AppErrorCode::InvalidTicket, "invalid Room-v5 ticket")
                .with_detail(error.to_string())
        })?;
        let mut config = ReplicaRoomNetworkConfig::join(&ticket);
        config.relay = relay_policy_from_environment()?;
        self.start_room_v5(None, config, DiscoverySource::Ticket, None)
            .await
    }

    async fn start_room_v5(
        &self,
        room_name: Option<String>,
        config: ReplicaRoomNetworkConfig,
        bootstrap_source: DiscoverySource,
        room_authority: Option<hhhs_proof::SigningKey>,
    ) -> Result<CommandAck, AppError> {
        self.stop_active_room().await;

        let topic = config.topic();
        let topic_string = topic.to_string();
        let owner_string = walkie_songie::net::PeerId(config.owner.0).to_hex();
        let room_directory = self.data_dir.join("rooms");
        std::fs::create_dir_all(&room_directory).map_err(persistence_error)?;
        let music = JournalStorage::open(
            room_directory.join(format!("{topic_string}.{owner_string}.music.v5.hhhs")),
        )
        .map_err(persistence_error)?;
        let extension = JournalStorage::open(
            room_directory.join(format!("{topic_string}.{owner_string}.extension.v5.hhhs")),
        )
        .map_err(persistence_error)?;
        let local_actor = self.identity.capability_actor_id();
        let room = RoomReplicas::initialize(config.room.clone(), config.owner, music, extension)
            .map_err(persistence_error)?;
        if let Some(authority) = room_authority.as_ref() {
            if ActorId::from_signing_key(authority) != config.owner {
                return Err(AppError::new(
                    AppErrorCode::InvalidRoom,
                    "open-room authority does not match the room owner",
                ));
            }
            let capabilities = room.capabilities_for(local_actor);
            if capabilities.music.is_empty() || capabilities.extension.is_empty() {
                room.grant_member(authority, local_actor)
                    .map_err(persistence_error)?;
            }
        }
        let recovered_view = room.view();
        let durable = Arc::new(tokio::sync::Mutex::new(DurableRoom {
            capabilities: room.capabilities_for(local_actor),
            room,
        }));

        let bootstrap = config.bootstrap.as_ref().map(|address| address.id);
        let bootstrap_support = config.bootstrap_support;
        let mut network = NativeRoomNetwork::bind(self.identity.iroh_secret(), config.clone())
            .await
            .map_err(network_error)?;
        let ticket = network.settle_ticket(Duration::from_millis(750)).await;
        let ticket_string = ticket.to_string();

        let (rendezvous_guard, mut rendezvous_rx) = if bootstrap.is_none() {
            let (rdv_tx, rdv_rx) = mpsc::channel::<(iroh::EndpointId, ProtocolSupport)>(64);
            let handle = spawn_rendezvous_v5(
                network.rendezvous_peering(),
                topic,
                ProtocolSupport::WALKIE,
                move |endpoint_id, support| {
                    let _ = rdv_tx.try_send((endpoint_id, support));
                },
            );
            (Some(handle), Some(rdv_rx))
        } else {
            (None, None)
        };

        let (control, mut control_rx) = mpsc::channel(64);
        let runtime = self.clone();
        let signing_key = self.identity.capability_signing_key();
        let endpoint = network.endpoint().clone();
        let own_endpoint = network.endpoint_id();
        let midi_events = self.lock_midi()?.service.input_events();
        let task = tauri::async_runtime::spawn(async move {
            let _rendezvous_guard = rendezvous_guard;
            let mut local_presence_session = 0_u64;
            let mut local_presence_sequence = 0_u64;
            let mut peers: BTreeMap<
                iroh::EndpointId,
                (DiscoverySource, PeerPath, ProtocolSupport),
            > = BTreeMap::new();
            if let Some(endpoint_id) = bootstrap {
                let support = bootstrap_support.unwrap_or(ProtocolSupport::WALKIE);
                peers.insert(
                    endpoint_id,
                    (bootstrap_source, PeerPath::Connecting, support),
                );
                runtime.update_peer(endpoint_id, bootstrap_source, PeerPath::Connecting, support);
            }

            let mut path_refresh = tokio::time::interval(Duration::from_secs(1));
            path_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut midi_refresh = tokio::time::interval(Duration::from_secs(2));
            midi_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let repair_period =
                Duration::from_secs(25 + u64::from(own_endpoint.as_bytes()[0] % 11));
            let mut repair_refresh = tokio::time::interval_at(
                tokio::time::Instant::now() + repair_period,
                repair_period,
            );
            repair_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    control = control_rx.recv() => match control {
                        Some(RoomControl::Commit { command, response }) => {
                            let result = commit_replica_command(
                                &durable,
                                &signing_key,
                                command,
                                &network,
                                &runtime,
                            ).await;
                            let _ = response.send(result.map(|accepted_sequence| CommandAck {
                                accepted_sequence,
                            }));
                        }
                        Some(RoomControl::Presence { session, pitch, response }) => {
                            if session == local_presence_session {
                                local_presence_sequence = local_presence_sequence.saturating_add(1);
                            } else {
                                local_presence_session = session;
                                local_presence_sequence = 0;
                            }
                            let result = broadcast_presence(
                                &durable,
                                &signing_key,
                                session,
                                local_presence_sequence,
                                pitch,
                                &network,
                                &runtime,
                            ).await;
                            let _ = response.send(result.map(|accepted_sequence| CommandAck {
                                accepted_sequence,
                            }));
                        }
                        Some(RoomControl::Shutdown) | None => break,
                    },
                    _ = midi_refresh.tick() => {
                        match runtime.refresh_midi_devices() {
                            Ok(actions) => for action in actions {
                                let command = match action {
                                    HeldInputAction::DegreeActivated(pitch) =>
                                        MusicOp::AddDegree { degree: pitch }.into(),
                                    HeldInputAction::DegreeReleased(pitch) =>
                                        MusicOp::RemoveDegree { degree: pitch }.into(),
                                };
                                if let Err(error) = commit_replica_command(
                                    &durable, &signing_key, command, &network, &runtime,
                                ).await {
                                    runtime.emit_diagnostic("midi_persistence", &error.message);
                                }
                            },
                            Err(error) => runtime.emit_diagnostic("midi_refresh", &error.message),
                        }
                    }
                    input = midi_events.recv() => {
                        let Ok(input) = input else {
                            runtime.emit_diagnostic(
                                "midi_input_closed",
                                "native MIDI input event channel closed",
                            );
                            continue;
                        };
                        for action in runtime.apply_midi_input(input) {
                            let command = match action {
                                HeldInputAction::DegreeActivated(pitch) =>
                                    MusicOp::AddDegree { degree: pitch }.into(),
                                HeldInputAction::DegreeReleased(pitch) =>
                                    MusicOp::RemoveDegree { degree: pitch }.into(),
                            };
                            if let Err(error) = commit_replica_command(
                                &durable, &signing_key, command, &network, &runtime,
                            ).await {
                                runtime.emit_diagnostic("midi_persistence", &error.message);
                            }
                        }
                    }
                    _ = path_refresh.tick() => {
                        for endpoint_id in peers.keys().copied().collect::<Vec<_>>() {
                            let path = map_peer_path(network.peer_path(endpoint_id).await);
                            let Some((source, previous, support)) = peers.get_mut(&endpoint_id) else {
                                continue;
                            };
                            if *previous != path {
                                *previous = path;
                                runtime.update_peer(endpoint_id, *source, path, *support);
                            }
                        }
                    }
                    _ = repair_refresh.tick() => {
                        for (peer, (_, path, support)) in &peers {
                            if *path == PeerPath::Disconnected
                                || !is_routine_repair_initiator(
                                    walkie_songie::net::PeerId(*own_endpoint.as_bytes()),
                                    walkie_songie::net::PeerId(*peer.as_bytes()),
                                )
                            {
                                continue;
                            }
                            for lane in [RoomLane::Music, RoomLane::Extension] {
                                if support.supports(lane) {
                                    spawn_replica_initiator(
                                        runtime.clone(), durable.clone(), endpoint.clone(), *peer, lane,
                                    );
                                }
                            }
                        }
                    }
                    discovered = async {
                        match rendezvous_rx.as_mut() {
                            Some(rx) => rx.recv().await,
                            None => std::future::pending::<Option<(iroh::EndpointId, ProtocolSupport)>>().await,
                        }
                    } => match discovered {
                        Some((endpoint_id, support)) => {
                            peers.entry(endpoint_id).or_insert((
                                DiscoverySource::AddressLookup,
                                PeerPath::Connecting,
                                support,
                            ));
                            runtime.update_peer(
                                endpoint_id,
                                DiscoverySource::AddressLookup,
                                PeerPath::Connecting,
                                support,
                            );
                        }
                        None => rendezvous_rx = None,
                    },
                    inbound = network.next_inbound() => {
                        let Some(inbound) = inbound else {
                            runtime.emit_diagnostic(
                                "native_network_closed",
                                "the native Iroh room task closed",
                            );
                            break;
                        };
                        match inbound {
                            RoomInbound::Repair(repair) => {
                                let Some(ReplicaProtocol::Repair(lane)) =
                                    ReplicaProtocol::from_alpn(repair.alpn)
                                else {
                                    repair.connection.close(4u32.into(), b"unsupported Room-v5 ALPN");
                                    continue;
                                };
                                spawn_replica_responder(
                                    runtime.clone(),
                                    durable.clone(),
                                    repair.endpoint_id,
                                    lane,
                                    repair.stream.owning(repair.connection),
                                );
                            }
                            RoomInbound::Event(event) => match event {
                                NativeNetworkEvent::MdnsDiscovered { endpoint_id } => {
                                    peers.entry(endpoint_id).or_insert((
                                        DiscoverySource::Mdns,
                                        PeerPath::Connecting,
                                        ProtocolSupport::WALKIE,
                                    ));
                                    runtime.update_peer(
                                        endpoint_id,
                                        DiscoverySource::Mdns,
                                        PeerPath::Connecting,
                                        ProtocolSupport::WALKIE,
                                    );
                                }
                                NativeNetworkEvent::MdnsExpired { endpoint_id } => {
                                    if peers.get(&endpoint_id).is_some_and(|(_, path, _)| {
                                        *path == PeerPath::Disconnected
                                    }) {
                                        peers.remove(&endpoint_id);
                                        runtime.remove_peer(endpoint_id);
                                    }
                                }
                                NativeNetworkEvent::NeighborUp { endpoint_id, discovery } => {
                                    let support = peers.get(&endpoint_id)
                                        .map(|(_, _, support)| *support)
                                        .unwrap_or(ProtocolSupport::WALKIE);
                                    let source = peers.get(&endpoint_id)
                                        .map(|(source, _, _)| *source)
                                        .unwrap_or(discovery);
                                    let path = map_peer_path(network.peer_path(endpoint_id).await);
                                    peers.insert(endpoint_id, (source, path, support));
                                    runtime.update_peer(endpoint_id, source, path, support);
                                    if let Err(error) = maybe_grant_peer(
                                        &durable,
                                        room_authority.as_ref().unwrap_or(&signing_key),
                                        local_actor,
                                        ActorId(*endpoint_id.as_bytes()),
                                        &network,
                                    ).await {
                                        runtime.emit_diagnostic("capability_grant", &error.message);
                                    }
                                    if is_routine_repair_initiator(
                                        walkie_songie::net::PeerId(*own_endpoint.as_bytes()),
                                        walkie_songie::net::PeerId(*endpoint_id.as_bytes()),
                                    ) {
                                        for lane in [RoomLane::Music, RoomLane::Extension] {
                                            if support.supports(lane) {
                                                spawn_replica_initiator(
                                                    runtime.clone(), durable.clone(), endpoint.clone(), endpoint_id, lane,
                                                );
                                            }
                                        }
                                    }
                                }
                                NativeNetworkEvent::NeighborDown { endpoint_id } => {
                                    if let Some((source, path, support)) = peers.get_mut(&endpoint_id) {
                                        *path = PeerPath::Disconnected;
                                        runtime.update_peer(
                                            endpoint_id,
                                            *source,
                                            PeerPath::Disconnected,
                                            *support,
                                        );
                                    }
                                }
                                NativeNetworkEvent::Message { bytes, .. } => {
                                    if let Some(live) = ReplicaLiveRecord::decode(&bytes) {
                                        let local = walkie_songie::net::PeerId(*own_endpoint.as_bytes());
                                        if live.source != local {
                                            let source = live.source;
                                            let lane = live.lane;
                                            let entry = live.record.entry_hash();
                                            let accepted = match apply_native_live_record(
                                                &durable,
                                                live,
                                                &runtime,
                                                local_actor,
                                            ).await {
                                                Ok(accepted) => accepted,
                                                Err(error) => {
                                                    runtime.emit_diagnostic(
                                                        "live_record_admission",
                                                        &error.message,
                                                    );
                                                    false
                                                }
                                            };
                                            if !accepted
                                                && let Ok(source_endpoint) =
                                                    iroh::EndpointId::from_bytes(source.as_bytes())
                                            {
                                                if is_routine_repair_initiator(local, source) {
                                                    spawn_replica_initiator(
                                                        runtime.clone(),
                                                        durable.clone(),
                                                        endpoint.clone(),
                                                        source_endpoint,
                                                        lane,
                                                    );
                                                } else if let Err(error) = network
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
                                                    runtime.emit_diagnostic(
                                                        "repair_hint_broadcast",
                                                        &format!(
                                                            "live delivery needs repair; hint failed: {error}"
                                                        ),
                                                    );
                                                }
                                            }
                                        }
                                    } else if let Some(hint) = ReplicaRepairHint::decode(&bytes)
                                        && is_routine_repair_initiator(
                                            walkie_songie::net::PeerId(*own_endpoint.as_bytes()),
                                            hint.source,
                                        )
                                        && let Ok(source) = iroh::EndpointId::from_bytes(hint.source.as_bytes())
                                    {
                                        spawn_replica_initiator(
                                            runtime.clone(), durable.clone(), endpoint.clone(), source, hint.lane,
                                        );
                                    } else if let Ok(presence) =
                                        durable.lock().await.room.verify_presence(&bytes)
                                    {
                                        runtime.apply_presence_v5(
                                            presence.actor,
                                            presence.session,
                                            presence.sequence,
                                            presence.pitch,
                                            unix_time_millis().saturating_add(1_500),
                                        );
                                    }
                                }
                                NativeNetworkEvent::Lagged => {
                                    for (peer, (_, path, support)) in &peers {
                                        if *path == PeerPath::Disconnected
                                            || !is_routine_repair_initiator(
                                                walkie_songie::net::PeerId(*own_endpoint.as_bytes()),
                                                walkie_songie::net::PeerId(*peer.as_bytes()),
                                            )
                                        {
                                            continue;
                                        }
                                        for lane in [RoomLane::Music, RoomLane::Extension] {
                                            if support.supports(lane) {
                                                spawn_replica_initiator(
                                                    runtime.clone(), durable.clone(), endpoint.clone(), *peer, lane,
                                                );
                                            }
                                        }
                                    }
                                }
                                NativeNetworkEvent::Diagnostic(message) =>
                                    runtime.emit_diagnostic("native_network", &message),
                                NativeNetworkEvent::Closed => break,
                            },
                        }
                    }
                }
            }
            let _ = network.shutdown().await;
        });

        let mut state = self.lock()?;
        state.active_room = Some(ActiveRoom { control, task });
        state.snapshot.room_name = room_name.clone();
        state.snapshot.room_topic = Some(topic_string.clone());
        state.snapshot.room_ticket = Some(ticket_string.clone());
        state.peer_sync.clear();
        state.snapshot.peers.clear();
        state.snapshot.voices.clear();
        emit_locked(
            &mut state,
            AppEvent::RoomChanged {
                room_name,
                room_topic: Some(topic_string),
                ticket: Some(ticket_string),
            },
        );
        drop(state);
        self.apply_room_view(recovered_view);
        Ok(CommandAck {
            accepted_sequence: self.lock()?.sequence,
        })
    }

    async fn leave_room(&self) -> Result<CommandAck, AppError> {
        self.stop_active_room().await;
        let mut state = self.lock()?;
        state.snapshot.room_name = None;
        state.snapshot.room_topic = None;
        state.snapshot.room_ticket = None;
        state.snapshot.shared_pitches = Default::default();
        state.snapshot.pieces.clear();
        state.snapshot.pieces_locked = false;
        state.snapshot.available_emojis = None;
        state.snapshot.voices.clear();
        state.peer_sync.clear();
        state.snapshot.peers.clear();
        emit_locked(
            &mut state,
            AppEvent::RoomChanged {
                room_name: None,
                room_topic: None,
                ticket: None,
            },
        );
        Ok(CommandAck {
            accepted_sequence: state.sequence,
        })
    }

    async fn stop_active_room(&self) {
        let active = self
            .state
            .lock()
            .ok()
            .and_then(|mut state| state.active_room.take());
        if let Some(active) = active {
            let _ = active.control.send(RoomControl::Shutdown).await;
            let _ = tokio::time::timeout(Duration::from_secs(3), active.task).await;
        }
        if let Err(error) = self.panic_midi_internal() {
            self.emit_diagnostic("midi_panic", &error.message);
        }
    }

    async fn shutdown(&self) {
        self.stop_active_room().await;
    }

    async fn submit_durable(&self, command: RoomCommand) -> Result<CommandAck, AppError> {
        let control = self
            .lock()?
            .active_room
            .as_ref()
            .map(|active| active.control.clone())
            .ok_or_else(|| {
                AppError::new(
                    AppErrorCode::NetworkUnavailable,
                    "enter a room before changing durable musical state",
                )
            })?;
        let (response, receive) = oneshot::channel();
        control
            .send(RoomControl::Commit { command, response })
            .await
            .map_err(|_| {
                AppError::new(
                    AppErrorCode::ShuttingDown,
                    "the active room task is shutting down",
                )
            })?;
        receive.await.map_err(|_| {
            AppError::new(
                AppErrorCode::Internal,
                "the active room task dropped its command response",
            )
        })?
    }

    async fn submit_presence(
        &self,
        session: u64,
        pitch: Option<walkie_songie::TunedPeriodicPitch>,
    ) -> Result<CommandAck, AppError> {
        let control = self
            .lock()?
            .active_room
            .as_ref()
            .map(|active| active.control.clone())
            .ok_or_else(|| {
                AppError::new(
                    AppErrorCode::NetworkUnavailable,
                    "enter a room before sending voice presence",
                )
            })?;
        let (response, receive) = oneshot::channel();
        control
            .send(RoomControl::Presence {
                session,
                pitch,
                response,
            })
            .await
            .map_err(|_| {
                AppError::new(
                    AppErrorCode::ShuttingDown,
                    "the active room task is shutting down",
                )
            })?;
        receive.await.map_err(|_| {
            AppError::new(
                AppErrorCode::Internal,
                "the active room task dropped its presence response",
            )
        })?
    }

    fn validate_degree(&self, pitch: walkie_songie::TunedDegree) -> Result<(), AppError> {
        let state = self.lock()?;
        let tuning = current_tuning(&state)?;
        pitch.validate(&tuning).map(|_| ()).map_err(|error| {
            AppError::new(AppErrorCode::InvalidCommand, "invalid tuning-scoped degree")
                .with_detail(error.to_string())
        })
    }

    fn validate_periodic_pitch(
        &self,
        pitch: walkie_songie::TunedPeriodicPitch,
    ) -> Result<(), AppError> {
        let state = self.lock()?;
        let tuning = current_tuning(&state)?;
        pitch.validate(&tuning).map(|_| ()).map_err(|error| {
            AppError::new(
                AppErrorCode::InvalidCommand,
                "invalid tuning-scoped periodic pitch",
            )
            .with_detail(error.to_string())
        })
    }

    async fn dispatch_nondurable(&self, command: ClientCommand) -> Result<CommandAck, AppError> {
        match command {
            ClientCommand::ListMidiPorts => {
                self.refresh_midi_devices()?;
                Ok(CommandAck {
                    accepted_sequence: self.lock()?.sequence,
                })
            }
            ClientCommand::SelectMidiInput { port_id } => {
                let (actions, ports) = {
                    let mut midi = self.lock_midi()?;
                    let actions = midi.input.clear();
                    midi.service
                        .select_input(port_id.as_deref())
                        .map_err(midi_error)?;
                    let ports = midi.service.list_ports().map_err(midi_error)?;
                    (actions, ports)
                };
                self.update_midi_ports(ports);
                for action in actions {
                    if let HeldInputAction::DegreeReleased(pitch) = action
                        && self.lock()?.active_room.is_some()
                    {
                        self.submit_durable(MusicOp::RemoveDegree { degree: pitch }.into())
                            .await?;
                    }
                }
                Ok(CommandAck {
                    accepted_sequence: self.lock()?.sequence,
                })
            }
            ClientCommand::SelectMidiOutput { port_id } => {
                {
                    let mut midi = self.lock_midi()?;
                    let messages = midi.ledger.panic();
                    midi.service.send_messages(messages).map_err(midi_error)?;
                    midi.service
                        .select_output(port_id.as_deref())
                        .map_err(midi_error)?;
                    let ports = midi.service.list_ports().map_err(midi_error)?;
                    drop(midi);
                    self.update_midi_ports(ports);
                }
                self.sync_midi_from_snapshot();
                Ok(CommandAck {
                    accepted_sequence: self.lock()?.sequence,
                })
            }
            ClientCommand::PanicMidi => {
                self.panic_midi_internal()?;
                Ok(CommandAck {
                    accepted_sequence: self.lock()?.sequence,
                })
            }
            _ => unreachable!("durable and room commands were dispatched earlier"),
        }
    }

    fn apply_midi_input(&self, event: walkie_songie::midi::MidiInputEvent) -> Vec<HeldInputAction> {
        let tuning = self.lock().and_then(|state| current_tuning(&state)).ok();
        let Some(tuning) = tuning else {
            return Vec::new();
        };
        let key = PhysicalMidiKey {
            port_id: event.port_id,
            channel: event.channel,
            note: event.note,
        };
        let Ok(mut midi) = self.lock_midi() else {
            return Vec::new();
        };
        if event.is_note_on && event.velocity > 0 {
            midi.input.note_on(key, &tuning)
        } else {
            midi.input.note_off(&key)
        }
    }

    fn refresh_midi_devices(&self) -> Result<Vec<HeldInputAction>, AppError> {
        let (ports, actions, output_lost) = {
            let mut midi = self.lock_midi()?;
            let selected_input = midi.service.selected_input().map(str::to_owned);
            let (ports, input_lost, output_lost) = midi.service.refresh().map_err(midi_error)?;
            let actions = if input_lost {
                selected_input
                    .as_deref()
                    .map(|port| midi.input.release_port(port))
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            if output_lost {
                // The device is already absent. Clear logical ownership so a
                // newly selected output receives a clean projection.
                midi.ledger.panic();
            }
            (ports, actions, output_lost)
        };
        self.update_midi_ports(ports);
        if output_lost {
            self.emit_diagnostic(
                "midi_output_removed",
                "the selected MIDI output disappeared; sounding ownership was released",
            );
        }
        Ok(actions)
    }

    fn update_midi_ports(&self, ports: Vec<walkie_songie::midi::NativePort>) -> u64 {
        let mut inputs = Vec::new();
        let mut outputs = Vec::new();
        for port in ports {
            let snapshot = MidiPortSnapshot {
                id: port.id,
                name: port.name,
                selected: port.selected,
            };
            match port.direction {
                MidiDeviceDirection::Input => inputs.push(snapshot),
                MidiDeviceDirection::Output => outputs.push(snapshot),
            }
        }
        let Ok(mut state) = self.lock() else {
            return 0;
        };
        if state.snapshot.midi_inputs == inputs && state.snapshot.midi_outputs == outputs {
            return state.sequence;
        }
        state.snapshot.midi_inputs = inputs.clone();
        state.snapshot.midi_outputs = outputs.clone();
        emit_locked(&mut state, AppEvent::MidiPortsChanged { inputs, outputs });
        state.sequence
    }

    fn panic_midi_internal(&self) -> Result<(), AppError> {
        let mut midi = self.lock_midi()?;
        midi.input.clear();
        let messages = midi.ledger.panic();
        midi.service.send_messages(messages).map_err(midi_error)?;
        midi.service.panic_output().map_err(midi_error)
    }

    fn sync_midi_from_snapshot(&self) {
        let snapshot = match self.lock() {
            Ok(state) => state.snapshot.clone(),
            Err(_) => return,
        };
        let Some(definition) = snapshot.tuning.as_ref() else {
            return;
        };
        let Ok(tuning) = definition.validate("MIDI room tuning") else {
            return;
        };
        let mut diagnostics = Vec::new();
        let Ok(mut midi) = self.lock_midi() else {
            return;
        };

        if midi.ledger.tuning_id() != tuning.id() {
            midi.input.clear();
            let config = midi_output_config(&tuning);
            match midi.ledger.change_tuning(&tuning, config) {
                Ok(messages) => {
                    if let Err(error) = midi.service.send_messages(messages) {
                        diagnostics.push(("midi_send", error.to_string()));
                    }
                }
                Err(error) => diagnostics.push(("midi_tuning", error.to_string())),
            }
        }

        let mut desired = BTreeMap::new();
        for pitch in &snapshot.shared_pitches.pitch_classes {
            let periodic = walkie_songie::TunedPeriodicPitch {
                tuning_id: pitch.tuning_id,
                pitch: walkie_songie::PeriodicPitch::from_degree(pitch.degree, 0),
            };
            desired.insert(MidiSource::SharedDegree { pitch: *pitch }, periodic);
        }
        for pitch in &snapshot.shared_pitches.pitches {
            desired.insert(MidiSource::SharedPitch { pitch: *pitch }, *pitch);
        }
        for piece in &snapshot.pieces {
            desired.insert(MidiSource::Piece { id: piece.id }, piece.pitch);
        }
        for voice in &snapshot.voices {
            if let Some(pitch) = voice.pitch {
                desired.insert(
                    MidiSource::Voice {
                        author: voice.author,
                        session: voice.session,
                    },
                    pitch,
                );
            }
        }

        let stale: Vec<_> = midi
            .ledger
            .sources()
            .map(|(source, _)| source.clone())
            .filter(|source| !desired.contains_key(source))
            .collect();
        for source in stale {
            match midi.ledger.set_source(source, None, &tuning) {
                Ok(messages) => {
                    if let Err(error) = midi.service.send_messages(messages) {
                        diagnostics.push(("midi_send", error.to_string()));
                    }
                }
                Err(error) => diagnostics.push(("midi_release", error.to_string())),
            }
        }
        for (source, pitch) in desired {
            match midi.ledger.set_source(source, Some(pitch), &tuning) {
                Ok(messages) => {
                    if let Err(error) = midi.service.send_messages(messages) {
                        diagnostics.push(("midi_send", error.to_string()));
                    }
                }
                Err(error) => diagnostics.push(("midi_route", error.to_string())),
            }
        }
        drop(midi);
        for (code, message) in diagnostics {
            self.emit_diagnostic(code, &message);
        }
    }

    fn apply_room_view(&self, view: RoomView) -> u64 {
        let Ok(mut state) = self.lock() else {
            return 0;
        };
        let mut events = Vec::new();

        if state.snapshot.tuning.as_ref() != Some(&view.music.tuning) {
            state.snapshot.tuning = Some(view.music.tuning.clone());
            state.snapshot.tuning_id = Some(view.music.tuning.id);
            events.push(AppEvent::TuningChanged {
                definition: view.music.tuning.clone(),
            });
        }

        if state.snapshot.shared_pitches != view.music.shared_pitches {
            state.snapshot.shared_pitches = view.music.shared_pitches.clone();
            events.push(AppEvent::PitchSetChanged {
                shared: view.music.shared_pitches.clone(),
            });
        }

        let new_pieces: Vec<_> = view
            .pieces
            .iter()
            .map(|(id, piece)| walkie_songie::client::PieceSnapshot {
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

        for event in events {
            emit_locked(&mut state, event);
        }
        drop(state);
        self.sync_midi_from_snapshot();
        self.lock().map(|state| state.sequence).unwrap_or(0)
    }

    fn apply_presence_v5(
        &self,
        author: ActorId,
        session: u64,
        sequence: u64,
        pitch: Option<walkie_songie::TunedPeriodicPitch>,
        expires_at_ms: u64,
    ) -> u64 {
        let Ok(mut state) = self.lock() else {
            return 0;
        };
        let voice = walkie_songie::client::VoiceSnapshot {
            author,
            session,
            sequence,
            pitch,
            expires_at_ms,
        };
        if let Some(existing) = state
            .snapshot
            .voices
            .iter_mut()
            .find(|existing| existing.author == author && existing.session == session)
        {
            if sequence <= existing.sequence {
                return state.sequence;
            }
            *existing = voice.clone();
        } else {
            state.snapshot.voices.push(voice.clone());
        }
        emit_locked(&mut state, AppEvent::VoiceUpdated { voice });
        state.sequence
    }

    fn update_peer(
        &self,
        endpoint_id: iroh::EndpointId,
        discovery: DiscoverySource,
        path: PeerPath,
        support: ProtocolSupport,
    ) {
        let Ok(mut state) = self.lock() else {
            return;
        };
        let author = ActorId(*endpoint_id.as_bytes());
        let synchronized = state
            .peer_sync
            .entry(author)
            .or_default()
            .update_requirements(support, path);
        let mut peer = PeerSnapshot {
            author,
            endpoint_id: endpoint_id.to_string(),
            path,
            discovery,
            round_trip_ms: None,
            synchronized,
        };
        if let Some(existing) = state
            .snapshot
            .peers
            .iter_mut()
            .find(|existing| existing.author == author)
        {
            peer.round_trip_ms = existing.round_trip_ms;
            *existing = peer.clone();
        } else {
            state.snapshot.peers.push(peer.clone());
            state.snapshot.peers.sort_by_key(|peer| peer.author);
        }
        emit_locked(&mut state, AppEvent::PeerUpdated { peer });
    }

    fn remove_peer(&self, endpoint_id: iroh::EndpointId) {
        let Ok(mut state) = self.lock() else {
            return;
        };
        let author = ActorId(*endpoint_id.as_bytes());
        state.peer_sync.remove(&author);
        state.snapshot.peers.retain(|peer| peer.author != author);
        emit_locked(&mut state, AppEvent::PeerRemoved { author });
    }

    fn mark_peer_lane_synchronized(
        &self,
        endpoint_id: iroh::EndpointId,
        lane: RoomLane,
        synchronized: bool,
    ) {
        let Ok(mut state) = self.lock() else {
            return;
        };
        let author = ActorId(*endpoint_id.as_bytes());
        let synchronized = state
            .peer_sync
            .entry(author)
            .or_insert(PeerSyncState {
                required: lane.tag(),
                complete: 0,
            })
            .mark_lane(lane, synchronized);
        let Some(peer) = state
            .snapshot
            .peers
            .iter_mut()
            .find(|peer| peer.author == author)
        else {
            return;
        };
        if peer.synchronized == synchronized {
            return;
        }
        peer.synchronized = synchronized;
        let peer = peer.clone();
        emit_locked(&mut state, AppEvent::PeerUpdated { peer });
    }

    fn emit_diagnostic(&self, code: &str, message: &str) {
        let Ok(mut state) = self.lock() else {
            return;
        };
        emit_locked(
            &mut state,
            AppEvent::Diagnostic {
                code: code.to_owned(),
                message: message.to_owned(),
            },
        );
    }
}

async fn broadcast_presence(
    durable: &SharedDurableRoom,
    signing_key: &hhhs_proof::SigningKey,
    session: u64,
    sequence: u64,
    pitch: Option<walkie_songie::TunedPeriodicPitch>,
    network: &NativeRoomNetwork,
    runtime: &AppRuntime,
) -> Result<u64, AppError> {
    let actor = ActorId::from_signing_key(signing_key);
    let wire = {
        let mut durable = durable.lock().await;
        durable.capabilities = durable.room.capabilities_for(actor);
        durable
            .room
            .sign_presence(signing_key, &durable.capabilities, session, sequence, pitch)
            .map_err(persistence_error)?
    };
    let accepted_sequence = runtime.apply_presence_v5(
        actor,
        session,
        sequence,
        pitch,
        unix_time_millis().saturating_add(1_500),
    );
    if let Err(error) = network.broadcast(wire).await {
        runtime.emit_diagnostic(
            "presence_broadcast",
            &format!("local presence applied; broadcast failed: {error}"),
        );
    }
    Ok(accepted_sequence)
}

async fn commit_replica_command(
    durable: &SharedDurableRoom,
    signing_key: &hhhs_proof::SigningKey,
    command: RoomCommand,
    network: &NativeRoomNetwork,
    runtime: &AppRuntime,
) -> Result<u64, AppError> {
    let (receipt, record, view) = {
        let mut durable = durable.lock().await;
        let actor = ActorId::from_signing_key(signing_key);
        durable.capabilities = durable.room.capabilities_for(actor);
        if durable.capabilities.for_lane(command.lane()).is_empty() {
            return Err(AppError::new(
                AppErrorCode::UnsupportedCapability,
                "this actor has not received a live Room-v5 capability for that Replica",
            ));
        }
        let prepared = durable
            .room
            .prepare_author(signing_key, &durable.capabilities, command)
            .map_err(persistence_error)?;
        let record = prepared.replica_record();
        let receipt = durable
            .room
            .commit_prepared(prepared)
            .map_err(persistence_error)?;
        (receipt, record, durable.room.view())
    };
    let accepted_sequence = runtime.apply_room_view(view);
    let live = ReplicaLiveRecord {
        lane: receipt.lane,
        source: walkie_songie::net::PeerId(*network.endpoint_id().as_bytes()),
        record,
    };
    if let Err(error) = network.broadcast(live.encode()).await {
        runtime.emit_diagnostic(
            "live_record_broadcast",
            &format!("command is durable; fast delivery failed: {error}"),
        );
        let hint = ReplicaRepairHint {
            lane: receipt.lane,
            source: walkie_songie::net::PeerId(*network.endpoint_id().as_bytes()),
            entry: receipt.entry,
        };
        if let Err(error) = network.broadcast(hint.encode()).await {
            runtime.emit_diagnostic(
                "repair_hint_broadcast",
                &format!("command is durable; repair hint failed: {error}"),
            );
        }
    }
    Ok(accepted_sequence)
}

async fn apply_native_live_record(
    durable: &SharedDurableRoom,
    live: ReplicaLiveRecord,
    runtime: &AppRuntime,
    local_actor: ActorId,
) -> Result<bool, AppError> {
    let lane = live.lane;
    let entry = live.record.entry_hash();
    let bytes = live.record.encode();
    let mut repair_host = {
        let durable = durable.lock().await;
        match lane {
            RoomLane::Music => durable.room.music_repair_host(),
            RoomLane::Extension => durable.room.extension_repair_host(),
        }
    };
    let report = hhhs_sync::RepairHost::apply(&mut repair_host, &[(entry, bytes)])
        .await
        .map_err(persistence_error)?;
    let view = {
        let mut durable = durable.lock().await;
        durable.capabilities = durable.room.capabilities_for(local_actor);
        durable.room.view()
    };
    runtime.apply_room_view(view);
    Ok(report.refused.is_empty() && report.admitted.contains(&entry))
}

async fn maybe_grant_peer(
    durable: &SharedDurableRoom,
    signing_key: &hhhs_proof::SigningKey,
    local_actor: ActorId,
    peer: ActorId,
    network: &NativeRoomNetwork,
) -> Result<(), AppError> {
    let invitation = {
        let durable = durable.lock().await;
        if durable.room.owner() != ActorId::from_signing_key(signing_key) || peer == local_actor {
            return Ok(());
        }
        let existing = durable.room.capabilities_for(peer);
        if !existing.music.is_empty() && !existing.extension.is_empty() {
            return Ok(());
        }
        durable
            .room
            .grant_member(signing_key, peer)
            .map_err(persistence_error)?
    };
    for (lane, entries) in [
        (RoomLane::Music, invitation.capabilities.music),
        (RoomLane::Extension, invitation.capabilities.extension),
    ] {
        for entry in entries {
            network
                .broadcast(
                    ReplicaRepairHint {
                        lane,
                        source: walkie_songie::net::PeerId(*network.endpoint_id().as_bytes()),
                        entry,
                    }
                    .encode(),
                )
                .await
                .map_err(network_error)?;
        }
    }
    Ok(())
}

fn spawn_replica_initiator(
    runtime: AppRuntime,
    durable: SharedDurableRoom,
    endpoint: iroh::Endpoint,
    peer: iroh::EndpointId,
    lane: RoomLane,
) {
    tauri::async_runtime::spawn(async move {
        let connection = match endpoint.connect(peer, lane.repair_alpn()).await {
            Ok(connection) => connection,
            Err(error) => {
                runtime.mark_peer_lane_synchronized(peer, lane, false);
                runtime.emit_diagnostic(
                    "replica_repair_dial",
                    &format!(
                        "{} with {peer} failed: {error}",
                        String::from_utf8_lossy(lane.repair_alpn())
                    ),
                );
                return;
            }
        };
        let stream = match IrohSyncStream::open(&connection).await {
            Ok(stream) => stream.owning(connection),
            Err(error) => {
                runtime.mark_peer_lane_synchronized(peer, lane, false);
                runtime.emit_diagnostic("replica_repair_stream", &error.to_string());
                return;
            }
        };
        let mut host = {
            let durable = durable.lock().await;
            match lane {
                RoomLane::Music => durable.room.music_repair_host(),
                RoomLane::Extension => durable.room.extension_repair_host(),
            }
        };
        match drive_replica_initiator(
            stream,
            &TokioTimer,
            &mut host,
            lane,
            hhhs_sync::SessionLimits::default(),
        )
        .await
        {
            Ok(confirmed)
                if confirmed.disposition() == hhhs_sync::RepairDisposition::Synchronized =>
            {
                let view = {
                    let mut durable = durable.lock().await;
                    durable.capabilities = durable
                        .room
                        .capabilities_for(runtime.identity.capability_actor_id());
                    durable.room.view()
                };
                runtime.apply_room_view(view);
                runtime.mark_peer_lane_synchronized(peer, lane, true);
            }
            Ok(confirmed) => {
                runtime.mark_peer_lane_synchronized(peer, lane, false);
                runtime.emit_diagnostic(
                    "replica_repair_incomplete",
                    &format!(
                        "{lane:?} repair with {peer} requires {:?}: {:?}",
                        confirmed.disposition(),
                        confirmed.outcome()
                    ),
                );
            }
            Err(error) => {
                runtime.mark_peer_lane_synchronized(peer, lane, false);
                runtime.emit_diagnostic(
                    "replica_repair",
                    &format!("{lane:?} repair with {peer} failed: {error}"),
                );
            }
        }
    });
}

fn spawn_replica_responder(
    runtime: AppRuntime,
    durable: SharedDurableRoom,
    peer: iroh::EndpointId,
    lane: RoomLane,
    stream: IrohSyncStream,
) {
    tauri::async_runtime::spawn(async move {
        let mut host = {
            let durable = durable.lock().await;
            match lane {
                RoomLane::Music => durable.room.music_repair_host(),
                RoomLane::Extension => durable.room.extension_repair_host(),
            }
        };
        match drive_replica_responder(
            stream,
            &TokioTimer,
            &mut host,
            lane,
            hhhs_sync::SessionLimits::default(),
        )
        .await
        {
            Ok(confirmed)
                if confirmed.disposition() == hhhs_sync::RepairDisposition::Synchronized =>
            {
                let view = {
                    let mut durable = durable.lock().await;
                    durable.capabilities = durable
                        .room
                        .capabilities_for(runtime.identity.capability_actor_id());
                    durable.room.view()
                };
                runtime.apply_room_view(view);
                runtime.mark_peer_lane_synchronized(peer, lane, true);
            }
            Ok(confirmed) => {
                runtime.mark_peer_lane_synchronized(peer, lane, false);
                runtime.emit_diagnostic(
                    "replica_repair_incomplete",
                    &format!(
                        "{lane:?} repair from {peer} requires {:?}: {:?}",
                        confirmed.disposition(),
                        confirmed.outcome()
                    ),
                );
            }
            Err(error) => {
                runtime.mark_peer_lane_synchronized(peer, lane, false);
                runtime.emit_diagnostic(
                    "replica_repair",
                    &format!("{lane:?} repair from {peer} failed: {error}"),
                );
            }
        }
    });
}

fn midi_output_config(tuning: &walkie_songie::Tuning) -> MidiOutputConfig {
    if tuning.supports_standard_note_names() {
        MidiOutputConfig::exact_twelve_tet()
    } else {
        MidiOutputConfig::default()
    }
}

fn midi_error(error: impl std::fmt::Display) -> AppError {
    AppError::new(
        AppErrorCode::MidiUnavailable,
        "native MIDI operation failed",
    )
    .with_detail(error.to_string())
}

fn persistence_error(error: impl std::fmt::Display) -> AppError {
    AppError::new(AppErrorCode::Persistence, "durable room storage failed")
        .with_detail(error.to_string())
}

fn relay_policy_from_environment() -> Result<RelayPolicy, AppError> {
    if let Ok(urls) = std::env::var("WALKIE_RELAY_URLS") {
        let urls: Vec<String> = urls
            .split(',')
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_owned)
            .collect();
        if urls.is_empty() {
            return Err(AppError::new(
                AppErrorCode::NetworkUnavailable,
                "WALKIE_RELAY_URLS does not contain a relay URL",
            ));
        }
        return Ok(RelayPolicy::Custom(urls));
    }
    match std::env::var("WALKIE_RELAY_MODE").as_deref() {
        Ok("disabled") => Ok(RelayPolicy::Disabled),
        Ok("n0") => Ok(RelayPolicy::N0Development),
        Ok("production") | Err(_) => Ok(RelayPolicy::Production),
        Ok(value) => Err(AppError::new(
            AppErrorCode::NetworkUnavailable,
            "unsupported WALKIE_RELAY_MODE",
        )
        .with_detail(value)),
    }
}

fn map_peer_path(path: PeerTransportPath) -> PeerPath {
    match path {
        PeerTransportPath::Connecting => PeerPath::Connecting,
        PeerTransportPath::Direct => PeerPath::Direct,
        PeerTransportPath::Relayed => PeerPath::Relayed,
        PeerTransportPath::Disconnected => PeerPath::Disconnected,
    }
}

fn network_error(error: impl std::fmt::Display) -> AppError {
    AppError::new(
        AppErrorCode::NetworkUnavailable,
        "could not start native Iroh room",
    )
    .with_detail(error.to_string())
}

fn unix_time_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn current_tuning(state: &RuntimeState) -> Result<walkie_songie::Tuning, AppError> {
    state
        .snapshot
        .tuning
        .as_ref()
        .ok_or_else(|| AppError::new(AppErrorCode::UnknownTuning, "room has no active tuning"))?
        .validate("room tuning")
        .map_err(|error| {
            AppError::new(AppErrorCode::InvalidTuning, "stored room tuning is invalid")
                .with_detail(error.to_string())
        })
}

fn emit_locked(state: &mut RuntimeState, event: AppEvent) {
    state.sequence = state.sequence.saturating_add(1);
    let envelope = AppEventEnvelope {
        sequence: state.sequence,
        event,
    };
    state
        .subscribers
        .retain(|subscriber| subscriber.send(envelope.clone()).is_ok());
}

#[tauri::command]
fn register_events(
    on_event: Channel<AppEventEnvelope>,
    runtime: tauri::State<'_, AppRuntime>,
) -> Result<CommandAck, AppError> {
    runtime.register(on_event)
}

#[tauri::command]
async fn dispatch(
    command: ClientCommand,
    runtime: tauri::State<'_, AppRuntime>,
) -> Result<CommandAck, AppError> {
    runtime.dispatch(command).await
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            let identity_path = data_dir.join("identity.seed");
            let identity = WalkieIdentity::load_or_create(&FileSeedStore::new(identity_path))?;
            app.manage(AppRuntime::new(identity, data_dir));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![register_events, dispatch])
        .build(tauri::generate_context!())
        .expect("failed to build walkie-songie desktop");

    app.run(|app_handle, event| {
        if matches!(event, tauri::RunEvent::Exit) {
            let runtime = app_handle.state::<AppRuntime>().inner().clone();
            tauri::async_runtime::block_on(runtime.shutdown());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_peer_sync_requires_every_lane_and_regresses_on_later_failure() {
        let mut sync = PeerSyncState::default();
        assert!(!sync.update_requirements(ProtocolSupport::WALKIE, PeerPath::Direct));
        assert!(!sync.mark_lane(RoomLane::Music, true));
        assert!(sync.mark_lane(RoomLane::Extension, true));

        // A later RetryFresh, policy divergence, transport failure, or other
        // non-synchronized Music result must revoke the peer-wide UI claim
        // without erasing the still-complete Extension lane.
        assert!(!sync.mark_lane(RoomLane::Music, false));
        assert_eq!(sync.complete, RoomLane::Extension.tag());
        assert!(sync.mark_lane(RoomLane::Music, true));

        assert!(!sync.update_requirements(ProtocolSupport::WALKIE, PeerPath::Disconnected,));
        assert_eq!(sync.complete, 0);

        let mut music_only = PeerSyncState::default();
        assert!(!music_only.update_requirements(ProtocolSupport::MUSIC, PeerPath::Relayed));
        assert!(music_only.mark_lane(RoomLane::Music, true));
    }

    #[test]
    fn production_host_exposes_only_room_v5_replica_protocols() {
        for lane in [RoomLane::Music, RoomLane::Extension] {
            let protocol = ReplicaProtocol::Repair(lane);
            assert_eq!(ReplicaProtocol::from_alpn(protocol.alpn()), Some(protocol));
        }
        assert_eq!(ReplicaProtocol::from_alpn(b"tutti/music/courier/1"), None);
    }
}
