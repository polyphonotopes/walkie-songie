//! Capability-native in-page Room-v5 host.
//!
//! The browser owns IndexedDB, iroh/WebRTC, rendezvous, tasks, and UI events.
//! HHHS owns each lane's admission, materialization, and repair state. IndexedDB
//! remains an async durability owner; it never masquerades as `ReplicaStorage`.

use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
    time::Duration,
};

use futures::{
    SinkExt, StreamExt,
    channel::{mpsc, oneshot},
    lock::Mutex,
};
use hhhs_replica::DurableReplicaHost;
use hhhs_store::MemoryStorage;
use wasm_bindgen_futures::spawn_local;

use crate::{
    client::{
        AppError, AppErrorCode, AppEvent, AppEventEnvelope, AppSnapshot, CLIENT_PROTOCOL_VERSION,
        Capabilities, ClientCommand, CommandAck, DiscoverySource, PeerPath, PeerSnapshot,
        PieceSnapshot, VoiceSnapshot,
    },
    is_valid_room_name,
    net::{
        BrowserNetHandle, BrowserRoomInbound, BrowserRoomNetwork, BrowserTimer, IrohSyncStream,
        NativeNetworkEvent, NativeRoomTicketV5, ReplicaLiveRecord, ReplicaProtocol,
        ReplicaRepairHint, ReplicaRoomNetworkConfig, WalkieIdentity, drive_replica_initiator,
        drive_replica_responder, is_routine_repair_initiator, spawn_rendezvous_v5,
    },
    room::v5::{
        ActorId, ExtensionCommand, MusicOp, ProtocolSupport, RoomAdmissionPolicy, RoomCommand,
        RoomLane, RoomReplicas, RoomView, open_room_authority,
    },
    tuning::{TunedDegree, TunedPeriodicPitch},
};

use super::storage::IndexedDbReplicaLogV5;

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
    Shutdown,
}

type BrowserDurableLane =
    DurableReplicaHost<MemoryStorage, RoomAdmissionPolicy, IndexedDbReplicaLogV5>;

struct DurableRoom {
    room: RoomReplicas<MemoryStorage, MemoryStorage>,
    music: Rc<Mutex<BrowserDurableLane>>,
    extension: Rc<Mutex<BrowserDurableLane>>,
}

impl DurableRoom {
    fn lane(&self, lane: RoomLane) -> Rc<Mutex<BrowserDurableLane>> {
        match lane {
            RoomLane::Music => Rc::clone(&self.music),
            RoomLane::Extension => Rc::clone(&self.extension),
        }
    }
}

type SharedDurableRoom = Rc<DurableRoom>;

async fn persist_initial_member_grant(
    room: &RoomReplicas<MemoryStorage, MemoryStorage>,
    host: &mut BrowserDurableLane,
    lane: RoomLane,
    authority: &hhhs_proof::SigningKey,
    member: ActorId,
) -> Result<(), AppError> {
    if !room.capabilities_for(member).for_lane(lane).is_empty() {
        return Ok(());
    }
    let prepared = room
        .prepare_member_grant(lane, authority, member)
        .map_err(persistence_error)?;
    host.commit_prepared(prepared.into_prepared())
        .await
        .map_err(persistence_error)?;
    Ok(())
}

struct ActiveRoom {
    control: mpsc::Sender<RoomControl>,
    alive: Rc<Cell<bool>>,
}

struct HostState {
    sequence: u64,
    snapshot: AppSnapshot,
    pitch_authors: BTreeMap<TunedDegree, Vec<ActorId>>,
    subscribers: Vec<Rc<dyn Fn(AppEventEnvelope)>>,
    active_room: Option<ActiveRoom>,
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
    let host = Rc::new(BrowserHost {
        state: Rc::new(RefCell::new(HostState {
            sequence: 0,
            snapshot: AppSnapshot::empty(browser_capabilities()),
            pitch_authors: BTreeMap::new(),
            subscribers: Vec::new(),
            active_room: None,
        })),
        identity: WalkieIdentity::from_seed(seed),
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
        let music_log = IndexedDbReplicaLogV5::open(&config.room, config.owner, RoomLane::Music)
            .await
            .map_err(persistence_error)?;
        let extension_log =
            IndexedDbReplicaLogV5::open(&config.room, config.owner, RoomLane::Extension)
                .await
                .map_err(persistence_error)?;
        let room = RoomReplicas::from_transaction_logs(
            config.room.clone(),
            config.owner,
            music_log.transactions().map_err(persistence_error)?,
            extension_log.transactions().map_err(persistence_error)?,
        )
        .map_err(persistence_error)?;
        let mut music = room.music_durable_host(music_log);
        let mut extension = room.extension_durable_host(extension_log);
        if let Some(authority) = room_authority.as_ref() {
            if ActorId::from_signing_key(authority) != config.owner {
                return Err(AppError::new(
                    AppErrorCode::InvalidRoom,
                    "open-room authority does not match the room owner",
                ));
            }
            persist_initial_member_grant(
                &room,
                &mut music,
                RoomLane::Music,
                authority,
                local_actor,
            )
            .await?;
            persist_initial_member_grant(
                &room,
                &mut extension,
                RoomLane::Extension,
                authority,
                local_actor,
            )
            .await?;
        }
        let recovered_view = room.view();
        let durable = Rc::new(DurableRoom {
            room,
            music: Rc::new(Mutex::new(music)),
            extension: Rc::new(Mutex::new(extension)),
        });

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
            self.update_peer(peer, bootstrap_source, PeerPath::Connecting, false);
        }
        let (control, control_rx) = mpsc::channel(64);
        spawn_room_loop(
            self.clone(),
            durable.clone(),
            network,
            handle.clone(),
            control_rx,
            rendezvous_rx,
            rendezvous_guard,
            peers.clone(),
            alive.clone(),
            room_authority,
        );
        spawn_periodic_repair(self.clone(), durable, handle, peers, alive.clone());

        {
            let mut state = self.state.borrow_mut();
            state.active_room = Some(ActiveRoom { control, alive });
            state.snapshot.room_name = room_name.clone();
            state.snapshot.room_topic = Some(topic_string.clone());
            state.snapshot.room_ticket = Some(ticket_string.clone());
            state.snapshot.peers.clear();
            state.snapshot.voices.clear();
        }
        self.emit(AppEvent::RoomChanged {
            room_name,
            room_topic: Some(topic_string),
            ticket: Some(ticket_string),
        });
        self.apply_room_view(recovered_view);
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
            state.snapshot.active_degrees.clear();
            state.pitch_authors.clear();
            state.snapshot.pieces.clear();
            state.snapshot.voices.clear();
            state.snapshot.peers.clear();
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
            let _ = active.control.send(RoomControl::Shutdown).await;
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
            let old_degrees = state.snapshot.active_degrees.clone();
            let new_degrees: Vec<_> = view.music.live.iter().copied().collect();
            for pitch in old_degrees {
                if !view.music.live.contains(&pitch) {
                    events.push(AppEvent::DegreeRemoved { pitch });
                }
            }
            let holders: BTreeMap<_, Vec<_>> = view
                .music
                .holders
                .iter()
                .map(|(pitch, actors)| (*pitch, actors.iter().copied().collect()))
                .collect();
            for pitch in &new_degrees {
                let authors = holders.get(pitch).cloned().unwrap_or_default();
                if state.pitch_authors.get(pitch) != Some(&authors) {
                    events.push(AppEvent::DegreeAdded {
                        pitch: *pitch,
                        authors,
                    });
                }
            }
            state.snapshot.active_degrees = new_degrees;
            state.pitch_authors = holders;

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
        synchronized: bool,
    ) {
        let actor = ActorId(*endpoint.as_bytes());
        let mut peer = PeerSnapshot {
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
                if path != PeerPath::Disconnected {
                    peer.synchronized |= existing.synchronized;
                }
                *existing = peer.clone();
            } else {
                state.snapshot.peers.push(peer.clone());
                state.snapshot.peers.sort_by_key(|peer| peer.author);
            }
        }
        self.emit(AppEvent::PeerUpdated { peer });
    }

    fn mark_synchronized(&self, endpoint: iroh::EndpointId) {
        let actor = ActorId(*endpoint.as_bytes());
        let event = {
            let mut state = self.state.borrow_mut();
            let Some(peer) = state
                .snapshot
                .peers
                .iter_mut()
                .find(|peer| peer.author == actor)
            else {
                return;
            };
            peer.synchronized = true;
            peer.clone()
        };
        self.emit(AppEvent::PeerUpdated { peer: event });
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_room_loop(
    host: Rc<BrowserHost>,
    durable: SharedDurableRoom,
    mut network: BrowserRoomNetwork,
    handle: BrowserNetHandle,
    mut control: mpsc::Receiver<RoomControl>,
    mut rendezvous: Option<mpsc::Receiver<(iroh::EndpointId, ProtocolSupport)>>,
    rendezvous_guard: Option<crate::net::RendezvousHandle>,
    peers: Rc<RefCell<BTreeMap<iroh::EndpointId, (DiscoverySource, PeerPath, ProtocolSupport)>>>,
    alive: Rc<Cell<bool>>,
    room_authority: Option<hhhs_proof::SigningKey>,
) {
    let signing_key = host.identity.capability_signing_key();
    let local_actor = host.identity.capability_actor_id();
    spawn_local(async move {
        let _rendezvous_guard = rendezvous_guard;
        let mut presence_session = 0_u64;
        let mut presence_sequence = 0_u64;
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
                            let result = commit_command(
                                &durable,
                                &signing_key,
                                command,
                                &handle,
                                host.clone(),
                            )
                            .await;
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
                            let wire = {
                                let capabilities = durable.room.capabilities_for(local_actor);
                                durable.room.sign_presence(
                                    &signing_key,
                                    &capabilities,
                                    session,
                                    presence_sequence,
                                    pitch,
                                )
                            };
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
                        Some(RoomControl::Shutdown) | None => break,
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
                                durable.clone(),
                                repair.endpoint_id,
                                lane,
                                repair.stream.owning(repair.connection),
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
                                host.update_peer(endpoint_id, discovery, path, false);
                                if let Err(error) = grant_peer(
                                    &durable,
                                    room_authority.as_ref().unwrap_or(&signing_key),
                                    local_actor,
                                    ActorId(*endpoint_id.as_bytes()),
                                    &handle,
                                )
                                .await
                                {
                                    host.emit_diagnostic("capability_grant", &error.message);
                                }
                                if is_routine_repair_initiator(local, remote) {
                                    for lane in [RoomLane::Music, RoomLane::Extension] {
                                        if support.supports(lane) {
                                            spawn_repair_initiator(
                                                host.clone(),
                                                durable.clone(),
                                                handle.clone(),
                                                endpoint_id,
                                                lane,
                                            );
                                        }
                                    }
                                }
                            }
                            NativeNetworkEvent::NeighborDown { endpoint_id } => {
                                if let Some((source, path, _)) =
                                    peers.borrow_mut().get_mut(&endpoint_id)
                                {
                                    *path = PeerPath::Disconnected;
                                    host.update_peer(
                                        endpoint_id,
                                        *source,
                                        PeerPath::Disconnected,
                                        false,
                                    );
                                }
                            }
                            NativeNetworkEvent::Message { bytes, .. } => {
                                if let Some(live) = ReplicaLiveRecord::decode(&bytes) {
                                    let local =
                                        crate::net::PeerId(*handle.endpoint_id().as_bytes());
                                    if live.source != local {
                                        let source = live.source;
                                        let lane = live.lane;
                                        let entry = live.record.entry_hash();
                                        let accepted =
                                            match apply_live_record(&durable, live, &host).await {
                                                Ok(accepted) => accepted,
                                                Err(error) => {
                                                    host.emit_diagnostic(
                                                        "live_record_admission",
                                                        &error.message,
                                                    );
                                                    false
                                                }
                                            };
                                        if !accepted {
                                            request_repair_after_live_failure(
                                                host.clone(),
                                                durable.clone(),
                                                handle.clone(),
                                                source,
                                                lane,
                                                entry,
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
                                        durable.clone(),
                                        handle.clone(),
                                        source,
                                        hint.lane,
                                    );
                                } else if let Ok(presence) = durable.room.verify_presence(&bytes) {
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
                                                durable.clone(),
                                                handle.clone(),
                                                *peer,
                                                lane,
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
                            false,
                        );
                    }
                    None => rendezvous = None,
                },
            }
        }
        alive.set(false);
        let _ = network.shutdown().await;
    });
}

fn spawn_periodic_repair(
    host: Rc<BrowserHost>,
    durable: SharedDurableRoom,
    handle: BrowserNetHandle,
    peers: Rc<RefCell<BTreeMap<iroh::EndpointId, (DiscoverySource, PeerPath, ProtocolSupport)>>>,
    alive: Rc<Cell<bool>>,
) {
    spawn_local(async move {
        let local = crate::net::PeerId(*handle.endpoint_id().as_bytes());
        while alive.get() {
            n0_future::time::sleep(Duration::from_secs(27)).await;
            for (peer, (_, path, support)) in peers.borrow().iter() {
                let remote = crate::net::PeerId(*peer.as_bytes());
                if *path == PeerPath::Disconnected || !is_routine_repair_initiator(local, remote) {
                    continue;
                }
                for lane in [RoomLane::Music, RoomLane::Extension] {
                    if support.supports(lane) {
                        spawn_repair_initiator(
                            host.clone(),
                            durable.clone(),
                            handle.clone(),
                            *peer,
                            lane,
                        );
                    }
                }
            }
        }
    });
}

async fn commit_command(
    durable: &SharedDurableRoom,
    signing_key: &hhhs_proof::SigningKey,
    command: RoomCommand,
    handle: &BrowserNetHandle,
    host: Rc<BrowserHost>,
) -> Result<u64, AppError> {
    let lane = command.lane();
    let (entry, record, view) = {
        let lane_host = durable.lane(lane);
        let mut writer = lane_host.lock().await;
        let actor = ActorId::from_signing_key(signing_key);
        let capabilities = durable.room.capabilities_for(actor);
        if capabilities.for_lane(lane).is_empty() {
            return Err(AppError::new(
                AppErrorCode::UnsupportedCapability,
                "this actor has not received a live Room-v5 capability for that Replica",
            ));
        }
        let prepared = durable
            .room
            .prepare_author(signing_key, &capabilities, command)
            .map_err(persistence_error)?;
        let committed = writer
            .commit_prepared(prepared.into_prepared())
            .await
            .map_err(persistence_error)?;
        let entry = committed.outcome().entry;
        let record = committed.replica_record().clone();
        (entry, record, durable.room.view())
    };
    let accepted_sequence = host.apply_room_view(view);
    let live = ReplicaLiveRecord {
        lane,
        source: crate::net::PeerId(*handle.endpoint_id().as_bytes()),
        record,
    };
    let handle = handle.clone();
    spawn_local(async move {
        if let Err(error) = handle.broadcast(live.encode()).await {
            host.emit_diagnostic(
                "live_record_broadcast",
                &format!("command is durable; fast delivery failed: {error}"),
            );
            if let Err(error) = handle
                .broadcast(
                    ReplicaRepairHint {
                        lane,
                        source: crate::net::PeerId(*handle.endpoint_id().as_bytes()),
                        entry,
                    }
                    .encode(),
                )
                .await
            {
                host.emit_diagnostic(
                    "repair_hint_broadcast",
                    &format!("command is durable; repair hint failed: {error}"),
                );
            }
        }
    });
    Ok(accepted_sequence)
}

async fn grant_peer(
    durable: &SharedDurableRoom,
    signing_key: &hhhs_proof::SigningKey,
    local_actor: ActorId,
    peer: ActorId,
    handle: &BrowserNetHandle,
) -> Result<(), AppError> {
    let mut hints = Vec::new();
    if peer == local_actor || durable.room.owner() != ActorId::from_signing_key(signing_key) {
        return Ok(());
    }
    for lane in [RoomLane::Music, RoomLane::Extension] {
        let lane_host = durable.lane(lane);
        let mut writer = lane_host.lock().await;
        let existing = durable.room.capabilities_for(peer);
        if !existing.for_lane(lane).is_empty() {
            continue;
        }
        let prepared = durable
            .room
            .prepare_member_grant(lane, signing_key, peer)
            .map_err(persistence_error)?;
        let committed = writer
            .commit_prepared(prepared.into_prepared())
            .await
            .map_err(persistence_error)?;
        hints.push((lane, committed.outcome().entry));
    }
    for (lane, entry) in hints {
        handle
            .broadcast(
                ReplicaRepairHint {
                    lane,
                    source: crate::net::PeerId(*handle.endpoint_id().as_bytes()),
                    entry,
                }
                .encode(),
            )
            .await
            .map_err(network_error)?;
    }
    Ok(())
}

fn spawn_repair_initiator(
    host: Rc<BrowserHost>,
    durable: SharedDurableRoom,
    handle: BrowserNetHandle,
    peer: iroh::EndpointId,
    lane: RoomLane,
) {
    spawn_local(async move {
        let lane_host = durable.lane(lane);
        let Some(mut repair_host) = lane_host.try_lock() else {
            return;
        };
        let connection = match handle.begin_replica(peer, lane).await {
            Ok(connection) => connection,
            Err(error) => {
                host.emit_diagnostic("replica_repair_dial", &error.to_string());
                return;
            }
        };
        let stream = match IrohSyncStream::open(&connection).await {
            Ok(stream) => stream.owning(connection),
            Err(error) => {
                host.emit_diagnostic("replica_repair_stream", &error.to_string());
                return;
            }
        };
        run_repair(host, durable, peer, lane, &mut repair_host, stream, true).await;
    });
}

fn spawn_repair_responder(
    host: Rc<BrowserHost>,
    durable: SharedDurableRoom,
    peer: iroh::EndpointId,
    lane: RoomLane,
    stream: IrohSyncStream,
) {
    spawn_local(async move {
        let lane_host = durable.lane(lane);
        let Some(mut repair_host) = lane_host.try_lock() else {
            return;
        };
        run_repair(host, durable, peer, lane, &mut repair_host, stream, false).await;
    });
}

async fn apply_live_record(
    durable: &SharedDurableRoom,
    live: ReplicaLiveRecord,
    host: &BrowserHost,
) -> Result<bool, AppError> {
    let lane = live.lane;
    let entry = live.record.entry_hash();
    let bytes = live.record.encode();
    let lane_host = durable.lane(lane);
    let mut writer = lane_host.lock().await;
    let result = hhhs_sync::RepairHost::apply(&mut *writer, &[(entry, bytes)]).await;
    let view = durable.room.view();
    drop(writer);
    let report = result.map_err(persistence_error)?;
    host.apply_room_view(view);
    Ok(report.refused.is_empty() && report.admitted.contains(&entry))
}

fn request_repair_after_live_failure(
    host: Rc<BrowserHost>,
    durable: SharedDurableRoom,
    handle: BrowserNetHandle,
    source: crate::net::PeerId,
    lane: RoomLane,
    entry: hhhs::EntryHash,
) {
    let local = crate::net::PeerId(*handle.endpoint_id().as_bytes());
    let Ok(source_endpoint) = iroh::EndpointId::from_bytes(source.as_bytes()) else {
        return;
    };
    if is_routine_repair_initiator(local, source) {
        spawn_repair_initiator(host, durable, handle, source_endpoint, lane);
        return;
    }
    spawn_local(async move {
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
    durable: SharedDurableRoom,
    peer: iroh::EndpointId,
    lane: RoomLane,
    repair_host: &mut BrowserDurableLane,
    stream: IrohSyncStream,
    initiator: bool,
) {
    let result = if initiator {
        drive_replica_initiator(
            stream,
            &BrowserTimer,
            repair_host,
            lane,
            hhhs_sync::SessionLimits::default(),
        )
        .await
    } else {
        drive_replica_responder(
            stream,
            &BrowserTimer,
            repair_host,
            lane,
            hhhs_sync::SessionLimits::default(),
        )
        .await
    };
    let view = durable.room.view();
    match result {
        Ok(outcome) if !outcome.incomplete && !outcome.root_mismatch => {
            host.apply_room_view(view);
            host.mark_synchronized(peer);
        }
        Ok(outcome) => host.emit_diagnostic(
            "replica_repair_incomplete",
            &format!("{lane:?} repair with {peer} ended incomplete: {outcome:?}"),
        ),
        Err(error) => host.emit_diagnostic(
            "replica_repair",
            &format!("{lane:?} repair with {peer} failed: {error}"),
        ),
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
