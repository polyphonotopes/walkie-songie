#![cfg(feature = "native-net")]

//! Native room-v4 release gate: real local iroh negotiation, concurrent
//! two-lane repair, durable admission, lane isolation, dropped-gossip repair,
//! and journal reopen.

use std::{
    marker::PhantomData,
    sync::{Arc, Mutex},
    time::Duration,
};

use hhhs_sync::sync_session::SyncMessage;
use tutti_core::{OpLanguage, Store, VerifiedOpG, WindowIngest, signing_key_from_seed};
use walkie_songie::net::{
    ExtensionLane, IncomingOp, IrohSyncStream, LaneIngest, LaneProtocol, LaneSpec, LaneStoreAccess,
    LaneSyncSource, MusicLane, NativeRoomNetwork, NativeRoomNetworkConfig, RBSR_ALPN, RelayPolicy,
    RoomInbound, RoomTopic, SyncApply, SyncError, SyncLimits, SyncStream, TokioTimer,
    TransportError, drive_initiator, drive_responder, ingest_pairs,
};
use walkie_songie::room::{
    lane_journal::FileLaneJournal,
    ops::{OpId, SigningKey},
    test_support::{tet_degree, tet_pitch},
    v4::{ExtensionLang, ExtensionOp, LocalRoomOp, MusicLang, MusicOp, Room, RoomLane},
};

const TS: u64 = 1_700_000_000_000_000;

trait TestLane: LaneSpec {
    const LANE: RoomLane;
    fn store(room: &Room) -> &Store<Self::Lang>;
    fn ingest(room: &mut Room, op: VerifiedOpG<Self::Lang>) -> WindowIngest;
}

impl TestLane for MusicLane {
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

impl TestLane for ExtensionLane {
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

struct Durable {
    room: Room,
    journal: FileLaneJournal,
}

type SharedDurable = Arc<tokio::sync::Mutex<Durable>>;

struct Sink<'a, P: TestLane> {
    durable: &'a mut Durable,
    lane: PhantomData<P>,
}

impl<P: TestLane> LaneIngest<P::Lang> for Sink<'_, P> {
    fn lifted_entry(&self, id: OpId) -> Option<hhhs_sync::EntryHash> {
        P::store(&self.durable.room).lifted_entry(id)
    }

    fn knows_op(&self, id: OpId) -> bool {
        P::store(&self.durable.room).knows_op(id)
    }

    fn ingest_lane(
        &mut self,
        wire: &[u8],
        op: VerifiedOpG<P::Lang>,
    ) -> Result<WindowIngest, SyncError> {
        self.durable
            .journal
            .append(P::LANE, wire)
            .map_err(|error| SyncError::Persistence(error.to_string()))?;
        Ok(P::ingest(&mut self.durable.room, op))
    }
}

struct Access<P: TestLane> {
    durable: SharedDurable,
    lane: PhantomData<P>,
}

impl<P: TestLane> Access<P> {
    fn new(durable: SharedDurable) -> Self {
        Self {
            durable,
            lane: PhantomData,
        }
    }
}

impl<P: TestLane> LaneStoreAccess<P::Lang> for Access<P> {
    async fn capture(&mut self, salt: [u8; 16]) -> Result<LaneSyncSource<P::Lang>, SyncError> {
        let durable = self.durable.lock().await;
        Ok(LaneSyncSource::capture(P::store(&durable.room), salt)?)
    }

    async fn apply(
        &mut self,
        topic: &str,
        pairs: &[(hhhs_sync::EntryHash, Vec<u8>)],
        source: &mut LaneSyncSource<P::Lang>,
    ) -> Result<SyncApply, SyncError> {
        let mut durable = self.durable.lock().await;
        let report = {
            let mut sink = Sink::<P> {
                durable: &mut durable,
                lane: PhantomData,
            };
            ingest_pairs::<P::Lang, _>(&mut sink, topic, pairs.iter().map(IncomingOp::from))?
        };
        source.absorb(P::store(&durable.room), &report.lifted)?;
        Ok(SyncApply {
            admitted: report.admitted,
            lifted: report.lifted.len(),
            courier: report.courier,
        })
    }
}

#[derive(Clone)]
struct RecordedFrame {
    from: &'static str,
    protocol: LaneProtocol,
    bytes: Vec<u8>,
}

type Transcript = Arc<Mutex<Vec<RecordedFrame>>>;

struct RecordingStream {
    inner: IrohSyncStream,
    from: &'static str,
    protocol: LaneProtocol,
    transcript: Transcript,
}

impl SyncStream for RecordingStream {
    async fn send_frame(&mut self, frame: &[u8]) -> Result<(), TransportError> {
        self.transcript.lock().unwrap().push(RecordedFrame {
            from: self.from,
            protocol: self.protocol,
            bytes: frame.to_vec(),
        });
        self.inner.send_frame(frame).await
    }

    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        self.inner.recv_frame().await
    }

    async fn close(self) {
        self.inner.close().await;
    }
}

fn append_local(
    durable: &mut Durable,
    key: &SigningKey,
    topic: &str,
    timestamp: u64,
    op: LocalRoomOp,
) {
    let prepared = durable.room.prepare(key, topic, timestamp, op);
    let wire = prepared.to_wire_bytes().unwrap();
    durable.journal.append(prepared.lane(), &wire).unwrap();
    durable.room.ingest_prepared(topic, &prepared).unwrap();
}

async fn accept_repairs(
    network: &mut NativeRoomNetwork,
    count: usize,
) -> Vec<walkie_songie::net::IncomingRepair> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut accepted = Vec::new();
        while accepted.len() < count {
            match network.next_inbound().await {
                Some(RoomInbound::Repair(repair)) => accepted.push(repair),
                Some(RoomInbound::Event(_)) => {}
                None => panic!("native network closed while accepting lane repairs"),
            }
        }
        accepted
    })
    .await
    .expect("lane connections were not accepted")
}

fn recorded(
    inner: IrohSyncStream,
    from: &'static str,
    protocol: LaneProtocol,
    transcript: &Transcript,
) -> RecordingStream {
    RecordingStream {
        inner,
        from,
        protocol,
        transcript: transcript.clone(),
    }
}

async fn run_two_lane_round(
    dialer: &NativeRoomNetwork,
    acceptor: &mut NativeRoomNetwork,
    dialer_room: SharedDurable,
    acceptor_room: SharedDurable,
    topic: &str,
    transcript: &Transcript,
) {
    let endpoint_id = acceptor.endpoint_id();
    let (music_connection, extension_connection) = tokio::join!(
        dialer.begin_lane(endpoint_id, LaneProtocol::Repair(RoomLane::Music)),
        dialer.begin_lane(endpoint_id, LaneProtocol::Repair(RoomLane::Extension)),
    );
    let music_connection = music_connection.expect("music ALPN negotiates");
    let extension_connection = extension_connection.expect("extension ALPN negotiates");
    let music_keepalive = music_connection.clone();
    let extension_keepalive = extension_connection.clone();
    let (music_stream, extension_stream) = tokio::join!(
        IrohSyncStream::open(&music_connection),
        IrohSyncStream::open(&extension_connection),
    );
    let music_stream = music_stream
        .expect("music bi-stream opens")
        .owning(music_connection);
    let extension_stream = extension_stream
        .expect("extension bi-stream opens")
        .owning(extension_connection);

    // QUIC does not expose an opened stream to the acceptor until the first
    // bytes are written, so start both initiators before awaiting the inbound
    // queue. They immediately send their lane Hello frames.
    let music_dialer_room = dialer_room.clone();
    let music_topic = topic.to_owned();
    let music_transcript = transcript.clone();
    let music_task = tokio::spawn(async move {
        let mut access = Access::<MusicLane>::new(music_dialer_room);
        drive_initiator::<MusicLane, _, _, _>(
            recorded(
                music_stream,
                "dialer",
                LaneProtocol::Repair(RoomLane::Music),
                &music_transcript,
            ),
            &TokioTimer,
            &mut access,
            &music_topic,
            SyncLimits::default(),
        )
        .await
    });
    let extension_topic = topic.to_owned();
    let extension_transcript = transcript.clone();
    let extension_task = tokio::spawn(async move {
        let mut access = Access::<ExtensionLane>::new(dialer_room);
        drive_initiator::<ExtensionLane, _, _, _>(
            recorded(
                extension_stream,
                "dialer",
                LaneProtocol::Repair(RoomLane::Extension),
                &extension_transcript,
            ),
            &TokioTimer,
            &mut access,
            &extension_topic,
            SyncLimits::default(),
        )
        .await
    });

    let incoming = accept_repairs(acceptor, 2).await;
    assert_eq!(incoming.len(), 2, "exactly two concurrent lane connections");
    let mut incoming_music = None;
    let mut incoming_extension = None;
    let mut acceptor_keepalives = Vec::with_capacity(2);
    for repair in incoming {
        acceptor_keepalives.push(repair.connection.clone());
        match LaneProtocol::from_alpn(repair.alpn) {
            Some(LaneProtocol::Repair(RoomLane::Music)) => {
                incoming_music = Some(repair.stream.owning(repair.connection));
            }
            Some(LaneProtocol::Repair(RoomLane::Extension)) => {
                incoming_extension = Some(repair.stream.owning(repair.connection));
            }
            other => panic!("unexpected accepted protocol: {other:?}"),
        }
    }

    let mut acceptor_music = Access::<MusicLane>::new(acceptor_room.clone());
    let mut acceptor_extension = Access::<ExtensionLane>::new(acceptor_room);
    let (b, d) = tokio::join!(
        drive_responder::<MusicLane, _, _, _>(
            recorded(
                incoming_music.expect("music responder stream"),
                "acceptor",
                LaneProtocol::Repair(RoomLane::Music),
                transcript,
            ),
            &TokioTimer,
            &mut acceptor_music,
            topic,
            SyncLimits::default(),
        ),
        drive_responder::<ExtensionLane, _, _, _>(
            recorded(
                incoming_extension.expect("extension responder stream"),
                "acceptor",
                LaneProtocol::Repair(RoomLane::Extension),
                transcript,
            ),
            &TokioTimer,
            &mut acceptor_extension,
            topic,
            SyncLimits::default(),
        ),
    );
    let a = music_task.await.expect("music initiator task joins");
    let c = extension_task
        .await
        .expect("extension initiator task joins");
    for outcome in [a, b, c, d] {
        let outcome = outcome.expect("lane repair completes");
        assert!(!outcome.root_mismatch && !outcome.incomplete);
    }
    drop(music_keepalive);
    drop(extension_keepalive);
    drop(acceptor_keepalives);
}

async fn run_music_round(
    dialer: &NativeRoomNetwork,
    acceptor: &mut NativeRoomNetwork,
    dialer_room: SharedDurable,
    acceptor_room: SharedDurable,
    topic: &str,
    transcript: &Transcript,
) {
    let connection = dialer
        .begin_lane(
            acceptor.endpoint_id(),
            LaneProtocol::Repair(RoomLane::Music),
        )
        .await
        .expect("music repair reconnects");
    let keepalive = connection.clone();
    let stream = IrohSyncStream::open(&connection)
        .await
        .expect("music reconnect stream")
        .owning(connection);
    let topic_owned = topic.to_owned();
    let initiator_transcript = transcript.clone();
    let initiator_task = tokio::spawn(async move {
        let mut access = Access::<MusicLane>::new(dialer_room);
        drive_initiator::<MusicLane, _, _, _>(
            recorded(
                stream,
                "dropped-gossip-dialer",
                LaneProtocol::Repair(RoomLane::Music),
                &initiator_transcript,
            ),
            &TokioTimer,
            &mut access,
            &topic_owned,
            SyncLimits::default(),
        )
        .await
    });
    let incoming = accept_repairs(acceptor, 1).await.pop().unwrap();
    assert_eq!(
        LaneProtocol::from_alpn(incoming.alpn),
        Some(LaneProtocol::Repair(RoomLane::Music))
    );
    let acceptor_keepalive = incoming.connection.clone();
    let responder_stream = incoming.stream.owning(incoming.connection);
    let mut acceptor_access = Access::<MusicLane>::new(acceptor_room);
    let responder = drive_responder::<MusicLane, _, _, _>(
        recorded(
            responder_stream,
            "dropped-gossip-acceptor",
            LaneProtocol::Repair(RoomLane::Music),
            transcript,
        ),
        &TokioTimer,
        &mut acceptor_access,
        topic,
        SyncLimits::default(),
    )
    .await;
    let initiator = initiator_task.await.expect("music initiator task joins");
    assert!(
        initiator.is_ok() && responder.is_ok(),
        "initiator={initiator:?}, responder={responder:?}"
    );
    drop(keepalive);
    drop(acceptor_keepalive);
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_room_v4_live_two_lane_release_gate() {
    let topic = RoomTopic::from_room_name_v4("quiet-cactus-song");
    let topic_string = topic.to_string();
    let left_seed = [71; 32];
    let right_seed = [72; 32];

    let test_dir =
        std::env::temp_dir().join(format!("walkie-room-v4-live-{}", rand::random::<u64>()));
    let left_path = test_dir.join("left.v4.ops");
    let right_path = test_dir.join("right.v4.ops");
    let (left_journal, _) = FileLaneJournal::open(&left_path).unwrap();
    let (right_journal, _) = FileLaneJournal::open(&right_path).unwrap();
    let left = Arc::new(tokio::sync::Mutex::new(Durable {
        room: Room::new(),
        journal: left_journal,
    }));
    let right = Arc::new(tokio::sync::Mutex::new(Durable {
        room: Room::new(),
        journal: right_journal,
    }));

    {
        let mut durable = left.lock().await;
        let key = signing_key_from_seed(&left_seed);
        append_local(
            &mut durable,
            &key,
            &topic_string,
            TS,
            MusicOp::AddDegree {
                degree: tet_degree(0),
            }
            .into(),
        );
        append_local(
            &mut durable,
            &key,
            &topic_string,
            TS + 1,
            ExtensionOp::SetConfig {
                pieces_locked: Some(true),
                available_emojis: Some("🌵🎵".into()),
            }
            .into(),
        );
    }
    {
        let mut durable = right.lock().await;
        let key = signing_key_from_seed(&right_seed);
        append_local(
            &mut durable,
            &key,
            &topic_string,
            TS + 2,
            MusicOp::AddDegree {
                degree: tet_degree(7),
            }
            .into(),
        );
        append_local(
            &mut durable,
            &key,
            &topic_string,
            TS + 3,
            ExtensionOp::PutPiece {
                emoji: "🌵".into(),
                pitch: tet_pitch(60),
            }
            .into(),
        );
    }

    let mut left_network = NativeRoomNetwork::bind(
        iroh::SecretKey::from_bytes(&left_seed),
        NativeRoomNetworkConfig {
            topic,
            relay: RelayPolicy::Disabled,
            bootstrap: None,
            bootstrap_lanes: None,
        },
    )
    .await
    .unwrap();
    let right_network = NativeRoomNetwork::bind(
        iroh::SecretKey::from_bytes(&right_seed),
        NativeRoomNetworkConfig {
            topic,
            relay: RelayPolicy::Disabled,
            bootstrap: Some(left_network.ticket().endpoint_addr().clone()),
            bootstrap_lanes: Some(walkie_songie::room::v4::LaneSet::WALKIE),
        },
    )
    .await
    .unwrap();

    assert!(
        right_network
            .endpoint()
            .connect(left_network.endpoint_id(), RBSR_ALPN)
            .await
            .is_err(),
        "the v3 repair ALPN must fail at negotiation"
    );

    let transcript: Transcript = Arc::new(Mutex::new(Vec::new()));
    run_two_lane_round(
        &right_network,
        &mut left_network,
        right.clone(),
        left.clone(),
        &topic_string,
        &transcript,
    )
    .await;

    {
        let left = left.lock().await;
        let right = right.lock().await;
        assert_eq!(
            left.room.music().entry_hashes(),
            right.room.music().entry_hashes()
        );
        assert_eq!(
            left.room.extension().entry_hashes(),
            right.room.extension().entry_hashes()
        );
        assert_eq!(left.room.music().ops_root(), right.room.music().ops_root());
        assert_eq!(
            left.room.extension().ops_root(),
            right.room.extension().ops_root()
        );
        assert_eq!(left.room.view(), right.room.view());
        assert_eq!(left.room.state_root(), right.room.state_root());
    }

    // Commit locally and deliberately skip gossip. The next music-only repair
    // must carry the missing operation.
    {
        let mut durable = left.lock().await;
        append_local(
            &mut durable,
            &signing_key_from_seed(&left_seed),
            &topic_string,
            TS + 4,
            MusicOp::AddDegree {
                degree: tet_degree(11),
            }
            .into(),
        );
    }
    assert_ne!(
        left.lock().await.room.music().entry_hashes(),
        right.lock().await.room.music().entry_hashes(),
        "the dropped gossip op creates a real gap"
    );
    run_music_round(
        &right_network,
        &mut left_network,
        right.clone(),
        left.clone(),
        &topic_string,
        &transcript,
    )
    .await;

    let (music_hashes, extension_hashes) = {
        let room = left.lock().await;
        (
            room.room.music().entry_hashes(),
            room.room.extension().entry_hashes(),
        )
    };
    for frame in transcript.lock().unwrap().iter() {
        let message = SyncMessage::decode(&frame.bytes).expect("every recorded frame is RBSR");
        let (expected_magic, forbidden_magic, forbidden_hashes) = match frame.protocol {
            LaneProtocol::Repair(RoomLane::Music) => (
                MusicLang::WIRE_MAGIC,
                ExtensionLang::WIRE_MAGIC,
                &extension_hashes,
            ),
            LaneProtocol::Repair(RoomLane::Extension) => (
                ExtensionLang::WIRE_MAGIC,
                MusicLang::WIRE_MAGIC,
                &music_hashes,
            ),
            other => panic!("unexpected transcript protocol {other:?}"),
        };
        assert!(!contains_subslice(&frame.bytes, forbidden_magic));
        for hash in forbidden_hashes {
            assert!(
                !contains_subslice(&frame.bytes, hash.as_bytes()),
                "{} {:?} frame leaked a foreign-lane hash {}",
                frame.from,
                frame.protocol,
                hash.to_hex(),
            );
        }
        if let SyncMessage::Entries { pairs, .. } = message {
            for (_, wire) in pairs {
                assert!(wire.starts_with(expected_magic));
                assert!(!wire.starts_with(forbidden_magic));
            }
        }
    }

    assert_eq!(
        left.lock().await.room.music().entry_hashes(),
        right.lock().await.room.music().entry_hashes(),
        "repair recovers the dropped gossip op"
    );

    right_network.shutdown().await.unwrap();
    left_network.shutdown().await.unwrap();
    drop(left);
    drop(right);

    let (_, left_records) = FileLaneJournal::open(&left_path).unwrap();
    let (_, right_records) = FileLaneJournal::open(&right_path).unwrap();
    let left_reopened = Room::recover(&topic_string, &left_records).unwrap();
    let right_reopened = Room::recover(&topic_string, &right_records).unwrap();
    assert_eq!(left_reopened.music().pending_len(), 0);
    assert_eq!(left_reopened.extension().pending_len(), 0);
    assert_eq!(right_reopened.music().pending_len(), 0);
    assert_eq!(right_reopened.extension().pending_len(), 0);
    assert_eq!(left_reopened.view(), right_reopened.view());
    assert_eq!(left_reopened.state_root(), right_reopened.state_root());

    let _ = std::fs::remove_dir_all(test_dir);
}
