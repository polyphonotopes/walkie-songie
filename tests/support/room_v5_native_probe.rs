//! Independent native peer for the opt-in browser/native Room-v5 release gate.
//!
//! This is deliberately a consumer of Walkie's public protocol surface, not a
//! second application host. The browser harness drives it over JSON lines while
//! it uses the production native Iroh endpoint, capability-native replicas,
//! live-record admission, and HHHS repair driver.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use hhhs::DagRead;
use hhhs_replica::ReplicaRecord;
use hhhs_store::{MemoryStorage, history_root};
use hhhs_sync::{RepairHost, SessionLimits, SyncMessage};
use serde::Deserialize;
use serde_json::{Value, json};
use walkie_songie::net::{
    IrohSyncStream, NativeNetworkEvent, NativeRoomNetwork, PeerId, RelayPolicy, ReplicaLiveRecord,
    ReplicaRepairHint, ReplicaRepairProbe, ReplicaRoomNetworkConfig, RoomInbound, SyncStream,
    TokioTimer, TransportError, WalkieIdentity, drive_replica_initiator, drive_replica_responder,
    is_routine_repair_initiator, replica_frontier_digest, spawn_rendezvous_v5,
};
use walkie_songie::room::v5::{
    ActorId, ExtensionCommand, ProtocolSupport, RoomCommand, RoomLane, RoomReplicas,
    open_room_authority,
};
use walkie_songie::{TunedDegree, TunedPeriodicPitch, Tuning};

type ProbeResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
type SharedRoom = Arc<RoomReplicas<MemoryStorage, MemoryStorage>>;
type InFlight = Arc<tokio::sync::Mutex<BTreeSet<(iroh::EndpointId, RoomLane)>>>;

#[derive(Default)]
struct Audit {
    music_frames: usize,
    extension_frames: usize,
    violations: Vec<String>,
}

type SharedAudit = Arc<tokio::sync::Mutex<Audit>>;

struct AuditedStream {
    inner: IrohSyncStream,
    lane: RoomLane,
    room: SharedRoom,
    audit: SharedAudit,
}

impl SyncStream for AuditedStream {
    async fn send_frame(&mut self, frame: &[u8]) -> Result<(), TransportError> {
        audit_frame(self.lane, frame, &self.room, &self.audit).await;
        self.inner.send_frame(frame).await
    }

    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        let frame = self.inner.recv_frame().await?;
        if let Some(frame) = frame.as_deref() {
            audit_frame(self.lane, frame, &self.room, &self.audit).await;
        }
        Ok(frame)
    }

    async fn close(self) -> Result<(), TransportError> {
        self.inner.close().await
    }
}

async fn audit_frame(lane: RoomLane, frame: &[u8], room: &SharedRoom, audit: &SharedAudit) {
    let foreign = match lane {
        RoomLane::Music => room.extension_snapshot().history.all_hashes(),
        RoomLane::Extension => room.music_snapshot().history.all_hashes(),
    }
    .into_iter()
    .collect::<BTreeSet<_>>();
    let mut audit = audit.lock().await;
    match lane {
        RoomLane::Music => audit.music_frames += 1,
        RoomLane::Extension => audit.extension_frames += 1,
    }
    let Ok(SyncMessage::Entries { pairs, .. }) = SyncMessage::decode(frame) else {
        return;
    };
    for (claimed, bytes) in pairs {
        if foreign.contains(&claimed) {
            audit.violations.push(format!(
                "{} repair carried foreign entry {}",
                lane_name(lane),
                hex(claimed.as_bytes())
            ));
        }
        match ReplicaRecord::decode(&bytes) {
            Ok(record) if record.entry_hash() == claimed => {}
            Ok(record) => audit.violations.push(format!(
                "{} repair claimed {} for record {}",
                lane_name(lane),
                hex(claimed.as_bytes()),
                hex(record.entry_hash().as_bytes())
            )),
            Err(error) => audit.violations.push(format!(
                "{} repair carried an invalid ReplicaRecord: {error}",
                lane_name(lane)
            )),
        }
    }
}

fn spawn_responder(
    repair: walkie_songie::net::IncomingRepair,
    room: SharedRoom,
    lane: RoomLane,
    audit: SharedAudit,
    partitioned: Arc<AtomicBool>,
) {
    tokio::spawn(async move {
        if partitioned.load(Ordering::SeqCst) {
            repair.connection.close(9u32.into(), b"test partition");
            emit(json!({"event": "repair_dropped", "lane": lane_name(lane)}));
            return;
        }
        let stream = AuditedStream {
            inner: repair.stream.owning(repair.connection),
            lane,
            room: room.clone(),
            audit,
        };
        let mut host = repair_host(&room, lane);
        let result = drive_replica_responder(
            stream,
            &TokioTimer,
            &mut host,
            lane,
            SessionLimits::default(),
        )
        .await;
        emit_repair("responder", lane, result);
    });
}

fn spawn_initiator(
    endpoint: iroh::Endpoint,
    peer: iroh::EndpointId,
    room: SharedRoom,
    lane: RoomLane,
    audit: SharedAudit,
    in_flight: InFlight,
    partitioned: Arc<AtomicBool>,
) {
    tokio::spawn(async move {
        if partitioned.load(Ordering::SeqCst) || !in_flight.lock().await.insert((peer, lane)) {
            return;
        }
        let result = async {
            let connection = endpoint
                .connect(peer, lane.repair_alpn())
                .await
                .map_err(|error| error.to_string())?;
            let stream = IrohSyncStream::open(&connection)
                .await
                .map_err(|error| error.to_string())?
                .owning(connection);
            let stream = AuditedStream {
                inner: stream,
                lane,
                room: room.clone(),
                audit,
            };
            let mut host = repair_host(&room, lane);
            drive_replica_initiator(
                stream,
                &TokioTimer,
                &mut host,
                lane,
                SessionLimits::default(),
            )
            .await
            .map_err(|error| error.to_string())
        }
        .await;
        emit_repair("initiator", lane, result);
        in_flight.lock().await.remove(&(peer, lane));
    });
}

fn repair_host(
    room: &SharedRoom,
    lane: RoomLane,
) -> hhhs_replica::ReplicaRepairHost<MemoryStorage, walkie_songie::room::v5::RoomAdmissionPolicy> {
    match lane {
        RoomLane::Music => room.music_repair_host(),
        RoomLane::Extension => room.extension_repair_host(),
    }
}

fn emit_repair<E>(role: &str, lane: RoomLane, result: Result<hhhs_sync::ConfirmedRepair, E>)
where
    E: std::fmt::Display,
{
    match result {
        Ok(confirmed) => {
            let outcome = confirmed.outcome();
            emit(json!({
                "event": "repair",
                "role": role,
                "lane": lane_name(lane),
                "ok": confirmed.disposition() == hhhs_sync::RepairDisposition::Synchronized,
                "disposition": format!("{:?}", confirmed.disposition()),
                "freshness": format!("{:?}", confirmed.freshness()),
                "incomplete": outcome.incomplete,
                "root_mismatch": outcome.root_mismatch,
                "admitted": outcome.admitted,
                "refused": outcome.refused,
                "frames_sent": outcome.frames_sent,
                "frames_received": outcome.frames_received,
            }))
        }
        Err(error) => emit(json!({
            "event": "repair",
            "role": role,
            "lane": lane_name(lane),
            "ok": false,
            "error": error.to_string(),
        })),
    }
}

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Command {
    Partition {
        enabled: bool,
    },
    Music {
        degree: u16,
        #[serde(default = "yes")]
        broadcast: bool,
    },
    Piece {
        emoji: String,
        degree: u16,
        #[serde(default = "yes")]
        broadcast: bool,
    },
    Status,
    Repair,
    Shutdown,
}

const fn yes() -> bool {
    true
}

fn command_reader() -> tokio::sync::mpsc::UnboundedReceiver<String> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

fn emit(value: Value) {
    let mut stdout = std::io::stdout().lock();
    let _ = serde_json::to_writer(&mut stdout, &value);
    let _ = stdout.write_all(b"\n");
    let _ = stdout.flush();
}

async fn commit_local(
    room: &SharedRoom,
    key: &hhhs_proof::SigningKey,
    endpoint: iroh::EndpointId,
    command: RoomCommand,
    network: &NativeRoomNetwork,
    broadcast: bool,
) -> ProbeResult<()> {
    let capabilities = room.capabilities_for(ActorId::from_signing_key(key));
    let prepared = room.prepare_author(key, &capabilities, command)?;
    let record = prepared.replica_record();
    let receipt = room.commit_prepared(prepared)?;
    if broadcast {
        network
            .broadcast(
                ReplicaLiveRecord {
                    lane: receipt.lane,
                    source: PeerId(*endpoint.as_bytes()),
                    record,
                }
                .encode(),
            )
            .await?;
    }
    emit(json!({
        "event": "committed",
        "lane": lane_name(receipt.lane),
        "entry": hex(receipt.entry.as_bytes()),
        "broadcast": broadcast,
    }));
    Ok(())
}

async fn apply_live(room: &SharedRoom, live: ReplicaLiveRecord) -> ProbeResult<bool> {
    let entry = live.record.entry_hash();
    let mut host = repair_host(room, live.lane);
    let report = RepairHost::apply(&mut host, &[(entry, live.record.encode())]).await?;
    Ok(report.refused.is_empty() && report.admitted.contains(&entry))
}

async fn grant_peer(
    room: &SharedRoom,
    authority: &hhhs_proof::SigningKey,
    peer: iroh::EndpointId,
    network: &NativeRoomNetwork,
) -> ProbeResult<()> {
    let actor = ActorId(*peer.as_bytes());
    let existing = room.capabilities_for(actor);
    if !existing.music.is_empty() && !existing.extension.is_empty() {
        return Ok(());
    }
    let invitation = room.grant_member(authority, actor)?;
    for (lane, entries) in [
        (RoomLane::Music, invitation.capabilities.music),
        (RoomLane::Extension, invitation.capabilities.extension),
    ] {
        for entry in entries {
            network
                .broadcast(
                    ReplicaRepairHint {
                        lane,
                        source: PeerId(*network.endpoint_id().as_bytes()),
                        entry,
                    }
                    .encode(),
                )
                .await?;
        }
    }
    emit(json!({"event": "peer_granted", "peer": peer.to_string()}));
    Ok(())
}

async fn emit_status(room: &SharedRoom, audit: &SharedAudit, partitioned: bool) {
    let view = room.view();
    let music = room.music_snapshot();
    let extension = room.extension_snapshot();
    let degrees = view
        .music
        .live
        .iter()
        .map(|degree| degree.degree.index())
        .collect::<Vec<_>>();
    let pieces = view
        .pieces
        .values()
        .map(|piece| piece.emoji.clone())
        .collect::<Vec<_>>();
    let audit = audit.lock().await;
    emit(json!({
        "event": "status",
        "partitioned": partitioned,
        "degrees": degrees,
        "pieces": pieces,
        "pieces_locked": view.pieces_locked,
        "music_entries": music.history.all_hashes().len(),
        "extension_entries": extension.history.all_hashes().len(),
        "music_root": hex(history_root(&music.history).as_bytes()),
        "extension_root": hex(history_root(&extension.history).as_bytes()),
        "music_frontier": hex(replica_frontier_digest(&music.history.frontier()).as_bytes()),
        "extension_frontier": hex(replica_frontier_digest(&extension.history.frontier()).as_bytes()),
        "music_frames": audit.music_frames,
        "extension_frames": audit.extension_frames,
        "violations": audit.violations,
    }));
}

fn lane_name(lane: RoomLane) -> &'static str {
    match lane {
        RoomLane::Music => "music",
        RoomLane::Extension => "extension",
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(
        String::with_capacity(bytes.len() * 2),
        |mut encoded, byte| {
            let _ = write!(encoded, "{byte:02x}");
            encoded
        },
    )
}

#[tokio::main]
async fn main() -> ProbeResult<()> {
    let room_name = std::env::args()
        .nth(1)
        .ok_or("usage: walkie-room-interop-probe <room-name>")?;
    if !walkie_songie::is_valid_room_name(&room_name) {
        return Err(format!("invalid room name {room_name:?}").into());
    }

    let authority = open_room_authority(&room_name);
    let owner = ActorId::from_signing_key(&authority);
    let seed = blake3::derive_key(
        "walkie-songie Room-v5 native probe identity",
        format!("{room_name}:{}", std::process::id()).as_bytes(),
    );
    let identity = WalkieIdentity::from_seed(seed);
    let signing_key = identity.capability_signing_key();
    let local_actor = identity.capability_actor_id();
    let room = Arc::new(RoomReplicas::memory(&room_name, owner)?);
    room.grant_member(&authority, local_actor)?;

    let mut config = ReplicaRoomNetworkConfig::create(&room_name, owner);
    config.relay = RelayPolicy::Production;
    let mut network = NativeRoomNetwork::bind(identity.iroh_secret(), config).await?;
    let endpoint_id = network.endpoint_id();
    let endpoint = network.endpoint().clone();
    let ticket = network.settle_ticket(Duration::from_secs(10)).await;

    let audit = Arc::new(tokio::sync::Mutex::new(Audit::default()));
    let in_flight = Arc::new(tokio::sync::Mutex::new(BTreeSet::new()));
    let partitioned = Arc::new(AtomicBool::new(false));
    let mut peers = BTreeMap::<iroh::EndpointId, ProtocolSupport>::new();
    let (rendezvous_tx, mut rendezvous_rx) = tokio::sync::mpsc::unbounded_channel();
    let _rendezvous = spawn_rendezvous_v5(
        network.rendezvous_peering(),
        network.topic(),
        ProtocolSupport::WALKIE,
        move |peer, support| {
            let _ = rendezvous_tx.send((peer, support));
        },
    );
    let mut commands = command_reader();

    emit(json!({
        "event": "ready",
        "endpoint": endpoint_id.to_string(),
        "actor": hex(&local_actor.0),
        "room": room_name,
        "ticket": ticket.to_string(),
    }));

    loop {
        tokio::select! {
            discovered = rendezvous_rx.recv() => {
                if let Some((peer, support)) = discovered {
                    peers.insert(peer, support);
                    emit(json!({"event": "discovered", "peer": peer.to_string(), "support": support.bits()}));
                }
            }
            inbound = network.next_inbound() => {
                let Some(inbound) = inbound else { break };
                match inbound {
                    RoomInbound::Repair(repair) => {
                        let Some(protocol) = walkie_songie::net::ReplicaProtocol::from_alpn(repair.alpn) else {
                            repair.connection.close(4u32.into(), b"unsupported Room-v5 ALPN");
                            continue;
                        };
                        spawn_responder(
                            *repair,
                            room.clone(),
                            protocol.lane(),
                            audit.clone(),
                            partitioned.clone(),
                        );
                    }
                    RoomInbound::Event(event) => match event {
                        NativeNetworkEvent::NeighborUp { endpoint_id: peer, .. } => {
                            peers.entry(peer).or_insert(ProtocolSupport::WALKIE);
                            emit(json!({"event": "peer_up", "peer": peer.to_string()}));
                            if let Err(error) = grant_peer(&room, &authority, peer, &network).await {
                                emit(json!({"event": "grant_failed", "peer": peer.to_string(), "error": error.to_string()}));
                            }
                            let local = PeerId(*endpoint_id.as_bytes());
                            let remote = PeerId(*peer.as_bytes());
                            if local < remote {
                                for lane in [RoomLane::Music, RoomLane::Extension] {
                                    spawn_initiator(endpoint.clone(), peer, room.clone(), lane, audit.clone(), in_flight.clone(), partitioned.clone());
                                }
                            }
                        }
                        NativeNetworkEvent::NeighborDown { endpoint_id: peer } => {
                            emit(json!({"event": "peer_down", "peer": peer.to_string()}));
                        }
                        NativeNetworkEvent::Message { bytes, .. } => {
                            if partitioned.load(Ordering::SeqCst) {
                                emit(json!({"event": "message_dropped", "bytes": bytes.len()}));
                            } else if let Some(live) = ReplicaLiveRecord::decode(&bytes) {
                                match apply_live(&room, live).await {
                                    Ok(accepted) => emit(json!({"event": "live_applied", "accepted": accepted})),
                                    Err(error) => emit(json!({"event": "live_failed", "error": error.to_string()})),
                                }
                            }
                        }
                        NativeNetworkEvent::Lagged => emit(json!({"event": "lagged"})),
                        NativeNetworkEvent::Diagnostic(message) => emit(json!({"event": "diagnostic", "message": message})),
                        NativeNetworkEvent::Closed => break,
                        NativeNetworkEvent::MdnsDiscovered { .. } | NativeNetworkEvent::MdnsExpired { .. } => {}
                    }
                }
            }
            command = commands.recv() => {
                let Some(command) = command else { break };
                let command: Command = match serde_json::from_str(&command) {
                    Ok(command) => command,
                    Err(error) => {
                        emit(json!({"event": "command_error", "error": error.to_string()}));
                        continue;
                    }
                };
                match command {
                    Command::Partition { enabled } => {
                        partitioned.store(enabled, Ordering::SeqCst);
                        emit(json!({"event": "partition", "enabled": enabled}));
                    }
                    Command::Music { degree, broadcast } => {
                        let degree = TunedDegree::new(&Tuning::twelve_tet(), degree)?;
                        commit_local(&room, &signing_key, endpoint_id, tutti_music::MusicOp::AddDegree { degree }.into(), &network, broadcast).await?;
                    }
                    Command::Piece { emoji, degree, broadcast } => {
                        let pitch = TunedPeriodicPitch::new(&Tuning::twelve_tet(), degree, 0)?;
                        commit_local(&room, &signing_key, endpoint_id, ExtensionCommand::PutPiece { emoji, pitch }.into(), &network, broadcast).await?;
                    }
                    Command::Status => emit_status(&room, &audit, partitioned.load(Ordering::SeqCst)).await,
                    Command::Repair => {
                        let local = PeerId(*endpoint_id.as_bytes());
                        for (peer, support) in peers.iter().map(|(peer, support)| (*peer, *support)).collect::<Vec<_>>() {
                            let remote = PeerId(*peer.as_bytes());
                            for lane in [RoomLane::Music, RoomLane::Extension] {
                                if !support.supports(lane) {
                                    continue;
                                }
                                if is_routine_repair_initiator(local, remote) {
                                    spawn_initiator(endpoint.clone(), peer, room.clone(), lane, audit.clone(), in_flight.clone(), partitioned.clone());
                                } else {
                                    let snapshot = match lane {
                                        RoomLane::Music => room.music_snapshot(),
                                        RoomLane::Extension => room.extension_snapshot(),
                                    };
                                    network.broadcast(ReplicaRepairProbe {
                                        lane,
                                        source: local,
                                        frontier: replica_frontier_digest(&snapshot.history.frontier()),
                                    }.encode()).await?;
                                    emit(json!({"event": "repair_probe", "lane": lane_name(lane), "peer": peer.to_string()}));
                                }
                            }
                        }
                    }
                    Command::Shutdown => break,
                }
            }
        }
    }

    network.shutdown().await?;
    emit(json!({"event": "shutdown"}));
    Ok(())
}
