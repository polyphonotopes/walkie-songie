//! Room-v5 music interoperability gate.
//!
//! A package with no Walkie dependency runs the upstream tutti-music HHHS
//! Replica policy against Walkie's production repair adapter. The full room's
//! extension Replica is live and changes during repair, yet no extension entry
//! can cross the music ALPN.

use std::sync::{Arc, Mutex};

use bare_music_peer::BareMusicPeer;
use futures::{
    StreamExt,
    channel::{mpsc, oneshot},
    future::join3,
};
use hhhs::DagRead;
use hhhs_cap::decode_op as decode_capability;
use hhhs_proof::SigningKey;
use hhhs_replica::decode_replica_record;
use hhhs_sync::{FrameStream, SessionLimits, SyncMessage, SyncTimer as HhhsTimer};
use tutti_music::{MusicOp, TunedDegree, TunedPeriodicPitch, Tuning};
use walkie_songie::{
    net::{SyncStream, SyncTimer, TransportError, drive_replica_responder},
    room::v5::{ActorId, EXTENSION_REPAIR_ALPN, ExtensionCommand, PieceId, RoomLane, RoomReplicas},
};

const OWNER_SEED: [u8; 32] = [1; 32];
const BARE_SEED: [u8; 32] = [7; 32];

#[derive(Debug)]
struct TestIoError(String);

impl std::fmt::Display for TestIoError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for TestIoError {}

#[derive(Clone, Debug)]
struct RecordedFrame {
    from: &'static str,
    alpn: &'static [u8],
    bytes: Vec<u8>,
}

type Transcript = Arc<Mutex<Vec<RecordedFrame>>>;
type Barrier = Arc<Mutex<Option<oneshot::Sender<()>>>>;

struct RecordingEnd {
    from: &'static str,
    alpn: &'static [u8],
    send: mpsc::UnboundedSender<Vec<u8>>,
    receive: mpsc::UnboundedReceiver<Vec<u8>>,
    transcript: Transcript,
    first_frame: Barrier,
}

impl RecordingEnd {
    fn send_recorded(&mut self, bytes: &[u8]) -> Result<(), TestIoError> {
        self.transcript.lock().unwrap().push(RecordedFrame {
            from: self.from,
            alpn: self.alpn,
            bytes: bytes.to_vec(),
        });
        if let Some(barrier) = self.first_frame.lock().unwrap().take() {
            let _ = barrier.send(());
        }
        self.send
            .unbounded_send(bytes.to_vec())
            .map_err(|error| TestIoError(error.to_string()))
    }
}

impl FrameStream for RecordingEnd {
    type Error = TestIoError;

    async fn send_frame(&mut self, frame: &[u8]) -> Result<(), Self::Error> {
        self.send_recorded(frame)
    }

    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, Self::Error> {
        Ok(self.receive.next().await)
    }

    async fn close(self) {}
}

impl SyncStream for RecordingEnd {
    async fn send_frame(&mut self, frame: &[u8]) -> Result<(), TransportError> {
        self.send_recorded(frame)
            .map_err(|error| TransportError::Backend(error.to_string()))
    }

    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        Ok(self.receive.next().await)
    }

    async fn close(self) {}
}

struct NeverTimer;

impl HhhsTimer for NeverTimer {
    async fn sleep(&self, _duration: std::time::Duration) {
        futures::future::pending().await
    }
}

impl SyncTimer for NeverTimer {
    async fn sleep(&self, _duration: std::time::Duration) {
        futures::future::pending().await
    }
}

struct Endpoint {
    name: &'static str,
    alpns: &'static [&'static [u8]],
}

fn connect(
    dialer: &Endpoint,
    acceptor: &Endpoint,
    alpn: &'static [u8],
    transcript: &Transcript,
    first_frame: Barrier,
) -> Result<(RecordingEnd, RecordingEnd), String> {
    for endpoint in [dialer, acceptor] {
        if !endpoint.alpns.contains(&alpn) {
            return Err(format!(
                "{} does not support {}",
                endpoint.name,
                String::from_utf8_lossy(alpn)
            ));
        }
    }
    let (to_acceptor, from_dialer) = mpsc::unbounded();
    let (to_dialer, from_acceptor) = mpsc::unbounded();
    Ok((
        RecordingEnd {
            from: dialer.name,
            alpn,
            send: to_acceptor,
            receive: from_acceptor,
            transcript: transcript.clone(),
            first_frame: first_frame.clone(),
        },
        RecordingEnd {
            from: acceptor.name,
            alpn,
            send: to_dialer,
            receive: from_dialer,
            transcript: transcript.clone(),
            first_frame,
        },
    ))
}

fn degree(index: u16) -> TunedDegree {
    TunedDegree::new(&Tuning::twelve_tet(), index).unwrap()
}

fn pitch(index: u16) -> TunedPeriodicPitch {
    TunedPeriodicPitch::new(&Tuning::twelve_tet(), index, 0).unwrap()
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[test]
fn bare_music_replica_converges_without_receiving_extension_state() {
    let owner_key = SigningKey::from_bytes(&OWNER_SEED);
    let owner = ActorId::from_signing_key(&owner_key);
    let room = RoomReplicas::memory("sunny-garden-melody", owner).unwrap();
    let identity = room.identity().clone();
    let bare = BareMusicPeer::new(identity.music, owner, BARE_SEED).unwrap();
    let invitation = room.grant_member(&owner_key, bare.actor()).unwrap();

    for index in 0..12 {
        room.author(
            &owner_key,
            &room.owner_capabilities(),
            MusicOp::AddDegree {
                degree: degree(index),
            }
            .into(),
        )
        .unwrap();
    }
    let put = room
        .author(
            &owner_key,
            &room.owner_capabilities(),
            ExtensionCommand::PutPiece {
                emoji: "🌵".into(),
                pitch: pitch(4),
            }
            .into(),
        )
        .unwrap();
    let piece = PieceId::from_entry(put.entry);
    let extension_before = room.extension_snapshot().history.all_hashes().len();

    let walkie_endpoint = Endpoint {
        name: "walkie",
        alpns: &[tutti_music_hhhs::REPAIR_ALPN, EXTENSION_REPAIR_ALPN],
    };
    let bare_endpoint = Endpoint {
        name: "bare",
        alpns: &[tutti_music_hhhs::REPAIR_ALPN],
    };
    let transcript = Arc::new(Mutex::new(Vec::new()));

    assert!(
        connect(
            &bare_endpoint,
            &walkie_endpoint,
            EXTENSION_REPAIR_ALPN,
            &transcript,
            Arc::new(Mutex::new(None)),
        )
        .is_err(),
        "a music-only peer must refuse the extension ALPN before bytes flow"
    );

    let (bare_end, walkie_end) = connect(
        &bare_endpoint,
        &walkie_endpoint,
        tutti_music_hhhs::REPAIR_ALPN,
        &transcript,
        Arc::new(Mutex::new(None)),
    )
    .unwrap();
    let mut responder = room.music_repair_host();
    let (bare_first, walkie_first) = futures::executor::block_on(futures::future::join(
        bare.drive_music_initiator(bare_end, &NeverTimer, SessionLimits::default()),
        drive_replica_responder(
            walkie_end,
            &NeverTimer,
            &mut responder,
            RoomLane::Music,
            SessionLimits::default(),
        ),
    ));
    let bare_first = bare_first.unwrap();
    let walkie_first = walkie_first.unwrap();
    assert_eq!(bare_first.refused + walkie_first.refused, 0);

    let bare_entry = bare
        .author(
            invitation.capabilities.music.clone(),
            MusicOp::RemoveDegree { degree: degree(2) },
        )
        .unwrap();
    room.author(
        &owner_key,
        &room.owner_capabilities(),
        MusicOp::SetEnvelope {
            degree: degree(7),
            env: tutti_music::Envelope {
                points: vec![(10, 127), (80, 0)],
                interp: tutti_music::Interp::Linear,
            },
        }
        .into(),
    )
    .unwrap();

    let (barrier_tx, barrier_rx) = oneshot::channel();
    let (bare_end, walkie_end) = connect(
        &bare_endpoint,
        &walkie_endpoint,
        tutti_music_hhhs::REPAIR_ALPN,
        &transcript,
        Arc::new(Mutex::new(Some(barrier_tx))),
    )
    .unwrap();
    let mut responder = room.music_repair_host();
    let (bare_outcome, walkie_outcome, ()) = futures::executor::block_on(join3(
        bare.drive_music_initiator(bare_end, &NeverTimer, SessionLimits::default()),
        drive_replica_responder(
            walkie_end,
            &NeverTimer,
            &mut responder,
            RoomLane::Music,
            SessionLimits::default(),
        ),
        async {
            barrier_rx.await.unwrap();
            room.author(
                &owner_key,
                &room.owner_capabilities(),
                ExtensionCommand::MovePiece {
                    piece,
                    pitch: pitch(8),
                }
                .into(),
            )
            .unwrap();
            room.author(
                &owner_key,
                &room.owner_capabilities(),
                ExtensionCommand::SetConfig {
                    pieces_locked: Some(true),
                    available_emojis: Some("🌵🎵".into()),
                }
                .into(),
            )
            .unwrap();
        },
    ));
    let bare_outcome = bare_outcome.unwrap();
    let walkie_outcome = walkie_outcome.unwrap();
    for outcome in [&bare_outcome, &walkie_outcome] {
        assert!(!outcome.incomplete);
        assert!(!outcome.root_mismatch);
        assert_eq!(outcome.refused, 0);
    }

    let bare_hashes: std::collections::BTreeSet<_> = bare.entry_hashes().into_iter().collect();
    let walkie_hashes: std::collections::BTreeSet<_> = room
        .music_snapshot()
        .history
        .all_hashes()
        .into_iter()
        .collect();
    assert_eq!(bare_hashes, walkie_hashes);
    assert!(walkie_hashes.contains(&bare_entry));
    let bare_view = bare.view();
    let walkie_view = room.view().music;
    assert_eq!(bare_view.live, walkie_view.live);
    assert_eq!(bare_view.holders, walkie_view.holders);
    assert_eq!(bare_view.envelopes, walkie_view.envelopes);
    assert_eq!(bare_view.tuning, walkie_view.tuning);
    assert!(room.extension_snapshot().history.all_hashes().len() > extension_before);
    assert!(room.view().pieces_locked);

    let extension_hashes = room.extension_snapshot().history.all_hashes();
    let transcript = transcript.lock().unwrap();
    assert!(!transcript.is_empty());
    let mut delivered = 0;
    for frame in transcript.iter() {
        assert_eq!(frame.alpn, tutti_music_hhhs::REPAIR_ALPN);
        for extension in &extension_hashes {
            assert!(
                !contains_subslice(&frame.bytes, extension.as_bytes()),
                "{} leaked extension hash {extension:?}",
                frame.from
            );
        }
        if let SyncMessage::Entries { pairs, .. } = SyncMessage::decode(&frame.bytes).unwrap() {
            for (claimed, bytes) in pairs {
                delivered += 1;
                assert!(!extension_hashes.contains(&claimed));
                let (entry, _) = decode_replica_record(&bytes).unwrap();
                assert_eq!(entry.hash(), claimed);
                if decode_capability(&entry.payload).is_err() {
                    let envelope = tutti_music_hhhs::decode_command(&entry.payload)
                        .expect("every non-capability entry is a shared music command");
                    assert_eq!(envelope.namespace(), identity.music);
                }
            }
        }
    }
    assert!(delivered > 0, "real Replica records crossed the music lane");
}
