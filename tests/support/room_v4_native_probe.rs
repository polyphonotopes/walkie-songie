//! Opt-in browser/native Room-v4 release-test peer.
//!
//! This is deliberately a protocol peer, not a second app implementation. The
//! browser release gate drives it over JSON lines on stdin/stdout while it uses
//! the production native endpoint, v4 rendezvous, lane drivers, and admission
//! path. It exists as a Cargo binary so the JavaScript CDP harness can exercise
//! a real native peer without automating a desktop window.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, Write},
    marker::PhantomData,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use hhhs_sync::{EntryHash, sync_session::SyncMessage};
use serde::Deserialize;
use serde_json::{Value, json};
use tutti_core::{OpLanguage, SignedOp, Store, VerifiedOpG, WindowIngest, signing_key_from_seed};
use walkie_songie::net::{
    ExtensionLane, IncomingOp, IncomingRepair, IrohSyncStream, LaneIngest, LaneProtocol, LaneSpec,
    LaneStoreAccess, LaneSyncSource, MusicLane, NativeNetworkEvent, NativeRoomNetwork,
    NativeRoomNetworkConfig, RelayPolicy, RoomInbound, RoomTopic, SyncApply, SyncError, SyncLimits,
    SyncStream, TokioTimer, TransportError, drive_initiator, drive_responder, ingest_pairs,
    spawn_rendezvous_v4,
};
use walkie_songie::room::{
    ops::{OpId, SigningKey},
    v4::{ExtensionLang, ExtensionOp, LaneSet, LocalRoomOp, MusicLang, MusicOp, Room, RoomLane},
};
use walkie_songie::{TunedDegree, TunedPeriodicPitch, Tuning};

trait ProbeLane: LaneSpec {
    const LANE: RoomLane;
    fn store(room: &Room) -> &Store<Self::Lang>;
    fn ingest(room: &mut Room, op: VerifiedOpG<Self::Lang>) -> WindowIngest;
}

impl ProbeLane for MusicLane {
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

impl ProbeLane for ExtensionLane {
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

type SharedRoom = Arc<tokio::sync::Mutex<Room>>;

struct Sink<'a, P: ProbeLane> {
    room: &'a mut Room,
    lane: PhantomData<P>,
}

impl<P: ProbeLane> LaneIngest<P::Lang> for Sink<'_, P> {
    fn lifted_entry(&self, id: OpId) -> Option<EntryHash> {
        P::store(self.room).lifted_entry(id)
    }

    fn knows_op(&self, id: OpId) -> bool {
        P::store(self.room).knows_op(id)
    }

    fn ingest_lane(
        &mut self,
        _wire: &[u8],
        op: VerifiedOpG<P::Lang>,
    ) -> Result<WindowIngest, SyncError> {
        Ok(P::ingest(self.room, op))
    }
}

struct Access<P: ProbeLane> {
    room: SharedRoom,
    lane: PhantomData<P>,
}

impl<P: ProbeLane> Access<P> {
    fn new(room: SharedRoom) -> Self {
        Self {
            room,
            lane: PhantomData,
        }
    }
}

impl<P: ProbeLane> LaneStoreAccess<P::Lang> for Access<P> {
    async fn capture(&mut self, salt: [u8; 16]) -> Result<LaneSyncSource<P::Lang>, SyncError> {
        let room = self.room.lock().await;
        Ok(LaneSyncSource::capture(P::store(&room), salt)?)
    }

    async fn apply(
        &mut self,
        topic: &str,
        pairs: &[(EntryHash, Vec<u8>)],
        source: &mut LaneSyncSource<P::Lang>,
    ) -> Result<SyncApply, SyncError> {
        let mut room = self.room.lock().await;
        let report = {
            let mut sink = Sink::<P> {
                room: &mut room,
                lane: PhantomData,
            };
            ingest_pairs::<P::Lang, _>(&mut sink, topic, pairs.iter().map(IncomingOp::from))?
        };
        source.absorb(P::store(&room), &report.lifted)?;
        Ok(SyncApply {
            admitted: report.admitted,
            lifted: report.lifted.len(),
            courier: report.courier,
        })
    }
}

#[derive(Default)]
struct Audit {
    music_frames: usize,
    extension_frames: usize,
    violations: Vec<String>,
}

type SharedAudit = Arc<tokio::sync::Mutex<Audit>>;

async fn audit_frame(protocol: LaneProtocol, bytes: &[u8], room: &SharedRoom, audit: &SharedAudit) {
    let lane = protocol.lane();
    let (expected_magic, forbidden_magic, foreign_hashes) = {
        let room = room.lock().await;
        match lane {
            RoomLane::Music => (
                MusicLang::WIRE_MAGIC,
                ExtensionLang::WIRE_MAGIC,
                room.extension().entry_hashes(),
            ),
            RoomLane::Extension => (
                ExtensionLang::WIRE_MAGIC,
                MusicLang::WIRE_MAGIC,
                room.music().entry_hashes(),
            ),
        }
    };
    let mut audit = audit.lock().await;
    match lane {
        RoomLane::Music => audit.music_frames += 1,
        RoomLane::Extension => audit.extension_frames += 1,
    }
    if contains(bytes, forbidden_magic) {
        audit
            .violations
            .push(format!("{protocol:?} frame contained foreign wire magic"));
    }
    if let Some(hash) = foreign_hashes
        .iter()
        .find(|hash| contains(bytes, hash.as_bytes()))
    {
        audit.violations.push(format!(
            "{protocol:?} frame contained foreign entry hash {}",
            hash.to_hex()
        ));
    }
    if let Ok(SyncMessage::Entries { pairs, .. }) = SyncMessage::decode(bytes) {
        for (_, wire) in pairs {
            if !wire.starts_with(expected_magic) {
                audit
                    .violations
                    .push(format!("{protocol:?} delivered a foreign-lane entry"));
            }
        }
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

struct AuditedStream {
    inner: IrohSyncStream,
    protocol: LaneProtocol,
    room: SharedRoom,
    audit: SharedAudit,
}

impl SyncStream for AuditedStream {
    async fn send_frame(&mut self, frame: &[u8]) -> Result<(), TransportError> {
        audit_frame(self.protocol, frame, &self.room, &self.audit).await;
        self.inner.send_frame(frame).await
    }

    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        let frame = self.inner.recv_frame().await?;
        if let Some(frame) = frame.as_deref() {
            audit_frame(self.protocol, frame, &self.room, &self.audit).await;
        }
        Ok(frame)
    }

    async fn close(self) {
        self.inner.close().await;
    }
}

#[derive(Clone)]
struct NetworkHandle {
    endpoint: iroh::Endpoint,
}

impl NetworkHandle {
    async fn begin_lane(
        &self,
        endpoint_id: iroh::EndpointId,
        protocol: LaneProtocol,
    ) -> Result<iroh::endpoint::Connection> {
        self.endpoint
            .connect(endpoint_id, protocol.alpn())
            .await
            .map_err(Into::into)
    }
}

type InFlight = Arc<tokio::sync::Mutex<BTreeSet<iroh::EndpointId>>>;

fn spawn_responder<P: ProbeLane>(
    repair: IncomingRepair,
    room: SharedRoom,
    topic: String,
    audit: SharedAudit,
) {
    tokio::spawn(async move {
        let protocol = LaneProtocol::Repair(P::LANE);
        let stream = AuditedStream {
            inner: repair.stream.owning(repair.connection),
            protocol,
            room: room.clone(),
            audit,
        };
        let mut access = Access::<P>::new(room);
        let result = drive_responder::<P, _, _, _>(
            stream,
            &TokioTimer,
            &mut access,
            &topic,
            SyncLimits::default(),
        )
        .await;
        emit(json!({
            "event": "repair",
            "role": "responder",
            "lane": lane_name(P::LANE),
            "ok": result.is_ok(),
            "error": result.err().map(|error| error.to_string()),
        }));
    });
}

fn spawn_round(
    endpoint_id: iroh::EndpointId,
    advertised: Option<LaneSet>,
    handle: NetworkHandle,
    room: SharedRoom,
    topic: String,
    audit: SharedAudit,
    in_flight: InFlight,
) {
    tokio::spawn(async move {
        if !in_flight.lock().await.insert(endpoint_id) {
            return;
        }
        let attempted = advertised.unwrap_or(LaneSet::WALKIE);
        let music = run_initiator::<MusicLane>(
            attempted,
            endpoint_id,
            handle.clone(),
            room.clone(),
            topic.clone(),
            audit.clone(),
        );
        let extension =
            run_initiator::<ExtensionLane>(attempted, endpoint_id, handle, room, topic, audit);
        let (music, extension) = tokio::join!(music, extension);
        emit(json!({
            "event": "repair_round",
            "peer": endpoint_id.to_string(),
            "music": music,
            "extension": extension,
        }));
        in_flight.lock().await.remove(&endpoint_id);
    });
}

async fn run_initiator<P: ProbeLane>(
    attempted: LaneSet,
    endpoint_id: iroh::EndpointId,
    handle: NetworkHandle,
    room: SharedRoom,
    topic: String,
    audit: SharedAudit,
) -> Option<bool> {
    if !attempted.contains(P::LANE) {
        return None;
    }
    let protocol = LaneProtocol::Repair(P::LANE);
    let connection = match handle.begin_lane(endpoint_id, protocol).await {
        Ok(connection) => connection,
        Err(error) => {
            emit(json!({
                "event": "repair_connect_failed",
                "lane": lane_name(P::LANE),
                "error": error.to_string(),
            }));
            return Some(false);
        }
    };
    let stream = match IrohSyncStream::open(&connection).await {
        Ok(stream) => stream.owning(connection),
        Err(error) => {
            emit(json!({
                "event": "repair_stream_failed",
                "lane": lane_name(P::LANE),
                "error": error.to_string(),
            }));
            return Some(false);
        }
    };
    let stream = AuditedStream {
        inner: stream,
        protocol,
        room: room.clone(),
        audit,
    };
    let mut access = Access::<P>::new(room);
    Some(
        drive_initiator::<P, _, _, _>(
            stream,
            &TokioTimer,
            &mut access,
            &topic,
            SyncLimits::default(),
        )
        .await
        .is_ok(),
    )
}

fn lane_name(lane: RoomLane) -> &'static str {
    match lane {
        RoomLane::Music => "music",
        RoomLane::Extension => "extension",
    }
}

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Command {
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
            match line {
                Ok(line) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
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

fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        .try_into()
        .unwrap_or(u64::MAX)
}

async fn commit_local(
    room: &SharedRoom,
    key: &SigningKey,
    topic: &str,
    op: LocalRoomOp,
) -> Result<Vec<u8>> {
    let mut room = room.lock().await;
    let prepared = room.prepare(key, topic, now_micros(), op);
    let wire = prepared.to_wire_bytes()?;
    room.ingest_prepared(topic, &prepared)?;
    Ok(wire)
}

async fn apply_gossip(room: &SharedRoom, topic: &str, bytes: &[u8]) -> Result<()> {
    let mut room = room.lock().await;
    if bytes.starts_with(MusicLang::WIRE_MAGIC) {
        let signed = SignedOp::from_wire_bytes_in::<MusicLang>(bytes)?;
        let verified = walkie_songie::room::v4::verify_music_op(&signed, topic)?;
        room.ingest_music(verified);
    } else if bytes.starts_with(ExtensionLang::WIRE_MAGIC) {
        let signed = SignedOp::from_wire_bytes_in::<ExtensionLang>(bytes)?;
        let verified = walkie_songie::room::v4::verify_extension_op(&signed, topic)?;
        room.ingest_extension(verified);
    }
    Ok(())
}

async fn emit_status(room: &SharedRoom, audit: &SharedAudit) {
    let room = room.lock().await;
    let view = room.view();
    let degrees = view
        .pitches
        .iter()
        .map(|degree| degree.degree.index())
        .collect::<Vec<_>>();
    let pieces = view
        .pieces
        .values()
        .map(|piece| piece.emoji.clone())
        .collect::<Vec<_>>();
    let music_entries = room.music().len();
    let extension_entries = room.extension().len();
    drop(room);
    let audit = audit.lock().await;
    emit(json!({
        "event": "status",
        "degrees": degrees,
        "pieces": pieces,
        "pieces_locked": view.pieces_locked,
        "music_entries": music_entries,
        "extension_entries": extension_entries,
        "music_frames": audit.music_frames,
        "extension_frames": audit.extension_frames,
        "violations": audit.violations,
    }));
}

#[tokio::main]
async fn main() -> Result<()> {
    let room_name = std::env::args()
        .nth(1)
        .context("usage: room-v4-native-probe <room-name>")?;
    if !walkie_songie::is_valid_room_name(&room_name) {
        bail!("invalid room name {room_name:?}");
    }
    let topic = RoomTopic::from_room_name_v4(&room_name);
    let topic_string = topic.to_string();
    let seed = blake3::derive_key(
        "walkie-songie room-v4 native probe seed",
        format!("{room_name}:{}", std::process::id()).as_bytes(),
    );
    let signing_key = signing_key_from_seed(&seed);
    let mut network = NativeRoomNetwork::bind(
        iroh::SecretKey::from_bytes(&seed),
        NativeRoomNetworkConfig {
            topic,
            relay: RelayPolicy::Production,
            bootstrap: None,
            bootstrap_lanes: None,
        },
    )
    .await
    .context("bind native Room-v4 endpoint")?;
    let own_endpoint = network.endpoint_id();
    let handle = NetworkHandle {
        endpoint: network.endpoint().clone(),
    };
    let ticket = network.settle_ticket(Duration::from_secs(10)).await;

    let room = Arc::new(tokio::sync::Mutex::new(Room::new()));
    let audit = Arc::new(tokio::sync::Mutex::new(Audit::default()));
    let in_flight = Arc::new(tokio::sync::Mutex::new(BTreeSet::new()));
    let mut peers = BTreeSet::new();
    let mut peer_lanes = BTreeMap::new();
    let (rdv_tx, mut rdv_rx) = tokio::sync::mpsc::unbounded_channel();
    let _rendezvous = spawn_rendezvous_v4(
        network.rendezvous_peering(),
        topic,
        LaneSet::WALKIE,
        move |endpoint_id, lanes| {
            let _ = rdv_tx.send((endpoint_id, lanes));
        },
    );
    let mut commands = command_reader();
    let repair_period = Duration::from_secs(25 + u64::from(own_endpoint.as_bytes()[0] % 11));
    let mut repair_tick =
        tokio::time::interval_at(tokio::time::Instant::now() + repair_period, repair_period);
    repair_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    emit(json!({
        "event": "ready",
        "endpoint": own_endpoint.to_string(),
        "room": room_name,
        "ticket": ticket.to_string(),
    }));

    loop {
        tokio::select! {
            discovered = rdv_rx.recv() => {
                if let Some((endpoint_id, lanes)) = discovered {
                    peer_lanes.insert(endpoint_id, lanes);
                    emit(json!({
                        "event": "discovered",
                        "peer": endpoint_id.to_string(),
                        "lanes": lanes.bits(),
                    }));
                }
            }
            inbound = network.next_inbound() => {
                let Some(inbound) = inbound else { break };
                match inbound {
                    RoomInbound::Repair(repair) => {
                        match LaneProtocol::from_alpn(repair.alpn) {
                            Some(LaneProtocol::Repair(RoomLane::Music)) => spawn_responder::<MusicLane>(
                                repair,
                                room.clone(),
                                topic_string.clone(),
                                audit.clone(),
                            ),
                            Some(LaneProtocol::Repair(RoomLane::Extension)) => spawn_responder::<ExtensionLane>(
                                repair,
                                room.clone(),
                                topic_string.clone(),
                                audit.clone(),
                            ),
                            _ => repair.connection.close(4u32.into(), b"probe does not need courier"),
                        }
                    }
                    RoomInbound::Event(event) => match event {
                        NativeNetworkEvent::NeighborUp { endpoint_id, .. } => {
                            peers.insert(endpoint_id);
                            emit(json!({"event": "peer_up", "peer": endpoint_id.to_string()}));
                            if own_endpoint.as_bytes() < endpoint_id.as_bytes() {
                                spawn_round(
                                    endpoint_id,
                                    peer_lanes.get(&endpoint_id).copied(),
                                    handle.clone(),
                                    room.clone(),
                                    topic_string.clone(),
                                    audit.clone(),
                                    in_flight.clone(),
                                );
                            }
                        }
                        NativeNetworkEvent::NeighborDown { endpoint_id } => {
                            peers.remove(&endpoint_id);
                            emit(json!({"event": "peer_down", "peer": endpoint_id.to_string()}));
                        }
                        NativeNetworkEvent::Message { bytes, .. } => {
                            if let Err(error) = apply_gossip(&room, &topic_string, &bytes).await {
                                emit(json!({"event": "gossip_rejected", "error": error.to_string()}));
                            }
                        }
                        NativeNetworkEvent::Lagged => {
                            for endpoint_id in peers.iter().copied().collect::<Vec<_>>() {
                                if own_endpoint.as_bytes() < endpoint_id.as_bytes() {
                                    spawn_round(
                                        endpoint_id,
                                        peer_lanes.get(&endpoint_id).copied(),
                                        handle.clone(),
                                        room.clone(),
                                        topic_string.clone(),
                                        audit.clone(),
                                        in_flight.clone(),
                                    );
                                }
                            }
                        }
                        NativeNetworkEvent::Closed => break,
                        NativeNetworkEvent::Diagnostic(message) => {
                            emit(json!({"event": "diagnostic", "message": message}));
                        }
                        NativeNetworkEvent::MdnsDiscovered { .. }
                        | NativeNetworkEvent::MdnsExpired { .. } => {}
                    }
                }
            }
            _ = repair_tick.tick() => {
                for endpoint_id in peers.iter().copied().collect::<Vec<_>>() {
                    if own_endpoint.as_bytes() < endpoint_id.as_bytes() {
                        spawn_round(
                            endpoint_id,
                            peer_lanes.get(&endpoint_id).copied(),
                            handle.clone(),
                            room.clone(),
                            topic_string.clone(),
                            audit.clone(),
                            in_flight.clone(),
                        );
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
                    Command::Music { degree, broadcast } => {
                        let tuning = Tuning::twelve_tet();
                        let degree = TunedDegree::new(&tuning, degree)?;
                        let wire = commit_local(
                            &room,
                            &signing_key,
                            &topic_string,
                            MusicOp::AddDegree { degree }.into(),
                        ).await?;
                        if broadcast {
                            network.broadcast(wire).await?;
                        }
                        emit(json!({"event": "committed", "lane": "music", "broadcast": broadcast}));
                    }
                    Command::Piece { emoji, degree, broadcast } => {
                        let tuning = Tuning::twelve_tet();
                        let pitch = TunedPeriodicPitch::new(&tuning, degree, 0)?;
                        let wire = commit_local(
                            &room,
                            &signing_key,
                            &topic_string,
                            ExtensionOp::PutPiece { emoji, pitch }.into(),
                        ).await?;
                        if broadcast {
                            network.broadcast(wire).await?;
                        }
                        emit(json!({"event": "committed", "lane": "extension", "broadcast": broadcast}));
                    }
                    Command::Status => emit_status(&room, &audit).await,
                    Command::Repair => {
                        for endpoint_id in peers.iter().copied().collect::<Vec<_>>() {
                            if own_endpoint.as_bytes() < endpoint_id.as_bytes() {
                                spawn_round(
                                    endpoint_id,
                                    peer_lanes.get(&endpoint_id).copied(),
                                    handle.clone(),
                                    room.clone(),
                                    topic_string.clone(),
                                    audit.clone(),
                                    in_flight.clone(),
                                );
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
