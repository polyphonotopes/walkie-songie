use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use hhhs_core::EntryHash;
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
        FileSeedStore, IrohSyncStream, NativeNetworkEvent, NativeRoomNetwork,
        NativeRoomNetworkConfig, NativeRoomTicket, PeerTransportPath, RelayPolicy, RoomInbound,
        RoomSyncSource, RoomTopic, SyncApply, SyncLimits, SyncOutcome, SyncStoreAccess, TokioTimer,
        WalkieIdentity, drive_initiator, drive_responder,
    },
    room::{
        journal::FileOpJournal,
        ops::{AuthorId, SignedOp, SigningKey, WalkieOp, verify_signed_op_for_topic},
        presence::{PresenceBody, SignedPresence},
        store::{RoomStore, RoomView},
    },
};

enum RoomControl {
    Commit {
        op: WalkieOp,
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
    store: RoomStore,
    journal: FileOpJournal,
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
                self.submit_durable(WalkieOp::SetTuning { definition })
                    .await
            }
            ClientCommand::AddDegree { pitch } => {
                self.validate_degree(pitch)?;
                self.submit_durable(WalkieOp::AddDegree { pitch }).await
            }
            ClientCommand::ToggleDegree { pitch } => {
                self.validate_degree(pitch)?;
                let author = self.identity.author_id();
                let present = self
                    .lock()?
                    .pitch_authors
                    .get(&pitch)
                    .is_some_and(|authors| authors.contains(&author));
                self.submit_durable(if present {
                    WalkieOp::RemoveDegree { pitch }
                } else {
                    WalkieOp::AddDegree { pitch }
                })
                .await
            }
            ClientCommand::RemoveDegree { pitch } => {
                self.validate_degree(pitch)?;
                self.submit_durable(WalkieOp::RemoveDegree { pitch }).await
            }
            ClientCommand::PutPiece { emoji, pitch } => {
                self.validate_periodic_pitch(pitch)?;
                self.submit_durable(WalkieOp::PutPiece { emoji, pitch })
                    .await
            }
            ClientCommand::MovePiece { piece, pitch } => {
                self.validate_periodic_pitch(pitch)?;
                self.submit_durable(WalkieOp::MovePiece { piece, pitch })
                    .await
            }
            ClientCommand::RemovePiece { piece } => {
                self.submit_durable(WalkieOp::RemovePiece { piece }).await
            }
            ClientCommand::SetRoomConfig {
                pieces_locked,
                available_emojis,
            } => {
                self.submit_durable(WalkieOp::SetConfig {
                    pieces_locked,
                    available_emojis,
                })
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
        let topic = RoomTopic::from_room_name(&room_name);
        let config = NativeRoomNetworkConfig {
            topic,
            relay: relay_policy_from_environment()?,
            bootstrap: None,
        };
        self.start_room(Some(room_name), config, DiscoverySource::Mdns)
            .await
    }

    async fn join_ticket(&self, encoded: String) -> Result<CommandAck, AppError> {
        let ticket = encoded.parse::<NativeRoomTicket>().map_err(|error| {
            AppError::new(AppErrorCode::InvalidTicket, "invalid room ticket")
                .with_detail(error.to_string())
        })?;
        let config = NativeRoomNetworkConfig {
            topic: ticket.topic(),
            relay: relay_policy_from_environment()?,
            bootstrap: Some(ticket.endpoint_addr().clone()),
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
            .join(format!("{topic_string}.ops"));
        let (journal, recovered) = FileOpJournal::open(journal_path).map_err(persistence_error)?;
        let mut store = RoomStore::new();
        for signed in recovered {
            let verified = verify_signed_op_for_topic(&signed, &topic_string).map_err(|error| {
                persistence_error(format!("stored operation failed verification: {error}"))
            })?;
            store.ingest_verified(verified);
        }
        if store.pending_len() != 0 {
            return Err(persistence_error(format!(
                "{} stored operations are missing causal predecessors",
                store.pending_len()
            )));
        }
        let recovered_view = store.view();
        let durable = Arc::new(tokio::sync::Mutex::new(DurableRoom { store, journal }));

        let bootstrap = config.bootstrap.as_ref().map(|address| address.id);
        let mut network = NativeRoomNetwork::bind(self.identity.iroh_secret(), config.clone())
            .await
            .map_err(network_error)?;
        let ticket = network.settle_ticket(Duration::from_millis(750)).await;
        let ticket_string = ticket.to_string();

        let (control, mut control_rx) = mpsc::channel(64);
        let runtime = self.clone();
        let signing_key = self.identity.signing_key();
        let signed_topic = topic_string.clone();
        let presence_topic = *config.topic.as_bytes();
        let midi_events = self.lock_midi()?.service.input_events();
        let task = tauri::async_runtime::spawn(async move {
            let mut local_presence_session = 0_u64;
            let mut local_presence_sequence = 0_u64;
            // author -> (session, sequence, issued_at_ms, local_expires_at_ms)
            let mut presence_order: BTreeMap<AuthorId, (u64, u64, u64, u64)> = BTreeMap::new();
            let mut peers: BTreeMap<iroh::EndpointId, (DiscoverySource, PeerPath)> =
                BTreeMap::new();
            if let Some(endpoint_id) = bootstrap {
                peers.insert(endpoint_id, (bootstrap_source, PeerPath::Connecting));
                runtime.update_peer(endpoint_id, bootstrap_source, PeerPath::Connecting, false);
            }

            let mut path_refresh = tokio::time::interval(Duration::from_secs(1));
            path_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut presence_refresh = tokio::time::interval(Duration::from_millis(250));
            presence_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut midi_refresh = tokio::time::interval(Duration::from_secs(2));
            midi_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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
                                match SignedPresence::sign(
                                    &signing_key,
                                    PresenceBody::new(
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
                                            WalkieOp::AddDegree { pitch },
                                        HeldInputAction::DegreeReleased(pitch) =>
                                            WalkieOp::RemoveDegree { pitch },
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
                                    WalkieOp::AddDegree { pitch },
                                HeldInputAction::DegreeReleased(pitch) =>
                                    WalkieOp::RemoveDegree { pitch },
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
                                spawn_repair(
                                    runtime.clone(),
                                    durable.clone(),
                                    signed_topic.clone(),
                                    repair.endpoint_id,
                                    repair.connection,
                                    repair.stream,
                                    false,
                                );
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
                                    match dial_repair(&network, endpoint_id).await {
                                        Ok((connection, stream)) => spawn_repair(
                                            runtime.clone(),
                                            durable.clone(),
                                            signed_topic.clone(),
                                            endpoint_id,
                                            connection,
                                            stream,
                                            true,
                                        ),
                                        Err(error) => runtime.emit_diagnostic(
                                            "repair_connect",
                                            &error,
                                        ),
                                    }
                                }
                            }
                            NativeNetworkEvent::NeighborDown { endpoint_id } => {
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
                                if let Ok(signed) = SignedOp::from_wire_bytes(&bytes) {
                                    match verify_signed_op_for_topic(&signed, &signed_topic) {
                                        Ok(verified) => {
                                            if verified.author().0 != *delivered_from.as_bytes() {
                                                // Gossip may forward through a neighbor.
                                                // The operation signature, not the
                                                // delivery hop, establishes authorship.
                                                runtime.emit_diagnostic(
                                                    "gossip_forwarded",
                                                    "received a valid operation through a forwarding neighbor",
                                                );
                                            }
                                            let view = {
                                                let mut durable =
                                                    durable.lock().await;
                                                if !durable
                                                    .store
                                                    .knows_op(verified.id())
                                                    && let Err(error) =
                                                        durable.journal.append(&signed)
                                                {
                                                    runtime.emit_diagnostic(
                                                        "operation_persistence",
                                                        &error.to_string(),
                                                    );
                                                    continue;
                                                }
                                                durable.store.ingest_verified(verified);
                                                durable.store.view()
                                            };
                                            runtime.apply_room_view(view);
                                        }
                                        Err(error) => runtime.emit_diagnostic(
                                            "gossip_rejected",
                                            &error.to_string(),
                                        ),
                                    }
                                } else if let Ok(signed) =
                                    SignedPresence::from_wire_bytes(&bytes)
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
                            NativeNetworkEvent::Lagged => runtime.emit_diagnostic(
                                "gossip_lagged",
                                "the gossip event consumer lagged; anti-entropy repair is required",
                            ),
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

    async fn submit_durable(&self, op: WalkieOp) -> Result<CommandAck, AppError> {
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
                        self.submit_durable(WalkieOp::RemoveDegree { pitch })
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

fn spawn_repair(
    runtime: AppRuntime,
    durable: SharedDurableRoom,
    signed_topic: String,
    endpoint_id: iroh::EndpointId,
    connection: Connection,
    stream: IrohSyncStream,
    initiator: bool,
) {
    tauri::async_runtime::spawn(async move {
        let telemetry_connection = connection.clone();
        match run_repair_session(stream, initiator, durable, signed_topic, runtime.clone()).await {
            Ok(ingested) => {
                if let Some(rtt) = telemetry_connection.rtt(iroh::endpoint::PathId::ZERO) {
                    runtime.update_peer_rtt(endpoint_id, rtt);
                }
                runtime.mark_peer_synchronized(endpoint_id);
                runtime.emit_diagnostic(
                    "repair_complete",
                    &format!(
                        "HHHS H6 repair with {endpoint_id} completed; ingested {ingested} operations"
                    ),
                );
            }
            Err(error) => runtime.emit_diagnostic(
                "repair_failed",
                &format!("HHHS H6 repair with {endpoint_id} failed: {error}"),
            ),
        }
    });
}

/// Dial a peer and open the one bi-stream a repair session runs over.
///
/// The connection comes back alongside the stream purely for telemetry (RTT);
/// the stream owns its own handle, so the connection outlives this call either
/// way.
async fn dial_repair(
    network: &NativeRoomNetwork,
    endpoint_id: iroh::EndpointId,
) -> Result<(Connection, IrohSyncStream), String> {
    let connection = network
        .begin_repair(endpoint_id)
        .await
        .map_err(|error| error.to_string())?;
    let stream = IrohSyncStream::open(&connection)
        .await
        .map_err(|error| error.to_string())?;
    Ok((connection.clone(), stream.owning(connection)))
}

/// [`SyncStoreAccess`] over the durable room.
///
/// Locks per call and NEVER across a network round trip. The store sits behind
/// the same mutex the room loop needs for gossip ingest, local commits and view
/// updates, so holding it for a whole session would freeze the app for as long
/// as the peer took to answer — and the peer controls that.
///
/// It also journals before ingesting, which is why this is a bespoke impl rather
/// than the blanket one on `RoomStore`: an op must be durable before it becomes
/// visible, and one that cannot be journalled is skipped rather than ingested.
struct DurableSyncStore {
    durable: SharedDurableRoom,
    runtime: AppRuntime,
}

impl SyncStoreAccess for DurableSyncStore {
    async fn capture(&mut self, salt: [u8; 16]) -> RoomSyncSource {
        let durable = self.durable.lock().await;
        RoomSyncSource::capture(&durable.store, salt)
    }

    async fn apply(
        &mut self,
        topic: &str,
        pairs: &[(EntryHash, Vec<u8>)],
        source: &mut RoomSyncSource,
    ) -> SyncApply {
        let mut journal_failure = None;
        let (admitted, lifted, view) = {
            let mut durable = self.durable.lock().await;
            // The session's admitted set: every hash verified and KEPT (lifted
            // or parked). A pair that fails decode/verification — or that
            // cannot be journalled, since un-durable means un-kept here — is
            // left out, which is what makes it eligible to be asked for again.
            let mut admitted = Vec::new();
            let mut lifted = Vec::new();
            for (wire_hash, bytes) in pairs {
                // A peer can send anything; undecodable or unverifiable frames
                // cost it bandwidth and cost us nothing.
                let Ok(signed) = SignedOp::from_wire_bytes(bytes) else {
                    continue;
                };
                let Ok(verified) = verify_signed_op_for_topic(&signed, topic) else {
                    continue;
                };
                let id = verified.id();
                if let Some(entry) = durable.store.lifted_entry(id) {
                    // Already materialized (gossip raced the session): kept,
                    // under the store-derived entry hash.
                    admitted.push(entry);
                    continue;
                }
                if durable.store.knows_op(id) {
                    // Already parked: kept; the wire claim is its only name
                    // until its causal past resolves.
                    admitted.push(*wire_hash);
                    continue;
                }
                if let Err(error) = durable.journal.append(&signed) {
                    journal_failure.get_or_insert_with(|| error.to_string());
                    continue;
                }
                let newly = durable.store.ingest_verified(verified);
                if newly.is_empty() {
                    // Parked just now: kept, named by the wire claim.
                    admitted.push(*wire_hash);
                } else {
                    admitted.extend(newly.iter().copied());
                    lifted.extend(newly);
                }
            }
            source.absorb(&durable.store, &lifted);
            let view = durable.store.view();
            (admitted, lifted, view)
        };
        if let Some(error) = journal_failure {
            self.runtime.emit_diagnostic("repair_journal", &error);
        }
        if !lifted.is_empty() {
            self.runtime.apply_room_view(view);
        }
        SyncApply {
            admitted,
            lifted: lifted.len(),
        }
    }
}

/// Drive one HHHS H6 anti-entropy session over an established stream.
///
/// The session logic itself lives in `walkie_songie::net::sync` and is shared
/// with the loopback tests, so what runs here is what CI exercises — this used
/// to be a second, separately-maintained copy of the same state machine.
async fn run_repair_session(
    stream: IrohSyncStream,
    initiator: bool,
    durable: SharedDurableRoom,
    signed_topic: String,
    runtime: AppRuntime,
) -> Result<usize, String> {
    let mut store = DurableSyncStore {
        durable,
        runtime: runtime.clone(),
    };
    let limits = SyncLimits::default();
    let outcome: SyncOutcome = if initiator {
        drive_initiator(stream, &TokioTimer, &mut store, &signed_topic, limits).await
    } else {
        drive_responder(stream, &TokioTimer, &mut store, &signed_topic, limits).await
    }
    .map_err(|error| error.to_string())?;

    if outcome.root_mismatch {
        // The only signal that a session completed WITHOUT converging.
        runtime.emit_diagnostic(
            "repair_root_mismatch",
            "HHHS repair peers reported different roots; periodic repair will retry",
        );
    }
    if outcome.incomplete {
        runtime.emit_diagnostic(
            "repair_incomplete",
            "HHHS repair ended before both halves finished; periodic repair will retry",
        );
    }
    Ok(outcome.ingested)
}

async fn commit_room_op(
    durable: &SharedDurableRoom,
    signing_key: &SigningKey,
    signed_topic: &str,
    op: WalkieOp,
    network: &NativeRoomNetwork,
    runtime: &AppRuntime,
) -> Result<u64, AppError> {
    let (signed, view) = {
        let mut durable = durable.lock().await;
        let signed =
            durable
                .store
                .prepare_commit(signing_key, signed_topic, unix_time_micros(), op);
        durable.journal.append(&signed).map_err(persistence_error)?;
        let verified = verify_signed_op_for_topic(&signed, signed_topic)
            .expect("a just-signed, topic-scoped operation verifies");
        durable.store.ingest_verified(verified);
        (signed, durable.store.view())
    };
    match signed.to_wire_bytes() {
        Ok(bytes) => {
            if let Err(error) = network.broadcast(bytes).await {
                runtime.emit_diagnostic(
                    "gossip_broadcast",
                    &format!("operation committed locally but broadcast failed: {error}"),
                );
            }
        }
        Err(error) => runtime.emit_diagnostic("operation_frame", &error.to_string()),
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
    use walkie_songie::{
        room::ops::signing_key_from_seed,
        tuning::{TunedDegree, Tuning},
    };

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires local UDP sockets"]
    async fn h6_repairs_a_late_joiner_over_the_dedicated_iroh_alpn() {
        let topic = RoomTopic::from_room_name("quiet-cactus-song");
        let topic_string = topic.to_string();
        let first_seed = [51; 32];
        let second_seed = [52; 32];
        let mut first_network = NativeRoomNetwork::bind(
            iroh::SecretKey::from_bytes(&first_seed),
            NativeRoomNetworkConfig {
                topic,
                relay: RelayPolicy::Disabled,
                bootstrap: None,
            },
        )
        .await
        .unwrap();
        let second_network = NativeRoomNetwork::bind(
            iroh::SecretKey::from_bytes(&second_seed),
            NativeRoomNetworkConfig {
                topic,
                relay: RelayPolicy::Disabled,
                bootstrap: Some(first_network.ticket().endpoint_addr().clone()),
            },
        )
        .await
        .unwrap();

        let test_dir =
            std::env::temp_dir().join(format!("walkie-h6-test-{}", rand::random::<u64>()));
        let first_dir = test_dir.join("first");
        let second_dir = test_dir.join("second");
        let (mut first_journal, _) = FileOpJournal::open(first_dir.join("room.ops")).unwrap();
        let (second_journal, _) = FileOpJournal::open(second_dir.join("room.ops")).unwrap();
        let tuning = Tuning::twelve_tet();
        let degree = TunedDegree::new(&tuning, 7).unwrap();
        let mut first_store = RoomStore::new();
        let signed = first_store.commit(
            &signing_key_from_seed(&first_seed),
            &topic_string,
            1,
            WalkieOp::AddDegree { pitch: degree },
        );
        first_journal.append(&signed).unwrap();
        let first_durable = Arc::new(tokio::sync::Mutex::new(DurableRoom {
            store: first_store,
            journal: first_journal,
        }));
        let second_durable = Arc::new(tokio::sync::Mutex::new(DurableRoom {
            store: RoomStore::new(),
            journal: second_journal,
        }));

        let (_initiator_connection, initiator_stream) =
            dial_repair(&second_network, first_network.endpoint_id())
                .await
                .expect("repair dial");
        // The bi-stream is accepted inside the protocol handler, so what arrives
        // on the repair queue is already drivable.
        let responder_stream = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(RoomInbound::Repair(repair)) = first_network.next_inbound().await {
                    break repair.stream.owning(repair.connection);
                }
            }
        })
        .await
        .expect("repair connection was not accepted");

        let first_runtime =
            AppRuntime::new(WalkieIdentity::from_seed(first_seed), first_dir.clone());
        let second_runtime =
            AppRuntime::new(WalkieIdentity::from_seed(second_seed), second_dir.clone());
        let (initiator, responder) = tokio::join!(
            run_repair_session(
                initiator_stream,
                true,
                second_durable.clone(),
                topic_string.clone(),
                second_runtime,
            ),
            run_repair_session(
                responder_stream,
                false,
                first_durable.clone(),
                topic_string,
                first_runtime,
            ),
        );
        assert!(
            initiator.is_ok() && responder.is_ok(),
            "initiator={initiator:?}, responder={responder:?}"
        );
        assert!(
            second_durable
                .lock()
                .await
                .store
                .view()
                .pitches
                .contains(&degree)
        );

        second_network.shutdown().await.unwrap();
        first_network.shutdown().await.unwrap();
        drop(second_durable);
        drop(first_durable);
        let _ = std::fs::remove_dir_all(test_dir);
    }
}
