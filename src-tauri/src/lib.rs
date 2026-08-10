use std::{
    collections::{BTreeMap, BTreeSet},
    marker::PhantomData,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use hhhs::EntryHash;
use iroh::endpoint::Connection;
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
        CourierFrame, CourierResponder, ExtensionLane, FileSeedStore, IncomingOp, IrohSyncStream,
        LaneIngest, LaneProtocol, LaneSpec, LaneStoreAccess, LaneSyncSource, MusicLane,
        NativeNetworkEvent, NativeRoomNetwork, NativeRoomNetworkConfig, NativeRoomTicketV4, PeerId,
        PeerTransportPath, RelayPolicy, RoomInbound, RoomTopic, SyncApply, SyncError, SyncLimits,
        SyncOutcome, SyncStream, TokioTimer, TrackedDiscardHistory, WalkieIdentity,
        drive_initiator, drive_responder, ingest_pairs, spawn_rendezvous_v4,
    },
    room::{
        lane_journal::FileLaneJournal,
        ops::{AuthorId, OpLanguage, SigningKey, VerifiedOpG, WindowIngest},
        presence::{PresenceBody, SignedPresenceV4},
        store::{RoomView, Store},
        v4::{
            ExtensionLang, ExtensionOp, LaneSet, LocalRoomOp, MusicLang, MusicOp, Room, RoomLane,
        },
    },
};

enum RoomControl {
    Commit {
        op: LocalRoomOp,
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
    pitch_authors: BTreeMap<walkie_songie::TunedDegree, Vec<AuthorId>>,
    subscribers: Vec<Channel<AppEventEnvelope>>,
    active_room: Option<ActiveRoom>,
}

struct MidiRuntime {
    service: NativeMidiService,
    ledger: MidiLedger,
    input: MidiInputTracker,
}

struct DurableRoom {
    room: Room,
    journal: FileLaneJournal,
    courier: BTreeMap<(PeerId, RoomLane), TrackedDiscardHistory>,
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
                pitch_authors: BTreeMap::new(),
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
                snapshot: state.snapshot.clone(),
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
            ClientCommand::EnterRoom { room_name } => self.enter_room(room_name).await,
            ClientCommand::JoinTicket { ticket } => self.join_ticket(ticket).await,
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
                self.submit_durable(ExtensionOp::PutPiece { emoji, pitch }.into())
                    .await
            }
            ClientCommand::MovePiece { piece, pitch } => {
                self.validate_periodic_pitch(pitch)?;
                self.submit_durable(ExtensionOp::MovePiece { piece, pitch }.into())
                    .await
            }
            ClientCommand::RemovePiece { piece } => {
                self.submit_durable(ExtensionOp::RemovePiece { piece }.into())
                    .await
            }
            ClientCommand::SetRoomConfig {
                pieces_locked,
                available_emojis,
            } => {
                self.submit_durable(
                    ExtensionOp::SetConfig {
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

    async fn enter_room(&self, room_name: String) -> Result<CommandAck, AppError> {
        if !is_valid_room_name(&room_name) {
            return Err(AppError::new(
                AppErrorCode::InvalidRoom,
                "room names use the form adjective-noun-noun",
            ));
        }
        let topic = RoomTopic::from_room_name_v4(&room_name);
        let config = NativeRoomNetworkConfig {
            topic,
            relay: relay_policy_from_environment()?,
            bootstrap: None,
            bootstrap_lanes: None,
        };
        self.start_room(Some(room_name), config, DiscoverySource::Mdns)
            .await
    }

    async fn join_ticket(&self, encoded: String) -> Result<CommandAck, AppError> {
        let ticket = encoded.parse::<NativeRoomTicketV4>().map_err(|error| {
            AppError::new(AppErrorCode::InvalidTicket, "invalid room ticket")
                .with_detail(error.to_string())
        })?;
        let config = NativeRoomNetworkConfig {
            topic: ticket.topic(),
            relay: relay_policy_from_environment()?,
            bootstrap: Some(ticket.endpoint_addr().clone()),
            bootstrap_lanes: Some(ticket.lanes()),
        };
        self.start_room(None, config, DiscoverySource::Ticket).await
    }

    async fn start_room(
        &self,
        room_name: Option<String>,
        config: NativeRoomNetworkConfig,
        bootstrap_source: DiscoverySource,
    ) -> Result<CommandAck, AppError> {
        self.stop_active_room().await;

        let topic_string = config.topic.to_string();
        let journal_path = self
            .data_dir
            .join("rooms")
            .join(format!("{topic_string}.v4.ops"));
        let (journal, recovered) =
            FileLaneJournal::open(journal_path).map_err(persistence_error)?;
        let room = Room::recover(&topic_string, &recovered).map_err(|error| {
            persistence_error(format!("stored operation failed recovery: {error}"))
        })?;
        let pending = room.music().pending_len() + room.extension().pending_len();
        if pending != 0 {
            return Err(persistence_error(format!(
                "{} stored operations are missing causal predecessors",
                pending
            )));
        }
        let recovered_view = room.view();
        let durable = Arc::new(tokio::sync::Mutex::new(DurableRoom {
            room,
            journal,
            courier: BTreeMap::new(),
        }));

        let bootstrap = config.bootstrap.as_ref().map(|address| address.id);
        let bootstrap_lanes = config.bootstrap_lanes;
        let mut network = NativeRoomNetwork::bind(self.identity.iroh_secret(), config.clone())
            .await
            .map_err(network_error)?;
        let ticket = network.settle_ticket(Duration::from_millis(750)).await;
        let ticket_string = ticket.to_string();

        // ---- topic rendezvous: auto-peer everyone in this room by code ----
        // Room-NAME path only (no bootstrap ticket); additive to the ticket
        // flow. This also gives native cross-network peering — mDNS is LAN-only.
        // Discovered ids flow to the room loop below, which seeds the peer map
        // (the rendezvous task itself already fed MemoryLookup + gossip).
        let (rendezvous_guard, mut rendezvous_rx) = if bootstrap.is_none() {
            let (rdv_tx, rdv_rx) = mpsc::channel::<(iroh::EndpointId, LaneSet)>(64);
            let handle = spawn_rendezvous_v4(
                network.rendezvous_peering(),
                config.topic,
                LaneSet::WALKIE,
                move |endpoint_id, lanes| {
                    let _ = rdv_tx.try_send((endpoint_id, lanes));
                },
            );
            (Some(handle), Some(rdv_rx))
        } else {
            (None, None)
        };

        let (control, mut control_rx) = mpsc::channel(64);
        let runtime = self.clone();
        let signing_key = self.identity.signing_key();
        let signed_topic = topic_string.clone();
        let presence_topic = *config.topic.as_bytes();
        let midi_events = self.lock_midi()?.service.input_events();
        let network_handle = NativeRoomNetworkHandle {
            endpoint: network.endpoint().clone(),
        };
        let sync_progress = Arc::new(tokio::sync::Mutex::new(PeerSyncProgress::default()));
        let task = tauri::async_runtime::spawn(async move {
            // Own the rendezvous task for the room's lifetime; dropping it when
            // this task ends (below) aborts the signaling connection.
            let _rendezvous_guard = rendezvous_guard;
            let mut local_presence_session = 0_u64;
            let mut local_presence_sequence = 0_u64;
            // author -> (session, sequence, issued_at_ms, local_expires_at_ms)
            let mut presence_order: BTreeMap<AuthorId, (u64, u64, u64, u64)> = BTreeMap::new();
            let mut peers: BTreeMap<iroh::EndpointId, (DiscoverySource, PeerPath)> =
                BTreeMap::new();
            let mut peer_lanes: BTreeMap<iroh::EndpointId, LaneSet> = BTreeMap::new();
            if let Some(endpoint_id) = bootstrap {
                peers.insert(endpoint_id, (bootstrap_source, PeerPath::Connecting));
                if let Some(lanes) = bootstrap_lanes {
                    peer_lanes.insert(endpoint_id, lanes);
                }
                runtime.update_peer(endpoint_id, bootstrap_source, PeerPath::Connecting, false);
            }

            let mut path_refresh = tokio::time::interval(Duration::from_secs(1));
            path_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut presence_refresh = tokio::time::interval(Duration::from_millis(250));
            presence_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut midi_refresh = tokio::time::interval(Duration::from_secs(2));
            midi_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // Gossip is intentionally lossy. Give every room a stable, slightly
            // jittered anti-entropy cadence so a missed operation converges even
            // when membership never changes and no Lagged event is surfaced.
            let repair_period =
                Duration::from_secs(25 + u64::from(network.endpoint_id().as_bytes()[0] % 11));
            let mut repair_refresh = tokio::time::interval_at(
                tokio::time::Instant::now() + repair_period,
                repair_period,
            );
            repair_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    control = control_rx.recv() => {
                        match control {
                            Some(RoomControl::Commit { op, response }) => {
                                let result = commit_room_op(
                                    &durable,
                                    &signing_key,
                                    &signed_topic,
                                    op,
                                    &network,
                                    &runtime,
                                )
                                .await;
                                let _ = response.send(result.map(|accepted_sequence| {
                                    CommandAck { accepted_sequence }
                                }));
                            }
                            Some(RoomControl::Presence {
                                session,
                                pitch,
                                response,
                            }) => {
                                if session == local_presence_session {
                                    local_presence_sequence =
                                        local_presence_sequence.saturating_add(1);
                                } else {
                                    local_presence_session = session;
                                    local_presence_sequence = 0;
                                }
                                let now = unix_time_millis();
                                match SignedPresenceV4::sign(
                                    &signing_key,
                                    PresenceBody::new_v4(
                                        presence_topic,
                                        session,
                                        local_presence_sequence,
                                        now,
                                        pitch,
                                    ),
                                ) {
                                    Ok(signed) => {
                                        let body = signed
                                            .verify(presence_topic)
                                            .expect("just-signed presence verifies")
                                            .body;
                                        let expires = now.saturating_add(u64::from(body.lease_ms));
                                        let author = AuthorId(
                                            *signing_key.verifying_key().as_bytes()
                                        );
                                        presence_order.insert(
                                            author,
                                            (
                                                body.session,
                                                body.sequence,
                                                body.issued_at_ms,
                                                expires,
                                            ),
                                        );
                                        let accepted_sequence = runtime.apply_presence(
                                            author,
                                            body,
                                            expires,
                                        );
                                        if let Ok(bytes) = signed.to_wire_bytes()
                                            && let Err(error) = network.broadcast(bytes).await
                                        {
                                            runtime.emit_diagnostic(
                                                "presence_broadcast",
                                                &error.to_string(),
                                            );
                                        }
                                        let _ = response.send(Ok(CommandAck {
                                            accepted_sequence,
                                        }));
                                    }
                                    Err(error) => {
                                        let _ = response.send(Err(
                                            AppError::new(
                                                AppErrorCode::InvalidCommand,
                                                "could not sign voice presence",
                                            )
                                            .with_detail(error.to_string()),
                                        ));
                                    }
                                }
                            }
                            Some(RoomControl::Shutdown) | None => break,
                        }
                    }
                    _ = presence_refresh.tick() => {
                        let now = unix_time_millis();
                        let expired: Vec<_> = presence_order
                            .iter()
                            .filter(|(_, (_, _, _, expires))| *expires <= now)
                            .map(|(author, _)| *author)
                            .collect();
                        for author in expired {
                            if let Some((session, _, _, _)) =
                                presence_order.remove(&author)
                            {
                                runtime.expire_presence(author, session);
                            }
                        }
                    }
                    _ = midi_refresh.tick() => {
                        match runtime.refresh_midi_devices() {
                            Ok(actions) => {
                                for action in actions {
                                    let op = match action {
                                        HeldInputAction::DegreeActivated(pitch) =>
                                            MusicOp::AddDegree { degree: pitch }.into(),
                                        HeldInputAction::DegreeReleased(pitch) =>
                                            MusicOp::RemoveDegree { degree: pitch }.into(),
                                    };
                                    if let Err(error) = commit_room_op(
                                        &durable,
                                        &signing_key,
                                        &signed_topic,
                                        op,
                                        &network,
                                        &runtime,
                                    )
                                    .await
                                    {
                                        runtime.emit_diagnostic(
                                            "midi_persistence",
                                            &error.message,
                                        );
                                    }
                                }
                            }
                            Err(error) => runtime.emit_diagnostic(
                                "midi_refresh",
                                &error.message,
                            ),
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
                            let op = match action {
                                HeldInputAction::DegreeActivated(pitch) =>
                                    MusicOp::AddDegree { degree: pitch }.into(),
                                HeldInputAction::DegreeReleased(pitch) =>
                                    MusicOp::RemoveDegree { degree: pitch }.into(),
                            };
                            if let Err(error) = commit_room_op(
                                &durable,
                                &signing_key,
                                &signed_topic,
                                op,
                                &network,
                                &runtime,
                            )
                            .await
                            {
                                runtime.emit_diagnostic(
                                    "midi_persistence",
                                    &error.message,
                                );
                            }
                        }
                    }
                    _ = path_refresh.tick() => {
                        for endpoint_id in peers.keys().copied().collect::<Vec<_>>() {
                            let path = map_peer_path(network.peer_path(endpoint_id).await);
                            let Some((source, previous)) = peers.get_mut(&endpoint_id) else {
                                continue;
                            };
                            if *previous != path {
                                *previous = path;
                                runtime.update_peer(
                                    endpoint_id,
                                    *source,
                                    path,
                                    false,
                                );
                            }
                        }
                    }
                    _ = repair_refresh.tick() => {
                        for endpoint_id in peers
                            .iter()
                            .filter(|(_, (_, path))| *path != PeerPath::Disconnected)
                            .map(|(endpoint_id, _)| *endpoint_id)
                            .collect::<Vec<_>>()
                        {
                            if network.endpoint_id().as_bytes() < endpoint_id.as_bytes() {
                                spawn_repair_round(
                                    runtime.clone(),
                                    durable.clone(),
                                    sync_progress.clone(),
                                    peer_lanes.get(&endpoint_id).copied(),
                                    signed_topic.clone(),
                                    endpoint_id,
                                    network_handle.clone(),
                                );
                            }
                        }
                    }
                    // Topic-rendezvous discovery. Seed the peer map like an mDNS
                    // discovery; the rendezvous task already fed MemoryLookup +
                    // gossip join_peers. The `pending` branch is inert when this
                    // is a ticket join (no rendezvous); latching the receiver to
                    // `None` on close keeps a drained channel from spinning.
                    discovered = async {
                        match rendezvous_rx.as_mut() {
                            Some(rx) => rx.recv().await,
                            None => std::future::pending::<Option<(iroh::EndpointId, LaneSet)>>().await,
                        }
                    } => {
                        match discovered {
                            Some((endpoint_id, lanes)) => {
                                peer_lanes.insert(endpoint_id, lanes);
                                let inserted = !peers.contains_key(&endpoint_id);
                                peers.entry(endpoint_id).or_insert((
                                    DiscoverySource::AddressLookup,
                                    PeerPath::Connecting,
                                ));
                                if inserted {
                                    runtime.update_peer(
                                        endpoint_id,
                                        DiscoverySource::AddressLookup,
                                        PeerPath::Connecting,
                                        false,
                                    );
                                }
                            }
                            None => rendezvous_rx = None,
                        }
                    }
                    // Repair arrives on its OWN bounded queue inside
                    // `next_inbound`, so a peer opening repair sessions can never
                    // head-of-line block op delivery, and each session is spawned
                    // rather than driven here — the room loop must keep serving
                    // commits and gossip while anti-entropy runs.
                    inbound = network.next_inbound() => {
                        let Some(inbound) = inbound else {
                            runtime.emit_diagnostic(
                                "native_network_closed",
                                "the native Iroh room task closed",
                            );
                            break;
                        };
                        let event = match inbound {
                            RoomInbound::Repair(repair) => {
                                let expected = peer_lanes
                                    .get(&repair.endpoint_id)
                                    .copied()
                                    .unwrap_or(LaneSet::WALKIE);
                                match LaneProtocol::from_alpn(repair.alpn) {
                                    Some(LaneProtocol::Repair(RoomLane::Music)) => {
                                        spawn_repair::<MusicLane>(
                                            runtime.clone(),
                                            durable.clone(),
                                            sync_progress.clone(),
                                            expected,
                                            signed_topic.clone(),
                                            repair.endpoint_id,
                                            repair.connection,
                                            repair.stream,
                                        );
                                    }
                                    Some(LaneProtocol::Repair(RoomLane::Extension)) => {
                                        spawn_repair::<ExtensionLane>(
                                            runtime.clone(),
                                            durable.clone(),
                                            sync_progress.clone(),
                                            expected,
                                            signed_topic.clone(),
                                            repair.endpoint_id,
                                            repair.connection,
                                            repair.stream,
                                        );
                                    }
                                    Some(LaneProtocol::Courier(RoomLane::Music)) => {
                                        spawn_courier::<MusicLane>(
                                            runtime.clone(),
                                            durable.clone(),
                                            repair.endpoint_id,
                                            repair.connection,
                                            repair.stream,
                                        );
                                    }
                                    Some(LaneProtocol::Courier(RoomLane::Extension)) => {
                                        spawn_courier::<ExtensionLane>(
                                            runtime.clone(),
                                            durable.clone(),
                                            repair.endpoint_id,
                                            repair.connection,
                                            repair.stream,
                                        );
                                    }
                                    None => repair
                                        .connection
                                        .close(4u32.into(), b"unsupported lane protocol"),
                                }
                                continue;
                            }
                            RoomInbound::Event(event) => event,
                        };
                        match event {
                            NativeNetworkEvent::MdnsDiscovered { endpoint_id } => {
                                peers
                                    .entry(endpoint_id)
                                    .and_modify(|peer| peer.0 = DiscoverySource::Mdns)
                                    .or_insert((DiscoverySource::Mdns, PeerPath::Connecting));
                                runtime.update_peer(
                                    endpoint_id,
                                    DiscoverySource::Mdns,
                                    PeerPath::Connecting,
                                    false,
                                );
                            }
                            NativeNetworkEvent::MdnsExpired { endpoint_id } => {
                                // Expiry removes LAN discovery, not necessarily an
                                // already-active gossip/relay connection.
                                if peers.get(&endpoint_id).is_some_and(|peer| {
                                    peer.0 == DiscoverySource::Mdns
                                        && peer.1 == PeerPath::Disconnected
                                }) {
                                    peers.remove(&endpoint_id);
                                    runtime.remove_peer(endpoint_id);
                                }
                            }
                            NativeNetworkEvent::NeighborUp {
                                endpoint_id,
                                discovery,
                            } => {
                                let source = peers
                                    .get(&endpoint_id)
                                    .map(|peer| peer.0)
                                    .unwrap_or(discovery);
                                let path = map_peer_path(network.peer_path(endpoint_id).await);
                                peers.insert(endpoint_id, (source, path));
                                runtime.update_peer(endpoint_id, source, path, false);
                                if network.endpoint_id().as_bytes()
                                    < endpoint_id.as_bytes()
                                {
                                    let advertised = peer_lanes.get(&endpoint_id).copied();
                                    spawn_repair_round(
                                        runtime.clone(),
                                        durable.clone(),
                                        sync_progress.clone(),
                                        advertised,
                                        signed_topic.clone(),
                                        endpoint_id,
                                        network_handle.clone(),
                                    );
                                }
                            }
                            NativeNetworkEvent::NeighborDown { endpoint_id } => {
                                sync_progress.lock().await.clear(endpoint_id);
                                if let Some((source, path)) = peers.get_mut(&endpoint_id) {
                                    *path = PeerPath::Disconnected;
                                    runtime.update_peer(
                                        endpoint_id,
                                        *source,
                                        PeerPath::Disconnected,
                                        false,
                                    );
                                }
                            }
                            NativeNetworkEvent::Message {
                                delivered_from,
                                bytes,
                            } => {
                                if bytes.starts_with(MusicLang::WIRE_MAGIC) {
                                    let mut access = DurableLaneAccess::<MusicLane>::new(
                                        durable.clone(),
                                        runtime.clone(),
                                    );
                                    if let Err(error) = access
                                        .apply_gossip(&signed_topic, &bytes)
                                        .await
                                    {
                                        runtime.emit_diagnostic(
                                            "gossip_rejected",
                                            &format!(
                                                "music frame from {delivered_from} was refused: {error}"
                                            ),
                                        );
                                    }
                                } else if bytes.starts_with(ExtensionLang::WIRE_MAGIC) {
                                    let mut access = DurableLaneAccess::<ExtensionLane>::new(
                                        durable.clone(),
                                        runtime.clone(),
                                    );
                                    if let Err(error) = access
                                        .apply_gossip(&signed_topic, &bytes)
                                        .await
                                    {
                                        runtime.emit_diagnostic(
                                            "gossip_rejected",
                                            &format!(
                                                "extension frame from {delivered_from} was refused: {error}"
                                            ),
                                        );
                                    }
                                } else if let Ok(signed) =
                                    SignedPresenceV4::from_wire_bytes(&bytes)
                                {
                                    match signed.verify(presence_topic) {
                                        Ok(verified) => {
                                            let body = verified.body;
                                            let now = unix_time_millis();
                                            let future_limit =
                                                now.saturating_add(30_000);
                                            let order = presence_order
                                                .get(&verified.author)
                                                .copied();
                                            let is_newer = match order {
                                                None => true,
                                                Some((session, sequence, _issued, _))
                                                    if session == body.session =>
                                                {
                                                    body.sequence > sequence
                                                }
                                                Some((_, _, issued, _)) => {
                                                    body.issued_at_ms > issued
                                                }
                                            };
                                            if body.issued_at_ms <= future_limit
                                                && is_newer
                                                && runtime.presence_pitch_is_valid(
                                                    body.pitch,
                                                )
                                            {
                                                let expires = now.saturating_add(
                                                    u64::from(body.lease_ms),
                                                );
                                                presence_order.insert(
                                                    verified.author,
                                                    (
                                                        body.session,
                                                        body.sequence,
                                                        body.issued_at_ms,
                                                        expires,
                                                    ),
                                                );
                                                runtime.apply_presence(
                                                    verified.author,
                                                    body,
                                                    expires,
                                                );
                                            }
                                        }
                                        Err(error) => runtime.emit_diagnostic(
                                            "presence_rejected",
                                            &error.to_string(),
                                        ),
                                    }
                                } else {
                                    runtime.emit_diagnostic(
                                        "gossip_rejected",
                                        "unknown or malformed room message",
                                    );
                                }
                            }
                            NativeNetworkEvent::Lagged => {
                                runtime.emit_diagnostic(
                                    "gossip_lagged",
                                    "the gossip event consumer lagged; scheduling anti-entropy repair",
                                );
                                for endpoint_id in peers
                                    .iter()
                                    .filter(|(_, (_, path))| *path != PeerPath::Disconnected)
                                    .map(|(endpoint_id, _)| *endpoint_id)
                                    .collect::<Vec<_>>()
                                {
                                    if network.endpoint_id().as_bytes() < endpoint_id.as_bytes() {
                                        spawn_repair_round(
                                            runtime.clone(),
                                            durable.clone(),
                                            sync_progress.clone(),
                                            peer_lanes.get(&endpoint_id).copied(),
                                            signed_topic.clone(),
                                            endpoint_id,
                                            network_handle.clone(),
                                        );
                                    }
                                }
                            }
                            NativeNetworkEvent::Closed => break,
                            NativeNetworkEvent::Diagnostic(message) => runtime.emit_diagnostic(
                                "native_network",
                                &message,
                            ),
                        }
                    }
                }
            }
            if let Err(error) = network.shutdown().await {
                runtime.emit_diagnostic("native_network_shutdown", &error.to_string());
            }
        });

        let mut state = self.lock()?;
        state.snapshot.room_name = room_name.clone();
        state.snapshot.room_topic = Some(topic_string.clone());
        state.snapshot.room_ticket = Some(ticket_string.clone());
        state.snapshot.active_degrees.clear();
        state.pitch_authors.clear();
        state.snapshot.pieces.clear();
        state.snapshot.pieces_locked = false;
        state.snapshot.available_emojis = None;
        state.snapshot.voices.clear();
        state.snapshot.peers.clear();
        state.snapshot.tuning = Some(walkie_songie::TuningDefinition::twelve_tet());
        state.snapshot.tuning_id = state
            .snapshot
            .tuning
            .as_ref()
            .map(|definition| definition.id);
        state.active_room = Some(ActiveRoom { control, task });
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
        state.snapshot.active_degrees.clear();
        state.pitch_authors.clear();
        state.snapshot.pieces.clear();
        state.snapshot.pieces_locked = false;
        state.snapshot.available_emojis = None;
        state.snapshot.voices.clear();
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

    async fn submit_durable(&self, op: LocalRoomOp) -> Result<CommandAck, AppError> {
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
            .send(RoomControl::Commit { op, response })
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
        let (snapshot, pitch_authors) = match self.lock() {
            Ok(state) => (state.snapshot.clone(), state.pitch_authors.clone()),
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
        for pitch in &snapshot.active_degrees {
            let periodic = walkie_songie::TunedPeriodicPitch {
                tuning_id: pitch.tuning_id,
                pitch: walkie_songie::PeriodicPitch::from_degree(pitch.degree, 0),
            };
            for author in pitch_authors.get(pitch).into_iter().flatten() {
                desired.insert(
                    MidiSource::DurableDegree {
                        author: *author,
                        pitch: *pitch,
                    },
                    periodic,
                );
            }
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

        if state.snapshot.tuning != view.tuning {
            state.snapshot.tuning = view.tuning.clone();
            state.snapshot.tuning_id = view.tuning.as_ref().map(|definition| definition.id);
            if let Some(definition) = view.tuning.clone() {
                events.push(AppEvent::TuningChanged { definition });
            }
        }

        let old_degrees = state.snapshot.active_degrees.clone();
        let new_degrees: Vec<_> = view.pitches.iter().copied().collect();
        for pitch in &old_degrees {
            if !view.pitches.contains(pitch) {
                events.push(AppEvent::DegreeRemoved { pitch: *pitch });
            }
        }
        let new_authors: BTreeMap<_, Vec<_>> = view
            .pitch_authors
            .iter()
            .map(|(pitch, authors)| (*pitch, authors.iter().copied().collect()))
            .collect();
        for pitch in &new_degrees {
            let authors: Vec<_> = view
                .pitch_authors
                .get(pitch)
                .into_iter()
                .flatten()
                .copied()
                .collect();
            if state.pitch_authors.get(pitch) != Some(&authors) {
                events.push(AppEvent::DegreeAdded {
                    pitch: *pitch,
                    authors,
                });
            }
        }
        state.snapshot.active_degrees = new_degrees;
        state.pitch_authors = new_authors;

        let new_pieces: Vec<_> = view
            .pieces
            .values()
            .map(|piece| walkie_songie::client::PieceSnapshot {
                id: piece.id,
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

    fn presence_pitch_is_valid(&self, pitch: Option<walkie_songie::TunedPeriodicPitch>) -> bool {
        let Ok(state) = self.lock() else {
            return false;
        };
        let Ok(tuning) = current_tuning(&state) else {
            return false;
        };
        pitch.is_none_or(|pitch| pitch.validate(&tuning).is_ok())
    }

    fn apply_presence(&self, author: AuthorId, body: PresenceBody, expires_at_ms: u64) -> u64 {
        let Ok(mut state) = self.lock() else {
            return 0;
        };
        match body.pitch {
            Some(pitch) => {
                let voice = walkie_songie::client::VoiceSnapshot {
                    author,
                    session: body.session,
                    sequence: body.sequence,
                    pitch: Some(pitch),
                    expires_at_ms,
                };
                if let Some(existing) = state
                    .snapshot
                    .voices
                    .iter_mut()
                    .find(|voice| voice.author == author)
                {
                    *existing = voice.clone();
                } else {
                    state.snapshot.voices.push(voice.clone());
                    state.snapshot.voices.sort_by_key(|voice| voice.author);
                }
                emit_locked(&mut state, AppEvent::VoiceUpdated { voice });
            }
            None => {
                state.snapshot.voices.retain(|voice| voice.author != author);
                emit_locked(
                    &mut state,
                    AppEvent::VoiceExpired {
                        author,
                        session: body.session,
                    },
                );
            }
        }
        drop(state);
        self.sync_midi_from_snapshot();
        self.lock().map(|state| state.sequence).unwrap_or(0)
    }

    fn expire_presence(&self, author: AuthorId, session: u64) {
        let Ok(mut state) = self.lock() else {
            return;
        };
        let was_present = state
            .snapshot
            .voices
            .iter()
            .any(|voice| voice.author == author && voice.session == session);
        state
            .snapshot
            .voices
            .retain(|voice| voice.author != author || voice.session != session);
        if was_present {
            emit_locked(&mut state, AppEvent::VoiceExpired { author, session });
        }
        drop(state);
        self.sync_midi_from_snapshot();
    }

    fn update_peer(
        &self,
        endpoint_id: iroh::EndpointId,
        discovery: DiscoverySource,
        path: PeerPath,
        synchronized: bool,
    ) {
        let Ok(mut state) = self.lock() else {
            return;
        };
        let author = AuthorId(*endpoint_id.as_bytes());
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
            if path != PeerPath::Disconnected {
                peer.synchronized |= existing.synchronized;
                peer.round_trip_ms = existing.round_trip_ms;
            }
            *existing = peer.clone();
        } else {
            state.snapshot.peers.push(peer.clone());
            state.snapshot.peers.sort_by_key(|peer| peer.author);
        }
        emit_locked(&mut state, AppEvent::PeerUpdated { peer });
    }

    fn update_peer_rtt(&self, endpoint_id: iroh::EndpointId, rtt: Duration) {
        let Ok(mut state) = self.lock() else {
            return;
        };
        let author = AuthorId(*endpoint_id.as_bytes());
        let Some(peer) = state
            .snapshot
            .peers
            .iter_mut()
            .find(|peer| peer.author == author)
        else {
            return;
        };
        peer.round_trip_ms = Some(u32::try_from(rtt.as_millis()).unwrap_or(u32::MAX));
        let event = peer.clone();
        emit_locked(&mut state, AppEvent::PeerUpdated { peer: event });
    }

    fn remove_peer(&self, endpoint_id: iroh::EndpointId) {
        let Ok(mut state) = self.lock() else {
            return;
        };
        let author = AuthorId(*endpoint_id.as_bytes());
        state.snapshot.peers.retain(|peer| peer.author != author);
        emit_locked(&mut state, AppEvent::PeerRemoved { author });
    }

    fn mark_peer_synchronized(&self, endpoint_id: iroh::EndpointId) {
        let Ok(mut state) = self.lock() else {
            return;
        };
        let author = AuthorId(*endpoint_id.as_bytes());
        let Some(peer) = state
            .snapshot
            .peers
            .iter_mut()
            .find(|peer| peer.author == author)
        else {
            return;
        };
        if peer.synchronized {
            return;
        }
        peer.synchronized = true;
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

trait AppLane: LaneSpec {
    const LANE: RoomLane;

    fn store(room: &Room) -> &Store<Self::Lang>;
    fn ingest(room: &mut Room, op: VerifiedOpG<Self::Lang>) -> WindowIngest;
}

impl AppLane for MusicLane {
    const LANE: RoomLane = RoomLane::Music;

    fn store(room: &Room) -> &Store<MusicLang> {
        room.music()
    }

    fn ingest(room: &mut Room, op: VerifiedOpG<MusicLang>) -> WindowIngest {
        WindowIngest {
            lifted: room.ingest_music(op),
            courier: Vec::new(),
        }
    }
}

impl AppLane for ExtensionLane {
    const LANE: RoomLane = RoomLane::Extension;

    fn store(room: &Room) -> &Store<ExtensionLang> {
        room.extension()
    }

    fn ingest(room: &mut Room, op: VerifiedOpG<ExtensionLang>) -> WindowIngest {
        WindowIngest {
            lifted: room.ingest_extension(op),
            courier: Vec::new(),
        }
    }
}

struct DurableLaneSink<'a, P: AppLane> {
    durable: &'a mut DurableRoom,
    lane: PhantomData<P>,
}

impl<P: AppLane> LaneIngest<P::Lang> for DurableLaneSink<'_, P> {
    fn lifted_entry(&self, id: walkie_songie::room::ops::OpId) -> Option<EntryHash> {
        P::store(&self.durable.room).lifted_entry(id)
    }

    fn knows_op(&self, id: walkie_songie::room::ops::OpId) -> bool {
        P::store(&self.durable.room).knows_op(id)
    }

    fn ingest_lane(
        &mut self,
        wire: &[u8],
        op: VerifiedOpG<P::Lang>,
    ) -> Result<WindowIngest, SyncError> {
        self.durable
            .journal
            .append(P::LANE, wire)
            .map_err(|error| SyncError::Persistence(error.to_string()))?;
        Ok(P::ingest(&mut self.durable.room, op))
    }
}

/// Lane-generic durable access. The room lock is held only while capturing or
/// applying one batch, never across a network round trip.
struct DurableLaneAccess<P: AppLane> {
    durable: SharedDurableRoom,
    runtime: AppRuntime,
    lane: PhantomData<P>,
}

impl<P: AppLane> DurableLaneAccess<P> {
    fn new(durable: SharedDurableRoom, runtime: AppRuntime) -> Self {
        Self {
            durable,
            runtime,
            lane: PhantomData,
        }
    }

    async fn apply_gossip(&mut self, topic: &str, wire: &[u8]) -> Result<usize, SyncError> {
        let (report, view) = {
            let mut durable = self.durable.lock().await;
            let mut sink = DurableLaneSink::<P> {
                durable: &mut durable,
                lane: PhantomData,
            };
            let report = ingest_pairs::<P::Lang, _>(
                &mut sink,
                topic,
                [IncomingOp {
                    claimed_entry: None,
                    wire,
                }],
            )?;
            let view = durable.room.view();
            (report, view)
        };
        if !report.lifted.is_empty() {
            self.runtime.apply_room_view(view);
        }
        Ok(report.lifted.len())
    }
}

impl<P: AppLane> LaneStoreAccess<P::Lang> for DurableLaneAccess<P> {
    async fn capture(&mut self, salt: [u8; 16]) -> Result<LaneSyncSource<P::Lang>, SyncError> {
        let durable = self.durable.lock().await;
        Ok(LaneSyncSource::capture(P::store(&durable.room), salt)?)
    }

    async fn apply(
        &mut self,
        topic: &str,
        pairs: &[(EntryHash, Vec<u8>)],
        source: &mut LaneSyncSource<P::Lang>,
    ) -> Result<SyncApply, SyncError> {
        let (report, view) = {
            let mut durable = self.durable.lock().await;
            let report = {
                let mut sink = DurableLaneSink::<P> {
                    durable: &mut durable,
                    lane: PhantomData,
                };
                ingest_pairs::<P::Lang, _>(&mut sink, topic, pairs.iter().map(IncomingOp::from))?
            };
            source.absorb(P::store(&durable.room), &report.lifted)?;
            let view = durable.room.view();
            (report, view)
        };
        if !report.lifted.is_empty() {
            self.runtime.apply_room_view(view);
        }
        Ok(SyncApply {
            admitted: report.admitted,
            lifted: report.lifted.len(),
            courier: report.courier,
        })
    }
}

#[derive(Default)]
struct PeerSyncProgress {
    completed: BTreeMap<iroh::EndpointId, u8>,
    outbound_in_flight: BTreeSet<iroh::EndpointId>,
}

impl PeerSyncProgress {
    fn record(&mut self, peer: iroh::EndpointId, lane: RoomLane, expected: LaneSet) -> bool {
        let completed = self.completed.entry(peer).or_default();
        *completed |= lane.tag();
        *completed & expected.bits() == expected.bits()
    }

    fn clear(&mut self, peer: iroh::EndpointId) {
        self.completed.remove(&peer);
    }

    fn begin_outbound(&mut self, peer: iroh::EndpointId) -> bool {
        self.outbound_in_flight.insert(peer)
    }

    fn finish_outbound(&mut self, peer: iroh::EndpointId) {
        self.outbound_in_flight.remove(&peer);
    }
}

type SharedSyncProgress = Arc<tokio::sync::Mutex<PeerSyncProgress>>;

async fn finish_repair<P: AppLane>(
    result: Result<SyncOutcome, String>,
    runtime: &AppRuntime,
    progress: &SharedSyncProgress,
    expected: LaneSet,
    endpoint_id: iroh::EndpointId,
    telemetry: &Connection,
) {
    match result {
        Ok(outcome) if !outcome.root_mismatch && !outcome.incomplete => {
            if let Some(rtt) = telemetry.rtt(iroh::endpoint::PathId::ZERO) {
                runtime.update_peer_rtt(endpoint_id, rtt);
            }
            if progress.lock().await.record(endpoint_id, P::LANE, expected) {
                runtime.mark_peer_synchronized(endpoint_id);
            }
            runtime.emit_diagnostic(
                "repair_complete",
                &format!(
                    "{} repair with {endpoint_id} completed; ingested {} operations",
                    String::from_utf8_lossy(P::ALPN),
                    outcome.ingested,
                ),
            );
        }
        Ok(outcome) => runtime.emit_diagnostic(
            "repair_incomplete",
            &format!(
                "{} repair with {endpoint_id} ended without convergence (root_mismatch={}, incomplete={})",
                String::from_utf8_lossy(P::ALPN),
                outcome.root_mismatch,
                outcome.incomplete,
            ),
        ),
        Err(error) => runtime.emit_diagnostic(
            "repair_failed",
            &format!(
                "{} repair with {endpoint_id} failed: {error}",
                String::from_utf8_lossy(P::ALPN)
            ),
        ),
    }
}

fn spawn_repair<P: AppLane>(
    runtime: AppRuntime,
    durable: SharedDurableRoom,
    progress: SharedSyncProgress,
    expected: LaneSet,
    signed_topic: String,
    endpoint_id: iroh::EndpointId,
    connection: Connection,
    stream: IrohSyncStream,
) {
    tauri::async_runtime::spawn(async move {
        let telemetry = connection.clone();
        let result = run_repair_session::<P>(
            stream.owning(connection),
            false,
            durable,
            signed_topic,
            runtime.clone(),
        )
        .await;
        finish_repair::<P>(
            result,
            &runtime,
            &progress,
            expected,
            endpoint_id,
            &telemetry,
        )
        .await;
    });
}

fn spawn_repair_round(
    runtime: AppRuntime,
    durable: SharedDurableRoom,
    progress: SharedSyncProgress,
    advertised: Option<LaneSet>,
    signed_topic: String,
    endpoint_id: iroh::EndpointId,
    network: NativeRoomNetworkHandle,
) {
    tauri::async_runtime::spawn(async move {
        if !progress.lock().await.begin_outbound(endpoint_id) {
            return;
        }
        let attempted = advertised.unwrap_or(LaneSet::WALKIE);
        let music_network = network.clone();
        let music_durable = durable.clone();
        let music_topic = signed_topic.clone();
        let music_runtime = runtime.clone();
        let music = async move {
            if attempted.contains(RoomLane::Music) {
                dial_and_run::<MusicLane>(
                    &music_network,
                    endpoint_id,
                    music_durable,
                    music_topic,
                    music_runtime,
                )
                .await
            } else {
                None
            }
        };
        let extension_network = network;
        let extension_runtime = runtime.clone();
        let extension = async move {
            if attempted.contains(RoomLane::Extension) {
                dial_and_run::<ExtensionLane>(
                    &extension_network,
                    endpoint_id,
                    durable,
                    signed_topic,
                    extension_runtime,
                )
                .await
            } else {
                None
            }
        };
        let (music, extension) = tokio::join!(music, extension);
        let negotiated_bits = u8::from(music.is_some()) * RoomLane::Music.tag()
            | u8::from(extension.is_some()) * RoomLane::Extension.tag();
        if let Some(expected) = advertised.or_else(|| LaneSet::from_bits(negotiated_bits)) {
            if let Some((telemetry, result)) = music {
                finish_repair::<MusicLane>(
                    result,
                    &runtime,
                    &progress,
                    expected,
                    endpoint_id,
                    &telemetry,
                )
                .await;
            }
            if let Some((telemetry, result)) = extension {
                finish_repair::<ExtensionLane>(
                    result,
                    &runtime,
                    &progress,
                    expected,
                    endpoint_id,
                    &telemetry,
                )
                .await;
            }
        }
        progress.lock().await.finish_outbound(endpoint_id);
    });
}

/// A cloneable subset of the native network used by concurrent repair dials.
#[derive(Clone)]
struct NativeRoomNetworkHandle {
    endpoint: iroh::Endpoint,
}

impl NativeRoomNetworkHandle {
    async fn dial(
        &self,
        endpoint_id: iroh::EndpointId,
        protocol: LaneProtocol,
    ) -> Result<(Connection, IrohSyncStream), String> {
        let connection = self
            .endpoint
            .connect(endpoint_id, protocol.alpn())
            .await
            .map_err(|error| error.to_string())?;
        let stream = IrohSyncStream::open(&connection)
            .await
            .map_err(|error| error.to_string())?;
        Ok((connection.clone(), stream.owning(connection)))
    }
}

async fn dial_and_run<P: AppLane>(
    network: &NativeRoomNetworkHandle,
    endpoint_id: iroh::EndpointId,
    durable: SharedDurableRoom,
    signed_topic: String,
    runtime: AppRuntime,
) -> Option<(Connection, Result<SyncOutcome, String>)> {
    let (connection, stream) = match network
        .dial(endpoint_id, LaneProtocol::Repair(P::LANE))
        .await
    {
        Ok(opened) => opened,
        Err(error) => {
            runtime.emit_diagnostic(
                "repair_connect",
                &format!(
                    "{} repair connection to {endpoint_id} failed: {error}",
                    String::from_utf8_lossy(P::ALPN)
                ),
            );
            return None;
        }
    };
    let result = run_repair_session::<P>(stream, true, durable, signed_topic, runtime).await;
    Some((connection, result))
}

async fn run_repair_session<P: AppLane>(
    stream: IrohSyncStream,
    initiator: bool,
    durable: SharedDurableRoom,
    signed_topic: String,
    runtime: AppRuntime,
) -> Result<SyncOutcome, String> {
    let mut access = DurableLaneAccess::<P>::new(durable, runtime);
    let limits = SyncLimits::default();
    if initiator {
        drive_initiator::<P, _, _, _>(stream, &TokioTimer, &mut access, &signed_topic, limits).await
    } else {
        drive_responder::<P, _, _, _>(stream, &TokioTimer, &mut access, &signed_topic, limits).await
    }
    .map_err(|error| error.to_string())
}

fn spawn_courier<P: AppLane>(
    runtime: AppRuntime,
    durable: SharedDurableRoom,
    endpoint_id: iroh::EndpointId,
    connection: Connection,
    mut stream: IrohSyncStream,
) {
    tauri::async_runtime::spawn(async move {
        let result: Result<(), String> = async {
            let Some(frame) = stream
                .recv_frame()
                .await
                .map_err(|error| error.to_string())?
            else {
                return Ok(());
            };
            let request = match CourierFrame::decode(&frame).map_err(|error| error.to_string())? {
                CourierFrame::Request(request) => request,
                CourierFrame::Response(_) => return Err("courier opener was a response".into()),
            };
            let response = {
                let durable = durable.lock().await;
                let empty = TrackedDiscardHistory::new();
                let peer = PeerId(*endpoint_id.as_bytes());
                let history = durable.courier.get(&(peer, P::LANE)).unwrap_or(&empty);
                CourierResponder {
                    history,
                    full: P::store(&durable.room),
                }
                .answer(&request)
            };
            let bytes = CourierFrame::Response(response)
                .encode()
                .map_err(|error| error.to_string())?;
            stream
                .send_frame(&bytes)
                .await
                .map_err(|error| error.to_string())?;
            Ok(())
        }
        .await;
        stream.close().await;
        drop(connection);
        if let Err(error) = result {
            runtime.emit_diagnostic(
                "courier_failed",
                &format!(
                    "{} courier with {endpoint_id} failed: {error}",
                    String::from_utf8_lossy(P::COURIER_ALPN)
                ),
            );
        }
    });
}

async fn commit_room_op(
    durable: &SharedDurableRoom,
    signing_key: &SigningKey,
    signed_topic: &str,
    op: LocalRoomOp,
    network: &NativeRoomNetwork,
    runtime: &AppRuntime,
) -> Result<u64, AppError> {
    let (wire, view) = {
        let mut durable = durable.lock().await;
        let prepared = durable
            .room
            .prepare(signing_key, signed_topic, unix_time_micros(), op);
        let wire = prepared.to_wire_bytes().map_err(persistence_error)?;
        durable
            .journal
            .append(prepared.lane(), &wire)
            .map_err(persistence_error)?;
        durable
            .room
            .ingest_prepared(signed_topic, &prepared)
            .expect("a just-signed, topic-scoped lane operation verifies");
        (wire, durable.room.view())
    };
    if let Err(error) = network.broadcast(wire).await {
        runtime.emit_diagnostic(
            "gossip_broadcast",
            &format!("operation committed locally but broadcast failed: {error}"),
        );
    }
    Ok(runtime.apply_room_view(view))
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

fn unix_time_micros() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_micros()).unwrap_or(u64::MAX))
        .unwrap_or(0)
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
    fn peer_sync_requires_every_advertised_lane() {
        let peer = iroh::SecretKey::from_bytes(&[51; 32]).public();
        let mut progress = PeerSyncProgress::default();
        assert!(!progress.record(peer, RoomLane::Music, LaneSet::WALKIE));
        assert!(progress.record(peer, RoomLane::Extension, LaneSet::WALKIE));
        progress.clear(peer);
        assert!(
            !progress.record(peer, RoomLane::Extension, LaneSet::WALKIE),
            "a reconnect cannot reuse a lane completed by the previous connection"
        );

        let music_only = iroh::SecretKey::from_bytes(&[52; 32]).public();
        assert!(progress.record(music_only, RoomLane::Music, LaneSet::MUSIC));
    }

    #[test]
    fn app_lane_protocols_are_disjoint_and_pinned() {
        assert_eq!(MusicLane::LANE, RoomLane::Music);
        assert_eq!(ExtensionLane::LANE, RoomLane::Extension);
        assert_eq!(
            LaneProtocol::Repair(MusicLane::LANE).alpn(),
            MusicLane::ALPN
        );
        assert_eq!(
            LaneProtocol::Courier(ExtensionLane::LANE).alpn(),
            ExtensionLane::COURIER_ALPN
        );
    }
}
