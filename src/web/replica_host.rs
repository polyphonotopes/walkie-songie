//! Capability-native in-page Room-v5 host.
//!
//! The window owns iroh/WebRTC, rendezvous, tasks, audio, and UI events. One
//! dedicated worker owns both HHHS lanes, materialization, and their existing
//! Room-v5 IndexedDB logs.

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    future::Future,
    rc::{Rc, Weak},
    time::Duration,
};

use futures::{
    FutureExt, SinkExt, StreamExt,
    channel::{mpsc, oneshot},
};
use futures_signals::signal::{Mutable, SignalExt};
use wasm_bindgen_futures::spawn_local;

#[cfg(feature = "browser-acceptance-faults")]
use crate::room::session::{RoomSessionRenewalTraceStage, is_session_offer, session_offer_target};

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
    room::performance_feedback::{
        PerformanceFeedbackEvent, PerformanceFeedbackResolution, PerformanceIntentToken,
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

#[cfg(feature = "browser-acceptance-faults")]
use crate::room::worker::SessionDrainTestCut;

#[cfg(feature = "browser-acceptance-faults")]
use crate::room::worker::{RoomWorkerProjection, encode_projection_witness};

#[cfg(feature = "browser-acceptance-faults")]
thread_local! {
    static SESSION_DRAIN_CUT_CONSUMED: Cell<bool> = const { Cell::new(false) };
}

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

#[cfg(feature = "browser-acceptance-faults")]
fn log_opened_authoritative_worker_state(
    generation: u64,
    projection: &RoomWorkerProjection,
) -> Result<(), String> {
    if !browser_query_flag("acceptanceWorkerStateTrace") {
        return Ok(());
    }
    let projection = encode_projection_witness(projection)?;
    let encoded = format!("{{\"generation\":{generation},\"projection\":{projection}}}");
    let storage = web_sys::window()
        .and_then(|window| window.local_storage().ok())
        .flatten()
        .ok_or("acceptance page has no localStorage for the checked Open projection")?;
    storage
        .set_item("walkie-acceptance-authoritative-worker-state", &encoded)
        .map_err(|error| {
            format!("could not mirror the checked Open projection to localStorage: {error:?}")
        })?;
    web_sys::console::info_1(
        &format!("[replica_worker_state] generation={generation} projection={projection}").into(),
    );
    Ok(())
}

fn session_tracing_enabled() -> bool {
    browser_query_flag("sessionTrace")
}

#[cfg(feature = "browser-acceptance-faults")]
fn session_renewal_test_cut_enabled() -> bool {
    session_tracing_enabled() && browser_query_flag("renewalCut")
}

#[cfg(feature = "browser-acceptance-faults")]
fn session_renewal_stale_replay_enabled() -> bool {
    session_tracing_enabled() && browser_query_flag("renewalReplayStale")
}

#[cfg(feature = "browser-acceptance-faults")]
fn take_session_drain_test_cut() -> Option<SessionDrainTestCut> {
    let cut = web_sys::window()
        .and_then(|window| window.location().search().ok())
        .and_then(|search| {
            search
                .trim_start_matches('?')
                .split('&')
                .find_map(|part| match part {
                    "sessionDrainCut=before" => Some(SessionDrainTestCut::BeforeCommit),
                    "sessionDrainCut=after" => Some(SessionDrainTestCut::AfterCommit),
                    _ => None,
                })
        });
    cut.filter(|_| SESSION_DRAIN_CUT_CONSUMED.with(|consumed| !consumed.replace(true)))
}

#[cfg(feature = "browser-acceptance-faults")]
const STALE_RENEWAL_OFFER_KEY: &str = "walkie-acceptance-stale-renewal-offer";

#[cfg(feature = "browser-acceptance-faults")]
const STALE_RENEWAL_OFFER_DIGEST_KEY: &str = "walkie-acceptance-stale-renewal-offer-digest";

#[cfg(feature = "browser-acceptance-faults")]
const STALE_RENEWAL_REPLAY_ARM_KEY: &str = "walkie-acceptance-stale-renewal-replay-armed";

#[cfg(feature = "browser-acceptance-faults")]
fn session_renewal_stale_replay_armed() -> bool {
    browser_query_flag("renewalReplayArmed")
        || web_sys::window()
            .and_then(|window| window.local_storage().ok())
            .flatten()
            .and_then(|storage| storage.get_item(STALE_RENEWAL_REPLAY_ARM_KEY).ok())
            .flatten()
            .as_deref()
            == Some("1")
}

#[cfg(feature = "browser-acceptance-faults")]
fn retain_stale_renewal_offer(bytes: &[u8]) {
    let encoded = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if let Some(storage) = web_sys::window()
        .and_then(|window| window.local_storage().ok())
        .flatten()
        && storage
            .get_item(STALE_RENEWAL_OFFER_KEY)
            .ok()
            .flatten()
            .is_none()
    {
        let _ = storage.set_item(STALE_RENEWAL_OFFER_KEY, &encoded);
        let _ = storage.set_item(
            STALE_RENEWAL_OFFER_DIGEST_KEY,
            &hhhs::Digest::of(bytes).to_hex(),
        );
    }
}

#[cfg(feature = "browser-acceptance-faults")]
fn load_stale_renewal_offer() -> Option<Vec<u8>> {
    let storage = web_sys::window()?.local_storage().ok()??;
    let encoded = storage.get_item(STALE_RENEWAL_OFFER_KEY).ok()??;
    if encoded.len() % 2 != 0 {
        return None;
    }
    (0..encoded.len())
        .step_by(2)
        .map(|offset| u8::from_str_radix(&encoded[offset..offset + 2], 16).ok())
        .collect()
}

#[cfg(feature = "browser-acceptance-faults")]
fn clear_stale_renewal_offer() {
    if let Some(storage) = web_sys::window()
        .and_then(|window| window.local_storage().ok())
        .flatten()
    {
        let _ = storage.remove_item(STALE_RENEWAL_OFFER_KEY);
        let _ = storage.remove_item(STALE_RENEWAL_OFFER_DIGEST_KEY);
    }
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
        intent_token: Option<PerformanceIntentToken>,
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
    scope: OwnedRoomGenerationScope,
    worker: BrowserReplicaHandle,
    generation: u64,
    operation: u64,
    restart: RoomRestartSpec,
    failed: Rc<Cell<bool>>,
    stopped: oneshot::Receiver<()>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RoomGenerationExitCause {
    Completed,
    Superseded,
    Failed(String),
    Refused,
    ParentClosed,
}

#[derive(Clone)]
struct RoomGenerationToken {
    generation: u64,
    alive: Rc<Cell<bool>>,
    cancelled: Mutable<bool>,
    exit: Rc<RefCell<Option<RoomGenerationExitCause>>>,
}

impl RoomGenerationToken {
    fn new(generation: u64) -> Self {
        Self {
            generation,
            alive: Rc::new(Cell::new(true)),
            cancelled: Mutable::new(false),
            exit: Rc::new(RefCell::new(None)),
        }
    }

    fn generation(&self) -> u64 {
        self.generation
    }

    fn is_alive(&self) -> bool {
        self.alive.get()
    }

    fn close(&self, cause: RoomGenerationExitCause) {
        if self.exit.borrow().is_some() {
            return;
        }
        *self.exit.borrow_mut() = Some(cause);
        self.alive.set(false);
        self.cancelled.set(true);
    }

    fn exit_cause(&self) -> Option<RoomGenerationExitCause> {
        self.exit.borrow().clone()
    }

    async fn cancelled(&self) {
        self.cancelled.signal().wait_for(true).await;
    }
}

struct RoomGenerationScopeState {
    token: RoomGenerationToken,
    tasks: RefCell<Option<n0_future::task::JoinSet<()>>>,
}

/// Weak capability for adding a child task to one room generation.
///
/// Child tasks never retain the owning scope. Dropping the sole owner therefore
/// drops the `JoinSet`, whose n0-future implementation aborts every remaining
/// browser task.
#[derive(Clone)]
struct RoomGenerationSpawner(Weak<RoomGenerationScopeState>);

impl RoomGenerationSpawner {
    fn spawn(
        &self,
        future: impl Future<Output = ()> + 'static,
    ) -> Result<(), RoomGenerationExitCause> {
        let Some(state) = self.0.upgrade() else {
            return Err(RoomGenerationExitCause::ParentClosed);
        };
        if !state.token.is_alive() {
            return Err(state
                .token
                .exit_cause()
                .unwrap_or(RoomGenerationExitCause::Refused));
        }
        let mut tasks = state.tasks.borrow_mut();
        let Some(tasks) = tasks.as_mut() else {
            return Err(RoomGenerationExitCause::Refused);
        };
        tasks.spawn_local(future);
        Ok(())
    }
}

/// Sole owner of all asynchronous work belonging to one worker generation.
///
/// `close` first publishes the typed exit cause and wakes cooperative children.
/// `graceful_shutdown` then joins those children. If the owner is dropped before
/// that path completes, n0-future's abort-on-drop `JoinSet` is the fail-closed
/// resource fence.
struct OwnedRoomGenerationScope(Rc<RoomGenerationScopeState>);

impl OwnedRoomGenerationScope {
    fn new(generation: u64) -> Self {
        Self(Rc::new(RoomGenerationScopeState {
            token: RoomGenerationToken::new(generation),
            tasks: RefCell::new(Some(n0_future::task::JoinSet::new())),
        }))
    }

    fn token(&self) -> RoomGenerationToken {
        self.0.token.clone()
    }

    fn spawner(&self) -> RoomGenerationSpawner {
        RoomGenerationSpawner(Rc::downgrade(&self.0))
    }

    fn close(&self, cause: RoomGenerationExitCause) {
        self.0.token.close(cause);
    }

    async fn graceful_shutdown(self, cause: RoomGenerationExitCause) {
        self.close(cause);
        let tasks = self.0.tasks.borrow_mut().take();
        if let Some(mut tasks) = tasks {
            while tasks.join_next().await.is_some() {}
        }
    }
}

impl Drop for OwnedRoomGenerationScope {
    fn drop(&mut self) {
        self.close(RoomGenerationExitCause::ParentClosed);
    }
}

async fn until_generation_cancelled<F: Future>(
    lifetime: &RoomGenerationToken,
    future: F,
) -> Option<F::Output> {
    let future = future.fuse();
    let cancelled = lifetime.cancelled().fuse();
    futures::pin_mut!(future, cancelled);
    match futures::future::select(future, cancelled).await {
        futures::future::Either::Left((output, _)) => Some(output),
        futures::future::Either::Right(((), _)) => None,
    }
}

#[derive(Clone)]
struct RoomRestartSpec {
    room_name: Option<String>,
    config: ReplicaRoomNetworkConfig,
    bootstrap_source: DiscoverySource,
    room_authority: Option<[u8; 32]>,
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
    performance_feedback: Rc<dyn Fn(PerformanceFeedbackEvent) -> Result<bool, String>>,
    performance_generation: u64,
    performance_sequence: u64,
    room_operation: u64,
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
    intent_token: Option<PerformanceIntentToken>,
    on_error: Box<dyn FnOnce(String)>,
}

const WINDOW_COMMAND_CAPACITY: usize = 64;

thread_local! {
    static COMMANDS: RefCell<Option<mpsc::Sender<QueuedCommand>>> = const { RefCell::new(None) };
    static HOST: RefCell<Option<Rc<BrowserHost>>> = const { RefCell::new(None) };
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

pub async fn init(
    on_event: impl Fn(AppEventEnvelope) + 'static,
    on_performance_feedback: impl Fn(PerformanceFeedbackEvent) -> Result<bool, String> + 'static,
) -> Result<(), String> {
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
            performance_feedback: Rc::new(on_performance_feedback),
            performance_generation: 0,
            performance_sequence: 0,
            room_operation: 0,
            active_room: None,
        })),
        identity,
    });
    host.register(Rc::new(on_event));
    let (commands, mut command_rx) = mpsc::channel::<QueuedCommand>(WINDOW_COMMAND_CAPACITY);
    COMMANDS.with(|slot| *slot.borrow_mut() = Some(commands));
    HOST.with(|slot| *slot.borrow_mut() = Some(host.clone()));
    spawn_local(async move {
        while let Some(queued) = command_rx.next().await {
            if let Err(error) = host.dispatch(queued.command, queued.intent_token).await {
                if let Some(intent_token) = queued.intent_token {
                    if error.code == AppErrorCode::Internal {
                        host.reset_performance_feedback(intent_token.generation);
                    } else {
                        host.resolve_performance_intent(
                            intent_token,
                            PerformanceFeedbackResolution::Rejected,
                        );
                    }
                }
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
    let intent_token = match performance_target(&command) {
        Some((target, desired_active)) => {
            match HOST.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .ok_or("browser networking is not initialized".to_owned())?
                    .begin_performance_intent(target, desired_active)
            }) {
                Ok(token) => Some(token),
                Err(error) => {
                    on_error(error);
                    return;
                }
            }
        }
        None => None,
    };
    let queued = QueuedCommand {
        command,
        intent_token,
        on_error: Box::new(on_error),
    };
    let result = COMMANDS.with(|slot| match slot.borrow_mut().as_mut() {
        Some(commands) => commands
            .try_send(queued)
            .map_err(|error| (error.is_full(), error.into_inner())),
        None => Err((false, queued)),
    });
    match result {
        Ok(()) => {
            if let Some(intent_token) = intent_token
                && let Err(error) = HOST.with(|slot| {
                    slot.borrow()
                        .as_ref()
                        .ok_or("browser networking is not initialized".to_owned())?
                        .commit_performance_intent(intent_token)
                })
            {
                HOST.with(|slot| {
                    if let Some(host) = slot.borrow().as_ref() {
                        host.reset_performance_feedback(intent_token.generation);
                        host.emit_diagnostic("performance_feedback_commit", &error);
                    }
                });
            }
        }
        Err((full, queued)) => {
            if let Some(intent_token) = queued.intent_token {
                HOST.with(|slot| {
                    if let Some(host) = slot.borrow().as_ref() {
                        host.rollback_performance_intent(intent_token);
                    }
                });
            }
            (queued.on_error)(if full {
                "browser command queue is full".to_owned()
            } else {
                "browser networking is not initialized or has closed".to_owned()
            });
        }
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

    fn begin_performance_intent(
        &self,
        target: TunedDegree,
        desired_active: bool,
    ) -> Result<PerformanceIntentToken, String> {
        let (token, feedback) = {
            let mut state = self.state.borrow_mut();
            if state.active_room.is_none() || state.performance_generation == 0 {
                return Err("enter a room before changing shared state".into());
            }
            let sequence = state
                .performance_sequence
                .checked_add(1)
                .ok_or("performance intent sequence exhausted")?;
            state.performance_sequence = sequence;
            (
                PerformanceIntentToken {
                    generation: state.performance_generation,
                    sequence,
                },
                Rc::clone(&state.performance_feedback),
            )
        };
        feedback(PerformanceFeedbackEvent::Begin {
            token,
            target,
            desired_active,
        })?;
        Ok(token)
    }

    fn resolve_performance_intent(
        &self,
        token: PerformanceIntentToken,
        resolution: PerformanceFeedbackResolution,
    ) {
        let feedback = Rc::clone(&self.state.borrow().performance_feedback);
        if let Err(error) = feedback(PerformanceFeedbackEvent::Resolved { token, resolution }) {
            self.emit_diagnostic("performance_feedback", &error);
        }
    }

    fn commit_performance_intent(&self, token: PerformanceIntentToken) -> Result<(), String> {
        let feedback = Rc::clone(&self.state.borrow().performance_feedback);
        feedback(PerformanceFeedbackEvent::CommitBegin { token }).map(|_| ())
    }

    fn rollback_performance_intent(&self, token: PerformanceIntentToken) {
        let feedback = Rc::clone(&self.state.borrow().performance_feedback);
        if let Err(error) = feedback(PerformanceFeedbackEvent::RollbackBegin { token }) {
            self.emit_diagnostic("performance_feedback_rollback", &error);
        }
    }

    fn reset_performance_feedback(&self, generation: u64) {
        let feedback = Rc::clone(&self.state.borrow().performance_feedback);
        match feedback(PerformanceFeedbackEvent::Reset { generation }) {
            Ok(_) =>
            {
                #[cfg(feature = "browser-acceptance-faults")]
                if browser_query_flag("acceptanceWorkerStateTrace") {
                    self.emit_diagnostic(
                        "performance_feedback_reset_applied",
                        &format!("generation {generation}"),
                    );
                }
            }
            Err(error) => self.emit_diagnostic("performance_feedback_reset", &error),
        }
    }

    fn invalidate_performance_generation(&self, generation: u64) {
        let feedback = {
            let mut state = self.state.borrow_mut();
            if state.performance_generation != generation {
                return;
            }
            state.performance_generation = 0;
            Rc::clone(&state.performance_feedback)
        };
        if let Err(error) = feedback(PerformanceFeedbackEvent::Reset { generation }) {
            self.emit_diagnostic("performance_feedback_worker_loss", &error);
        }
    }

    fn install_performance_generation(&self, generation: u64) -> Result<(), AppError> {
        let (feedback, event) = {
            let mut state = self.state.borrow_mut();
            let event = if state.performance_generation == generation {
                PerformanceFeedbackEvent::Reset { generation }
            } else if state.performance_generation < generation {
                state.performance_generation = generation;
                state.performance_sequence = 0;
                PerformanceFeedbackEvent::InstallGeneration { generation }
            } else {
                return Err(AppError::new(
                    AppErrorCode::Internal,
                    "replacement worker generation regressed",
                ));
            };
            (Rc::clone(&state.performance_feedback), event)
        };
        feedback(event).map_err(|error| {
            AppError::new(
                AppErrorCode::Internal,
                "could not install worker feedback generation",
            )
            .with_detail(error)
        })?;
        Ok(())
    }

    async fn dispatch(
        self: &Rc<Self>,
        command: ClientCommand,
        intent_token: Option<PerformanceIntentToken>,
    ) -> Result<CommandAck, AppError> {
        match command {
            ClientCommand::EnterRoom { room_name } => self.enter_room(room_name).await,
            ClientCommand::JoinTicket { ticket } => self.join_ticket(ticket).await,
            ClientCommand::LeaveRoom => self.leave_room().await,
            ClientCommand::SetTuning { definition } => {
                definition.validate("room tuning").map_err(invalid_tuning)?;
                self.submit(MusicOp::SetTuning { definition }.into(), None)
                    .await
            }
            ClientCommand::AddDegree { pitch } => {
                self.validate_degree(pitch)?;
                self.submit(
                    MusicOp::AddDegree { degree: pitch }.into(),
                    Some(require_intent_token(intent_token)?),
                )
                .await
            }
            ClientCommand::RemoveDegree { pitch } => {
                self.validate_degree(pitch)?;
                self.submit(
                    MusicOp::RemoveDegree { degree: pitch }.into(),
                    Some(require_intent_token(intent_token)?),
                )
                .await
            }
            ClientCommand::SetRoundTable { config } => {
                let config = config.validate().map_err(|error| {
                    AppError::new(AppErrorCode::InvalidCommand, "invalid round-table config")
                        .with_detail(error.to_string())
                })?;
                self.submit(MusicOp::SetRoundTable { config }.into(), None)
                    .await
            }
            ClientCommand::AddPitch { pitch } => {
                self.validate_pitch(pitch)?;
                self.submit(
                    MusicOp::AddPitch { pitch }.into(),
                    Some(require_intent_token(intent_token)?),
                )
                .await
            }
            ClientCommand::RemovePitch { pitch } => {
                self.validate_pitch(pitch)?;
                self.submit(
                    MusicOp::RemovePitch { pitch }.into(),
                    Some(require_intent_token(intent_token)?),
                )
                .await
            }
            ClientCommand::PutPiece { emoji, pitch } => {
                self.validate_pitch(pitch)?;
                self.submit(ExtensionCommand::PutPiece { emoji, pitch }.into(), None)
                    .await
            }
            ClientCommand::MovePiece { piece, pitch } => {
                self.validate_pitch(pitch)?;
                self.submit(ExtensionCommand::MovePiece { piece, pitch }.into(), None)
                    .await
            }
            ClientCommand::RemovePiece { piece } => {
                self.submit(ExtensionCommand::RemovePiece { piece }.into(), None)
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
                    None,
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
        let operation = {
            let mut state = self.state.borrow_mut();
            state.room_operation = state.room_operation.saturating_add(1).max(1);
            state.room_operation
        };
        self.stop_active_room(RoomGenerationExitCause::Superseded)
            .await;
        self.launch_room(
            RoomRestartSpec {
                room_name,
                config,
                bootstrap_source,
                room_authority: room_authority.map(|authority| authority.to_bytes()),
            },
            operation,
        )
        .await
    }

    async fn launch_room(
        self: &Rc<Self>,
        restart: RoomRestartSpec,
        operation: u64,
    ) -> Result<CommandAck, AppError> {
        let room_name = restart.room_name.clone();
        let config = restart.config.clone();
        let bootstrap_source = restart.bootstrap_source;
        let room_authority = restart
            .room_authority
            .map(|bytes| hhhs_proof::SigningKey::from_bytes(&bytes));
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
        let worker_open = worker_open
            .with_session_renewal_test_cut(session_renewal_test_cut_enabled())
            .with_session_drain_test_cut(take_session_drain_test_cut());
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
                    ) =>
            {
                #[cfg(feature = "browser-acceptance-faults")]
                if let Err(error) =
                    log_opened_authoritative_worker_state(worker.generation(), &projection)
                {
                    self.emit_diagnostic("acceptance_authoritative_worker_state", &error);
                }
            }
            RoomWorkerResponse::Opened { .. } => {
                worker.terminate();
                return Err(AppError::new(
                    AppErrorCode::Internal,
                    "Replica worker opened with the wrong local actor",
                ));
            }
            _ => unreachable!("BrowserReplicaHandle validates its Open response"),
        }
        self.install_performance_generation(worker.generation())?;

        let room_identity = config.room.clone();
        let topic = config.topic();
        let topic_string = topic.to_string();
        let bootstrap = config.bootstrap.as_ref().map(|address| address.id);
        let bootstrap_support = config.bootstrap_support;
        let network = match BrowserRoomNetwork::bind(self.identity.iroh_secret(), config).await {
            Ok(network) => network,
            Err(error) => {
                worker.terminate();
                return Err(network_error(error));
            }
        };
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

        let scope = OwnedRoomGenerationScope::new(worker.generation());
        let lifetime = scope.token();
        let spawner = scope.spawner();
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
        if self.state.borrow().room_operation != operation {
            worker.terminate();
            let _ = network.shutdown().await;
            return Err(shutting_down());
        }
        let failed = Rc::new(Cell::new(false));
        let (stopped_tx, stopped) = oneshot::channel();
        let task_lifetime = lifetime.clone();
        let task_failed = failed.clone();
        {
            let mut state = self.state.borrow_mut();
            // The operation was checked immediately above and there is no
            // await between that check and this install.  Install ownership
            // before spawning any task so even future scheduling changes
            // cannot create an unowned worker/network/Web-Lock generation.
            debug_assert_eq!(state.room_operation, operation);
            state.active_room = Some(ActiveRoom {
                control,
                scope,
                worker: worker.clone(),
                generation: worker.generation(),
                operation,
                restart,
                failed,
                stopped,
            });
            state.snapshot.room_name = room_name.clone();
            state.snapshot.room_topic = Some(topic_string.clone());
            state.snapshot.room_ticket = Some(ticket_string.clone());
            state.snapshot.peers.clear();
            state.peer_sync.clear();
            state.snapshot.voices.clear();
        }
        spawn_room_loop(
            spawner.clone(),
            self.clone(),
            worker.clone(),
            network,
            handle.clone(),
            control_rx,
            rendezvous_rx,
            rendezvous_guard,
            peers.clone(),
            task_lifetime.clone(),
            room_identity,
            repairs.clone(),
            session_gate.clone(),
            session_reset_outstanding.clone(),
            task_failed,
            stopped_tx,
        );
        spawn_session_loop(
            spawner.clone(),
            self.clone(),
            worker.clone(),
            handle.clone(),
            task_lifetime.clone(),
            session_gate,
            session_reset_outstanding,
        );
        spawn_periodic_repair(
            spawner.clone(),
            self.clone(),
            worker.clone(),
            handle,
            peers,
            task_lifetime.clone(),
        );
        spawn_worker_failure_loop(spawner, self.clone(), worker.clone(), task_lifetime);
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
        {
            let mut state = self.state.borrow_mut();
            state.room_operation = state.room_operation.saturating_add(1).max(1);
        }
        self.stop_active_room(RoomGenerationExitCause::Completed)
            .await;
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

    async fn stop_active_room(&self, cause: RoomGenerationExitCause) {
        let (active, generation) = {
            let mut state = self.state.borrow_mut();
            (state.active_room.take(), state.performance_generation)
        };
        if generation != 0 {
            self.reset_performance_feedback(generation);
        }
        if let Some(mut active) = active {
            active.scope.close(cause.clone());
            let (response, closed) = oneshot::channel();
            let _ = active.control.try_send(RoomControl::Shutdown { response });
            drop(closed);
            let _ = active.stopped.await;
            active.scope.graceful_shutdown(cause).await;
        }
    }

    fn fail_active_room_generation(self: &Rc<Self>, generation: u64, reason: String) {
        let mut active = {
            let mut state = self.state.borrow_mut();
            if state.active_room.as_ref().map(|active| active.generation) != Some(generation) {
                return;
            }
            state
                .active_room
                .take()
                .expect("matching active room generation was checked")
        };

        self.emit_diagnostic(
            "replica_worker_generation_failed",
            &format!("generation {generation}: {reason}"),
        );
        self.reset_performance_feedback(generation);
        active.failed.set(true);
        active
            .scope
            .close(RoomGenerationExitCause::Failed(reason.clone()));
        active.worker.fail_and_terminate(reason.clone());
        self.emit_diagnostic(
            "replica_worker_generation_terminal",
            &format!(
                "generation {generation} lifecycle={:?}",
                active.worker.lifecycle().get_cloned()
            ),
        );

        // Wake the room loop even when it is otherwise idle.  If this bounded
        // lane is full, queued work already wakes it; `alive = false` prevents
        // another iteration.  The worker was synchronously terminated above,
        // so an in-flight request settles as cancelled or as an already
        // committed IndexedDB transaction before the checked reopen.
        let (response, closed) = oneshot::channel();
        let _ = active.control.try_send(RoomControl::Shutdown { response });
        drop(closed);

        let operation = active.operation;
        let restart = active.restart;
        let stopped = active.stopped;
        let scope = active.scope;
        let host = self.clone();
        spawn_local(async move {
            let _ = stopped.await;
            scope
                .graceful_shutdown(RoomGenerationExitCause::Failed(reason))
                .await;
            // `terminate()` releases the worker-owned Web Lock asynchronously.
            // HHHS itself refuses a second writer immediately, so retry that
            // fail-closed acquisition on a bounded, operation-fenced schedule.
            // A user leave/join increments `room_operation` and cancels every
            // stale retry before it can install an old room.
            const REOPEN_BACKOFF_MS: [u32; 6] = [0, 25, 100, 300, 900, 2_000];
            for (attempt, delay_ms) in REOPEN_BACKOFF_MS.into_iter().enumerate() {
                if delay_ms != 0 {
                    gloo_timers::future::TimeoutFuture::new(delay_ms).await;
                }
                let should_restart = {
                    let state = host.state.borrow();
                    state.room_operation == operation && state.active_room.is_none()
                };
                if !should_restart {
                    return;
                }
                match host.launch_room(restart.clone(), operation).await {
                    Ok(_) => {
                        host.emit_diagnostic(
                            "replica_worker_generation_recovered",
                            "reopened the sealed canonical placement and established a fresh worker generation",
                        );
                        return;
                    }
                    Err(error) => host.emit_diagnostic(
                        "replica_worker_generation_recovery",
                        &format!(
                            "automatic canonical reopen attempt {}/{} failed: {}",
                            attempt + 1,
                            REOPEN_BACKOFF_MS.len(),
                            error.message
                        ),
                    ),
                }
            }
            host.emit_diagnostic(
                "replica_worker_generation_recovery_exhausted",
                "sealed canonical placement remained unavailable after bounded reopen attempts",
            );
        });
    }

    async fn submit(
        &self,
        command: RoomCommand,
        intent_token: Option<PerformanceIntentToken>,
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
            .send(RoomControl::Commit {
                command,
                intent_token,
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
    spawner: RoomGenerationSpawner,
    host: Rc<BrowserHost>,
    worker: BrowserReplicaHandle,
    mut network: BrowserRoomNetwork,
    handle: BrowserNetHandle,
    mut control: mpsc::Receiver<RoomControl>,
    mut rendezvous: Option<mpsc::Receiver<(iroh::EndpointId, ProtocolSupport)>>,
    rendezvous_guard: Option<crate::net::RendezvousHandle>,
    peers: Rc<RefCell<BTreeMap<iroh::EndpointId, (DiscoverySource, PeerPath, ProtocolSupport)>>>,
    lifetime: RoomGenerationToken,
    room_identity: crate::room::v5::RoomIdentity,
    repairs: RepairCoordinator,
    _session_gate: Rc<RefCell<RoomSessionProjectionGate>>,
    session_reset_outstanding: Rc<Cell<bool>>,
    failed: Rc<Cell<bool>>,
    stopped: oneshot::Sender<()>,
) {
    let local_actor = host.identity.capability_actor_id();
    let generation = lifetime.generation();
    let failure_host = host.clone();
    let task_spawner = spawner.clone();
    if spawner
        .spawn(async move {
        let _rendezvous_guard = rendezvous_guard;
        let mut presence_session = 0_u64;
        let mut presence_sequence = 0_u64;
        let mut realtime_replay = BTreeMap::<(crate::net::PeerId, u64), u64>::new();
        let mut shutdown_response = None;
        while lifetime.is_alive() {
            let control_next = control.next();
            let inbound_next = network.next_inbound();
            let rendezvous_next = async {
                match rendezvous.as_mut() {
                    Some(rx) => rx.next().await,
                    None => std::future::pending().await,
                }
            };
            futures::pin_mut!(control_next, inbound_next, rendezvous_next);
            let next = futures::future::select(
                futures::future::select(control_next, inbound_next),
                rendezvous_next,
            )
            .fuse();
            let cancelled = lifetime.cancelled().fuse();
            futures::pin_mut!(next, cancelled);
            let selected = match futures::future::select(next, cancelled).await {
                futures::future::Either::Left((selected, _)) => selected,
                futures::future::Either::Right(((), _)) => break,
            };
            match selected {
                futures::future::Either::Left((futures::future::Either::Left((control, _)), _)) => {
                    match control {
                        Some(RoomControl::Commit {
                            command,
                            intent_token,
                            response,
                        }) => {
                            let result = match command {
                                RoomCommand::Music(command) if is_session_pitch_edit(&command) => {
                                    let intent_token = require_intent_token(intent_token);
                                    match intent_token {
                                        Ok(intent_token) => worker
                                            .send_session(RoomSessionIngress::LocalPitchEdit {
                                                command,
                                                intent_token,
                                                trace_token: None,
                                            })
                                            .await
                                            .map(|()| host.sequence())
                                            .map_err(|error| {
                                                AppError::new(
                                                    AppErrorCode::Internal,
                                                    "compact session intent outcome is ambiguous",
                                                )
                                                .with_detail(error)
                                            }),
                                        Err(error) => Err(error),
                                    }
                                }
                                command => {
                                    if intent_token.is_some() {
                                        Err(AppError::new(
                                            AppErrorCode::Internal,
                                            "non-pitch command carried a performance intent token",
                                        ))
                                    } else {
                                        worker
                                            .commit(command)
                                            .await
                                            .map(|receipt| {
                                                let _ =
                                                    (receipt.entry, receipt.projection_revision);
                                                host.sequence()
                                            })
                                            .map_err(persistence_error)
                                    }
                                }
                            };
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
                                task_spawner.clone(),
                                host.clone(),
                                worker.clone(),
                                lifetime.clone(),
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
                                                task_spawner.clone(),
                                                host.clone(),
                                                worker.clone(),
                                                handle.clone(),
                                                lifetime.clone(),
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
                            NativeNetworkEvent::DirectReady { endpoint_id } => {
                                let (discovery, support) = peers
                                    .borrow()
                                    .get(&endpoint_id)
                                    .map(|(discovery, _, support)| (*discovery, *support))
                                    .unwrap_or((
                                        DiscoverySource::AddressLookup,
                                        ProtocolSupport::WALKIE,
                                    ));
                                peers
                                    .borrow_mut()
                                    .insert(endpoint_id, (discovery, PeerPath::Direct, support));
                                host.update_peer(endpoint_id, discovery, PeerPath::Direct, support);
                                let local = crate::net::PeerId(*handle.endpoint_id().as_bytes());
                                let remote = crate::net::PeerId(*endpoint_id.as_bytes());
                                if is_routine_repair_initiator(local, remote) {
                                    for lane in [RoomLane::Music, RoomLane::Extension] {
                                        if support.supports(lane) {
                                            spawn_repair_initiator(
                                                task_spawner.clone(),
                                                host.clone(),
                                                worker.clone(),
                                                handle.clone(),
                                                lifetime.clone(),
                                                endpoint_id,
                                                lane,
                                                repairs.clone(),
                                            );
                                        }
                                    }
                                }
                            }
                            NativeNetworkEvent::Message { bytes, .. } => {
                                if is_session_carrier(&bytes) {
                                    #[cfg(feature = "browser-acceptance-faults")]
                                    if session_renewal_stale_replay_enabled()
                                        && is_session_offer(&bytes)
                                    {
                                        retain_stale_renewal_offer(&bytes);
                                    }
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
                                                task_spawner.clone(),
                                                host.clone(),
                                                worker.clone(),
                                                handle.clone(),
                                                lifetime.clone(),
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
                                        task_spawner.clone(),
                                        host.clone(),
                                        worker.clone(),
                                        handle.clone(),
                                        lifetime.clone(),
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
                                            task_spawner.clone(),
                                            host.clone(),
                                            worker.clone(),
                                            handle.clone(),
                                            lifetime.clone(),
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
                                                task_spawner.clone(),
                                                host.clone(),
                                                worker.clone(),
                                                handle.clone(),
                                                lifetime.clone(),
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
                            NativeNetworkEvent::Closed => {
                                if lifetime.is_alive() {
                                    host.fail_active_room_generation(
                                        worker.generation(),
                                        "browser-native network event stream closed unexpectedly"
                                            .into(),
                                    );
                                }
                                break;
                            }
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
        lifetime.close(RoomGenerationExitCause::ParentClosed);
        let _ = network.shutdown().await;
        if !failed.get()
            && let Err(error) = worker.close().await
        {
            host.emit_diagnostic("replica_worker_close", &error);
        }
        if let Some(response) = shutdown_response {
            let _ = response.send(());
        }
        let _ = stopped.send(());
        })
        .is_err()
    {
        failure_host.fail_active_room_generation(
            generation,
            "room generation scope refused its primary room task".into(),
        );
    }
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

fn performance_target(command: &ClientCommand) -> Option<(TunedDegree, bool)> {
    match command {
        ClientCommand::AddDegree { pitch } => Some((*pitch, true)),
        ClientCommand::RemoveDegree { pitch } => Some((*pitch, false)),
        ClientCommand::AddPitch { pitch } => Some((pitch.degree(), true)),
        ClientCommand::RemovePitch { pitch } => Some((pitch.degree(), false)),
        _ => None,
    }
}

fn require_intent_token(
    intent_token: Option<PerformanceIntentToken>,
) -> Result<PerformanceIntentToken, AppError> {
    intent_token.ok_or_else(|| {
        AppError::new(
            AppErrorCode::Internal,
            "shared pitch edit lost its performance intent token",
        )
    })
}

fn spawn_worker_failure_loop(
    spawner: RoomGenerationSpawner,
    host: Rc<BrowserHost>,
    worker: BrowserReplicaHandle,
    lifetime: RoomGenerationToken,
) {
    let generation = worker.generation();
    let failure_host = host.clone();
    if spawner
        .spawn(async move {
            let Some(reason) = until_generation_cancelled(&lifetime, worker.next_failure()).await
            else {
                return;
            };
            let Some(reason) = reason else {
                return;
            };
            if lifetime.is_alive() {
                host.fail_active_room_generation(worker.generation(), reason);
            }
        })
        .is_err()
    {
        failure_host.fail_active_room_generation(
            generation,
            "room generation scope refused its worker-failure task".into(),
        );
    }
}

fn spawn_session_loop(
    spawner: RoomGenerationSpawner,
    host: Rc<BrowserHost>,
    worker: BrowserReplicaHandle,
    handle: BrowserNetHandle,
    lifetime: RoomGenerationToken,
    gate: Rc<RefCell<RoomSessionProjectionGate>>,
    reset_outstanding: Rc<Cell<bool>>,
) {
    let draining = Rc::new(Cell::new(false));
    #[cfg(feature = "browser-acceptance-faults")]
    let stale_replay_enabled = session_renewal_stale_replay_enabled();
    #[cfg(feature = "browser-acceptance-faults")]
    let retained_offer = Rc::new(RefCell::new(load_stale_renewal_offer()));
    #[cfg(feature = "browser-acceptance-faults")]
    let stale_offer_replayed = Rc::new(Cell::new(false));
    let generation = worker.generation();
    let failure_host = host.clone();
    let task_spawner = spawner.clone();
    if spawner
        .spawn(async move {
        while lifetime.is_alive() {
            let event = {
                let next = worker.next_session_event().fuse();
                let cancelled = lifetime.cancelled().fuse();
                futures::pin_mut!(next, cancelled);
                match futures::future::select(next, cancelled).await {
                    futures::future::Either::Left((event, _)) => event,
                    futures::future::Either::Right(((), _)) => break,
                }
            };
            let Some(event) = event else {
                if lifetime.is_alive() {
                    host.fail_active_room_generation(
                        worker.generation(),
                        "worker session event stream closed before the room generation stopped"
                            .into(),
                    );
                }
                return;
            };
            match event {
                RoomSessionEgress::Carrier(carrier) => {
                    #[cfg(feature = "browser-acceptance-faults")]
                    if stale_replay_enabled
                        && retained_offer.borrow().is_none()
                        && is_session_offer(&carrier)
                    {
                        retain_stale_renewal_offer(&carrier);
                        *retained_offer.borrow_mut() = Some(carrier.clone());
                    }
                    let host = host.clone();
                    let handle = handle.clone();
                    let lifetime = lifetime.clone();
                    let _ = task_spawner.spawn(async move {
                        let Some(result) =
                            until_generation_cancelled(&lifetime, handle.broadcast(carrier)).await
                        else {
                            return;
                        };
                        if let Err(error) = result {
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
                    intent_token,
                    trace,
                }) => {
                    let durable_intent_resolved = matches!(
                        projection.kind,
                        RoomSessionProjectionKind::Confirmed
                            | RoomSessionProjectionKind::Corrected
                            | RoomSessionProjectionKind::Advanced
                    );
                    match gate.borrow_mut().accept(&projection) {
                        Ok(true) => {
                            if let Some(trace) = trace.as_ref() {
                                log_session_trace("projection_gate_accepted", trace);
                            }
                            if projection.kind == RoomSessionProjectionKind::Reset {
                                reset_outstanding.set(false);
                                host.reset_performance_feedback(worker.generation());
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
                                let lifetime = lifetime.clone();
                                let _ = task_spawner.spawn(async move {
                                    let Some(result) = until_generation_cancelled(
                                        &lifetime,
                                        worker.reset_session_projection(),
                                    )
                                    .await
                                    else {
                                        return;
                                    };
                                    match result {
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
                    if durable_intent_resolved && let Some(intent_token) = intent_token {
                        host.resolve_performance_intent(
                            intent_token,
                            PerformanceFeedbackResolution::Accepted,
                        );
                    }

                    if let Some(carrier) = carrier {
                        let host = host.clone();
                        let handle = handle.clone();
                        let trace = trace.clone();
                        let lifetime = lifetime.clone();
                        let _ = task_spawner.spawn(async move {
                            if let Some(trace) = trace.as_ref() {
                                log_session_trace("carrier_broadcast_call_started", trace);
                            }
                            let Some(result) =
                                until_generation_cancelled(&lifetime, handle.broadcast(carrier))
                                    .await
                            else {
                                return;
                            };
                            if let Err(error) = result {
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
                        let lifetime = lifetime.clone();
                        let _ = task_spawner.spawn(async move {
                            loop {
                                let Some(result) =
                                    until_generation_cancelled(&lifetime, worker.drain_session())
                                        .await
                                else {
                                    break;
                                };
                                match result {
                                    Ok(true) => {}
                                    Ok(false) => break,
                                    Err(error) => {
                                        host.emit_diagnostic("session_reification", &error);
                                        host.fail_active_room_generation(
                                            worker.generation(),
                                            format!(
                                                "session reification durability outcome is ambiguous and requires checked placement reopen: {error}"
                                            ),
                                        );
                                        break;
                                    }
                                }
                            }
                            draining.set(false);
                        });
                    }
                }
                RoomSessionEgress::FallbackDurable {
                    command,
                    intent_token,
                } => {
                    let host = host.clone();
                    let worker = worker.clone();
                    let lifetime = lifetime.clone();
                    let _ = task_spawner.spawn(async move {
                        let Some(result) = until_generation_cancelled(
                            &lifetime,
                            worker.commit(RoomCommand::Music(command)),
                        )
                        .await
                        else {
                            return;
                        };
                        match result {
                            Ok(_) => host.resolve_performance_intent(
                                intent_token,
                                PerformanceFeedbackResolution::Accepted,
                            ),
                            Err(error) => {
                                host.emit_diagnostic("session_durable_fallback", &error);
                                host.fail_active_room_generation(
                                    worker.generation(),
                                    format!(
                                        "durable fallback outcome is ambiguous and requires checked placement reopen: {error}"
                                    ),
                                );
                            }
                        }
                    });
                }
                RoomSessionEgress::IntentRejected {
                    intent_token,
                    reason,
                } => {
                    host.resolve_performance_intent(
                        intent_token,
                        PerformanceFeedbackResolution::Rejected,
                    );
                    host.emit_diagnostic("session_intent_rejected", &reason);
                }
                RoomSessionEgress::Diagnostic(message) => {
                    host.emit_diagnostic("hhhs_session", &message);
                }
                RoomSessionEgress::RenewalTrace(trace) => {
                    log_session_renewal_trace(&trace);
                    #[cfg(feature = "browser-acceptance-faults")]
                    if stale_replay_enabled
                        && trace.stage == RoomSessionRenewalTraceStage::SessionInstalled
                    {
                        let armed = session_renewal_stale_replay_armed();
                        let stale_offer = retained_offer
                            .borrow()
                            .clone()
                            .or_else(load_stale_renewal_offer);
                        let target_matches = stale_offer.as_ref().is_some_and(|offer| {
                            session_offer_target(offer) == Some(host.identity.capability_actor_id())
                        });
                        let offer_digest = stale_offer
                            .as_deref()
                            .map(|offer| hhhs::Digest::of(offer).to_hex())
                            .unwrap_or_else(|| "none".into());
                        host.emit_diagnostic(
                            "session_stale_offer_replay_probe",
                            &format!(
                                "generation {} installed_epoch={} armed={armed} retained={} target_matches={target_matches} offer_digest={offer_digest} already_replayed={}",
                                worker.generation(),
                                trace.epoch,
                                stale_offer.is_some(),
                                stale_offer_replayed.get()
                            ),
                        );
                        if armed
                            && !stale_offer_replayed.get()
                            && target_matches
                            && let Some(stale_offer) = stale_offer
                        {
                            stale_offer_replayed.set(true);
                            clear_stale_renewal_offer();
                            let host = host.clone();
                            let worker = worker.clone();
                            let lifetime = lifetime.clone();
                            let _ = task_spawner.spawn(async move {
                                host.emit_diagnostic(
                                    "session_stale_offer_replay",
                                    "delivering the retained signed Offer to the recovered worker",
                                );
                                let Some(result) = until_generation_cancelled(
                                    &lifetime,
                                    worker.send_session(RoomSessionIngress::Carrier {
                                        bytes: stale_offer,
                                        received_at_micros: browser_time_micros(),
                                    }),
                                )
                                .await
                                else {
                                    return;
                                };
                                if let Err(error) = result {
                                    host.emit_diagnostic(
                                        "session_stale_offer_replay",
                                        &format!("acceptance stale-offer delivery failed: {error}"),
                                    );
                                }
                            });
                        }
                    }
                }
            }
        }
        if lifetime.is_alive() {
            host.fail_active_room_generation(
                worker.generation(),
                "worker session event stream closed before the room generation stopped".into(),
            );
        } else {
            host.invalidate_performance_generation(worker.generation());
        }
        })
        .is_err()
    {
        failure_host.fail_active_room_generation(
            generation,
            "room generation scope refused its session task".into(),
        );
    }
}

fn spawn_periodic_repair(
    spawner: RoomGenerationSpawner,
    host: Rc<BrowserHost>,
    worker: BrowserReplicaHandle,
    handle: BrowserNetHandle,
    peers: Rc<RefCell<BTreeMap<iroh::EndpointId, (DiscoverySource, PeerPath, ProtocolSupport)>>>,
    lifetime: RoomGenerationToken,
) {
    let _ = spawner.spawn(async move {
        let local = crate::net::PeerId(*handle.endpoint_id().as_bytes());
        while lifetime.is_alive() {
            let sleep = n0_future::time::sleep(Duration::from_secs(27)).fuse();
            let cancelled = lifetime.cancelled().fuse();
            futures::pin_mut!(sleep, cancelled);
            if matches!(
                futures::future::select(sleep, cancelled).await,
                futures::future::Either::Right(_)
            ) {
                break;
            }
            if !lifetime.is_alive() {
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
    spawner: RoomGenerationSpawner,
    host: Rc<BrowserHost>,
    worker: BrowserReplicaHandle,
    handle: BrowserNetHandle,
    lifetime: RoomGenerationToken,
    peer: iroh::EndpointId,
    lane: RoomLane,
    repairs: RepairCoordinator,
) {
    if !repairs.schedule(peer, lane) {
        return;
    }
    let refused_repairs = repairs.clone();
    if spawner
        .spawn(async move {
        const DIVERGENT_BACKOFF_MS: [u64; 3] = [100, 300, 900];
        loop {
            let mut retry = 0;
            let mut completed = false;
            while lifetime.is_alive() {
                let attempt =
                    run_initiator_repair_attempt(&worker, &handle, &lifetime, peer, lane).fuse();
                let cancelled = lifetime.cancelled().fuse();
                futures::pin_mut!(attempt, cancelled);
                let (session, result) = match futures::future::select(attempt, cancelled).await {
                    futures::future::Either::Left((result, _)) => result,
                    futures::future::Either::Right(((), _)) => {
                        repairs.finish(peer, lane);
                        return;
                    }
                };
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
                    &lifetime,
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
                let sleep = n0_future::time::sleep(Duration::from_millis(delay_ms)).fuse();
                let cancelled = lifetime.cancelled().fuse();
                futures::pin_mut!(sleep, cancelled);
                if matches!(
                    futures::future::select(sleep, cancelled).await,
                    futures::future::Either::Right(_)
                ) {
                    repairs.finish(peer, lane);
                    return;
                }
                retry += 1;
            }

            if !lifetime.is_alive() || !completed {
                repairs.finish(peer, lane);
                break;
            }
            if !repairs.continue_pending(peer, lane) {
                break;
            }
        }
        })
        .is_err()
    {
        refused_repairs.finish(peer, lane);
    }
}

async fn run_initiator_repair_attempt(
    worker: &BrowserReplicaHandle,
    handle: &BrowserNetHandle,
    lifetime: &RoomGenerationToken,
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
    if !lifetime.is_alive() {
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
    spawner: RoomGenerationSpawner,
    host: Rc<BrowserHost>,
    worker: BrowserReplicaHandle,
    lifetime: RoomGenerationToken,
    peer: iroh::EndpointId,
    lane: RoomLane,
    stream: IrohSyncStream,
    repairs: RepairCoordinator,
) {
    if !repairs.begin_responder(peer, lane) {
        let close_host = host.clone();
        let _ = spawner.spawn(async move {
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
    let refused_repairs = repairs.clone();
    if spawner
        .spawn(async move {
            if !lifetime.is_alive() {
                repairs.finish_responder(peer, lane);
                if let Err(error) = stream.close().await {
                    host.emit_diagnostic(
                        "replica_repair_cancel_close",
                        &format!("{lane:?} responder stream with {peer} failed to close: {error}"),
                    );
                }
                return;
            }
            let repair = run_repair(host, worker, &lifetime, peer, lane, stream, false).fuse();
            let cancelled = lifetime.cancelled().fuse();
            futures::pin_mut!(repair, cancelled);
            let _ = futures::future::select(repair, cancelled).await;
            repairs.finish_responder(peer, lane);
        })
        .is_err()
    {
        refused_repairs.finish_responder(peer, lane);
    }
}

fn request_repair_after_live_failure(
    spawner: RoomGenerationSpawner,
    host: Rc<BrowserHost>,
    worker: BrowserReplicaHandle,
    handle: BrowserNetHandle,
    lifetime: RoomGenerationToken,
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
        spawn_repair_initiator(
            spawner,
            host,
            worker,
            handle,
            lifetime,
            source_endpoint,
            lane,
            repairs,
        );
        return;
    }
    let _ = spawner.spawn(async move {
        if !lifetime.is_alive() {
            return;
        }
        let broadcast = handle
            .broadcast(
                ReplicaRepairHint {
                    lane,
                    source: local,
                    entry,
                }
                .encode(),
            )
            .fuse();
        let cancelled = lifetime.cancelled().fuse();
        futures::pin_mut!(broadcast, cancelled);
        let result = match futures::future::select(broadcast, cancelled).await {
            futures::future::Either::Left((result, _)) => result,
            futures::future::Either::Right(((), _)) => return,
        };
        if let Err(error) = result {
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
    lifetime: &RoomGenerationToken,
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
        lifetime,
        peer,
        lane,
        if initiator { "initiator" } else { "responder" },
        session,
        result,
    );
}

fn report_repair(
    host: Rc<BrowserHost>,
    lifetime: &RoomGenerationToken,
    peer: iroh::EndpointId,
    lane: RoomLane,
    role: &'static str,
    session: u64,
    result: Result<(RoomWorkerRepairStatus, RoomWorkerRepairOutcome), String>,
) {
    if !lifetime.is_alive() {
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

#[cfg(test)]
mod generation_scope_tests {
    use super::*;

    #[test]
    fn generation_exit_is_first_writer_wins() {
        let token = RoomGenerationToken::new(17);
        token.close(RoomGenerationExitCause::Superseded);
        token.close(RoomGenerationExitCause::Failed("late failure".into()));

        assert_eq!(token.generation(), 17);
        assert!(!token.is_alive());
        assert_eq!(
            token.exit_cause(),
            Some(RoomGenerationExitCause::Superseded)
        );
    }

    #[test]
    fn closed_generation_refuses_new_children() {
        let scope = OwnedRoomGenerationScope::new(23);
        let spawner = scope.spawner();
        scope.close(RoomGenerationExitCause::Completed);

        assert_eq!(
            spawner.spawn(async {}),
            Err(RoomGenerationExitCause::Completed)
        );
    }

    #[test]
    fn dropping_owner_closes_the_parent_and_refuses_new_children() {
        let scope = OwnedRoomGenerationScope::new(29);
        let token = scope.token();
        let spawner = scope.spawner();
        drop(scope);

        assert_eq!(
            token.exit_cause(),
            Some(RoomGenerationExitCause::ParentClosed)
        );
        assert_eq!(
            spawner.spawn(async {}),
            Err(RoomGenerationExitCause::ParentClosed)
        );
    }
}
