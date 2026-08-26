//! Browser placement and typed window-side handle for the Room-v5 service.

use std::{cell::RefCell, rc::Rc};

use futures::channel::oneshot;
use futures_signals::signal::{Mutable, ReadOnlyMutable};
use hhhs_web_browser::{
    DedicatedWorkerClient, ProjectionSubscription, ProjectionUpdate, SubscriptionId,
    WorkerClientError, WorkerEvent, WorkerEventKind, WorkerRequestKind, WorkerResetReason,
    serve_dedicated_worker,
};
use js_sys::Array;
use wasm_bindgen::prelude::*;
use web_sys::{Blob, BlobPropertyBag, Url};

use crate::room::{
    v5::RoomIdentity,
    worker::{
        RoomDataPlane, RoomPresenceWire, RoomReplicaWorkerService, RoomWorkerCommand,
        RoomWorkerFactory, RoomWorkerOpen, RoomWorkerOpenFuture, RoomWorkerProjection,
        RoomWorkerRepairRequest, RoomWorkerRepairStep, RoomWorkerResponse, decode_projection,
        decode_response, encode_command, encode_open, encode_repair,
    },
};

use super::storage::IndexedDbReplicaLogV5;

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
    serve_dedicated_worker(
        RoomReplicaWorkerService::new(BrowserRoomWorkerFactory),
        hhhs_web_browser::DEFAULT_MAX_PENDING_REQUESTS,
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
        (self.on_diagnostic)(message.clone());
        self.lifecycle.set(BrowserReplicaLifecycle::Failed {
            message: message.clone(),
        });
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
}

impl BrowserReplicaHandle {
    pub(super) async fn open(
        request: RoomWorkerOpen,
        on_projection: impl Fn(RoomWorkerProjection) + 'static,
        on_outbound_record: impl Fn(Vec<u8>) + 'static,
        on_diagnostic: impl Fn(String) + 'static,
    ) -> Result<(Self, RoomWorkerResponse), String> {
        let state = Rc::new(RefCell::new(WindowWorkerState {
            subscription: None,
            projection: Mutable::new(None),
            lifecycle: Mutable::new(BrowserReplicaLifecycle::Opening),
            projection_waiters: Vec::new(),
            next_repair_session: 1,
            on_projection: Rc::new(on_projection),
            on_outbound_record: Rc::new(on_outbound_record),
            on_diagnostic: Rc::new(on_diagnostic),
        }));
        let state_for_events = Rc::clone(&state);
        let script_url = worker_module_url()?;
        let result =
            DedicatedWorkerClient::open(&script_url, encode_open(&request)?, move |event| {
                state_for_events.borrow_mut().receive(event)
            })
            .await;
        // The Worker has already resolved and loaded this module URL. Revoking
        // the temporary Blob is cleanup, not part of the Replica handshake;
        // a browser cleanup quirk must not discard an otherwise live handle.
        let _ = Url::revoke_object_url(&script_url);
        let (client, ready) = result.map_err(worker_error)?;
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
        Ok((Self { client, state }, opened))
    }

    async fn command(&self, command: RoomWorkerCommand) -> Result<RoomWorkerResponse, String> {
        self.request(WorkerRequestKind::Command, encode_command(&command)?)
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
