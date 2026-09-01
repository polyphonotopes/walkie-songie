//! Browser placement and typed window-side handle for the Room-v5 service.

use std::{
    cell::{Cell, RefCell},
    pin::Pin,
    rc::Rc,
    task::Poll,
};

use futures::{
    FutureExt, Stream,
    channel::{mpsc, oneshot},
};
use futures_signals::signal::{Mutable, ReadOnlyMutable};
#[cfg(feature = "browser-acceptance-faults")]
use hhhs_web_browser::WorkerGeneration;
use hhhs_web_browser::{
    DedicatedWorkerClient, ProjectionSubscription, ProjectionUpdate, SubscriptionId,
    WorkerClientError, WorkerEvent, WorkerEventKind, WorkerRequestKind, WorkerResetReason,
    serve_dedicated_worker_with_application_frames,
};
use js_sys::Array;
use wasm_bindgen::prelude::*;
use web_sys::{Blob, BlobPropertyBag, Url};

use crate::room::{
    session::{
        ROOM_SESSION_CHANNEL, RoomSessionEgress, RoomSessionIngress, RoomSessionLeaseClock,
        RoomSessionServicePort, RoomSessionTaskInput, RoomSessionTraceClock, RoomSessionTraceToken,
        decode_session_egress, encode_session_ingress, run_room_session_task,
    },
    v5::RoomIdentity,
    worker::{
        RoomDataPlane, RoomPresenceWire, RoomReplicaWorkerService, RoomWorkerCommand,
        RoomWorkerFactory, RoomWorkerOpen, RoomWorkerOpenFuture, RoomWorkerProjection,
        RoomWorkerRepairRequest, RoomWorkerRepairStep, RoomWorkerResponse, decode_projection,
        decode_response, encode_command, encode_open, encode_repair,
    },
};

#[cfg(feature = "browser-acceptance-faults")]
use crate::room::worker::encode_projection_witness;

use super::storage::{IndexedDbReplicaLogV5, IndexedDbSessionRenewalStore};

#[cfg(feature = "browser-acceptance-faults")]
thread_local! {
    static ACCEPTANCE_REALTIME_REJECTION_CONSUMED: Cell<bool> = const { Cell::new(false) };
}

struct BrowserSessionLeaseClock {
    performance: Option<web_sys::Performance>,
}

impl BrowserSessionLeaseClock {
    fn new() -> Self {
        let scope: web_sys::WorkerGlobalScope = js_sys::global().unchecked_into();
        Self {
            performance: scope.performance(),
        }
    }
}

impl RoomSessionLeaseClock for BrowserSessionLeaseClock {
    fn now_ticks(&self) -> Result<u64, String> {
        self.performance
            .as_ref()
            .map(|performance| performance.now().max(0.0) as u64)
            .ok_or_else(|| "worker has no monotonic Performance clock for session leases".into())
    }
}

struct BrowserSessionTraceClock {
    performance: Option<web_sys::Performance>,
}

impl BrowserSessionTraceClock {
    fn new() -> Self {
        let scope: web_sys::WorkerGlobalScope = js_sys::global().unchecked_into();
        Self {
            performance: scope.performance(),
        }
    }
}

impl RoomSessionTraceClock for BrowserSessionTraceClock {
    fn now_micros(&self) -> Option<u64> {
        self.performance.as_ref().and_then(performance_time_micros)
    }
}

fn performance_time_micros(performance: &web_sys::Performance) -> Option<u64> {
    let milliseconds = performance.time_origin() + performance.now();
    milliseconds
        .is_finite()
        .then_some((milliseconds.max(0.0) * 1_000.0).round() as u64)
}

fn window_time_micros() -> Option<u64> {
    web_sys::window()?
        .performance()
        .and_then(|performance| performance_time_micros(&performance))
}

fn session_tracing_enabled() -> bool {
    web_sys::window()
        .and_then(|window| window.location().search().ok())
        .is_some_and(|search| {
            search
                .split('&')
                .any(|part| part.contains("sessionTrace=1"))
        })
}

#[cfg(feature = "browser-acceptance-faults")]
fn reject_realtime_once_enabled() -> bool {
    web_sys::window()
        .and_then(|window| window.location().search().ok())
        .is_some_and(|search| {
            search
                .trim_start_matches('?')
                .split('&')
                .any(|part| part == "sessionRejectRealtimeOnce=1")
        })
}

#[cfg(feature = "browser-acceptance-faults")]
fn take_realtime_rejection() -> bool {
    reject_realtime_once_enabled()
        && ACCEPTANCE_REALTIME_REJECTION_CONSUMED.with(|consumed| !consumed.replace(true))
}

#[cfg(feature = "browser-acceptance-faults")]
fn authoritative_worker_state_trace_enabled() -> bool {
    web_sys::window()
        .and_then(|window| window.location().search().ok())
        .is_some_and(|search| {
            search.trim_start_matches('?').split('&').any(|part| {
                part == "acceptanceWorkerStateTrace=1"
                    || part == "sessionRejectRealtimeOnce=1"
                    || matches!(part, "sessionDrainCut=before" | "sessionDrainCut=after")
            })
        })
}

#[cfg(feature = "browser-acceptance-faults")]
fn log_authoritative_worker_state(generation: WorkerGeneration, projection: &RoomWorkerProjection) {
    if !authoritative_worker_state_trace_enabled() {
        return;
    }
    let projection = match encode_projection_witness(projection) {
        Ok(projection) => projection,
        Err(error) => {
            web_sys::console::warn_1(
                &format!(
                    "[replica_worker_state_error] generation={} {error}",
                    generation.get()
                )
                .into(),
            );
            return;
        }
    };
    web_sys::console::info_1(
        &format!(
            "[replica_worker_state] generation={} projection={projection}",
            generation.get()
        )
        .into(),
    );
}

fn log_session_trace(
    stage: &str,
    trace: &crate::room::session::RoomSessionCompactTrace,
    at_micros: Option<u64>,
) {
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
            at_micros.map_or_else(|| "null".to_owned(), |at| at.to_string())
        )
        .into(),
    );
}

fn log_session_trace_token(stage: &str, token: RoomSessionTraceToken, at_micros: Option<u64>) {
    if !session_tracing_enabled() {
        return;
    }
    let Ok(stage) = serde_json::to_string(stage) else {
        return;
    };
    let Ok(token) = serde_json::to_string(&token) else {
        return;
    };
    web_sys::console::info_1(
        &format!(
            "[session_trace] {{\"stage\":{stage},\"atMicros\":{},\"token\":{token}}}",
            at_micros.map_or_else(|| "null".to_owned(), |at| at.to_string())
        )
        .into(),
    );
}

#[derive(Default)]
pub(super) struct BrowserRoomWorkerFactory;

impl RoomWorkerFactory for BrowserRoomWorkerFactory {
    type Durability = IndexedDbReplicaLogV5;

    fn open<'a>(
        &'a mut self,
        request: RoomWorkerOpen,
    ) -> RoomWorkerOpenFuture<'a, Self::Durability> {
        Box::pin(async move {
            let identity = RoomIdentity::from_object(hhhs::Digest(request.object));
            let music = IndexedDbReplicaLogV5::open(
                &identity,
                request.owner,
                crate::room::v5::RoomLane::Music,
            )
            .await?;
            let extension = IndexedDbReplicaLogV5::open(
                &identity,
                request.owner,
                crate::room::v5::RoomLane::Extension,
            )
            .await?;
            let music_transactions = music.transactions()?;
            let extension_transactions = extension.transactions()?;
            RoomDataPlane::open(
                request,
                music,
                extension,
                music_transactions,
                extension_transactions,
            )
            .await
        })
    }
}

/// Install the Room-v5 service in the current module worker.
///
/// This is exported for the tiny Blob module created by the window-side
/// `ReplicaHandle`; ordinary application code uses the typed handle rather than
/// calling it directly.
#[wasm_bindgen(js_name = startWalkieReplicaWorker)]
pub fn start_walkie_replica_worker() {
    const SESSION_QUEUE_CAPACITY: usize = 64;
    let (task_sender, task_receiver) = mpsc::channel(SESSION_QUEUE_CAPACITY);
    let (reification_sender, reification_receiver) = mpsc::channel(SESSION_QUEUE_CAPACITY);
    wasm_bindgen_futures::spawn_local(run_room_session_task(
        task_receiver,
        reification_sender,
        Rc::new(IndexedDbSessionRenewalStore),
        Rc::new(BrowserSessionLeaseClock::new()),
        Rc::new(BrowserSessionTraceClock::new()),
    ));
    let session = RoomSessionServicePort {
        task: task_sender.clone(),
        reifications: reification_receiver,
    };
    let mut application_sender = task_sender;
    serve_dedicated_worker_with_application_frames(
        RoomReplicaWorkerService::with_session(BrowserRoomWorkerFactory, session),
        hhhs_web_browser::DEFAULT_MAX_PENDING_REQUESTS,
        move |channel, bytes| {
            application_sender
                .try_send(RoomSessionTaskInput::Application { channel, bytes })
                .map_err(|error| error.to_string())
        },
    )
    .expect("Room-v5 dedicated worker service must start in a worker global")
    .detach();
}

struct WindowWorkerState {
    subscription: Option<ProjectionSubscription>,
    projection: Mutable<Option<BrowserProjectionState>>,
    lifecycle: Mutable<BrowserReplicaLifecycle>,
    projection_waiters: Vec<(u64, oneshot::Sender<Result<RoomWorkerProjection, String>>)>,
    next_repair_session: u64,
    on_projection: Rc<dyn Fn(RoomWorkerProjection)>,
    on_outbound_record: Rc<dyn Fn(Vec<u8>)>,
    on_diagnostic: Rc<dyn Fn(String)>,
    on_failure: Rc<dyn Fn(String)>,
}

const REPAIR_FRAME_QUEUE_CAPACITY: usize = 8;
const SESSION_EVENT_QUEUE_CAPACITY: usize = 64;

async fn next_repair_frame(receiver: Rc<RefCell<mpsc::Receiver<Vec<u8>>>>) -> Option<Vec<u8>> {
    futures::future::poll_fn(move |context| {
        let mut receiver = receiver.borrow_mut();
        match Pin::new(&mut *receiver).poll_next(context) {
            Poll::Ready(frame) => Poll::Ready(frame),
            Poll::Pending => Poll::Pending,
        }
    })
    .await
}

/// Observable state of the remote Replica placement.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum BrowserReplicaLifecycle {
    Opening,
    Ready { generation: u64 },
    Closing,
    Closed,
    Failed { message: String },
}

/// Latest exact worker projection and its connection-local continuity cursor.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct BrowserProjectionState {
    pub revision: u64,
    pub kind: BrowserProjectionKind,
    pub value: RoomWorkerProjection,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum BrowserProjectionKind {
    Snapshot,
    Revision,
    Reset(WorkerResetReason),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct BrowserCommitReceipt {
    pub entry: [u8; 32],
    pub projection_revision: u64,
}

impl WindowWorkerState {
    fn receive(&mut self, event: WorkerEvent) {
        // The exact eaaade2 worker enum is exhaustive; the next upstream
        // candidate marks it non-exhaustive. Keep the fail-closed arm now so a
        // future lifecycle state cannot be interpreted as readiness.
        #[allow(unreachable_patterns)]
        match event.kind() {
            WorkerEventKind::Snapshot { .. }
            | WorkerEventKind::Revision { .. }
            | WorkerEventKind::Reset { .. } => {
                let Some(subscription) = self.subscription.as_mut() else {
                    (self.on_diagnostic)(
                        "Replica worker emitted a projection before subscription".into(),
                    );
                    return;
                };
                let update = match subscription.accept(&event) {
                    Ok(update) => update,
                    Err(error) => {
                        self.fail(format!(
                            "Replica worker projection continuity failed: {error}"
                        ));
                        return;
                    }
                };
                match decode_projection(event.payload()) {
                    Ok(projection) => {
                        self.accept_projection(update, projection.clone());
                        (self.on_projection)(projection);
                    }
                    Err(error) => self.fail(error),
                }
            }
            WorkerEventKind::OutboundRecord => {
                (self.on_outbound_record)(event.into_payload());
            }
            WorkerEventKind::RepairFrame { .. } => {
                (self.on_diagnostic)(
                    "Replica worker repair frame bypassed the bounded carrier lane".into(),
                );
            }
            WorkerEventKind::Error | WorkerEventKind::Backpressure | WorkerEventKind::Closed => {
                let message = format!(
                    "Replica worker emitted {:?}: {}",
                    event.kind(),
                    String::from_utf8_lossy(event.payload())
                );
                if matches!(
                    event.kind(),
                    WorkerEventKind::Error | WorkerEventKind::Closed
                ) {
                    self.fail(message);
                } else {
                    (self.on_diagnostic)(message);
                }
            }
            WorkerEventKind::Ready | WorkerEventKind::Response | WorkerEventKind::Pong => {
                (self.on_diagnostic)(format!(
                    "Replica worker emitted unexpected uncorrelated {:?}",
                    event.kind()
                ));
            }
            unknown => {
                self.fail(format!(
                    "Replica worker emitted an unsupported event kind: {unknown:?}"
                ));
            }
        }
    }

    fn accept_initial_snapshot(&mut self, event: &WorkerEvent) -> Result<(), String> {
        let update = self
            .subscription
            .as_mut()
            .ok_or("Replica worker projection subscription is absent")?
            .accept(event)
            .map_err(|error| error.to_string())?;
        let projection = decode_projection(event.payload())?;
        self.accept_projection(update, projection.clone());
        (self.on_projection)(projection);
        Ok(())
    }

    fn accept_projection(&mut self, update: ProjectionUpdate, projection: RoomWorkerProjection) {
        let (revision, kind) = match update {
            ProjectionUpdate::Snapshot { revision } => {
                (revision.get(), BrowserProjectionKind::Snapshot)
            }
            ProjectionUpdate::Revision { revision, .. } => {
                (revision.get(), BrowserProjectionKind::Revision)
            }
            ProjectionUpdate::Reset {
                revision, reason, ..
            } => (revision.get(), BrowserProjectionKind::Reset(reason)),
        };
        self.projection.set(Some(BrowserProjectionState {
            revision,
            kind,
            value: projection.clone(),
        }));
        #[cfg(feature = "browser-acceptance-faults")]
        if let Some(generation) = self
            .subscription
            .as_ref()
            .map(ProjectionSubscription::generation)
        {
            log_authoritative_worker_state(generation, &projection);
        }
        let mut pending = Vec::new();
        for (target, waiter) in self.projection_waiters.drain(..) {
            if target <= revision {
                let _ = waiter.send(Ok(projection.clone()));
            } else {
                pending.push((target, waiter));
            }
        }
        self.projection_waiters = pending;
    }

    fn fail(&mut self, message: String) {
        if matches!(
            self.lifecycle.get_cloned(),
            BrowserReplicaLifecycle::Failed { .. }
                | BrowserReplicaLifecycle::Closing
                | BrowserReplicaLifecycle::Closed
        ) {
            return;
        }
        (self.on_diagnostic)(message.clone());
        self.lifecycle.set(BrowserReplicaLifecycle::Failed {
            message: message.clone(),
        });
        (self.on_failure)(message.clone());
        for (_, waiter) in self.projection_waiters.drain(..) {
            let _ = waiter.send(Err(message.clone()));
        }
    }
}

/// Typed window-side façade over one Room-v5 dedicated worker.
///
/// The window sees domain commands and projections, not request IDs or raw
/// postMessage envelopes. Methods still remain explicitly async/fallible: a
/// remote worker is not a transparent local object, and hiding that fact would
/// erase the ordering, crash, and backpressure semantics applications need.
#[derive(Clone)]
pub(super) struct BrowserReplicaHandle {
    client: DedicatedWorkerClient,
    state: Rc<RefCell<WindowWorkerState>>,
    repair_frames: Rc<RefCell<mpsc::Receiver<Vec<u8>>>>,
    repair_failure: Rc<RefCell<Option<String>>>,
    repair_serial: Rc<futures::lock::Mutex<()>>,
    session_events: Rc<RefCell<mpsc::Receiver<RoomSessionEgress>>>,
    failure_events: Rc<RefCell<mpsc::Receiver<String>>>,
    trace_scope: u64,
    trace_sequence: Rc<Cell<u64>>,
}

impl BrowserReplicaHandle {
    pub(super) fn generation(&self) -> u64 {
        self.client.current_generation().get()
    }

    pub(super) async fn open(
        request: RoomWorkerOpen,
        on_projection: impl Fn(RoomWorkerProjection) + 'static,
        on_outbound_record: impl Fn(Vec<u8>) + 'static,
        on_diagnostic: impl Fn(String) + 'static,
    ) -> Result<(Self, RoomWorkerResponse), String> {
        let trace_scope = if request.session_trace {
            let mut bytes = [0_u8; 8];
            getrandom::fill(&mut bytes)
                .map_err(|error| format!("browser session trace token failed: {error}"))?;
            u64::from_le_bytes(bytes).max(1)
        } else {
            0
        };
        let (failure_sender, failure_receiver) = mpsc::channel(1);
        let failure_sender = Rc::new(RefCell::new(failure_sender));
        let failure_events = Rc::new(RefCell::new(failure_receiver));
        let state = Rc::new(RefCell::new(WindowWorkerState {
            subscription: None,
            projection: Mutable::new(None),
            lifecycle: Mutable::new(BrowserReplicaLifecycle::Opening),
            projection_waiters: Vec::new(),
            next_repair_session: 1,
            on_projection: Rc::new(on_projection),
            on_outbound_record: Rc::new(on_outbound_record),
            on_diagnostic: Rc::new(on_diagnostic),
            on_failure: {
                let failure_sender = Rc::clone(&failure_sender);
                Rc::new(move |message| {
                    let _ = failure_sender.borrow_mut().try_send(message);
                })
            },
        }));
        let state_for_events = Rc::clone(&state);
        let (repair_sender, repair_receiver) = mpsc::channel(REPAIR_FRAME_QUEUE_CAPACITY);
        let repair_sender = Rc::new(RefCell::new(repair_sender));
        let repair_frames = Rc::new(RefCell::new(repair_receiver));
        let repair_failure = Rc::new(RefCell::new(None::<String>));
        let (session_sender, session_receiver) = mpsc::channel(SESSION_EVENT_QUEUE_CAPACITY);
        let session_sender = Rc::new(RefCell::new(session_sender));
        let session_events = Rc::new(RefCell::new(session_receiver));
        let client_for_events = Rc::new(RefCell::new(None::<DedicatedWorkerClient>));
        let script_url = worker_module_url()?;
        let result = DedicatedWorkerClient::open(&script_url, encode_open(&request)?, {
            let repair_sender = Rc::clone(&repair_sender);
            let repair_failure = Rc::clone(&repair_failure);
            let session_sender = Rc::clone(&session_sender);
            let client_for_events = Rc::clone(&client_for_events);
            move |event| {
                if let WorkerEventKind::ApplicationFrame { channel, .. } = event.kind() {
                    let channel = *channel;
                    let Some(delivery) = event.delivery_token() else {
                        state_for_events.borrow_mut().fail(
                            "Replica worker application frame did not carry a generation-bound delivery token"
                                .into(),
                        );
                        return;
                    };
                    let accepted = if channel != ROOM_SESSION_CHANNEL {
                        Err(format!("unsupported worker application channel {}", channel.get()))
                    } else {
                        decode_session_egress(event.payload()).and_then(|event| {
                            #[cfg(feature = "browser-acceptance-faults")]
                            if reject_realtime_once_enabled()
                                && matches!(
                                    &event,
                                    RoomSessionEgress::Realtime(realtime)
                                        if !realtime.durable.is_empty()
                                )
                                && take_realtime_rejection()
                            {
                                return Err(
                                    "acceptance-injected rejection of one combined realtime frame"
                                        .into(),
                                );
                            }
                            let trace = match &event {
                                RoomSessionEgress::Realtime(realtime) => realtime.trace.clone(),
                                _ => None,
                            };
                            session_sender
                                .borrow_mut()
                                .try_send(event)
                                .map(|()| trace)
                                .map_err(|error| error.to_string())
                        })
                    };
                    let Some(client) = client_for_events.borrow().clone() else {
                        state_for_events.borrow_mut().fail(
                            "Replica worker emitted an application frame before its client was ready"
                                .into(),
                        );
                        return;
                    };
                    if let Ok(Some(trace)) = accepted.as_ref() {
                        log_session_trace(
                            "window_queue_accepted",
                            trace,
                            window_time_micros(),
                        );
                    }
                    if let Err(error) = accepted.as_ref() {
                        state_for_events.borrow_mut().fail(format!(
                            "Replica worker application frame was rejected before bounded window acceptance: {error}"
                        ));
                    }
                    wasm_bindgen_futures::spawn_local(async move {
                        match accepted {
                            Ok(trace) => {
                                if client
                                    .acknowledge_application_frame(delivery)
                                    .await
                                    .is_ok()
                                    && let Some(trace) = trace
                                {
                                    log_session_trace(
                                        "sideband_acknowledged",
                                        &trace,
                                        window_time_micros(),
                                    );
                                }
                            }
                            Err(error) => {
                                let _ = client
                                    .reject_application_frame(delivery, error)
                                    .await;
                                // Rejection invalidates the stateful producer's
                                // whole placement generation.  Do not leave the
                                // dedicated worker alive to drain reifications,
                                // durability, repair, or later commands after a
                                // truncated combined frame.
                                client.terminate();
                            }
                        }
                    });
                    return;
                }
                let WorkerEventKind::RepairFrame { .. } = event.kind() else {
                    state_for_events.borrow_mut().receive(event);
                    return;
                };
                let Some(delivery) = event.delivery_token() else {
                    state_for_events.borrow_mut().fail(
                        "Replica worker repair frame did not carry a generation-bound delivery token"
                            .into(),
                    );
                    return;
                };
                let accepted = repair_failure.borrow().clone().map_or_else(
                    || {
                        repair_sender
                            .borrow_mut()
                            .try_send(event.into_payload())
                            .map_err(|error| error.to_string())
                    },
                    Err,
                );
                let Some(client) = client_for_events.borrow().clone() else {
                    state_for_events.borrow_mut().fail(
                        "Replica worker emitted a repair frame before its client was ready".into(),
                    );
                    return;
                };
                wasm_bindgen_futures::spawn_local(async move {
                    match accepted {
                        Ok(()) => {
                            let _ = client.acknowledge_repair_frame(delivery).await;
                        }
                        Err(error) => {
                            let _ = client.reject_repair_frame(delivery, error).await;
                        }
                    }
                });
            }
        })
        .await;
        // The Worker has already resolved and loaded this module URL. Revoking
        // the temporary Blob is cleanup, not part of the Replica handshake;
        // a browser cleanup quirk must not discard an otherwise live handle.
        let _ = Url::revoke_object_url(&script_url);
        let (client, ready) = result.map_err(worker_error)?;
        *client_for_events.borrow_mut() = Some(client.clone());
        let opened = decode_response(&ready)?;
        if !matches!(opened, RoomWorkerResponse::Opened { .. }) {
            client.terminate();
            return Err("Replica worker Open returned an unexpected response".into());
        }

        let subscription_id = SubscriptionId::new(1);
        state.borrow_mut().subscription = Some(ProjectionSubscription::new(
            client.current_generation(),
            subscription_id,
        ));
        let snapshot = client
            .request(WorkerRequestKind::Subscribe(subscription_id), Vec::new())
            .await
            .map_err(worker_error)?;
        state.borrow_mut().accept_initial_snapshot(&snapshot)?;
        state
            .borrow()
            .lifecycle
            .set(BrowserReplicaLifecycle::Ready {
                generation: client.current_generation().get(),
            });
        web_sys::console::info_1(
            &format!(
                "[replica_worker] ready generation {}",
                client.current_generation().get()
            )
            .into(),
        );
        Ok((
            Self {
                client,
                state,
                repair_frames,
                repair_failure,
                repair_serial: Rc::new(futures::lock::Mutex::new(())),
                session_events,
                failure_events,
                trace_scope,
                trace_sequence: Rc::new(Cell::new(0)),
            },
            opened,
        ))
    }

    async fn command(&self, command: RoomWorkerCommand) -> Result<RoomWorkerResponse, String> {
        self.request(WorkerRequestKind::Command, encode_command(&command)?)
            .await
    }

    pub(super) async fn start_session_peer(
        &self,
        peer: crate::room::v5::ActorId,
    ) -> Result<(), String> {
        match self
            .command(RoomWorkerCommand::StartSessionPeer(peer))
            .await?
        {
            RoomWorkerResponse::SessionPeerStarted => Ok(()),
            response => Err(format!("unexpected session-peer response: {response:?}")),
        }
    }

    pub(super) async fn reset_session_projection(&self) -> Result<bool, String> {
        match self
            .command(RoomWorkerCommand::ResetSessionProjection)
            .await?
        {
            RoomWorkerResponse::SessionProjectionReset { emitted } => Ok(emitted),
            response => Err(format!("unexpected session-reset response: {response:?}")),
        }
    }

    pub(super) async fn send_session(&self, mut ingress: RoomSessionIngress) -> Result<(), String> {
        let token = match &mut ingress {
            RoomSessionIngress::LocalPitchEdit { trace_token, .. } if self.trace_scope != 0 => {
                let sequence = self.trace_sequence.get().saturating_add(1);
                self.trace_sequence.set(sequence);
                let token = RoomSessionTraceToken {
                    scope: self.trace_scope,
                    sequence,
                };
                *trace_token = Some(token);
                Some(token)
            }
            _ => None,
        };
        if let Some(token) = token {
            log_session_trace_token("window_to_worker_sent", token, window_time_micros());
        }
        let result = self
            .client
            .send_application_frame(ROOM_SESSION_CHANNEL, encode_session_ingress(&ingress)?)
            .await
            .map_err(worker_error);
        if result.is_ok()
            && let Some(token) = token
        {
            log_session_trace_token("worker_queue_acknowledged", token, window_time_micros());
        }
        result
    }

    pub(super) async fn drain_session(&self) -> Result<bool, String> {
        match self.command(RoomWorkerCommand::DrainSession).await? {
            RoomWorkerResponse::SessionDrained { committed, .. } => Ok(committed),
            response => Err(format!("unexpected session-drain response: {response:?}")),
        }
    }

    pub(super) async fn next_session_event(&self) -> Option<RoomSessionEgress> {
        futures::future::poll_fn(|context| {
            let mut receiver = self.session_events.borrow_mut();
            Pin::new(&mut *receiver).poll_next(context)
        })
        .await
    }

    pub(super) async fn next_failure(&self) -> Option<String> {
        futures::future::poll_fn(|context| {
            let mut receiver = self.failure_events.borrow_mut();
            Pin::new(&mut *receiver).poll_next(context)
        })
        .await
    }

    /// Durably commit one domain command and resolve only after its exact
    /// projection revision has crossed back into the window.
    pub(super) async fn commit(
        &self,
        command: crate::room::v5::RoomCommand,
    ) -> Result<BrowserCommitReceipt, String> {
        let response = self.command(RoomWorkerCommand::Commit(command)).await?;
        let RoomWorkerResponse::CommandCommitted {
            entry,
            projection_revision,
        } = response
        else {
            return Err("Replica worker returned the wrong commit response".into());
        };
        self.wait_for_projection(projection_revision).await?;
        Ok(BrowserCommitReceipt {
            entry,
            projection_revision,
        })
    }

    /// Grant the ordinary participant capabilities for both Room lanes.
    pub(super) async fn grant_peer(
        &self,
        peer: crate::room::v5::ActorId,
    ) -> Result<Vec<(crate::room::v5::RoomLane, [u8; 32])>, String> {
        let response = self.command(RoomWorkerCommand::GrantPeer(peer)).await?;
        let RoomWorkerResponse::PeerGranted {
            entries,
            projection_revision,
        } = response
        else {
            return Err("Replica worker returned the wrong grant response".into());
        };
        self.wait_for_projection(projection_revision).await?;
        Ok(entries)
    }

    pub(super) async fn sign_presence(
        &self,
        session: u64,
        sequence: u64,
        pitch: Option<crate::tuning::TunedPeriodicPitch>,
    ) -> Result<Vec<u8>, String> {
        match self
            .command(RoomWorkerCommand::SignPresence {
                session,
                sequence,
                pitch,
            })
            .await?
        {
            RoomWorkerResponse::PresenceSigned(wire) => Ok(wire),
            _ => Err("Replica worker returned the wrong presence-signing response".into()),
        }
    }

    pub(super) async fn verify_presence(&self, wire: Vec<u8>) -> Result<RoomPresenceWire, String> {
        match self
            .command(RoomWorkerCommand::VerifyPresence(wire))
            .await?
        {
            RoomWorkerResponse::PresenceVerified(presence) => Ok(presence),
            _ => Err("Replica worker returned the wrong presence-verification response".into()),
        }
    }

    /// Admit one public record and resolve after any resulting projection has
    /// crossed back into the window.
    pub(super) async fn inbound_record(&self, record: Vec<u8>) -> Result<bool, String> {
        let response = self
            .request(WorkerRequestKind::InboundRecord, record)
            .await?;
        let RoomWorkerResponse::InboundApplied {
            accepted,
            entry: _,
            projection_revision,
        } = response
        else {
            return Err("Replica worker returned the wrong inbound-record response".into());
        };
        self.wait_for_projection(projection_revision).await?;
        Ok(accepted)
    }

    pub(super) async fn repair(
        &self,
        request: RoomWorkerRepairRequest,
    ) -> Result<RoomWorkerRepairStep, String> {
        match self
            .request(WorkerRequestKind::RepairFrame, encode_repair(&request)?)
            .await?
        {
            RoomWorkerResponse::Repair(step) => Ok(step),
            _ => Err("Replica worker RepairFrame returned an unexpected response".into()),
        }
    }

    pub(super) async fn repair_with_stream<S>(
        &self,
        request: RoomWorkerRepairRequest,
        stream: &mut S,
    ) -> Result<RoomWorkerRepairStep, String>
    where
        S: crate::net::SyncStream,
    {
        let request = self
            .request(WorkerRequestKind::RepairFrame, encode_repair(&request)?)
            .fuse();
        futures::pin_mut!(request);
        let mut delivery_error = None;
        let response = loop {
            let frame = next_repair_frame(Rc::clone(&self.repair_frames)).fuse();
            futures::pin_mut!(frame);
            futures::select_biased! {
                outbound = frame => {
                    let Some(outbound) = outbound else {
                        delivery_error.get_or_insert_with(|| {
                            "Replica worker repair-frame lane closed".to_owned()
                        });
                        continue;
                    };
                    if delivery_error.is_none()
                        && let Err(error) = stream.send_frame(&outbound).await
                    {
                        let error = error.to_string();
                        *self.repair_failure.borrow_mut() = Some(error.clone());
                        delivery_error = Some(error);
                    }
                }
                response = request => break response,
            }
        };
        if let Some(error) = delivery_error {
            let _ = response;
            return Err(error);
        }
        match response? {
            RoomWorkerResponse::Repair(step) => Ok(step),
            _ => Err("Replica worker RepairFrame returned an unexpected response".into()),
        }
    }

    pub(super) async fn repair_permit(&self) -> futures::lock::MutexGuard<'_, ()> {
        let permit = self.repair_serial.lock().await;
        *self.repair_failure.borrow_mut() = None;
        while self.repair_frames.borrow_mut().try_recv().is_ok() {}
        permit
    }

    pub(super) async fn close(&self) -> Result<(), String> {
        self.state
            .borrow()
            .lifecycle
            .set(BrowserReplicaLifecycle::Closing);
        match self.client.close().await.map_err(worker_error) {
            Ok(()) => {
                self.state
                    .borrow()
                    .lifecycle
                    .set(BrowserReplicaLifecycle::Closed);
                Ok(())
            }
            Err(error) => {
                self.state.borrow_mut().fail(error.clone());
                Err(error)
            }
        }
    }

    pub(super) fn terminate(&self) {
        self.client.terminate();
        self.state
            .borrow()
            .lifecycle
            .set(BrowserReplicaLifecycle::Closed);
    }

    pub(super) fn fail_and_terminate(&self, message: String) {
        self.state.borrow_mut().fail(message);
        self.client.terminate();
    }

    pub(super) fn projection(&self) -> Option<RoomWorkerProjection> {
        self.state
            .borrow()
            .projection
            .get_cloned()
            .map(|projection| projection.value)
    }

    /// FRP-facing latest-value handle. Consumers can call `signal_cloned()` to
    /// compose exact snapshot/revision/reset state without callback plumbing.
    pub(super) fn projections(&self) -> ReadOnlyMutable<Option<BrowserProjectionState>> {
        self.state.borrow().projection.read_only()
    }

    pub(super) fn lifecycle(&self) -> ReadOnlyMutable<BrowserReplicaLifecycle> {
        self.state.borrow().lifecycle.read_only()
    }

    pub(super) fn next_repair_session(&self) -> u64 {
        let mut state = self.state.borrow_mut();
        let session = state.next_repair_session;
        state.next_repair_session = state.next_repair_session.checked_add(1).unwrap_or(1);
        session
    }

    async fn request(
        &self,
        kind: WorkerRequestKind,
        payload: Vec<u8>,
    ) -> Result<RoomWorkerResponse, String> {
        let response = match self.client.request(kind, payload).await {
            Ok(response) => response,
            Err(error) => {
                let fatal = matches!(
                    error,
                    WorkerClientError::Protocol(_)
                        | WorkerClientError::Spawn(_)
                        | WorkerClientError::Post(_)
                        | WorkerClientError::Worker(_)
                        | WorkerClientError::Cancelled
                );
                let error = worker_error(error);
                if fatal {
                    self.state.borrow_mut().fail(error.clone());
                }
                return Err(error);
            }
        };
        decode_response(response.payload()).inspect_err(|error| {
            self.state.borrow_mut().fail(error.clone());
        })
    }

    async fn wait_for_projection(&self, revision: u64) -> Result<RoomWorkerProjection, String> {
        let receiver = {
            let mut state = self.state.borrow_mut();
            if let Some(current) = state.projection.get_cloned()
                && current.revision >= revision
            {
                return Ok(current.value);
            }
            let (sender, receiver) = oneshot::channel();
            state.projection_waiters.push((revision, sender));
            receiver
        };
        receiver.await.map_err(|_| {
            "Replica worker closed before publishing its committed revision".to_owned()
        })?
    }
}

fn worker_module_url() -> Result<String, String> {
    let window = web_sys::window().ok_or("Replica worker requires a Window host")?;
    let document = window
        .document()
        .ok_or("Replica worker requires the current document")?;
    let links = document
        .query_selector_all("link[rel='modulepreload']")
        .map_err(js_error)?;
    let mut module = None;
    for index in 0..links.length() {
        let Some(link) = links.item(index) else {
            continue;
        };
        let Some(href) = link
            .dyn_ref::<web_sys::Element>()
            .and_then(|element| element.get_attribute("href"))
        else {
            continue;
        };
        if href.contains("walkie-songie-") && href.ends_with(".js") && !href.contains("/snippets/")
        {
            module = Some(
                Url::new_with_base(&href, &window.location().href().map_err(js_error)?)
                    .map_err(js_error)?
                    .href(),
            );
            break;
        }
    }
    let module = module.ok_or("could not locate the Trunk Wasm module for Replica worker")?;
    let wasm = document
        .query_selector("link[rel='preload'][as='fetch'][type='application/wasm']")
        .map_err(js_error)?
        .and_then(|element| element.get_attribute("href"))
        .ok_or("could not locate the Trunk Wasm binary for Replica worker")?;
    let wasm = Url::new_with_base(&wasm, &window.location().href().map_err(js_error)?)
        .map_err(js_error)?
        .href();
    let module_literal = serde_json::to_string(&module).map_err(|error| error.to_string())?;
    let wasm_literal = serde_json::to_string(&wasm).map_err(|error| error.to_string())?;
    let source = format!(
        "try {{\n\
           const bindings = await import({module_literal});\n\
           await bindings.default({{ module_or_path: {wasm_literal} }});\n\
           bindings.startWalkieReplicaWorker();\n\
         }} catch (error) {{\n\
           console.error('[replica_worker] bootstrap failed', error?.stack ?? error);\n\
           throw error;\n\
         }}\n"
    );
    let parts = Array::new();
    parts.push(&JsValue::from_str(&source));
    let options = BlobPropertyBag::new();
    options.set_type("text/javascript");
    let blob =
        Blob::new_with_str_sequence_and_options(parts.as_ref(), &options).map_err(js_error)?;
    Url::create_object_url_with_blob(&blob).map_err(js_error)
}

fn worker_error(error: WorkerClientError) -> String {
    error.to_string()
}

fn js_error(error: JsValue) -> String {
    error.as_string().unwrap_or_else(|| format!("{error:?}"))
}
