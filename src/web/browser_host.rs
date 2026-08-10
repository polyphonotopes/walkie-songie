//! In-page host for the browser iroh transport.
//!
//! A plain browser tab has no Tauri runtime, so this module plays the role
//! `src-tauri`'s `AppRuntime` plays on desktop: it owns the `WalkieIdentity`,
//! the two-lane v4 `Room`, and the live `BrowserRoomNetwork`, accepts the same
//! [`ClientCommand`]s, and emits the same ordered [`AppEventEnvelope`]s. The
//! UI cannot tell the difference — `app.rs` routes through one dispatch/apply
//! seam either way.
//!
//! Differences from the desktop runtime, all deliberate:
//! * a lane-tagged v4 journal in IndexedDB, keyed by the room topic hex, rather
//!   than the desktop file journal: the room is strictly recovered from it on
//!   start and each transaction commits before new state becomes visible;
//! * no native MIDI — the browser keeps Web MIDI, so MIDI commands are
//!   acknowledged and ignored;
//! * relay-only reachability (see [`crate::net::browser`]).

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    marker::PhantomData,
    rc::Rc,
    time::Duration,
};

use futures::{
    SinkExt, StreamExt,
    channel::{mpsc, oneshot},
    lock::Mutex,
};
use hhhs::EntryHash;
use wasm_bindgen_futures::spawn_local;
use web_time::{SystemTime, UNIX_EPOCH};

use crate::{
    client::{
        AppError, AppErrorCode, AppEvent, AppEventEnvelope, AppSnapshot, Capabilities,
        CLIENT_PROTOCOL_VERSION, ClientCommand, CommandAck, DiscoverySource, PeerPath,
        PeerSnapshot,
    },
    is_valid_room_name,
    net::{
        BrowserIncomingRepair, BrowserNetHandle, BrowserRoomInbound, BrowserRoomNetwork,
        BrowserTimer, CourierFrame, CourierResponder, ExtensionLane, IncomingOp, IrohSyncStream,
        LaneIngest, LaneProtocol, LaneSpec, LaneStoreAccess, LaneSyncSource, MusicLane,
        NativeNetworkEvent, NativeRoomNetworkConfig, NativeRoomTicketV4, PeerId, RelayPolicy,
        RoomTopic, SyncApply, SyncError, SyncLimits, SyncOutcome, SyncStream,
        TrackedDiscardHistory, WalkieIdentity, drive_initiator, drive_responder, ingest_pairs,
        spawn_rendezvous_v4,
    },
    room::{
        lane_journal::RoomJournalV4,
        ops::{AuthorId, OpId, OpLanguage, SignedOp, SigningKey, VerifiedOpG, WindowIngest},
        presence::{PresenceBody, SignedPresenceV4},
        store::{RoomView, Store},
        v4::{
            ExtensionLang, ExtensionOp, LaneSet, LocalRoomOp, LocalRoomPrepared, MusicLang,
            MusicOp, Room, RoomLane, verify_extension_op, verify_music_op,
        },
    },
    tuning::{TunedDegree, TunedPeriodicPitch, TuningDefinition},
};

enum RoomControl {
    Commit {
        op: LocalRoomOp,
        response: oneshot::Sender<Result<CommandAck, AppError>>,
    },
    Presence {
        session: u64,
        pitch: Option<TunedPeriodicPitch>,
        response: oneshot::Sender<Result<CommandAck, AppError>>,
    },
    Shutdown,
}

struct DurableRoom {
    topic_hex: String,
    room: Room,
    journal: RoomJournalV4,
    courier: BTreeMap<(PeerId, RoomLane), TrackedDiscardHistory>,
}

type SharedDurableRoom = Rc<Mutex<DurableRoom>>;

struct ActiveRoom {
    control: mpsc::Sender<RoomControl>,
    /// Cooperative stop for the timer tasks (path refresh, presence expiry).
    alive: Rc<Cell<bool>>,
}

struct HostState {
    sequence: u64,
    snapshot: AppSnapshot,
    pitch_authors: BTreeMap<TunedDegree, Vec<AuthorId>>,
    subscribers: Vec<Rc<dyn Fn(AppEventEnvelope)>>,
    active_room: Option<ActiveRoom>,
}

/// The in-page equivalent of the desktop `AppRuntime`. Single-threaded by
/// construction; every handle in here is `!Send`.
pub struct BrowserHost {
    state: Rc<RefCell<HostState>>,
    identity: WalkieIdentity,
}

thread_local! {
    static HOST: RefCell<Option<Rc<BrowserHost>>> = const { RefCell::new(None) };
}

fn current_host() -> Option<Rc<BrowserHost>> {
    HOST.with(|slot| slot.borrow().clone())
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

/// Testing/demo affordance: `?peer=<name>` in the URL derives a distinct identity
/// seed (blake3 of the name), so several browser contexts on one machine act as
/// separate peers instead of sharing the persisted IndexedDB identity (which
/// collides on a single endpoint id — same-id tabs cross signaling). `None` when
/// the param is absent/empty.
fn peer_override_seed() -> Option<[u8; 32]> {
    let search = web_sys::window()?.location().search().ok()?;
    let query = search.strip_prefix('?').unwrap_or(&search);
    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix("peer=") {
            if !value.is_empty() {
                return Some(*blake3::hash(value.as_bytes()).as_bytes());
            }
        }
    }
    None
}

/// Stand up the in-page host: load (or mint) the identity seed from IndexedDB,
/// then register `on_event` as the UI's ordered event subscriber. The first
/// envelope it receives is the initial (empty) snapshot.
pub async fn init(on_event: impl Fn(AppEventEnvelope) + 'static) -> Result<(), String> {
    // `?peer=<name>` gives this context a DISTINCT identity so multiple browser
    // contexts on one machine are separate peers; otherwise the persisted IndexedDB
    // identity collides (same endpoint id, crossed signaling). No param => persistent.
    let seed = match peer_override_seed() {
        Some(seed) => {
            web_sys::console::log_1(
                &"identity: ?peer= override (distinct per-context, not persisted)".into(),
            );
            seed
        }
        None => super::storage::get_or_create_identity_seed().await,
    };
    let identity = WalkieIdentity::from_seed(seed);
    let host = Rc::new(BrowserHost {
        state: Rc::new(RefCell::new(HostState {
            sequence: 0,
            snapshot: AppSnapshot::empty(browser_capabilities()),
            pitch_authors: BTreeMap::new(),
            subscribers: Vec::new(),
            active_room: None,
        })),
        identity,
    });
    host.register(Rc::new(on_event));
    HOST.with(|slot| *slot.borrow_mut() = Some(host));
    Ok(())
}

/// Route one command into the host, mirroring `native_bridge::dispatch`'s
/// fire-and-forget shape: errors go to `on_error`, acks are dropped.
pub fn dispatch(command: ClientCommand, on_error: impl Fn(String) + 'static) {
    let Some(host) = current_host() else {
        on_error("browser networking is not initialized".to_owned());
        return;
    };
    spawn_local(async move {
        if let Err(error) = host.dispatch(command).await {
            let detail = error
                .detail
                .as_deref()
                .map(|detail| format!(" ({detail})"))
                .unwrap_or_default();
            on_error(format!("{}{detail}", error.message));
        }
    });
}

impl BrowserHost {
    fn register(&self, subscriber: Rc<dyn Fn(AppEventEnvelope)>) {
        let envelope = {
            let mut state = self.state.borrow_mut();
            state.sequence = state.sequence.saturating_add(1);
            let envelope = AppEventEnvelope {
                sequence: state.sequence,
                event: AppEvent::Snapshot {
                    snapshot: state.snapshot.clone(),
                },
            };
            state.subscribers.push(subscriber.clone());
            envelope
        };
        subscriber(envelope);
    }

    /// Bump the sequence and fan `event` out. Subscribers run OUTSIDE the state
    /// borrow: a subscriber is UI code, and UI code may dispatch.
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
                self.submit_durable(ExtensionOp::SetConfig {
                    pieces_locked,
                    available_emojis,
                }
                .into())
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
            // MIDI is Web MIDI in a browser and never crosses this seam.
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
        let config = NativeRoomNetworkConfig {
            topic: RoomTopic::from_room_name_v4(&room_name),
            relay: RelayPolicy::Production,
            bootstrap: None,
            bootstrap_lanes: None,
        };
        self.start_room(Some(room_name), config).await
    }

    async fn join_ticket(self: &Rc<Self>, encoded: String) -> Result<CommandAck, AppError> {
        let ticket = encoded.parse::<NativeRoomTicketV4>().map_err(|error| {
            AppError::new(AppErrorCode::InvalidTicket, "invalid room ticket")
                .with_detail(error.to_string())
        })?;
        let config = NativeRoomNetworkConfig {
            topic: ticket.topic(),
            relay: RelayPolicy::Production,
            bootstrap: Some(ticket.endpoint_addr().clone()),
            bootstrap_lanes: Some(ticket.lanes()),
        };
        self.start_room(None, config).await
    }

    async fn start_room(
        self: &Rc<Self>,
        room_name: Option<String>,
        config: NativeRoomNetworkConfig,
    ) -> Result<CommandAck, AppError> {
        self.stop_active_room().await;

        let topic_string = config.topic.to_string();
        // The IndexedDB journal key. `to_string()` and `to_hex()` are the same
        // hex here, but name the key by its contract: keyed by room topic hex.
        let topic_hex = config.topic.to_hex();
        let presence_topic = *config.topic.as_bytes();
        let bootstrap = config.bootstrap.as_ref().map(|address| address.id);
        let bootstrap_lanes = config.bootstrap_lanes;

        // Room-v4 recovery is strict: the disjoint, lane-tagged IndexedDB blob
        // must decode and every complete record must verify in its declared
        // lane. Storage errors and causal holes refuse room entry instead of
        // silently presenting an empty room over durable history.
        let recovered = super::storage::get_op_journal_v4(&topic_hex)
            .await
            .map_err(persistence_error)?;
        let room = Room::recover(&topic_string, &recovered).map_err(|error| {
            persistence_error(format!("stored operation failed recovery: {error}"))
        })?;
        let pending = room.music().pending_len() + room.extension().pending_len();
        if pending != 0 {
            return Err(persistence_error(format!(
                "{pending} stored operations are missing causal predecessors"
            )));
        }
        let recovered_view = room.view();
        let mut journal = RoomJournalV4::from_records(recovered).map_err(persistence_error)?;
        seed_journal_known(&mut journal, &topic_string).map_err(persistence_error)?;
        let durable = Rc::new(Mutex::new(DurableRoom {
            topic_hex,
            room,
            journal,
            courier: BTreeMap::new(),
        }));

        let mut network = BrowserRoomNetwork::bind(self.identity.iroh_secret(), config)
            .await
            .map_err(|error| {
                AppError::new(
                    AppErrorCode::NetworkUnavailable,
                    "could not start browser Iroh room",
                )
                .with_detail(error.to_string())
            })?;
        let handle = network.handle();
        let own_endpoint = handle.endpoint_id();
        // Wait up to ~5s (iroh's NET_REPORT_TIMEOUT guidance) for the home relay
        // handshake so the emitted ticket carries a real relay address. On wasm
        // the endpoint address is relay-only and empty until then; a 750ms wait
        // routinely lost the race and shipped an undialable, address-less ticket.
        let ticket = handle.settle_ticket(Duration::from_millis(5000)).await;
        let ticket_string = ticket.to_string();

        let (control, mut control_rx) = mpsc::channel(64);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let alive = Rc::new(Cell::new(true));
        let peers: Rc<RefCell<BTreeMap<iroh::EndpointId, (DiscoverySource, PeerPath)>>> =
            Rc::new(RefCell::new(BTreeMap::new()));
        let peer_lanes: Rc<RefCell<BTreeMap<iroh::EndpointId, LaneSet>>> =
            Rc::new(RefCell::new(BTreeMap::new()));
        // author -> (session, sequence, issued_at_ms, local_expires_at_ms)
        let presence_order: Rc<RefCell<BTreeMap<AuthorId, (u64, u64, u64, u64)>>> =
            Rc::new(RefCell::new(BTreeMap::new()));
        let sync_progress = Rc::new(Mutex::new(PeerSyncProgress::default()));

        if let Some(endpoint_id) = bootstrap {
            peers
                .borrow_mut()
                .insert(endpoint_id, (DiscoverySource::Ticket, PeerPath::Connecting));
            self.update_peer(
                endpoint_id,
                DiscoverySource::Ticket,
                PeerPath::Connecting,
                false,
            );
            if let Some(lanes) = bootstrap_lanes {
                peer_lanes.borrow_mut().insert(endpoint_id, lanes);
            }
        }

        // ---- topic rendezvous: auto-peer everyone in this room by code ----
        // Only the room-NAME path (no bootstrap ticket). Additive — the ticket
        // flow is untouched. Each discovered id is seeded as AddressLookup /
        // Connecting; the rendezvous itself feeds iroh's MemoryLookup + gossip
        // join_peers, and iroh resolves the relay address from the hello.
        let rendezvous = if bootstrap.is_none() {
            let peers_for_rdv = peers.clone();
            let lanes_for_rdv = peer_lanes.clone();
            let host_for_rdv = self.clone();
            Some(spawn_rendezvous_v4(
                handle.rendezvous_peering(),
                handle.topic(),
                LaneSet::WALKIE,
                move |endpoint_id, lanes| {
                    lanes_for_rdv.borrow_mut().insert(endpoint_id, lanes);
                    let inserted = {
                        let mut peers = peers_for_rdv.borrow_mut();
                        if peers.contains_key(&endpoint_id) {
                            false
                        } else {
                            peers.insert(
                                endpoint_id,
                                (DiscoverySource::AddressLookup, PeerPath::Connecting),
                            );
                            true
                        }
                    };
                    if inserted {
                        host_for_rdv.update_peer(
                            endpoint_id,
                            DiscoverySource::AddressLookup,
                            PeerPath::Connecting,
                            false,
                        );
                    }
                },
            ))
        } else {
            None
        };

        let signing_key = self.identity.signing_key();

        // ---- control task: local commits and presence --------------------
        {
            let host = self.clone();
            let durable = durable.clone();
            let handle = handle.clone();
            let topic = topic_string.clone();
            let alive = alive.clone();
            let presence_order = presence_order.clone();
            let signing_key = signing_key.clone();
            let mut shutdown_tx = Some(shutdown_tx);
            spawn_local(async move {
                let mut local_presence_session = 0_u64;
                let mut local_presence_sequence = 0_u64;
                loop {
                    match control_rx.next().await {
                        Some(RoomControl::Commit { op, response }) => {
                            let result = commit_room_op(
                                &durable,
                                &signing_key,
                                &topic,
                                op,
                                &handle,
                                &host,
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
                                    let expires =
                                        now.saturating_add(u64::from(body.lease_ms));
                                    let author =
                                        AuthorId(*signing_key.verifying_key().as_bytes());
                                    presence_order.borrow_mut().insert(
                                        author,
                                        (body.session, body.sequence, body.issued_at_ms, expires),
                                    );
                                    let accepted_sequence =
                                        host.apply_presence(author, body, expires);
                                    if let Ok(bytes) = signed.to_wire_bytes()
                                        && let Err(error) = handle.broadcast(bytes).await
                                    {
                                        host.emit_diagnostic(
                                            "presence_broadcast",
                                            &error.to_string(),
                                        );
                                    }
                                    let _ = response.send(Ok(CommandAck { accepted_sequence }));
                                }
                                Err(error) => {
                                    let _ = response.send(Err(AppError::new(
                                        AppErrorCode::InvalidCommand,
                                        "could not sign voice presence",
                                    )
                                    .with_detail(error.to_string())));
                                }
                            }
                        }
                        Some(RoomControl::Shutdown) | None => {
                            alive.set(false);
                            if let Some(shutdown) = shutdown_tx.take() {
                                let _ = shutdown.send(());
                            }
                            break;
                        }
                    }
                }
            });
        }

        // ---- inbound task: gossip, repair queue, membership --------------
        {
            let host = self.clone();
            let durable = durable.clone();
            let handle = handle.clone();
            let topic = topic_string.clone();
            let peers = peers.clone();
            let peer_lanes = peer_lanes.clone();
            let sync_progress = sync_progress.clone();
            let presence_order = presence_order.clone();
            let mut shutdown_rx = shutdown_rx;
            let rendezvous_guard = rendezvous;
            spawn_local(async move {
                use futures::FutureExt;

                // Own the rendezvous task for the room's lifetime; dropping this
                // on shutdown (loop exit below) aborts it.
                let _rendezvous_guard = rendezvous_guard;

                /// One turn of the inbound loop: either the room was told to
                /// shut down, or the network produced (or ended) its stream.
                enum Step {
                    Shutdown,
                    Inbound(Option<BrowserRoomInbound>),
                }

                loop {
                    // Resolve the select FIRST, then act: the shutdown arm
                    // consumes `network`, which it may not do while the other
                    // arm's future still borrows it.
                    let step = futures::select! {
                        _ = shutdown_rx => Step::Shutdown,
                        inbound = network.next_inbound().fuse() => Step::Inbound(inbound),
                    };
                    let inbound = match step {
                        Step::Shutdown => {
                            if let Err(error) = network.shutdown().await {
                                host.emit_diagnostic(
                                    "browser_network_shutdown",
                                    &error.to_string(),
                                );
                            }
                            break;
                        }
                        Step::Inbound(Some(inbound)) => inbound,
                        Step::Inbound(None) => {
                            host.emit_diagnostic(
                                "browser_network_closed",
                                "the browser Iroh room task closed",
                            );
                            break;
                        }
                    };
                    let event = match inbound {
                        BrowserRoomInbound::Repair(repair) => {
                            let expected = peer_lanes
                                .borrow()
                                .get(&repair.endpoint_id)
                                .copied()
                                .unwrap_or(LaneSet::WALKIE);
                            match LaneProtocol::from_alpn(repair.alpn) {
                                Some(LaneProtocol::Repair(RoomLane::Music)) => {
                                    spawn_repair::<MusicLane>(
                                        host.clone(),
                                        durable.clone(),
                                        sync_progress.clone(),
                                        expected,
                                        topic.clone(),
                                        repair,
                                    );
                                }
                                Some(LaneProtocol::Repair(RoomLane::Extension)) => {
                                    spawn_repair::<ExtensionLane>(
                                        host.clone(),
                                        durable.clone(),
                                        sync_progress.clone(),
                                        expected,
                                        topic.clone(),
                                        repair,
                                    );
                                }
                                Some(LaneProtocol::Courier(RoomLane::Music)) => {
                                    spawn_courier::<MusicLane>(
                                        host.clone(),
                                        durable.clone(),
                                        repair,
                                    );
                                }
                                Some(LaneProtocol::Courier(RoomLane::Extension)) => {
                                    spawn_courier::<ExtensionLane>(
                                        host.clone(),
                                        durable.clone(),
                                        repair,
                                    );
                                }
                                None => repair
                                    .connection
                                    .close(4u32.into(), b"unsupported lane protocol"),
                            }
                            continue;
                        }
                        BrowserRoomInbound::Event(event) => event,
                    };
                    match event {
                        NativeNetworkEvent::NeighborUp {
                            endpoint_id,
                            discovery,
                        } => {
                            let source = peers
                                .borrow()
                                .get(&endpoint_id)
                                .map(|peer| peer.0)
                                .unwrap_or(discovery);
                            let path: PeerPath = handle.peer_path(endpoint_id).await.into();
                            peers.borrow_mut().insert(endpoint_id, (source, path));
                            host.update_peer(endpoint_id, source, path, false);
                            // Same deterministic tie-break as desktop: the
                            // smaller endpoint id dials the repair session.
                            if own_endpoint.as_bytes() < endpoint_id.as_bytes() {
                                let advertised = peer_lanes.borrow().get(&endpoint_id).copied();
                                spawn_repair_round(
                                    host.clone(),
                                    durable.clone(),
                                    sync_progress.clone(),
                                    advertised,
                                    topic.clone(),
                                    endpoint_id,
                                    handle.clone(),
                                );
                            }
                        }
                        NativeNetworkEvent::NeighborDown { endpoint_id } => {
                            sync_progress.lock().await.clear(endpoint_id);
                            let known = {
                                let mut peers = peers.borrow_mut();
                                match peers.get_mut(&endpoint_id) {
                                    Some((source, path)) => {
                                        *path = PeerPath::Disconnected;
                                        Some(*source)
                                    }
                                    None => None,
                                }
                            };
                            if let Some(source) = known {
                                host.update_peer(
                                    endpoint_id,
                                    source,
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
                                let mut access = BrowserLaneAccess::<MusicLane>::new(
                                    durable.clone(),
                                    host.clone(),
                                );
                                if let Err(error) = access.apply_gossip(&topic, &bytes).await {
                                    host.emit_diagnostic(
                                        "gossip_rejected",
                                        &format!(
                                            "music frame from {delivered_from} was refused: {error}"
                                        ),
                                    );
                                }
                            } else if bytes.starts_with(ExtensionLang::WIRE_MAGIC) {
                                let mut access = BrowserLaneAccess::<ExtensionLane>::new(
                                    durable.clone(),
                                    host.clone(),
                                );
                                if let Err(error) = access.apply_gossip(&topic, &bytes).await {
                                    host.emit_diagnostic(
                                        "gossip_rejected",
                                        &format!(
                                            "extension frame from {delivered_from} was refused: {error}"
                                        ),
                                    );
                                }
                            } else if let Ok(signed) = SignedPresenceV4::from_wire_bytes(&bytes) {
                                match signed.verify(presence_topic) {
                                    Ok(verified) => {
                                        let body = verified.body;
                                        let now = unix_time_millis();
                                        let future_limit = now.saturating_add(30_000);
                                        let order =
                                            presence_order.borrow().get(&verified.author).copied();
                                        let is_newer = match order {
                                            None => true,
                                            Some((session, sequence, _issued, _))
                                                if session == body.session =>
                                            {
                                                body.sequence > sequence
                                            }
                                            Some((_, _, issued, _)) => body.issued_at_ms > issued,
                                        };
                                        if body.issued_at_ms <= future_limit
                                            && is_newer
                                            && host.presence_pitch_is_valid(body.pitch)
                                        {
                                            let expires =
                                                now.saturating_add(u64::from(body.lease_ms));
                                            presence_order.borrow_mut().insert(
                                                verified.author,
                                                (
                                                    body.session,
                                                    body.sequence,
                                                    body.issued_at_ms,
                                                    expires,
                                                ),
                                            );
                                            host.apply_presence(verified.author, body, expires);
                                        }
                                    }
                                    Err(error) => host
                                        .emit_diagnostic("presence_rejected", &error.to_string()),
                                }
                            } else {
                                host.emit_diagnostic(
                                    "gossip_rejected",
                                    "unknown or malformed room message",
                                );
                            }
                        }
                        NativeNetworkEvent::Lagged => {
                            host.emit_diagnostic(
                                "gossip_lagged",
                                "the gossip event consumer lagged; scheduling anti-entropy repair",
                            );
                            let endpoint_ids = peers
                                .borrow()
                                .iter()
                                .filter(|(_, (_, path))| *path != PeerPath::Disconnected)
                                .map(|(endpoint_id, _)| *endpoint_id)
                                .collect::<Vec<_>>();
                            for endpoint_id in endpoint_ids {
                                if own_endpoint.as_bytes() < endpoint_id.as_bytes() {
                                    let advertised =
                                        peer_lanes.borrow().get(&endpoint_id).copied();
                                    spawn_repair_round(
                                        host.clone(),
                                        durable.clone(),
                                        sync_progress.clone(),
                                        advertised,
                                        topic.clone(),
                                        endpoint_id,
                                        handle.clone(),
                                    );
                                }
                            }
                        }
                        NativeNetworkEvent::Closed => break,
                        NativeNetworkEvent::Diagnostic(message) => {
                            host.emit_diagnostic("browser_network", &message)
                        }
                        // Structurally impossible in a browser (no mDNS), kept
                        // for the shared event type's sake.
                        NativeNetworkEvent::MdnsDiscovered { .. }
                        | NativeNetworkEvent::MdnsExpired { .. } => {}
                    }
                }
            });
        }

        // ---- path refresh task -------------------------------------------
        {
            let host = self.clone();
            let handle = handle.clone();
            let peers = peers.clone();
            let alive = alive.clone();
            spawn_local(async move {
                while alive.get() {
                    n0_future::time::sleep(Duration::from_secs(1)).await;
                    let ids: Vec<_> = peers.borrow().keys().copied().collect();
                    for endpoint_id in ids {
                        let path: PeerPath = handle.peer_path(endpoint_id).await.into();
                        let changed = {
                            let mut peers = peers.borrow_mut();
                            match peers.get_mut(&endpoint_id) {
                                Some((source, previous)) if *previous != path => {
                                    *previous = path;
                                    Some(*source)
                                }
                                _ => None,
                            }
                        };
                        if let Some(source) = changed {
                            host.update_peer(endpoint_id, source, path, false);
                        }
                    }
                }
            });
        }

        // ---- periodic anti-entropy task ----------------------------------
        // Gossip can lose an individual operation without emitting Lagged.
        // Re-run both advertised lanes on a stable per-endpoint jitter, while
        // PeerSyncProgress prevents overlapping rounds to the same peer.
        {
            let host = self.clone();
            let durable = durable.clone();
            let handle = handle.clone();
            let topic = topic_string.clone();
            let peers = peers.clone();
            let peer_lanes = peer_lanes.clone();
            let sync_progress = sync_progress.clone();
            let alive = alive.clone();
            let repair_period =
                Duration::from_secs(25 + u64::from(own_endpoint.as_bytes()[0] % 11));
            spawn_local(async move {
                while alive.get() {
                    n0_future::time::sleep(repair_period).await;
                    if !alive.get() {
                        break;
                    }
                    let endpoint_ids = peers
                        .borrow()
                        .iter()
                        .filter(|(_, (_, path))| *path != PeerPath::Disconnected)
                        .map(|(endpoint_id, _)| *endpoint_id)
                        .collect::<Vec<_>>();
                    for endpoint_id in endpoint_ids {
                        if own_endpoint.as_bytes() < endpoint_id.as_bytes() {
                            let advertised = peer_lanes.borrow().get(&endpoint_id).copied();
                            spawn_repair_round(
                                host.clone(),
                                durable.clone(),
                                sync_progress.clone(),
                                advertised,
                                topic.clone(),
                                endpoint_id,
                                handle.clone(),
                            );
                        }
                    }
                }
            });
        }

        // ---- presence expiry task ----------------------------------------
        {
            let host = self.clone();
            let presence_order = presence_order.clone();
            let alive = alive.clone();
            spawn_local(async move {
                while alive.get() {
                    n0_future::time::sleep(Duration::from_millis(250)).await;
                    let now = unix_time_millis();
                    let expired: Vec<_> = presence_order
                        .borrow()
                        .iter()
                        .filter(|(_, (_, _, _, expires))| *expires <= now)
                        .map(|(author, _)| *author)
                        .collect();
                    for author in expired {
                        if let Some((session, _, _, _)) =
                            presence_order.borrow_mut().remove(&author)
                        {
                            host.expire_presence(author, session);
                        }
                    }
                }
            });
        }

        // ---- project the fresh room into the snapshot --------------------
        {
            let mut state = self.state.borrow_mut();
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
            state.snapshot.tuning = Some(TuningDefinition::twelve_tet());
            state.snapshot.tuning_id = state
                .snapshot
                .tuning
                .as_ref()
                .map(|definition| definition.id);
            state.active_room = Some(ActiveRoom { control, alive });
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
            state.snapshot.pieces_locked = false;
            state.snapshot.available_emojis = None;
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
        if let Some(active) = active {
            active.alive.set(false);
            let mut control = active.control;
            let _ = control.send(RoomControl::Shutdown).await;
        }
    }

    async fn submit_durable(&self, op: LocalRoomOp) -> Result<CommandAck, AppError> {
        let control = self
            .state
            .borrow()
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
        let mut control = control;
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
        pitch: Option<TunedPeriodicPitch>,
    ) -> Result<CommandAck, AppError> {
        let control = self
            .state
            .borrow()
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
        let mut control = control;
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

    fn current_tuning(&self) -> Result<crate::tuning::Tuning, AppError> {
        self.state
            .borrow()
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

    fn validate_degree(&self, pitch: TunedDegree) -> Result<(), AppError> {
        let tuning = self.current_tuning()?;
        pitch.validate(&tuning).map(|_| ()).map_err(|error| {
            AppError::new(AppErrorCode::InvalidCommand, "invalid tuning-scoped degree")
                .with_detail(error.to_string())
        })
    }

    fn validate_periodic_pitch(&self, pitch: TunedPeriodicPitch) -> Result<(), AppError> {
        let tuning = self.current_tuning()?;
        pitch.validate(&tuning).map(|_| ()).map_err(|error| {
            AppError::new(
                AppErrorCode::InvalidCommand,
                "invalid tuning-scoped periodic pitch",
            )
            .with_detail(error.to_string())
        })
    }

    fn presence_pitch_is_valid(&self, pitch: Option<TunedPeriodicPitch>) -> bool {
        let Ok(tuning) = self.current_tuning() else {
            return false;
        };
        pitch.is_none_or(|pitch| pitch.validate(&tuning).is_ok())
    }

    /// Project a fresh `RoomView` into the snapshot, emitting the same delta
    /// events the desktop runtime emits (minus MIDI, which is Web MIDI here).
    fn apply_room_view(&self, view: RoomView) -> u64 {
        let mut events = Vec::new();
        {
            let mut state = self.state.borrow_mut();

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
                .map(|piece| crate::client::PieceSnapshot {
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
        }
        for event in events {
            self.emit(event);
        }
        self.sequence()
    }

    fn apply_presence(&self, author: AuthorId, body: PresenceBody, expires_at_ms: u64) -> u64 {
        let event = {
            let mut state = self.state.borrow_mut();
            match body.pitch {
                Some(pitch) => {
                    let voice = crate::client::VoiceSnapshot {
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
                    AppEvent::VoiceUpdated { voice }
                }
                None => {
                    state.snapshot.voices.retain(|voice| voice.author != author);
                    AppEvent::VoiceExpired {
                        author,
                        session: body.session,
                    }
                }
            }
        };
        self.emit(event);
        self.sequence()
    }

    fn expire_presence(&self, author: AuthorId, session: u64) {
        let was_present = {
            let mut state = self.state.borrow_mut();
            let was_present = state
                .snapshot
                .voices
                .iter()
                .any(|voice| voice.author == author && voice.session == session);
            state
                .snapshot
                .voices
                .retain(|voice| voice.author != author || voice.session != session);
            was_present
        };
        if was_present {
            self.emit(AppEvent::VoiceExpired { author, session });
        }
    }

    fn update_peer(
        &self,
        endpoint_id: iroh::EndpointId,
        discovery: DiscoverySource,
        path: PeerPath,
        synchronized: bool,
    ) {
        let peer = {
            let mut state = self.state.borrow_mut();
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
            peer
        };
        self.emit(AppEvent::PeerUpdated { peer });
    }

    fn update_peer_rtt(&self, endpoint_id: iroh::EndpointId, rtt: Duration) {
        let peer = {
            let mut state = self.state.borrow_mut();
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
            peer.clone()
        };
        self.emit(AppEvent::PeerUpdated { peer });
    }

    fn mark_peer_synchronized(&self, endpoint_id: iroh::EndpointId) {
        let peer = {
            let mut state = self.state.borrow_mut();
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
            peer.clone()
        };
        self.emit(AppEvent::PeerUpdated { peer });
    }
}

fn seed_journal_known(journal: &mut RoomJournalV4, topic: &str) -> Result<(), String> {
    let records = journal.records().to_vec();
    for record in records {
        let id = match record.lane {
            RoomLane::Music => {
                let signed = SignedOp::from_wire_bytes_in::<MusicLang>(&record.wire)
                    .map_err(|error| error.to_string())?;
                verify_music_op(&signed, topic)
                    .map_err(|error| error.to_string())?
                    .id()
            }
            RoomLane::Extension => {
                let signed = SignedOp::from_wire_bytes_in::<ExtensionLang>(&record.wire)
                    .map_err(|error| error.to_string())?;
                verify_extension_op(&signed, topic)
                    .map_err(|error| error.to_string())?
                    .id()
            }
        };
        journal.mark_known(record.lane, id);
    }
    Ok(())
}

trait BrowserLane: LaneSpec {
    const LANE: RoomLane;

    fn store(room: &Room) -> &Store<Self::Lang>;
    fn ingest(room: &mut Room, op: VerifiedOpG<Self::Lang>) -> WindowIngest;
}

impl BrowserLane for MusicLane {
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

impl BrowserLane for ExtensionLane {
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

/// A private staging sink. It mutates only a room reconstructed from the last
/// durable journal; the shared room is replaced only after IndexedDB commits.
struct BrowserLaneSink<'a, P: BrowserLane> {
    room: &'a mut Room,
    journal: &'a mut RoomJournalV4,
    lane: PhantomData<P>,
}

impl<P: BrowserLane> LaneIngest<P::Lang> for BrowserLaneSink<'_, P> {
    fn lifted_entry(&self, id: OpId) -> Option<EntryHash> {
        P::store(self.room).lifted_entry(id)
    }

    fn knows_op(&self, id: OpId) -> bool {
        P::store(self.room).knows_op(id)
    }

    fn ingest_lane(
        &mut self,
        wire: &[u8],
        op: VerifiedOpG<P::Lang>,
    ) -> Result<WindowIngest, SyncError> {
        self.journal
            .admit(P::LANE, op.id(), wire)
            .map_err(|error| SyncError::Persistence(error.to_string()))?;
        Ok(P::ingest(self.room, op))
    }
}

struct BrowserLaneAccess<P: BrowserLane> {
    durable: SharedDurableRoom,
    host: Rc<BrowserHost>,
    lane: PhantomData<P>,
}

impl<P: BrowserLane> BrowserLaneAccess<P> {
    fn new(durable: SharedDurableRoom, host: Rc<BrowserHost>) -> Self {
        Self {
            durable,
            host,
            lane: PhantomData,
        }
    }

    async fn apply_gossip(&mut self, topic: &str, wire: &[u8]) -> Result<usize, SyncError> {
        let (report, view) = {
            let mut durable = self.durable.lock().await;
            let mut staged_room = Room::recover(topic, durable.journal.records())
                .map_err(|error| SyncError::Persistence(error.to_string()))?;
            let mut staged_journal = durable.journal.clone();
            let before = staged_journal.len();
            let report = {
                let mut sink = BrowserLaneSink::<P> {
                    room: &mut staged_room,
                    journal: &mut staged_journal,
                    lane: PhantomData,
                };
                ingest_pairs::<P::Lang, _>(
                    &mut sink,
                    topic,
                    [IncomingOp {
                        claimed_entry: None,
                        wire,
                    }],
                )?
            };
            if staged_journal.len() != before {
                super::storage::set_op_journal_v4(
                    &durable.topic_hex,
                    staged_journal.records(),
                )
                .await
                .map_err(SyncError::Persistence)?;
            }
            let view = staged_room.view();
            durable.room = staged_room;
            durable.journal = staged_journal;
            (report, view)
        };
        if !report.lifted.is_empty() {
            self.host.apply_room_view(view);
        }
        Ok(report.lifted.len())
    }
}

impl<P: BrowserLane> LaneStoreAccess<P::Lang> for BrowserLaneAccess<P> {
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
            let mut staged_room = Room::recover(topic, durable.journal.records())
                .map_err(|error| SyncError::Persistence(error.to_string()))?;
            let mut staged_journal = durable.journal.clone();
            let before = staged_journal.len();
            let report = {
                let mut sink = BrowserLaneSink::<P> {
                    room: &mut staged_room,
                    journal: &mut staged_journal,
                    lane: PhantomData,
                };
                ingest_pairs::<P::Lang, _>(
                    &mut sink,
                    topic,
                    pairs.iter().map(IncomingOp::from),
                )?
            };
            if staged_journal.len() != before {
                super::storage::set_op_journal_v4(
                    &durable.topic_hex,
                    staged_journal.records(),
                )
                .await
                .map_err(SyncError::Persistence)?;
            }
            source.absorb(P::store(&staged_room), &report.lifted)?;
            let view = staged_room.view();
            durable.room = staged_room;
            durable.journal = staged_journal;
            (report, view)
        };
        if !report.lifted.is_empty() {
            self.host.apply_room_view(view);
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

type SharedSyncProgress = Rc<Mutex<PeerSyncProgress>>;

async fn finish_repair<P: BrowserLane>(
    result: Result<SyncOutcome, String>,
    host: &Rc<BrowserHost>,
    progress: &SharedSyncProgress,
    expected: LaneSet,
    endpoint_id: iroh::EndpointId,
    telemetry: &iroh::endpoint::Connection,
) {
    match result {
        Ok(outcome) if !outcome.root_mismatch && !outcome.incomplete => {
            if let Some(rtt) = telemetry.rtt(iroh::endpoint::PathId::ZERO) {
                host.update_peer_rtt(endpoint_id, rtt);
            }
            if progress.lock().await.record(endpoint_id, P::LANE, expected) {
                host.mark_peer_synchronized(endpoint_id);
            }
            host.emit_diagnostic(
                "repair_complete",
                &format!(
                    "{} repair with {endpoint_id} completed; ingested {} operations",
                    String::from_utf8_lossy(P::ALPN),
                    outcome.ingested,
                ),
            );
        }
        Ok(outcome) => host.emit_diagnostic(
            "repair_incomplete",
            &format!(
                "{} repair with {endpoint_id} ended without convergence (root_mismatch={}, incomplete={})",
                String::from_utf8_lossy(P::ALPN),
                outcome.root_mismatch,
                outcome.incomplete,
            ),
        ),
        Err(error) => host.emit_diagnostic(
            "repair_failed",
            &format!(
                "{} repair with {endpoint_id} failed: {error}",
                String::from_utf8_lossy(P::ALPN)
            ),
        ),
    }
}

fn spawn_repair<P: BrowserLane>(
    host: Rc<BrowserHost>,
    durable: SharedDurableRoom,
    progress: SharedSyncProgress,
    expected: LaneSet,
    topic: String,
    repair: BrowserIncomingRepair,
) {
    spawn_local(async move {
        let endpoint_id = repair.endpoint_id;
        let telemetry = repair.connection.clone();
        let result = run_repair_session::<P>(
            repair.stream.owning(repair.connection),
            false,
            durable,
            topic,
            host.clone(),
        )
        .await;
        finish_repair::<P>(result, &host, &progress, expected, endpoint_id, &telemetry).await;
    });
}

fn spawn_repair_round(
    host: Rc<BrowserHost>,
    durable: SharedDurableRoom,
    progress: SharedSyncProgress,
    advertised: Option<LaneSet>,
    topic: String,
    endpoint_id: iroh::EndpointId,
    handle: BrowserNetHandle,
) {
    spawn_local(async move {
        if !progress.lock().await.begin_outbound(endpoint_id) {
            return;
        }
        let attempted = advertised.unwrap_or(LaneSet::WALKIE);
        let music = {
            let host = host.clone();
            let durable = durable.clone();
            let topic = topic.clone();
            let handle = handle.clone();
            async move {
                if attempted.contains(RoomLane::Music) {
                    dial_and_run::<MusicLane>(&handle, endpoint_id, durable, topic, host).await
                } else {
                    None
                }
            }
        };
        let extension = {
            let host = host.clone();
            async move {
                if attempted.contains(RoomLane::Extension) {
                    dial_and_run::<ExtensionLane>(&handle, endpoint_id, durable, topic, host).await
                } else {
                    None
                }
            }
        };
        let (music, extension) = futures::join!(music, extension);
        let negotiated_bits = u8::from(music.is_some()) * RoomLane::Music.tag()
            | u8::from(extension.is_some()) * RoomLane::Extension.tag();
        if let Some(expected) = advertised.or_else(|| LaneSet::from_bits(negotiated_bits)) {
            if let Some((telemetry, result)) = music {
                finish_repair::<MusicLane>(
                    result,
                    &host,
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
                    &host,
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

async fn dial_and_run<P: BrowserLane>(
    handle: &BrowserNetHandle,
    endpoint_id: iroh::EndpointId,
    durable: SharedDurableRoom,
    topic: String,
    host: Rc<BrowserHost>,
) -> Option<(iroh::endpoint::Connection, Result<SyncOutcome, String>)> {
    let connection = match handle
        .begin_lane(endpoint_id, LaneProtocol::Repair(P::LANE))
        .await
    {
        Ok(connection) => connection,
        Err(error) => {
            host.emit_diagnostic(
                "repair_connect",
                &format!(
                    "{} repair connection to {endpoint_id} failed: {error}",
                    String::from_utf8_lossy(P::ALPN)
                ),
            );
            return None;
        }
    };
    let stream = match IrohSyncStream::open(&connection).await {
        Ok(stream) => stream,
        Err(error) => {
            host.emit_diagnostic("repair_connect", &error.to_string());
            return None;
        }
    };
    let telemetry = connection.clone();
    let result = run_repair_session::<P>(
        stream.owning(connection),
        true,
        durable,
        topic,
        host,
    )
    .await;
    Some((telemetry, result))
}

async fn run_repair_session<P: BrowserLane>(
    stream: IrohSyncStream,
    initiator: bool,
    durable: SharedDurableRoom,
    topic: String,
    host: Rc<BrowserHost>,
) -> Result<SyncOutcome, String> {
    let mut access = BrowserLaneAccess::<P>::new(durable, host);
    let limits = SyncLimits::default();
    if initiator {
        drive_initiator::<P, _, _, _>(stream, &BrowserTimer, &mut access, &topic, limits).await
    } else {
        drive_responder::<P, _, _, _>(stream, &BrowserTimer, &mut access, &topic, limits).await
    }
    .map_err(|error| error.to_string())
}

fn spawn_courier<P: BrowserLane>(
    host: Rc<BrowserHost>,
    durable: SharedDurableRoom,
    repair: BrowserIncomingRepair,
) {
    spawn_local(async move {
        let endpoint_id = repair.endpoint_id;
        let mut stream = repair.stream.owning(repair.connection);
        let result: Result<(), String> = async {
            let Some(frame) = stream.recv_frame().await.map_err(|error| error.to_string())? else {
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
        if let Err(error) = result {
            host.emit_diagnostic(
                "courier_failed",
                &format!(
                    "{} courier with {endpoint_id} failed: {error}",
                    String::from_utf8_lossy(P::COURIER_ALPN)
                ),
            );
        }
    });
}

fn prepared_id(prepared: &LocalRoomPrepared, topic: &str) -> Result<OpId, AppError> {
    match prepared {
        LocalRoomPrepared::Music(signed) => verify_music_op(signed, topic)
            .map(|verified| verified.id())
            .map_err(persistence_error),
        LocalRoomPrepared::Extension(signed) => verify_extension_op(signed, topic)
            .map(|verified| verified.id())
            .map_err(persistence_error),
    }
}

async fn commit_room_op(
    durable: &SharedDurableRoom,
    signing_key: &SigningKey,
    topic: &str,
    op: LocalRoomOp,
    handle: &BrowserNetHandle,
    host: &Rc<BrowserHost>,
) -> Result<u64, AppError> {
    let (wire, view) = {
        let mut durable = durable.lock().await;
        let prepared = durable
            .room
            .prepare(signing_key, topic, unix_time_micros(), op);
        let wire = prepared.to_wire_bytes().map_err(persistence_error)?;
        let id = prepared_id(&prepared, topic)?;
        let mut staged_journal = durable.journal.clone();
        staged_journal
            .admit(prepared.lane(), id, &wire)
            .map_err(persistence_error)?;
        super::storage::set_op_journal_v4(&durable.topic_hex, staged_journal.records())
            .await
            .map_err(persistence_error)?;
        durable
            .room
            .ingest_prepared(topic, &prepared)
            .expect("a just-signed, topic-scoped lane operation verifies");
        durable.journal = staged_journal;
        (wire, durable.room.view())
    };
    if let Err(error) = handle.broadcast(wire).await {
        host.emit_diagnostic(
            "gossip_broadcast",
            &format!("operation committed locally but broadcast failed: {error}"),
        );
    }
    Ok(host.apply_room_view(view))
}

fn persistence_error(error: impl std::fmt::Display) -> AppError {
    AppError::new(AppErrorCode::Persistence, "durable room storage failed")
        .with_detail(error.to_string())
}

fn unix_time_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_micros()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn unix_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}
