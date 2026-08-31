//! `btleplug` implementation of the platform BLE central seam.
//!
//! The public methods only enqueue bounded commands. A private thread owns the
//! Tokio runtime, OS adapter, peripheral handles, and GATT notification stream,
//! so neither the plugin audio callback nor the bridge worker performs an
//! asynchronous Bluetooth operation.

use std::{
    collections::{BTreeSet, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use btleplug::{
    api::{
        Central, CharPropFlags, Characteristic, Manager as _, Peripheral as _, ScanFilter,
        WriteType,
    },
    platform::{Adapter, Manager, Peripheral},
};
use futures::StreamExt;
use tokio::{sync::mpsc, task::JoinHandle as TokioJoinHandle};
use tutti_ble::{
    FragmentCursor, HARD_MAX_WIRE_BYTES, INFO_CHARACTERISTIC_UUID, RX_CHARACTERISTIC_UUID,
    SERVICE_UUID, TX_CHARACTERISTIC_UUID,
};
use uuid::Uuid;

use super::{
    BleAddress, BleHost, BleHostError, BleHostEvent, BleScanResult, BleWriteMessage,
    BleWritePriority,
};

const COMMAND_CAPACITY: usize = 32;
const EVENT_CAPACITY: usize = 256;
const SCAN_REFRESH: Duration = Duration::from_millis(500);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_REDISCOVERY_TIMEOUT: Duration = Duration::from_secs(35);
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
const DISCONNECT_SETTLE_TIMEOUT: Duration = Duration::from_secs(2);
const TX_MESSAGE_CAPACITY: usize = 8;
const MAX_TX_RETAINED_BYTES: usize = HARD_MAX_WIRE_BYTES * 4;
/// BLE's baseline 23-byte ATT MTU leaves 20 characteristic-value bytes.
///
/// btleplug 0.12's BlueZ `Peripheral::mtu()` unwraps an optional D-Bus MTU and
/// can panic after otherwise-successful service discovery. Tutti fragments at
/// this boundary already, so the conservative cross-platform baseline is safe
/// and keeps dependency failures outside the plugin process.
const CONSERVATIVE_GATT_VALUE_BYTES: u16 = 20;

enum DriverCommand {
    StartScan,
    StopScan,
    Connect(BleAddress),
    Disconnect,
    WriteMessage(ReservedWriteMessage),
    Shutdown,
}

struct ReservedWriteMessage {
    message: Option<BleWriteMessage>,
    retained_bytes: usize,
    queued_messages: Arc<AtomicUsize>,
    queued_bytes: Arc<AtomicUsize>,
}

impl ReservedWriteMessage {
    fn reserve(
        message: BleWriteMessage,
        queued_messages: Arc<AtomicUsize>,
        queued_bytes: Arc<AtomicUsize>,
    ) -> Result<Self, BleHostError> {
        let retained_bytes = message.retained_wire_bytes();
        reserve_bounded(&queued_messages, 1, TX_MESSAGE_CAPACITY, "BLE TX message")?;
        if let Err(error) = reserve_bounded(
            &queued_bytes,
            retained_bytes,
            MAX_TX_RETAINED_BYTES,
            "BLE TX retained-byte",
        ) {
            queued_messages.fetch_sub(1, Ordering::AcqRel);
            return Err(error);
        }
        Ok(Self {
            message: Some(message),
            retained_bytes,
            queued_messages,
            queued_bytes,
        })
    }

    fn take_message(&mut self) -> BleWriteMessage {
        self.message
            .take()
            .expect("reserved BLE write message taken once")
    }

    fn message(&self) -> &BleWriteMessage {
        self.message
            .as_ref()
            .expect("reserved BLE write message is still present")
    }
}

impl Drop for ReservedWriteMessage {
    fn drop(&mut self) {
        self.queued_messages.fetch_sub(1, Ordering::AcqRel);
        self.queued_bytes
            .fetch_sub(self.retained_bytes, Ordering::AcqRel);
    }
}

fn reserve_bounded(
    counter: &AtomicUsize,
    amount: usize,
    maximum: usize,
    label: &'static str,
) -> Result<(), BleHostError> {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(amount).filter(|next| *next <= maximum)
        })
        .map(|_| ())
        .map_err(|_| BleHostError::Operation(format!("{label} queue is full")))
}

struct DriverTxMessage {
    _reservation: ReservedWriteMessage,
    message_id: u16,
    cursor: FragmentCursor,
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct PreparedTxFragment {
    priority: BleWritePriority,
    message_id: u16,
    completes_message: bool,
    value: Vec<u8>,
}

struct DriverTxScheduler {
    control: VecDeque<DriverTxMessage>,
    realtime: VecDeque<DriverTxMessage>,
    repair: VecDeque<DriverTxMessage>,
    pending: Option<PreparedTxFragment>,
    prefer_realtime: bool,
    faulted: bool,
}

impl Default for DriverTxScheduler {
    fn default() -> Self {
        Self {
            control: VecDeque::new(),
            realtime: VecDeque::new(),
            repair: VecDeque::new(),
            pending: None,
            prefer_realtime: true,
            faulted: true,
        }
    }
}

impl DriverTxScheduler {
    fn enqueue(&mut self, mut reserved: ReservedWriteMessage) -> Result<(), BleHostError> {
        if self.faulted {
            return Err(BleHostError::Operation(
                "BLE TX stream is faulted until a fresh connection".into(),
            ));
        }
        let message_id = reserved.message().message_id();
        let (priority, cursor) = reserved.take_message().into_cursor()?;
        let message = DriverTxMessage {
            _reservation: reserved,
            message_id,
            cursor,
        };
        self.queue_mut(priority).push_back(message);
        Ok(())
    }

    fn is_empty(&self) -> bool {
        self.control.is_empty() && self.realtime.is_empty() && self.repair.is_empty()
    }

    fn queue_mut(&mut self, priority: BleWritePriority) -> &mut VecDeque<DriverTxMessage> {
        match priority {
            BleWritePriority::Control => &mut self.control,
            BleWritePriority::Realtime => &mut self.realtime,
            BleWritePriority::Repair => &mut self.repair,
        }
    }

    fn next_priority(&mut self) -> Option<BleWritePriority> {
        if !self.control.is_empty() {
            return Some(BleWritePriority::Control);
        }
        match (self.realtime.is_empty(), self.repair.is_empty()) {
            (false, false) => {
                let priority = if self.prefer_realtime {
                    BleWritePriority::Realtime
                } else {
                    BleWritePriority::Repair
                };
                self.prefer_realtime = !self.prefer_realtime;
                Some(priority)
            }
            (false, true) => Some(BleWritePriority::Realtime),
            (true, false) => Some(BleWritePriority::Repair),
            (true, true) => None,
        }
    }

    fn prepare_next(&mut self) -> Result<Option<PreparedTxFragment>, BleHostError> {
        if let Some(pending) = self.pending.as_ref() {
            return Ok(Some(pending.clone()));
        }
        let Some(priority) = self.next_priority() else {
            return Ok(None);
        };
        let message = self
            .queue_mut(priority)
            .front_mut()
            .expect("selected BLE TX queue is non-empty");
        let mut value = vec![0; message.cursor.fragment_value_bytes()];
        let used = message
            .cursor
            .encode_next(&mut value)
            .map_err(|error| BleHostError::Operation(error.to_string()))?
            .ok_or_else(|| {
                BleHostError::Operation("completed BLE TX cursor was retained".into())
            })?;
        value.truncate(used);
        let pending = PreparedTxFragment {
            priority,
            message_id: message.message_id,
            completes_message: message.cursor.is_complete(),
            value,
        };
        self.pending = Some(pending.clone());
        Ok(Some(pending))
    }

    fn confirm_pending(&mut self) -> Option<u16> {
        let prepared = self.pending.take()?;
        if prepared.completes_message {
            self.queue_mut(prepared.priority).pop_front();
            return Some(prepared.message_id);
        }
        None
    }

    fn clear(&mut self) {
        self.control.clear();
        self.realtime.clear();
        self.repair.clear();
        self.pending = None;
    }

    fn fault(&mut self) {
        self.clear();
        self.faulted = true;
    }

    fn fresh_connection(&mut self) {
        self.clear();
        self.prefer_realtime = true;
        self.faulted = false;
    }
}

/// Cross-platform desktop BLE central backed by `btleplug`.
///
/// Construction does not open an adapter synchronously. Initialization errors
/// arrive through [`BleHostEvent::Error`], just like later OS failures.
pub struct BtleplugHost {
    commands: mpsc::Sender<DriverCommand>,
    events: Receiver<BleHostEvent>,
    shutdown: Arc<AtomicBool>,
    dropped_events: Arc<AtomicU64>,
    queued_write_messages: Arc<AtomicUsize>,
    queued_write_bytes: Arc<AtomicUsize>,
    driver: Option<JoinHandle<()>>,
}

impl BtleplugHost {
    pub fn spawn() -> Result<Self, BleHostError> {
        let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (event_tx, event_rx) = sync_channel(EVENT_CAPACITY);
        let dropped_events = Arc::new(AtomicU64::new(0));
        let event_sink = EventSink {
            sender: event_tx,
            dropped: Arc::clone(&dropped_events),
        };
        let shutdown = Arc::new(AtomicBool::new(false));
        let driver_shutdown = Arc::clone(&shutdown);
        let queued_write_messages = Arc::new(AtomicUsize::new(0));
        let queued_write_bytes = Arc::new(AtomicUsize::new(0));
        let driver = thread::Builder::new()
            .name("walkie-ble".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        send_event(
                            &event_sink,
                            BleHostEvent::Error(BleHostError::Unavailable(format!(
                                "could not create BLE runtime: {error}"
                            ))),
                        );
                        return;
                    }
                };
                runtime.block_on(driver_loop(command_rx, event_sink, driver_shutdown));
            })
            .map_err(|error| {
                BleHostError::Unavailable(format!("could not start BLE driver: {error}"))
            })?;

        Ok(Self {
            commands: command_tx,
            events: event_rx,
            shutdown,
            dropped_events,
            queued_write_messages,
            queued_write_bytes,
            driver: Some(driver),
        })
    }

    fn command(&self, command: DriverCommand) -> Result<(), BleHostError> {
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    BleHostError::Operation("BLE command queue is full".into())
                }
                mpsc::error::TrySendError::Closed(_) => {
                    BleHostError::Unavailable("BLE driver has stopped".into())
                }
            })
    }
}

impl BleHost for BtleplugHost {
    fn start_scan(&mut self) -> Result<(), BleHostError> {
        self.command(DriverCommand::StartScan)
    }

    fn stop_scan(&mut self) -> Result<(), BleHostError> {
        self.command(DriverCommand::StopScan)
    }

    fn connect(&mut self, address: &BleAddress) -> Result<(), BleHostError> {
        self.command(DriverCommand::Connect(address.clone()))
    }

    fn disconnect(&mut self) -> Result<(), BleHostError> {
        self.command(DriverCommand::Disconnect)
    }

    fn write_rx_message(&mut self, message: BleWriteMessage) -> Result<(), BleHostError> {
        let reserved = ReservedWriteMessage::reserve(
            message,
            Arc::clone(&self.queued_write_messages),
            Arc::clone(&self.queued_write_bytes),
        )?;
        self.command(DriverCommand::WriteMessage(reserved))
    }

    fn poll_event(&mut self) -> Option<BleHostEvent> {
        match self.events.try_recv() {
            Ok(event) => Some(event),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => {
                let dropped = self.dropped_events.swap(0, Ordering::AcqRel);
                (dropped != 0).then_some(BleHostEvent::EventsDropped(dropped))
            }
        }
    }
}

impl Drop for BtleplugHost {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.commands.try_send(DriverCommand::Shutdown);
        // An OS connect call can remain pending until its timeout. Detaching is
        // preferable to blocking a DAW while it destroys a plugin instance;
        // the shutdown flag makes the driver exit at its next scheduling point.
        let _ = self.driver.take();
    }
}

struct Connection {
    peripheral: Peripheral,
    address: BleAddress,
    rx: Characteristic,
    write_type: WriteType,
    notifications: TokioJoinHandle<()>,
    max_fragment_value_bytes: u16,
    initial_info: Option<Vec<u8>>,
}

struct PendingConnect {
    address: BleAddress,
    deadline: tokio::time::Instant,
    last_issue: Option<String>,
}

impl PendingConnect {
    fn new(address: BleAddress) -> Self {
        Self {
            address,
            deadline: tokio::time::Instant::now() + CONNECT_REDISCOVERY_TIMEOUT,
            last_issue: None,
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ScanState {
    Idle,
    /// Discovery was started by this driver and may be stopped by it.
    Owned,
    /// BlueZ was already discovering for another client. We may consume the
    /// shared results, but must not stop discovery on that client's behalf.
    Adopted,
}

impl ScanState {
    fn is_active(self) -> bool {
        self != Self::Idle
    }
}

#[derive(Clone)]
struct EventSink {
    sender: SyncSender<BleHostEvent>,
    dropped: Arc<AtomicU64>,
}

async fn driver_loop(
    mut commands: mpsc::Receiver<DriverCommand>,
    events: EventSink,
    shutdown: Arc<AtomicBool>,
) {
    let mut adapter = match open_adapter().await {
        Ok(adapter) => adapter,
        Err(error) => {
            send_host_error(&events, error);
            return;
        }
    };

    let mut scan_state = ScanState::Idle;
    let mut reported = BTreeSet::new();
    let mut connection: Option<Connection> = None;
    let mut pending_connect: Option<PendingConnect> = None;
    let mut tx = DriverTxScheduler::default();
    let mut refresh = tokio::time::interval(SCAN_REFRESH);
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        let tx_ready = !tx.is_empty() && connection.is_some();

        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { break };
                match command {
                    DriverCommand::StartScan => {
                        if let Err(error) = ensure_scan_active(
                            &adapter,
                            &mut scan_state,
                            &mut reported,
                        ).await {
                            send_host_error(&events, error);
                        }
                    }
                    DriverCommand::StopScan => {
                        // A connect that is reacquiring a transient BlueZ
                        // object owns its discovery window. A stale StopScan
                        // command must not cancel that in-flight acquisition.
                        if pending_connect.is_none()
                            && let Err(error) = stop_owned_scan(&adapter, &mut scan_state).await
                        {
                            send_host_error(&events, error);
                        }
                    }
                    DriverCommand::Connect(address) => {
                        tx.fault();
                        disconnect_current(&mut connection, &events, None).await;
                        pending_connect = Some(PendingConnect::new(address.clone()));
                        // Resolve and open the transient unpaired BlueZ object
                        // while discovery still keeps it visible. Discovery is
                        // released only after the complete GATT setup succeeds.
                        match try_connect_visible(
                            &adapter,
                            &address,
                            &mut scan_state,
                            events.clone(),
                        ).await {
                            Ok(Some(new_connection)) => {
                                pending_connect = None;
                                install_connection(&events, new_connection, &mut connection);
                                tx.fresh_connection();
                            }
                            Ok(None) => {
                                // BlueZ may evict an unpaired Device1 between
                                // the UI's scan result and this queued command.
                                // Keep the same attempt alive and reacquire the
                                // object from fresh advertisements.
                                if let Err(error) = ensure_scan_active(
                                    &adapter,
                                    &mut scan_state,
                                    &mut reported,
                                ).await {
                                    pending_connect = None;
                                    send_host_error(&events, error);
                                }
                            }
                            Err(error) => {
                                if retryable_connect_error(&error) {
                                    if let Some(pending) = pending_connect.as_mut() {
                                        pending.last_issue = Some(error.to_string());
                                    }
                                    send_event(
                                        &events,
                                        BleHostEvent::Diagnostic(format!(
                                            "{error}; continuing bounded board discovery"
                                        )),
                                    );
                                    // A cancelled BlueZ Device1.Connect can
                                    // poison this client's proxy even though
                                    // a new D-Bus client connects immediately.
                                    // Reopen the adapter proxy as well as the
                                    // transient peripheral before retrying.
                                    if let Err(scan_error) = reopen_adapter_and_scan(
                                        &mut adapter,
                                        &mut scan_state,
                                        &mut reported,
                                    ).await {
                                        pending_connect = None;
                                        send_host_error(&events, scan_error);
                                    }
                                } else {
                                    pending_connect = None;
                                    send_host_error(&events, error);
                                }
                            }
                        }
                    }
                    DriverCommand::Disconnect => {
                        tx.fault();
                        pending_connect = None;
                        if let Err(error) = stop_owned_scan(&adapter, &mut scan_state).await {
                            send_host_error(&events, error);
                        }
                        disconnect_current(&mut connection, &events, None).await;
                    }
                    DriverCommand::WriteMessage(message) => {
                        if connection.is_none() {
                            send_host_error(
                                &events,
                                BleHostError::Operation("cannot write: no board is connected".into()),
                            );
                        } else if let Err(error) = tx.enqueue(message) {
                            send_host_error(&events, error);
                        }
                    }
                    DriverCommand::Shutdown => break,
                }
            }
            _ = async {
                if !tx_ready {
                    std::future::pending::<()>().await;
                }
                tokio::task::yield_now().await;
            } => {
                let prepared = match tx.prepare_next() {
                    Ok(Some(prepared)) => prepared,
                    Ok(None) => continue,
                    Err(error) => {
                        tx.fault();
                        send_host_error(&events, error);
                        disconnect_current(
                            &mut connection,
                            &events,
                            Some("BLE TX framing fault".into()),
                        )
                        .await;
                        continue;
                    }
                };
                let active = connection.as_ref().expect("TX readiness requires a connection");
                match active
                    .peripheral
                    .write(&active.rx, &prepared.value, active.write_type)
                    .await
                {
                    Ok(()) => {
                        if let Some(message_id) = tx.confirm_pending() {
                            send_event(&events, BleHostEvent::WriteComplete { message_id });
                        }
                    }
                    Err(error) => {
                        // The cursor may already point beyond this fragment,
                        // but it remains fenced by `pending` until success.
                        // A platform write error invalidates the whole BLE
                        // stream, so drop every message and force a fresh
                        // authenticated connection rather than skipping bytes.
                        tx.fault();
                        operation_error(&events, "write RX characteristic", error);
                        disconnect_current(
                            &mut connection,
                            &events,
                            Some("BLE TX write failed".into()),
                        )
                        .await;
                    }
                }
            }
            _ = refresh.tick() => {
                if scan_state.is_active() {
                    refresh_scan_results(&adapter, &events, &mut reported).await;
                }
                if let Some(pending) = pending_connect.as_ref() {
                    if tokio::time::Instant::now() >= pending.deadline {
                        let address = pending.address.clone();
                        let last_issue = pending.last_issue.clone();
                        pending_connect = None;
                        if let Err(error) = stop_owned_scan(&adapter, &mut scan_state).await {
                            send_host_error(&events, error);
                        }
                        let mut message = format!(
                                "connect: board {} did not reappear during {CONNECT_REDISCOVERY_TIMEOUT:?} of BLE discovery",
                                address.0,
                            );
                        if let Some(issue) = last_issue {
                            message.push_str("; last issue: ");
                            message.push_str(&issue);
                        }
                        send_host_error(&events, BleHostError::Operation(message));
                    } else {
                        let address = pending.address.clone();
                        match try_connect_visible(
                            &adapter,
                            &address,
                            &mut scan_state,
                            events.clone(),
                        ).await {
                            Ok(Some(new_connection)) => {
                                pending_connect = None;
                                install_connection(&events, new_connection, &mut connection);
                                tx.fresh_connection();
                            }
                            Ok(None) => {}
                            Err(error) => {
                                if retryable_connect_error(&error) {
                                    if let Some(pending) = pending_connect.as_mut() {
                                        pending.last_issue = Some(error.to_string());
                                    }
                                    send_event(
                                        &events,
                                        BleHostEvent::Diagnostic(format!(
                                            "{error}; continuing bounded board discovery"
                                        )),
                                    );
                                    if let Err(scan_error) = reopen_adapter_and_scan(
                                        &mut adapter,
                                        &mut scan_state,
                                        &mut reported,
                                    ).await {
                                        pending_connect = None;
                                        send_host_error(&events, scan_error);
                                    }
                                } else {
                                    pending_connect = None;
                                    send_host_error(&events, error);
                                }
                            }
                        }
                    }
                }
                if let Some(active) = connection.as_ref() {
                    match active.peripheral.is_connected().await {
                        Ok(true) => {}
                        Ok(false) => {
                            tx.fault();
                            let address = active.address.clone();
                            active.notifications.abort();
                            connection = None;
                            send_event(&events, BleHostEvent::Disconnected {
                                address,
                                reason: Some("operating system reported link loss".into()),
                            });
                        }
                        Err(error) => {
                            tx.fault();
                            operation_error(&events, "check connection", error);
                            disconnect_current(
                                &mut connection,
                                &events,
                                Some("BLE connection check failed".into()),
                            )
                            .await;
                        }
                    }
                }
            }
        }
    }

    tx.fault();
    disconnect_current(&mut connection, &events, Some("BLE driver stopped".into())).await;
    if scan_state == ScanState::Owned {
        let _ = adapter.stop_scan().await;
    }
}

async fn ensure_scan_active(
    adapter: &Adapter,
    scan_state: &mut ScanState,
    reported: &mut BTreeSet<BleAddress>,
) -> Result<(), BleHostError> {
    if scan_state.is_active() {
        // Re-emit cached advertisements on the next refresh. BlueZ discovery
        // is adapter-global, so starting it twice is neither necessary nor
        // reliable.
        reported.clear();
        return Ok(());
    }
    match adapter
        .start_scan(ScanFilter {
            services: vec![service_uuid()],
        })
        .await
    {
        Ok(()) => *scan_state = ScanState::Owned,
        Err(error) if scan_already_active(&error) => {
            // Another desktop client owns discovery. Its advertisements are
            // still visible to this adapter, but we must not stop its scan.
            *scan_state = ScanState::Adopted;
        }
        Err(error) => return Err(operation("start scan")(error)),
    }
    reported.clear();
    Ok(())
}

async fn reopen_adapter_and_scan(
    adapter: &mut Adapter,
    scan_state: &mut ScanState,
    reported: &mut BTreeSet<BleAddress>,
) -> Result<(), BleHostError> {
    // Dropping the old proxy releases any client-owned discovery reference and
    // canceled Device1 transaction associated with that D-Bus connection.
    // Adapter-global discovery may still be active for another client; the
    // ordinary scan path then adopts it without trying to stop it.
    *adapter = open_adapter().await?;
    *scan_state = ScanState::Idle;
    ensure_scan_active(adapter, scan_state, reported).await
}

async fn stop_owned_scan(
    adapter: &Adapter,
    scan_state: &mut ScanState,
) -> Result<(), BleHostError> {
    match *scan_state {
        ScanState::Idle => Ok(()),
        ScanState::Adopted => {
            *scan_state = ScanState::Idle;
            Ok(())
        }
        ScanState::Owned => {
            adapter.stop_scan().await.map_err(operation("stop scan"))?;
            *scan_state = ScanState::Idle;
            Ok(())
        }
    }
}

async fn try_connect_visible(
    adapter: &Adapter,
    address: &BleAddress,
    scan_state: &mut ScanState,
    events: EventSink,
) -> Result<Option<Connection>, BleHostError> {
    let Some(peripheral) = find_board(adapter, address).await? else {
        return Ok(None);
    };

    // Keep discovery alive while opening an unpaired transient Device1.
    // Stopping it first lets BlueZ evict the object between lookup and the
    // Connect method call. Incomplete setup intentionally retains discovery
    // so the same bounded attempt can reacquire a fresh object.
    let result = connect_board(peripheral, address, events).await;
    if matches!(result, Ok(Some(_))) {
        if *scan_state == ScanState::Owned {
            // The live connection now owns the object. A failed stop should
            // not hide the more useful GATT result.
            let _ = adapter.stop_scan().await;
        }
        *scan_state = ScanState::Idle;
    }
    result
}

fn install_connection(
    events: &EventSink,
    mut new_connection: Connection,
    connection: &mut Option<Connection>,
) {
    send_event(
        events,
        BleHostEvent::Connected {
            address: new_connection.address.clone(),
            max_fragment_value_bytes: new_connection.max_fragment_value_bytes,
        },
    );
    // The transport must learn the negotiated ATT value size before INFO can
    // trigger TOFU or a fragmented authentication handshake.
    if let Some(info) = new_connection.initial_info.take() {
        send_event(events, BleHostEvent::Info(info));
    }
    *connection = Some(new_connection);
}

async fn open_adapter() -> Result<Adapter, BleHostError> {
    let manager = Manager::new()
        .await
        .map_err(|error| BleHostError::Unavailable(error.to_string()))?;
    manager
        .adapters()
        .await
        .map_err(|error| BleHostError::Unavailable(error.to_string()))?
        .into_iter()
        .next()
        .ok_or_else(|| BleHostError::Unavailable("no Bluetooth adapter was found".into()))
}

async fn refresh_scan_results(
    adapter: &Adapter,
    events: &EventSink,
    reported: &mut BTreeSet<BleAddress>,
) {
    let peripherals = match adapter.peripherals().await {
        Ok(peripherals) => peripherals,
        Err(error) => {
            operation_error(events, "list peripherals", error);
            return;
        }
    };

    for peripheral in peripherals {
        let properties = match peripheral.properties().await {
            Ok(Some(properties)) => properties,
            Ok(None) => continue,
            Err(error) => {
                send_event(
                    events,
                    BleHostEvent::Diagnostic(format!(
                        "skipped a disappearing BLE advertisement: {error}"
                    )),
                );
                continue;
            }
        };
        // Some platforms combine scan filters from multiple clients. Always
        // filter advertisements again in application code.
        if !properties.services.contains(&service_uuid()) {
            continue;
        }
        let address = BleAddress(peripheral.id().to_string());
        if reported.insert(address.clone()) {
            send_event(
                events,
                BleHostEvent::ScanResult(BleScanResult {
                    address,
                    display_name: properties.local_name.or(properties.advertisement_name),
                    signal_dbm: properties.rssi,
                }),
            );
        }
    }
}

async fn find_board(
    adapter: &Adapter,
    address: &BleAddress,
) -> Result<Option<Peripheral>, BleHostError> {
    Ok(adapter
        .peripherals()
        .await
        .map_err(operation("list peripherals"))?
        .into_iter()
        .find(|peripheral| peripheral.id().to_string() == address.0))
}

async fn connect_board(
    peripheral: Peripheral,
    address: &BleAddress,
    events: EventSink,
) -> Result<Option<Connection>, BleHostError> {
    ensure_connected(&peripheral).await?;
    let prepared = prepare_connected_board(&peripheral, address, &events).await;
    match prepared {
        Ok(Some(connection)) => Ok(Some(connection)),
        Ok(None) => {
            // The GATT object disappeared during atomic setup. Do not publish
            // Connected followed by an unrelated fatal Error. Settle the old
            // object and let the driver's existing pending-connect discovery
            // window reacquire the same selected board.
            let _ = disconnect_and_settle(&peripheral, "settle interrupted connection").await;
            Ok(None)
        }
        Err(error) => {
            // No partially subscribed connection escapes setup. The caller
            // receives one error for this attempt and cannot install a stale
            // peripheral alongside it.
            let _ = disconnect_and_settle(&peripheral, "clean up failed connection").await;
            Err(error)
        }
    }
}

async fn prepare_connected_board(
    peripheral: &Peripheral,
    address: &BleAddress,
    events: &EventSink,
) -> Result<Option<Connection>, BleHostError> {
    peripheral
        .discover_services_with_timeout(DISCOVERY_TIMEOUT)
        .await
        .map_err(operation("discover GATT services"))?;

    let characteristics = peripheral.characteristics();
    let required = (
        find_characteristic(&characteristics, RX_CHARACTERISTIC_UUID, "RX"),
        find_characteristic(&characteristics, TX_CHARACTERISTIC_UUID, "TX"),
        find_characteristic(&characteristics, INFO_CHARACTERISTIC_UUID, "INFO"),
    );
    let (rx, tx, info) = match required {
        (Ok(rx), Ok(tx), Ok(info)) => (rx, tx, info),
        _ => {
            // BlueZ can retain an unpaired Device1 across a peripheral reboot
            // and briefly report service discovery complete while exposing
            // only the advertised service UUID. Installing that partial cache
            // produces a false Connected state. Reacquire the transient
            // object within the same bounded pending-connect attempt.
            send_event(
                events,
                BleHostEvent::Diagnostic(
                    "Tutti GATT cache is incomplete; reacquiring the board service".into(),
                ),
            );
            return Ok(None);
        }
    };

    if !tx
        .properties
        .intersects(CharPropFlags::NOTIFY | CharPropFlags::INDICATE)
    {
        return Err(BleHostError::Operation(
            "Tutti TX characteristic cannot notify".into(),
        ));
    }
    if !info
        .properties
        .intersects(CharPropFlags::NOTIFY | CharPropFlags::INDICATE)
    {
        return Err(BleHostError::Operation(
            "Tutti INFO characteristic cannot notify".into(),
        ));
    }
    let write_type = select_write_type(rx.properties)?;

    let mut stream = peripheral
        .notifications()
        .await
        .map_err(operation("open notification stream"))?;
    peripheral
        .subscribe(&tx)
        .await
        .map_err(operation("subscribe TX characteristic"))?;
    peripheral
        .subscribe(&info)
        .await
        .map_err(operation("subscribe INFO characteristic"))?;

    let initial_info = if info.properties.contains(CharPropFlags::READ) {
        match peripheral.read(&info).await {
            Ok(value) => Some(value),
            Err(error) => {
                let connection_state = peripheral.is_connected().await;
                let link_lost = info_read_indicates_link_loss(&error, &connection_state);
                if link_lost {
                    return Ok(None);
                }
                // INFO is subscribed before this optional read. A platform
                // that cannot read it may still deliver the authoritative
                // notification; report that fallback without poisoning the
                // otherwise usable connection.
                send_event(
                    events,
                    BleHostEvent::Diagnostic(format!(
                        "read INFO characteristic was unavailable ({error}); awaiting notification"
                    )),
                );
                None
            }
        }
    } else {
        None
    };

    let notification_events = events.clone();
    let notifications = tokio::spawn(async move {
        while let Some(notification) = stream.next().await {
            let event = if notification.uuid == info.uuid {
                BleHostEvent::Info(notification.value)
            } else if notification.uuid == tx.uuid {
                BleHostEvent::Notification(notification.value)
            } else {
                continue;
            };
            send_event(&notification_events, event);
        }
    });

    let max_fragment_value_bytes = CONSERVATIVE_GATT_VALUE_BYTES;
    Ok(Some(Connection {
        peripheral: peripheral.clone(),
        address: address.clone(),
        rx,
        write_type,
        notifications,
        max_fragment_value_bytes,
        initial_info,
    }))
}

fn info_read_indicates_link_loss(
    read_error: &btleplug::Error,
    connection_state: &Result<bool, btleplug::Error>,
) -> bool {
    matches!(
        read_error,
        btleplug::Error::NotConnected | btleplug::Error::DeviceNotFound
    ) || matches!(
        connection_state,
        Ok(false) | Err(btleplug::Error::NotConnected | btleplug::Error::DeviceNotFound)
    )
}

trait ConnectPeripheral {
    async fn link_is_connected(&self) -> Result<bool, btleplug::Error>;
    async fn link_connect(&self) -> Result<(), btleplug::Error>;
    async fn link_disconnect(&self) -> Result<(), btleplug::Error>;
}

impl ConnectPeripheral for Peripheral {
    async fn link_is_connected(&self) -> Result<bool, btleplug::Error> {
        self.is_connected().await
    }

    async fn link_connect(&self) -> Result<(), btleplug::Error> {
        self.connect_with_timeout(CONNECT_TIMEOUT).await
    }

    async fn link_disconnect(&self) -> Result<(), btleplug::Error> {
        self.disconnect().await
    }
}

async fn ensure_connected<P: ConnectPeripheral>(peripheral: &P) -> Result<(), BleHostError> {
    if matches!(peripheral.link_is_connected().await, Ok(true)) {
        // A previous process or timed-out D-Bus call can leave BlueZ connected
        // while the application session and notification stream are gone. The
        // Tutti handshake is boot- and stream-bound, so inheriting that link is
        // unsafe: acquire a fresh GATT session for this driver.
        disconnect_and_settle(peripheral, "reset existing connection").await?;
    }
    match peripheral.link_connect().await {
        Ok(()) => Ok(()),
        Err(error) => {
            // Some platform stacks finish the link after their connect future
            // has timed out. Trust the observable link state in that race.
            if matches!(peripheral.link_is_connected().await, Ok(true)) {
                Ok(())
            } else {
                let result = Err(operation("connect")(error));
                // `connect_with_timeout` cancels its future, but BlueZ may
                // retain the underlying Device1.Connect transaction. Ask it
                // to settle before a later explicit retry.
                let _ = disconnect_and_settle(peripheral, "cancel failed connection").await;
                result
            }
        }
    }
}

async fn disconnect_and_settle<P: ConnectPeripheral>(
    peripheral: &P,
    action: &'static str,
) -> Result<(), BleHostError> {
    match peripheral.link_disconnect().await {
        Ok(()) | Err(btleplug::Error::NotConnected) => {}
        Err(error) => return Err(operation(action)(error)),
    }
    let deadline = tokio::time::Instant::now() + DISCONNECT_SETTLE_TIMEOUT;
    loop {
        match peripheral.link_is_connected().await {
            Ok(false) | Err(btleplug::Error::NotConnected) => return Ok(()),
            Ok(true) => {}
            Err(error) => return Err(operation(action)(error)),
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(BleHostError::Operation(format!(
                "{action}: link did not disconnect within {DISCONNECT_SETTLE_TIMEOUT:?}"
            )));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn find_characteristic(
    characteristics: &BTreeSet<Characteristic>,
    uuid: u128,
    label: &str,
) -> Result<Characteristic, BleHostError> {
    characteristics
        .iter()
        .find(|characteristic| {
            characteristic.service_uuid == service_uuid()
                && characteristic.uuid == Uuid::from_u128(uuid)
        })
        .cloned()
        .ok_or_else(|| BleHostError::Operation(format!("Tutti {label} characteristic is missing")))
}

fn select_write_type(properties: CharPropFlags) -> Result<WriteType, BleHostError> {
    // Handshake and profile frames are control-plane state, often fragmented at
    // the BLE 4.2 ATT value size. Prefer acknowledged writes when a peripheral
    // supports both modes so fragments are ordered and backpressured instead of
    // being silently dropped by an OS or a small embedded callback queue.
    if properties.contains(CharPropFlags::WRITE) {
        Ok(WriteType::WithResponse)
    } else if properties.contains(CharPropFlags::WRITE_WITHOUT_RESPONSE) {
        Ok(WriteType::WithoutResponse)
    } else {
        Err(BleHostError::Operation(
            "Tutti RX characteristic is not writable".into(),
        ))
    }
}

async fn disconnect_current(
    connection: &mut Option<Connection>,
    events: &EventSink,
    reason: Option<String>,
) {
    let Some(active) = connection.take() else {
        return;
    };
    active.notifications.abort();
    if let Err(error) = active.peripheral.disconnect().await {
        operation_error(events, "disconnect", error);
    }
    send_event(
        events,
        BleHostEvent::Disconnected {
            address: active.address,
            reason,
        },
    );
}

fn service_uuid() -> Uuid {
    Uuid::from_u128(SERVICE_UUID)
}

fn operation(action: &'static str) -> impl FnOnce(btleplug::Error) -> BleHostError {
    move |error| match error {
        btleplug::Error::PermissionDenied => BleHostError::PermissionDenied,
        error => BleHostError::Operation(format!("{action}: {error}")),
    }
}

fn scan_already_active(error: &btleplug::Error) -> bool {
    scan_already_active_message(&error.to_string())
}

fn scan_already_active_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("operation already in progress")
        || message.contains("org.bluez.error.inprogress")
}

fn retryable_connect_error(error: &BleHostError) -> bool {
    let BleHostError::Operation(message) = error else {
        return false;
    };
    let message = message.to_ascii_lowercase();
    message.contains("connect: timed out")
        || message.contains("device not found")
        || message.contains("not connected")
        || message.contains("org.bluez.error.failed")
        || message.contains("org.freedesktop.dbus.properties")
}

fn operation_error(events: &EventSink, action: &'static str, error: btleplug::Error) {
    send_host_error(events, operation(action)(error));
}

fn send_host_error(events: &EventSink, error: BleHostError) {
    match error {
        BleHostError::PermissionDenied => send_event(events, BleHostEvent::PermissionDenied),
        error => send_event(events, BleHostEvent::Error(error)),
    }
}

fn send_event(events: &EventSink, event: BleHostEvent) {
    match events.sender.try_send(event) {
        Ok(()) | Err(TrySendError::Disconnected(_)) => {}
        Err(TrySendError::Full(_)) => {
            // The consumer is stalled. Never let an OS Bluetooth callback
            // block indefinitely; the lane decoder will reject an incomplete
            // fragmented message and reconnect supervision can repair it.
            events.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, sync::Mutex};

    use btleplug::api::{CharPropFlags, Characteristic};

    use super::*;

    fn characteristic(uuid: u128, service: u128) -> Characteristic {
        Characteristic {
            uuid: Uuid::from_u128(uuid),
            service_uuid: Uuid::from_u128(service),
            properties: CharPropFlags::READ,
            descriptors: BTreeSet::new(),
        }
    }

    #[test]
    fn interrupted_info_read_is_reacquired_but_optional_read_failure_is_not_fatal() {
        assert!(info_read_indicates_link_loss(
            &btleplug::Error::NotConnected,
            &Ok(true),
        ));
        assert!(info_read_indicates_link_loss(
            &btleplug::Error::UnexpectedCallback,
            &Ok(false),
        ));
        assert!(!info_read_indicates_link_loss(
            &btleplug::Error::UnexpectedCallback,
            &Ok(true),
        ));
    }

    #[test]
    fn transient_bluez_connect_races_are_retried_but_permissions_are_not() {
        assert!(retryable_connect_error(&BleHostError::Operation(
            "connect: Timed out after 10s".into(),
        )));
        assert!(retryable_connect_error(&BleHostError::Operation(
            "Device not found".into(),
        )));
        assert!(!retryable_connect_error(&BleHostError::PermissionDenied));
        assert!(!retryable_connect_error(&BleHostError::Operation(
            "Tutti RX characteristic is not writable".into(),
        )));
    }

    #[test]
    fn characteristic_lookup_is_scoped_to_the_tutti_service() {
        let mut characteristics = BTreeSet::new();
        characteristics.insert(characteristic(RX_CHARACTERISTIC_UUID, 1));
        characteristics.insert(characteristic(RX_CHARACTERISTIC_UUID, SERVICE_UUID));

        let selected = find_characteristic(&characteristics, RX_CHARACTERISTIC_UUID, "RX")
            .expect("Tutti characteristic should be found");
        assert_eq!(selected.service_uuid, service_uuid());
    }

    #[test]
    fn acknowledged_writes_are_preferred_for_fragmented_control_messages() {
        let both = CharPropFlags::WRITE | CharPropFlags::WRITE_WITHOUT_RESPONSE;
        assert_eq!(
            select_write_type(both).expect("dual-mode RX should be writable"),
            WriteType::WithResponse
        );
        assert_eq!(
            select_write_type(CharPropFlags::WRITE_WITHOUT_RESPONSE)
                .expect("write-without-response-only RX should remain supported"),
            WriteType::WithoutResponse
        );
        assert!(select_write_type(CharPropFlags::READ).is_err());
    }

    #[test]
    fn bluez_in_progress_scan_is_adoptable() {
        assert!(scan_already_active_message(
            "Bluetooth operation failed: start scan: Operation already in progress"
        ));
        assert!(scan_already_active_message(
            "D-Bus error org.bluez.Error.InProgress"
        ));
        assert!(!scan_already_active_message("adapter is not powered"));
    }

    #[test]
    fn conservative_fragment_budget_fits_the_baseline_att_mtu() {
        assert_eq!(CONSERVATIVE_GATT_VALUE_BYTES, 23 - 3);
        assert!(usize::from(CONSERVATIVE_GATT_VALUE_BYTES) >= tutti_ble::MIN_FRAGMENT_VALUE_BYTES);
    }

    fn reserved_test_message(
        id: u16,
        wire: Vec<u8>,
        priority: BleWritePriority,
        messages: &Arc<AtomicUsize>,
        bytes: &Arc<AtomicUsize>,
    ) -> ReservedWriteMessage {
        ReservedWriteMessage::reserve(
            BleWriteMessage::new(id, wire, 20, priority).unwrap(),
            Arc::clone(messages),
            Arc::clone(bytes),
        )
        .unwrap()
    }

    #[test]
    fn failed_nonterminal_write_retries_identical_fragment_before_advancing() {
        let messages = Arc::new(AtomicUsize::new(0));
        let bytes = Arc::new(AtomicUsize::new(0));
        let mut tx = DriverTxScheduler::default();
        tx.fresh_connection();
        tx.enqueue(reserved_test_message(
            10,
            vec![0x52; 100],
            BleWritePriority::Repair,
            &messages,
            &bytes,
        ))
        .unwrap();

        let first_attempt = tx.prepare_next().unwrap().unwrap();
        assert!(!first_attempt.completes_message);
        let retry = tx.prepare_next().unwrap().unwrap();
        assert_eq!(retry, first_attempt);
        assert_eq!(messages.load(Ordering::Acquire), 1);
        assert_eq!(bytes.load(Ordering::Acquire), 100);

        tx.confirm_pending();
        let second = tx.prepare_next().unwrap().unwrap();
        assert_ne!(second.value, first_attempt.value);
    }

    #[test]
    fn failed_terminal_write_is_retried_and_not_retired_before_confirmation() {
        let messages = Arc::new(AtomicUsize::new(0));
        let bytes = Arc::new(AtomicUsize::new(0));
        let mut tx = DriverTxScheduler::default();
        tx.fresh_connection();
        tx.enqueue(reserved_test_message(
            11,
            b"terminal".to_vec(),
            BleWritePriority::Control,
            &messages,
            &bytes,
        ))
        .unwrap();

        let terminal = tx.prepare_next().unwrap().unwrap();
        assert!(terminal.completes_message);
        assert_eq!(tx.prepare_next().unwrap().unwrap(), terminal);
        assert!(!tx.is_empty());
        assert_eq!(messages.load(Ordering::Acquire), 1);

        tx.confirm_pending();
        assert!(tx.is_empty());
        assert_eq!(messages.load(Ordering::Acquire), 0);
        assert_eq!(bytes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn write_failure_faults_stream_drops_queue_and_refuses_later_messages() {
        let messages = Arc::new(AtomicUsize::new(0));
        let bytes = Arc::new(AtomicUsize::new(0));
        let mut tx = DriverTxScheduler::default();
        tx.fresh_connection();
        tx.enqueue(reserved_test_message(
            12,
            vec![0x52; 100],
            BleWritePriority::Repair,
            &messages,
            &bytes,
        ))
        .unwrap();
        let failed = tx.prepare_next().unwrap().unwrap();
        assert!(!failed.completes_message);

        tx.fault();
        assert!(tx.is_empty());
        assert_eq!(messages.load(Ordering::Acquire), 0);
        assert_eq!(bytes.load(Ordering::Acquire), 0);
        let later = reserved_test_message(
            13,
            b"must not send".to_vec(),
            BleWritePriority::Realtime,
            &messages,
            &bytes,
        );
        assert!(tx.enqueue(later).is_err());
        assert!(tx.prepare_next().unwrap().is_none());
        assert_eq!(messages.load(Ordering::Acquire), 0);

        tx.fresh_connection();
        tx.enqueue(reserved_test_message(
            14,
            b"fresh stream".to_vec(),
            BleWritePriority::Control,
            &messages,
            &bytes,
        ))
        .unwrap();
        assert!(tx.prepare_next().unwrap().is_some());
    }

    #[test]
    fn realtime_injected_during_max_repair_interleaves_and_reassembles_exactly() {
        let messages = Arc::new(AtomicUsize::new(0));
        let bytes = Arc::new(AtomicUsize::new(0));
        let repair = vec![0x52; 1_536];
        let realtime = vec![0x4d; 52];
        let mut tx = DriverTxScheduler::default();
        tx.fresh_connection();
        tx.enqueue(reserved_test_message(
            20,
            repair.clone(),
            BleWritePriority::Repair,
            &messages,
            &bytes,
        ))
        .unwrap();
        let first_repair = tx.prepare_next().unwrap().unwrap();
        assert_eq!(first_repair.priority, BleWritePriority::Repair);
        tx.confirm_pending();

        tx.enqueue(reserved_test_message(
            21,
            realtime.clone(),
            BleWritePriority::Realtime,
            &messages,
            &bytes,
        ))
        .unwrap();
        let mut receiver = tutti_ble::Reassembler::with_budget(
            tutti_ble::ReassemblyBudget::new(HARD_MAX_WIRE_BYTES, 3, HARD_MAX_WIRE_BYTES * 3)
                .unwrap(),
        )
        .unwrap();
        assert!(receiver.push(&first_repair.value).unwrap().is_none());
        let mut completed = Vec::new();
        while !tx.is_empty() {
            let fragment = tx.prepare_next().unwrap().unwrap();
            if let Some(message) = receiver.push(&fragment.value).unwrap() {
                completed.push(message);
            }
            tx.confirm_pending();
        }

        assert!(completed.contains(&realtime));
        assert!(completed.contains(&repair));
        assert_eq!(messages.load(Ordering::Acquire), 0);
        assert_eq!(bytes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn repair_lifecycle_control_cannot_overtake_its_repair_prefix() {
        let messages = Arc::new(AtomicUsize::new(0));
        let bytes = Arc::new(AtomicUsize::new(0));
        let mut tx = DriverTxScheduler::default();
        tx.fresh_connection();
        tx.enqueue(reserved_test_message(
            30,
            vec![0x52; 100],
            BleWritePriority::Repair,
            &messages,
            &bytes,
        ))
        .unwrap();
        // FIN is a Control-lane authenticated payload, but its carrier
        // priority is Repair so this second message stays behind the prefix.
        tx.enqueue(reserved_test_message(
            31,
            vec![0x46; 40],
            BleWritePriority::Repair,
            &messages,
            &bytes,
        ))
        .unwrap();

        let mut ids = Vec::new();
        while !tx.is_empty() {
            let fragment = tx.prepare_next().unwrap().unwrap();
            ids.push(fragment.message_id);
            tx.confirm_pending();
        }
        let fin_start = ids.iter().position(|id| *id == 31).unwrap();
        assert!(ids[..fin_start].iter().all(|id| *id == 30));
        assert!(ids[fin_start..].iter().all(|id| *id == 31));
    }

    #[derive(Default)]
    struct FakeLinkState {
        connected: bool,
        connects: usize,
        disconnects: usize,
    }

    #[derive(Default)]
    struct FakeLink(Mutex<FakeLinkState>);

    impl ConnectPeripheral for FakeLink {
        async fn link_is_connected(&self) -> Result<bool, btleplug::Error> {
            Ok(self.0.lock().unwrap().connected)
        }

        async fn link_connect(&self) -> Result<(), btleplug::Error> {
            let mut state = self.0.lock().unwrap();
            state.connects += 1;
            state.connected = true;
            Ok(())
        }

        async fn link_disconnect(&self) -> Result<(), btleplug::Error> {
            let mut state = self.0.lock().unwrap();
            state.disconnects += 1;
            state.connected = false;
            Ok(())
        }
    }

    #[tokio::test]
    async fn fresh_acquisition_connects_once_without_disconnect_its_own_link() {
        let peripheral = FakeLink::default();

        ensure_connected(&peripheral)
            .await
            .expect("fresh acquisition should connect");

        let state = peripheral.0.lock().unwrap();
        assert!(state.connected);
        assert_eq!(state.connects, 1);
        assert_eq!(state.disconnects, 0);
    }
}
