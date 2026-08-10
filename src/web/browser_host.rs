//! In-page host for the browser iroh transport.
//!
//! A plain browser tab has no Tauri runtime, so this module plays the role
//! `src-tauri`'s `AppRuntime` plays on desktop: it owns the `WalkieIdentity`,
//! the signed `RoomStore`, and the live `BrowserRoomNetwork`, accepts the same
//! [`ClientCommand`]s, and emits the same ordered [`AppEventEnvelope`]s. The
//! UI cannot tell the difference — `app.rs` routes through one dispatch/apply
//! seam either way.
//!
//! Differences from the desktop runtime, all deliberate:
//! * a signed-op journal in IndexedDB, keyed by the room topic hex, rather than
//!   the desktop file journal (`src/room/journal.rs`): the store is seeded from
//!   it on start and it grows on every admitted op, so a lone tab keeps its
//!   history across a reload. It is additive to gossip + anti-entropy — peers
//!   still converge exactly as before;
//! * no native MIDI — the browser keeps Web MIDI, so MIDI commands are
//!   acknowledged and ignored;
//! * relay-only reachability (see [`crate::net::browser`]).

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
        BrowserTimer, IrohSyncStream, LaneStoreAccess, NativeNetworkEvent,
        NativeRoomNetworkConfig, NativeRoomTicket, RelayPolicy, RoomSyncSource, RoomTopic,
        SyncApply, SyncError, SyncLimits, SyncOutcome, WalkieIdentity, WalkieLane,
        drive_initiator, drive_responder,
    },
    room::{
        ops::{AuthorId, OpId, SignedOp, SigningKey, WalkieLang, WalkieOp, verify_signed_op_for_topic},
        presence::{PresenceBody, SignedPresence},
        store::{RoomStore, RoomView},
    },
    tuning::{TunedDegree, TunedPeriodicPitch, TuningDefinition},
};

enum RoomControl {
    Commit {
        op: WalkieOp,
        response: oneshot::Sender<Result<CommandAck, AppError>>,
    },
    Presence {
        session: u64,
        pitch: Option<TunedPeriodicPitch>,
        response: oneshot::Sender<Result<CommandAck, AppError>>,
    },
    Shutdown,
}

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
        durable_storage: false,
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
                self.submit_durable(WalkieOp::SetTuning { definition })
                    .await
            }
            ClientCommand::AddDegree { pitch } => {
                self.validate_degree(pitch)?;
                self.submit_durable(WalkieOp::AddDegree { pitch }).await
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
            topic: RoomTopic::from_room_name(&room_name),
            relay: RelayPolicy::Production,
            bootstrap: None,
            bootstrap_lanes: None,
        };
        self.start_room(Some(room_name), config).await
    }

    async fn join_ticket(self: &Rc<Self>, encoded: String) -> Result<CommandAck, AppError> {
        let ticket = encoded.parse::<NativeRoomTicket>().map_err(|error| {
            AppError::new(AppErrorCode::InvalidTicket, "invalid room ticket")
                .with_detail(error.to_string())
        })?;
        let config = NativeRoomNetworkConfig {
            topic: ticket.topic(),
            relay: RelayPolicy::Production,
            bootstrap: Some(ticket.endpoint_addr().clone()),
            bootstrap_lanes: None,
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

        // Signed-op journal: rather than converge from empty, seed the store
        // from this room's IndexedDB journal so a solo reload keeps its history
        // with no peer present, then grow the journal on every admitted op.
        // Additive to gossip + anti-entropy; a read/write failure just falls
        // back to the old empty-then-converge behavior.
        let store = Rc::new(RefCell::new(RoomStore::new()));
        let journal = Rc::new(RefCell::new(RoomJournal::new(topic_hex.clone())));
        rehydrate_from_journal(&store, &journal, &topic_hex, &topic_string, self).await;

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
        // author -> (session, sequence, issued_at_ms, local_expires_at_ms)
        let presence_order: Rc<RefCell<BTreeMap<AuthorId, (u64, u64, u64, u64)>>> =
            Rc::new(RefCell::new(BTreeMap::new()));

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
        }

        // ---- topic rendezvous: auto-peer everyone in this room by code ----
        // Only the room-NAME path (no bootstrap ticket). Additive — the ticket
        // flow is untouched. Each discovered id is seeded as AddressLookup /
        // Connecting; the rendezvous itself feeds iroh's MemoryLookup + gossip
        // join_peers, and iroh resolves the relay address from the hello.
        let rendezvous = if bootstrap.is_none() {
            let peers_for_rdv = peers.clone();
            let host_for_rdv = self.clone();
            Some(crate::net::spawn_rendezvous(
                handle.rendezvous_peering(),
                handle.topic(),
                move |endpoint_id| {
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
            let store = store.clone();
            let handle = handle.clone();
            let topic = topic_string.clone();
            let alive = alive.clone();
            let presence_order = presence_order.clone();
            let signing_key = signing_key.clone();
            let journal = journal.clone();
            let mut shutdown_tx = Some(shutdown_tx);
            spawn_local(async move {
                let mut local_presence_session = 0_u64;
                let mut local_presence_sequence = 0_u64;
                loop {
                    match control_rx.next().await {
                        Some(RoomControl::Commit { op, response }) => {
                            let result = commit_room_op(
                                &store,
                                &signing_key,
                                &topic,
                                op,
                                &handle,
                                &host,
                                &journal,
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
            let store = store.clone();
            let handle = handle.clone();
            let topic = topic_string.clone();
            let peers = peers.clone();
            let presence_order = presence_order.clone();
            let journal = journal.clone();
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

                let own_endpoint = handle.endpoint_id();
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
                            spawn_repair(
                                host.clone(),
                                store.clone(),
                                journal.clone(),
                                topic.clone(),
                                repair,
                                false,
                            );
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
                                match dial_repair(&handle, endpoint_id).await {
                                    Ok(repair) => spawn_repair(
                                        host.clone(),
                                        store.clone(),
                                        journal.clone(),
                                        topic.clone(),
                                        repair,
                                        true,
                                    ),
                                    Err(error) => {
                                        host.emit_diagnostic("repair_connect", &error)
                                    }
                                }
                            }
                        }
                        NativeNetworkEvent::NeighborDown { endpoint_id } => {
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
                            if let Ok(signed) = SignedOp::from_wire_bytes(&bytes) {
                                match verify_signed_op_for_topic(&signed, &topic) {
                                    Ok(verified) => {
                                        if verified.author().0 != *delivered_from.as_bytes() {
                                            host.emit_diagnostic(
                                                "gossip_forwarded",
                                                "received a valid operation through a forwarding neighbor",
                                            );
                                        }
                                        let id = verified.id();
                                        let view = {
                                            let mut store = store.borrow_mut();
                                            store.ingest_verified(verified);
                                            store.view()
                                        };
                                        // Admitted (verified + kept): journal the
                                        // verbatim bytes so a solo reload keeps it.
                                        journal_admit(&journal, id, bytes);
                                        host.apply_room_view(view);
                                    }
                                    Err(error) => host
                                        .emit_diagnostic("gossip_rejected", &error.to_string()),
                                }
                            } else if let Ok(signed) = SignedPresence::from_wire_bytes(&bytes) {
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
                        NativeNetworkEvent::Lagged => host.emit_diagnostic(
                            "gossip_lagged",
                            "the gossip event consumer lagged; anti-entropy repair is required",
                        ),
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
        let initial_view = store.borrow().view();
        self.apply_room_view(initial_view);
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

    async fn submit_durable(&self, op: WalkieOp) -> Result<CommandAck, AppError> {
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

/// A room's local signed-op journal: the in-memory mirror of what is persisted
/// to IndexedDB under the room topic hex. Additive and behavior-preserving — it
/// seeds the store on start and grows on every admitted op so a lone reloader
/// keeps their history; it never touches op semantics, gossip, or RBSR.
///
/// Deduped by op id, so re-noting a known op (duplicate gossip, a repair that
/// re-delivers what we already hold) is a no-op. Its lifetime is the room's:
/// each room owns one keyed to its own topic, so switching rooms cannot
/// cross-contaminate.
struct RoomJournal {
    /// The room topic hex — the IndexedDB key this journal persists under.
    topic_hex: String,
    /// Op ids already recorded, for idempotent notes.
    known: BTreeSet<OpId>,
    /// Verbatim signed-op wire bytes, in admit order — exactly what each author
    /// produced, so a rehydrated op is byte-identical for anti-entropy.
    records: Vec<Vec<u8>>,
}

impl RoomJournal {
    fn new(topic_hex: String) -> Self {
        Self {
            topic_hex,
            known: BTreeSet::new(),
            records: Vec::new(),
        }
    }

    /// Record an admitted op's verbatim bytes. Idempotent by op id: returns
    /// `true` iff the op was newly added (so the caller should persist), `false`
    /// if it was already journaled.
    fn note(&mut self, id: OpId, wire_bytes: Vec<u8>) -> bool {
        if !self.known.insert(id) {
            return false;
        }
        self.records.push(wire_bytes);
        true
    }
}

/// Persist the journal's current records to IndexedDB off the hot path. A write
/// failure is logged and swallowed — the journal is a best-effort local cache
/// backstopped by anti-entropy, so it must never block or fail room activity.
fn persist_journal(journal: &Rc<RefCell<RoomJournal>>) {
    let (topic_hex, records) = {
        let journal = journal.borrow();
        (journal.topic_hex.clone(), journal.records.clone())
    };
    spawn_local(async move {
        if let Err(error) = super::storage::set_op_journal(&topic_hex, &records).await {
            web_sys::console::warn_1(
                &format!("op journal write failed (history may not survive reload): {error}")
                    .into(),
            );
        }
    });
}

/// Note one admitted op and persist iff it was new. Used by the single-op admit
/// paths (local commit, one gossip frame).
fn journal_admit(journal: &Rc<RefCell<RoomJournal>>, id: OpId, wire_bytes: Vec<u8>) {
    if journal.borrow_mut().note(id, wire_bytes) {
        persist_journal(journal);
    }
}

/// Seed the store (and the in-memory journal mirror) from the IndexedDB journal
/// before the network comes up, so the projection paints restored state with no
/// peer present. Malformed or unverifiable records are skipped; the store dedups
/// and strict-defers, so replay is idempotent and order-independent.
async fn rehydrate_from_journal(
    store: &Rc<RefCell<RoomStore>>,
    journal: &Rc<RefCell<RoomJournal>>,
    topic_hex: &str,
    verify_topic: &str,
    host: &BrowserHost,
) {
    let records = super::storage::get_op_journal(topic_hex).await;
    if records.is_empty() {
        return;
    }
    let mut restored = 0usize;
    let mut skipped = 0usize;
    {
        let mut store = store.borrow_mut();
        let mut journal = journal.borrow_mut();
        for bytes in records {
            let Ok(signed) = SignedOp::from_wire_bytes(&bytes) else {
                skipped += 1;
                continue;
            };
            let Ok(verified) = verify_signed_op_for_topic(&signed, verify_topic) else {
                skipped += 1;
                continue;
            };
            let id = verified.id();
            store.ingest_verified(verified);
            journal.note(id, bytes);
            restored += 1;
        }
    }
    host.emit_diagnostic(
        "journal_restored",
        &format!("restored {restored} operation(s) from the local journal ({skipped} skipped)"),
    );
    // If any records were dropped, rewrite the compacted journal so the on-disk
    // blob stays clean; harmless if everything was valid (identical bytes).
    if skipped > 0 {
        persist_journal(journal);
    }
}

/// Dial a peer and open the one bi-stream a repair session runs over.
async fn dial_repair(
    handle: &BrowserNetHandle,
    endpoint_id: iroh::EndpointId,
) -> Result<BrowserIncomingRepair, String> {
    let connection = handle
        .begin_repair(endpoint_id)
        .await
        .map_err(|error| error.to_string())?;
    let stream = IrohSyncStream::open(&connection)
        .await
        .map_err(|error| error.to_string())?;
    Ok(BrowserIncomingRepair {
        endpoint_id,
        alpn: crate::net::RBSR_ALPN,
        connection,
        stream,
    })
}

fn spawn_repair(
    host: Rc<BrowserHost>,
    store: Rc<RefCell<RoomStore>>,
    journal: Rc<RefCell<RoomJournal>>,
    topic: String,
    repair: BrowserIncomingRepair,
    initiator: bool,
) {
    spawn_local(async move {
        let endpoint_id = repair.endpoint_id;
        let telemetry_connection = repair.connection.clone();
        let stream = repair.stream.owning(repair.connection);
        match run_repair_session(stream, initiator, store, journal, topic, host.clone()).await {
            Ok(ingested) => {
                if let Some(rtt) = telemetry_connection.rtt(iroh::endpoint::PathId::ZERO) {
                    host.update_peer_rtt(endpoint_id, rtt);
                }
                host.mark_peer_synchronized(endpoint_id);
                host.emit_diagnostic(
                    "repair_complete",
                    &format!(
                        "HHHS H6 repair with {endpoint_id} completed; ingested {ingested} operations"
                    ),
                );
            }
            Err(error) => host.emit_diagnostic(
                "repair_failed",
                &format!("HHHS H6 repair with {endpoint_id} failed: {error}"),
            ),
        }
    });
}

/// [`LaneStoreAccess`] over the in-memory browser store's v3 single lane.
///
/// Borrows per call and NEVER across an await: the same store serves gossip
/// ingest and local commits on this thread, and a `RefCell` borrow held across
/// a network round trip is a guaranteed panic the first time gossip races a
/// session.
struct BrowserSyncStore {
    store: Rc<RefCell<RoomStore>>,
    journal: Rc<RefCell<RoomJournal>>,
    host: Rc<BrowserHost>,
}

impl LaneStoreAccess<WalkieLang> for BrowserSyncStore {
    async fn capture(&mut self, salt: [u8; 16]) -> Result<RoomSyncSource, SyncError> {
        Ok(RoomSyncSource::capture(&self.store.borrow(), salt)?)
    }

    async fn apply(
        &mut self,
        topic: &str,
        pairs: &[(EntryHash, Vec<u8>)],
        source: &mut RoomSyncSource,
    ) -> Result<SyncApply, SyncError> {
        let (admitted, lifted, view, journal_dirty) = {
            let mut store = self.store.borrow_mut();
            let mut journal = self.journal.borrow_mut();
            // The session's admitted set: every hash verified and KEPT (lifted
            // or parked). Undecodable or unverifiable pairs are left out, which
            // is what makes them eligible to be asked for again. Every admitted
            // op is also noted in the journal (idempotent by op id) so a solo
            // reload keeps repair-delivered history.
            let mut admitted = Vec::new();
            let mut lifted = Vec::new();
            let mut journal_dirty = false;
            for (wire_hash, bytes) in pairs {
                let Ok(signed) = SignedOp::from_wire_bytes(bytes) else {
                    continue;
                };
                let Ok(verified) = verify_signed_op_for_topic(&signed, topic) else {
                    continue;
                };
                let id = verified.id();
                journal_dirty |= journal.note(id, bytes.clone());
                if let Some(entry) = store.lifted_entry(id) {
                    admitted.push(entry);
                    continue;
                }
                if store.knows_op(id) {
                    admitted.push(*wire_hash);
                    continue;
                }
                let newly = store.ingest_verified(verified);
                if newly.is_empty() {
                    admitted.push(*wire_hash);
                } else {
                    admitted.extend(newly.iter().copied());
                    lifted.extend(newly);
                }
            }
            source.absorb(&store, &lifted)?;
            let view = store.view();
            (admitted, lifted, view, journal_dirty)
        };
        // One IndexedDB write per session, not per op.
        if journal_dirty {
            persist_journal(&self.journal);
        }
        if !lifted.is_empty() {
            self.host.apply_room_view(view);
        }
        Ok(SyncApply {
            admitted,
            lifted: lifted.len(),
            // The browser runs a full (non-windowed) store, so it never defers
            // ops for courier admission.
            courier: Vec::new(),
        })
    }
}

/// Drive one HHHS H6 anti-entropy session over an established stream. The
/// session logic is `net::sync`, shared with desktop and the loopback tests.
async fn run_repair_session(
    stream: IrohSyncStream,
    initiator: bool,
    store: Rc<RefCell<RoomStore>>,
    journal: Rc<RefCell<RoomJournal>>,
    topic: String,
    host: Rc<BrowserHost>,
) -> Result<usize, String> {
    let mut access = BrowserSyncStore {
        store,
        journal,
        host: host.clone(),
    };
    let limits = SyncLimits::default();
    let outcome: SyncOutcome = if initiator {
        drive_initiator::<WalkieLane, _, _, _>(stream, &BrowserTimer, &mut access, &topic, limits)
            .await
    } else {
        drive_responder::<WalkieLane, _, _, _>(stream, &BrowserTimer, &mut access, &topic, limits)
            .await
    }
    .map_err(|error| error.to_string())?;

    if outcome.root_mismatch {
        host.emit_diagnostic(
            "repair_root_mismatch",
            "HHHS repair peers reported different roots; periodic repair will retry",
        );
    }
    if outcome.incomplete {
        host.emit_diagnostic(
            "repair_incomplete",
            "HHHS repair ended before both halves finished; periodic repair will retry",
        );
    }
    Ok(outcome.ingested)
}

async fn commit_room_op(
    store: &Rc<RefCell<RoomStore>>,
    signing_key: &SigningKey,
    topic: &str,
    op: WalkieOp,
    handle: &BrowserNetHandle,
    host: &Rc<BrowserHost>,
    journal: &Rc<RefCell<RoomJournal>>,
) -> Result<u64, AppError> {
    let (signed, view) = {
        let mut store = store.borrow_mut();
        let signed = store.commit(signing_key, topic, unix_time_micros(), op);
        (signed, store.view())
    };
    match signed.to_wire_bytes() {
        Ok(bytes) => {
            // Admitted (a just-committed op is always kept): journal the verbatim
            // bytes before broadcast so history survives a solo reload.
            if let Ok(verified) = verify_signed_op_for_topic(&signed, topic) {
                journal_admit(journal, verified.id(), bytes.clone());
            }
            if let Err(error) = handle.broadcast(bytes).await {
                host.emit_diagnostic(
                    "gossip_broadcast",
                    &format!("operation committed locally but broadcast failed: {error}"),
                );
            }
        }
        Err(error) => host.emit_diagnostic("operation_frame", &error.to_string()),
    }
    Ok(host.apply_room_view(view))
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
