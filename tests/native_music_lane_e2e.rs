//! THE lane-isolation gate (net-layer generation, GO/NO-GO): a bare
//! tutti-music peer — the independent `bare-music-peer` crate, whose manifest
//! names NO walkie type — joins a walkie room's MUSIC lane over the REAL HHHS
//! RBSR session and converges, while the room's EXTENSION lane holds state and
//! CHANGES mid-session, and not one extension byte exists anywhere in the
//! recorded transcript.
//!
//! The lane is the ALPN. The in-process connection factory below negotiates
//! exactly like QUIC does: a connection opens only if the requested ALPN is in
//! BOTH endpoints' registered sets. The bare peer registers
//! `[MUSIC_RBSR_ALPN]` alone, so the extension lane is UNREACHABLE from it —
//! asserted here as a refused connection, not assumed.
//!
//! Both halves run the REAL reconciliation kernel (`hhhs_sync::SyncSession`):
//! walkie's production lane driver (`net::sync::drive_responder::<MusicLane>`)
//! against the bare crate's own initiator. Nothing is mocked; the transcript
//! records the exact encoded `SyncMessage` frames that crossed.

use std::sync::{Arc, Mutex};

use bare_music_peer::{BareError, BareFrameIo, BareMusicPeer};
use futures::channel::{mpsc, oneshot};
use futures::future::join3;
use hhhs_sync::sync_session::SyncMessage;
use tutti_core::{OpLanguage, SignedOp, Store, signing_key_from_seed, verify_signed_op_in};
use tutti_music::{MUSIC_RBSR_ALPN, MusicLang, MusicOp};
use walkie_songie::net::{
    EXTENSION_RBSR_ALPN, MusicLane, SyncLimits, SyncStream, SyncTimer, TransportError,
    drive_responder,
};
use walkie_songie::room::test_support::{tet_degree, tet_pitch};
use walkie_songie::room::v4::{ExtensionLang, ExtensionOp, verify_extension_op};

/// The exact string production signs and enforces: the DERIVED room topic's
/// lowercase hex (`blake3::derive_key("walkie-songie room topic v1",
/// "sunny-garden-melody")`), pinned in `tests/music_lane_interop.rs` — never
/// the human room name.
const TOPIC: &str = "072aaa8bdb9bea93fe8b3af1a3214533027e9973fb007440b55606e2fe452a7a";

const TS: u64 = 1_700_000_000_000_000; // µs

const SEED_WALKIE_MUSIC: [u8; 32] = [1; 32];
const SEED_WALKIE_EXTENSION: [u8; 32] = [2; 32];
const SEED_BARE: [u8; 32] = [7; 32];

// ---------------------------------------------------------------------------
// The recording in-process transport: the same duplex seam as the production
// loopback tests, plus a byte-exact transcript and ALPN negotiation.
// ---------------------------------------------------------------------------

/// One frame as it crossed the wire: the exact `SyncMessage::encode()` bytes,
/// tagged with the sending side and the connection's negotiated ALPN.
#[derive(Debug, Clone)]
struct RecordedFrame {
    from: &'static str,
    alpn: &'static [u8],
    bytes: Vec<u8>,
}

type Transcript = Arc<Mutex<Vec<RecordedFrame>>>;
type FetchBarrier = Arc<Mutex<Option<oneshot::Sender<()>>>>;

/// One end of a negotiated connection. Implements BOTH walkie's [`SyncStream`]
/// and the bare crate's [`BareFrameIo`], so the identical seam carries the
/// production responder and the walkie-free initiator.
struct RecordingDuplexEnd {
    label: &'static str,
    alpn: &'static [u8],
    tx: mpsc::UnboundedSender<Vec<u8>>,
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
    transcript: Transcript,
    /// Fires once, at the first `Fetch` sent by EITHER side — the moment real
    /// entry transfer is underway, gating the mid-session extension churn.
    first_fetch: FetchBarrier,
}

impl RecordingDuplexEnd {
    fn record_and_send(&mut self, frame: &[u8]) -> Result<(), String> {
        self.transcript.lock().unwrap().push(RecordedFrame {
            from: self.label,
            alpn: self.alpn,
            bytes: frame.to_vec(),
        });
        if let Ok(SyncMessage::Fetch(_)) = SyncMessage::decode(frame)
            && let Some(barrier) = self.first_fetch.lock().unwrap().take()
        {
            let _ = barrier.send(());
        }
        self.tx
            .unbounded_send(frame.to_vec())
            .map_err(|error| format!("peer hung up: {error}"))
    }

    async fn receive(&mut self) -> Option<Vec<u8>> {
        use futures::StreamExt;
        self.rx.next().await
    }
}

impl SyncStream for RecordingDuplexEnd {
    async fn send_frame(&mut self, frame: &[u8]) -> Result<(), TransportError> {
        self.record_and_send(frame).map_err(TransportError::Backend)
    }

    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        Ok(self.receive().await)
    }

    async fn close(self) {}
}

impl BareFrameIo for RecordingDuplexEnd {
    async fn send_frame(&mut self, bytes: &[u8]) -> Result<(), BareError> {
        self.record_and_send(bytes).map_err(BareError::Transport)
    }

    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, BareError> {
        Ok(self.receive().await)
    }
}

/// An endpoint's live capability declaration: its registered ALPN set.
struct SimEndpoint {
    name: &'static str,
    alpns: &'static [&'static [u8]],
}

/// QUIC-shaped negotiation: the connection exists only if BOTH endpoints
/// registered the requested ALPN. There is no partial success — a refused
/// ALPN yields no stream, so not one frame of that lane can ever exist.
fn connect(
    dialer: &SimEndpoint,
    acceptor: &SimEndpoint,
    alpn: &'static [u8],
    transcript: &Transcript,
    first_fetch: FetchBarrier,
) -> Result<(RecordingDuplexEnd, RecordingDuplexEnd), String> {
    for endpoint in [dialer, acceptor] {
        if !endpoint.alpns.contains(&alpn) {
            return Err(format!(
                "{} does not register ALPN {}",
                endpoint.name,
                String::from_utf8_lossy(alpn),
            ));
        }
    }
    let (to_acceptor, from_dialer) = mpsc::unbounded();
    let (to_dialer, from_acceptor) = mpsc::unbounded();
    Ok((
        RecordingDuplexEnd {
            label: dialer.name,
            alpn,
            tx: to_acceptor,
            rx: from_acceptor,
            transcript: transcript.clone(),
            first_fetch: first_fetch.clone(),
        },
        RecordingDuplexEnd {
            label: acceptor.name,
            alpn,
            tx: to_dialer,
            rx: from_dialer,
            transcript: transcript.clone(),
            first_fetch,
        },
    ))
}

/// Never fires: the test proves it needed no deadline.
struct NoTimeout;
impl SyncTimer for NoTimeout {
    async fn sleep(&self, _duration: std::time::Duration) {
        futures::future::pending::<()>().await
    }
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

// ---------------------------------------------------------------------------
// The gate.
// ---------------------------------------------------------------------------

#[test]
fn bare_music_peer_joins_the_music_lane_and_never_sees_an_extension_byte() {
    // -- Walkie's room state: a 24-op music chain plus a live extension lane.
    let walkie_music_key = signing_key_from_seed(&SEED_WALKIE_MUSIC);
    let mut walkie_music: Store<MusicLang> = Store::new();
    for offset in 0..24_u64 {
        walkie_music.commit(
            &walkie_music_key,
            TOPIC,
            TS + offset,
            MusicOp::AddDegree {
                degree: tet_degree((offset % 12) as u16),
            },
        );
    }

    let walkie_extension_key = signing_key_from_seed(&SEED_WALKIE_EXTENSION);
    let mut walkie_extension: Store<ExtensionLang> = Store::new();
    let put = walkie_extension.commit(
        &walkie_extension_key,
        TOPIC,
        TS + 100,
        ExtensionOp::PutPiece {
            emoji: "🌵".into(),
            pitch: tet_pitch(60),
        },
    );
    let piece = verify_extension_op(&put, TOPIC)
        .expect("extension op verifies")
        .id();
    walkie_extension.commit(
        &walkie_extension_key,
        TOPIC,
        TS + 101,
        ExtensionOp::SetConfig {
            pieces_locked: Some(false),
            available_emojis: Some("🌵🎵".into()),
        },
    );
    let extension_len_before = walkie_extension.len();
    assert_eq!(
        extension_len_before, 2,
        "extension state exists BEFORE the join"
    );

    // -- The bare peer: an independently authored, divergent music op.
    let mut bare = BareMusicPeer::new();
    let bare_key = signing_key_from_seed(&SEED_BARE);
    let bare_signed = bare.store.commit(
        &bare_key,
        TOPIC,
        TS + 200,
        MusicOp::AddDegree {
            degree: tet_degree(9),
        },
    );
    let bare_op = verify_signed_op_in::<MusicLang>(&bare_signed)
        .expect("the bare op verifies as pure music")
        .id();
    assert_ne!(
        bare.store.sync_root(),
        walkie_music.sync_root(),
        "the peers start genuinely diverged"
    );

    // -- Capability declaration: walkie registers BOTH lanes, the bare peer
    //    registers music alone.
    let walkie_endpoint = SimEndpoint {
        name: "walkie",
        alpns: &[MUSIC_RBSR_ALPN, EXTENSION_RBSR_ALPN],
    };
    let bare_endpoint = SimEndpoint {
        name: "bare",
        alpns: &[MUSIC_RBSR_ALPN],
    };

    let transcript: Transcript = Arc::new(Mutex::new(Vec::new()));

    // The extension lane is UNREACHABLE from the bare peer: negotiation
    // refuses the ALPN in both directions, so zero extension-lane frames can
    // exist. This is the strongest isolation layer — it fails before RBSR.
    assert!(
        connect(
            &walkie_endpoint,
            &bare_endpoint,
            EXTENSION_RBSR_ALPN,
            &transcript,
            Arc::new(Mutex::new(None)),
        )
        .is_err(),
        "walkie must not be able to open an extension-lane connection to a bare peer"
    );
    assert!(
        connect(
            &bare_endpoint,
            &walkie_endpoint,
            EXTENSION_RBSR_ALPN,
            &transcript,
            Arc::new(Mutex::new(None)),
        )
        .is_err(),
        "a bare peer must not be able to open an extension-lane connection either"
    );

    // -- The one negotiable connection: the music lane.
    let (first_fetch_tx, first_fetch_rx) = oneshot::channel();
    let barrier: FetchBarrier = Arc::new(Mutex::new(Some(first_fetch_tx)));
    let (bare_end, walkie_end) = connect(
        &bare_endpoint,
        &walkie_endpoint,
        MUSIC_RBSR_ALPN,
        &transcript,
        barrier,
    )
    .expect("both endpoints register the music ALPN");

    // -- Run the REAL session: the bare crate's initiator against walkie's
    //    production lane responder, with the extension lane churning the
    //    moment the first Fetch proves entry transfer is underway.
    let (bare_outcome, walkie_outcome, ()) = futures::executor::block_on(join3(
        bare.drive_music_initiator(bare_end, TOPIC),
        drive_responder::<MusicLane, _, _, _>(
            walkie_end,
            &NoTimeout,
            &mut walkie_music,
            TOPIC,
            SyncLimits::default(),
        ),
        async {
            first_fetch_rx
                .await
                .expect("a real music Fetch must occur before the session ends");
            walkie_extension.commit(
                &walkie_extension_key,
                TOPIC,
                TS + 300,
                ExtensionOp::MovePiece {
                    piece,
                    pitch: tet_pitch(64),
                },
            );
            walkie_extension.commit(
                &walkie_extension_key,
                TOPIC,
                TS + 301,
                ExtensionOp::SetConfig {
                    pieces_locked: Some(true),
                    available_emojis: None,
                },
            );
        },
    ));
    let bare_outcome = bare_outcome.expect("the bare initiator completes");
    let walkie_outcome = walkie_outcome.expect("the walkie responder completes");

    // -- GATE: full music convergence, through real reconciliation.
    assert_eq!(bare.store.pending_len(), 0, "bare drains completely");
    assert_eq!(walkie_music.pending_len(), 0, "walkie drains completely");
    assert_eq!(
        bare.store.entry_hashes(),
        walkie_music.entry_hashes(),
        "identical music identity sets"
    );
    assert_eq!(
        bare.store.sync_root(),
        walkie_music.sync_root(),
        "identical music sync roots"
    );
    assert_eq!(
        bare.store.view(),
        walkie_music.view(),
        "identical music read models"
    );
    assert!(
        walkie_music.knows_op(bare_op),
        "the bare peer's divergent op flowed INTO walkie"
    );
    assert!(
        bare.store.len() > 1,
        "the bare peer actually received the room"
    );
    assert_eq!(bare.store.len(), 25, "24 walkie ops + the bare op");
    assert!(!bare_outcome.incomplete && !walkie_outcome.incomplete);
    assert!(!bare_outcome.root_mismatch && !walkie_outcome.root_mismatch);
    assert!(bare_outcome.ingested >= 24, "the whole walkie chain lifted");
    assert!(walkie_outcome.ingested >= 1, "the bare op lifted");

    // -- GATE: the bare vocabulary decoded EVERYTHING it was sent.
    assert_eq!(
        bare.domain_decode_failures, 0,
        "not one delivered entry fell outside the pure-music vocabulary"
    );
    assert!(
        bare.music_frames_received > 0,
        "the session was not vacuous"
    );

    // -- GATE: extension state existed before AND changed mid-session.
    let extension_len_after = walkie_extension.len();
    assert!(
        extension_len_after > extension_len_before,
        "the extension lane churned DURING the music exchange"
    );
    assert!(!walkie_extension.is_empty());
    assert!(
        walkie_extension.view().pieces_locked,
        "the churned config applied"
    );

    // -- GATE: the transcript. Every frame rode the music ALPN; every
    //    delivered entry deframes as pure music; no frame anywhere contains
    //    the extension wire magic or any extension entry hash.
    let transcript = transcript.lock().unwrap();
    assert!(
        !transcript.is_empty(),
        "the transcript recorded the session"
    );
    let negotiated: std::collections::BTreeSet<&[u8]> =
        transcript.iter().map(|frame| frame.alpn).collect();
    assert_eq!(
        negotiated,
        std::collections::BTreeSet::from([MUSIC_RBSR_ALPN]),
        "the ONLY negotiated ALPN is the music lane's"
    );

    let extension_hashes = walkie_extension.entry_hashes();
    assert_eq!(
        extension_hashes.len(),
        4,
        "put + config + the two churn ops"
    );
    let mut entries_seen = 0_usize;
    for frame in transcript.iter() {
        assert_eq!(
            frame.alpn, MUSIC_RBSR_ALPN,
            "no frame outside the music lane"
        );
        // Byte-level scan of the WHOLE frame — headers, ranges, everything —
        // for extension material.
        assert!(
            !contains_subslice(&frame.bytes, <ExtensionLang as OpLanguage>::WIRE_MAGIC),
            "frame from {} contains the extension wire magic",
            frame.from
        );
        for hash in &extension_hashes {
            assert!(
                !contains_subslice(&frame.bytes, hash.as_bytes()),
                "frame from {} contains extension entry hash {}",
                frame.from,
                hash.to_hex()
            );
        }
        // Structural check of every delivered entry.
        let message =
            SyncMessage::decode(&frame.bytes).expect("transcript frames are SyncMessages");
        if let SyncMessage::Entries { pairs, .. } = message {
            for (hash, bytes) in pairs {
                entries_seen += 1;
                assert!(
                    bytes.starts_with(<MusicLang as OpLanguage>::WIRE_MAGIC),
                    "a delivered entry must be framed as music"
                );
                assert!(
                    !bytes.starts_with(<ExtensionLang as OpLanguage>::WIRE_MAGIC),
                    "a delivered entry must not be framed as extension"
                );
                SignedOp::from_wire_bytes_in::<MusicLang>(&bytes)
                    .expect("every delivered entry deframes as pure music");
                assert!(
                    !extension_hashes.contains(&hash),
                    "an extension entry hash crossed the music lane"
                );
            }
        }
    }
    assert!(
        entries_seen >= 25,
        "real entry transfer happened on the wire ({entries_seen} pairs)"
    );
}
