//! A bare tutti-music peer: everything an independent device needs to join a
//! room's MUSIC lane over the real HHHS RBSR session, spelled with ZERO
//! walkie-songie dependency.
//!
//! This crate is the walkie-free half of the decisive lane-isolation gate
//! (`tests/native_music_lane_e2e.rs` in walkie-songie). It holds a
//! `Store<MusicLang>`, builds its own salted `Index` from
//! `Store::repair_records()`, and drives the REAL
//! [`hhhs_sync::SyncSession`] as the initiator — decoding every delivered
//! entry with `SignedOp::from_wire_bytes_in::<MusicLang>` and verifying it
//! with `verify_signed_op_in::<MusicLang>`. If a session ever delivers a byte
//! this vocabulary cannot decode, [`BareMusicPeer::domain_decode_failures]`
//! counts it — the e2e gate asserts that count is ZERO.
//!
//! The lane is the ALPN: this peer registers [`tutti_music::MUSIC_RBSR_ALPN`]
//! alone, so an extension-lane connection fails at negotiation and no
//! extension byte can ever reach this code.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use hhhs_sync::{
    EntryHash, SortKey, StrategyId,
    reconciliation::Config,
    sync_session::{EntrySource, SessionStatus, SyncMessage, SyncSession},
};
use tutti_core::{SignedOp, Store, sync_root_of, verify_signed_op_in};
use tutti_music::{LANE_STRATEGY_VERSION, MUSIC_STRATEGY_NAME, MusicLang};

/// The music lane's RBSR strategy, from tutti-music's own identities.
pub fn music_strategy() -> StrategyId {
    StrategyId::new(MUSIC_STRATEGY_NAME, LANE_STRATEGY_VERSION)
}

/// This peer's fixed session salt. A production device draws a fresh random
/// salt per session (walkie's driver does); this crate is a test fixture and
/// carries no randomness dependency, and the kernel's correctness does not
/// depend on the salt's unpredictability — only collision *adversaries* do.
pub const BARE_SESSION_SALT: [u8; 16] = *b"bare-music-salt!";

#[derive(Debug)]
pub enum BareError {
    /// The frame carrier failed.
    Transport(String),
    /// The RBSR kernel refused a message or the handshake.
    Session(String),
    /// An inbound frame was not a `SyncMessage`.
    Decode(String),
    /// The loop guard tripped: frames kept flowing without completion.
    Stalled,
}

impl fmt::Display for BareError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(formatter, "bare transport failed: {error}"),
            Self::Session(error) => write!(formatter, "bare sync session failed: {error}"),
            Self::Decode(error) => write!(formatter, "bare frame decode failed: {error}"),
            Self::Stalled => write!(formatter, "bare session stalled without completing"),
        }
    }
}

impl std::error::Error for BareError {}

/// One bidirectional frame channel carrying a single session — the bare twin
/// of walkie's `SyncStream`, so one in-process duplex end can implement both.
pub trait BareFrameIo {
    /// Send one encoded `SyncMessage`.
    fn send_frame(
        &mut self,
        bytes: &[u8],
    ) -> impl core::future::Future<Output = Result<(), BareError>>;

    /// Receive one encoded `SyncMessage`; `Ok(None)` is a clean end of stream.
    fn recv_frame(
        &mut self,
    ) -> impl core::future::Future<Output = Result<Option<Vec<u8>>, BareError>>;
}

/// What one completed session did, from the bare side.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BareOutcome {
    /// Music ops newly lifted through this peer's own verified ingress.
    pub ingested: usize,
    pub frames_sent: usize,
    /// The `Done` cross-check disagreed: the session finished unconverged.
    pub root_mismatch: bool,
    /// The session ended without the closing handshake.
    pub incomplete: bool,
}

/// The bare peer: a music store and the counters the isolation gate reads.
#[derive(Default)]
pub struct BareMusicPeer {
    /// The peer's whole state — exactly a standalone `Store<MusicLang>`.
    pub store: Store<MusicLang>,
    /// Transport frames received during sessions.
    pub music_frames_received: usize,
    /// Delivered entries that failed the MUSIC vocabulary: wrong wire domain,
    /// failed verification, or a foreign topic. The gate requires ZERO.
    pub domain_decode_failures: usize,
}

impl BareMusicPeer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drive one music-lane session as the initiator, over any frame carrier.
    ///
    /// Runs the REAL `hhhs_sync::SyncSession` — nothing mocked: opening
    /// `Hello`, range reconciliation, `Fetch`/`Entries` waves, the
    /// `resume_admitted` contract after every `Entries`, and the `Done` root
    /// cross-check.
    pub async fn drive_music_initiator<S: BareFrameIo>(
        &mut self,
        mut stream: S,
        topic: &str,
    ) -> Result<BareOutcome, BareError> {
        let salt = BARE_SESSION_SALT;
        let mut source = BareSource::capture(&self.store, salt)?;
        let (session, opening) =
            SyncSession::initiate(music_strategy(), source.index(), Config::default(), salt);
        let mut session = session;
        session.set_root(Some(source.root));
        let mut outcome = BareOutcome::default();
        send_all(&mut stream, &opening, &mut outcome).await?;

        let mut iterations = 0_usize;
        loop {
            match session.status() {
                SessionStatus::Exchanging => {}
                SessionStatus::Complete => return Ok(outcome),
                SessionStatus::Divergent => {
                    outcome.root_mismatch = true;
                    return Ok(outcome);
                }
                SessionStatus::Aborted => {
                    outcome.incomplete = true;
                    return Ok(outcome);
                }
            }
            iterations += 1;
            if iterations > 16_384 {
                return Err(BareError::Stalled);
            }
            let Some(frame) = stream.recv_frame().await? else {
                outcome.incomplete = true;
                outcome.root_mismatch = session.root_divergence();
                return Ok(outcome);
            };
            self.music_frames_received += 1;
            let message =
                SyncMessage::decode(&frame).map_err(|error| BareError::Decode(error.to_string()))?;
            let answered_a_fetch = matches!(message, SyncMessage::Entries { .. });
            let output = session
                .on_message(message, &source)
                .map_err(|error| BareError::Session(error.to_string()))?;
            send_all(&mut stream, &output.send, &mut outcome).await?;

            if answered_a_fetch {
                // The kernel contract: EVERY `Entries` frame is followed by
                // `resume_admitted`, even an empty final one.
                let admitted = if output.ingest.is_empty() {
                    Vec::new()
                } else {
                    let (admitted, lifted) = self.ingest_music(topic, &output.ingest);
                    source.absorb(&self.store, &lifted)?;
                    outcome.ingested += lifted.len();
                    admitted
                };
                let more = session
                    .resume_admitted(source.index(), &admitted, Some(source.root))
                    .map_err(|error| BareError::Session(error.to_string()))?;
                send_all(&mut stream, &more, &mut outcome).await?;
            }
        }
    }

    /// The bare peer's whole ingress: MUSIC deframe, MUSIC verification, the
    /// room-topic gate — then the standard admitted/lifted bookkeeping
    /// (lifted ops under their store-derived hash, parked ops under the wire
    /// claim, duplicates kept).
    fn ingest_music(
        &mut self,
        topic: &str,
        pairs: &[(EntryHash, Vec<u8>)],
    ) -> (Vec<EntryHash>, Vec<EntryHash>) {
        let mut admitted = Vec::new();
        let mut lifted = Vec::new();
        for (wire_hash, bytes) in pairs {
            let Ok(signed) = SignedOp::from_wire_bytes_in::<MusicLang>(bytes) else {
                self.domain_decode_failures += 1;
                continue;
            };
            let Ok(verified) = verify_signed_op_in::<MusicLang>(&signed) else {
                self.domain_decode_failures += 1;
                continue;
            };
            if verified.topic() != Some(topic) {
                self.domain_decode_failures += 1;
                continue;
            }
            let id = verified.id();
            if let Some(entry) = self.store.lifted_entry(id) {
                admitted.push(entry);
                continue;
            }
            if self.store.knows_op(id) {
                admitted.push(*wire_hash);
                continue;
            }
            let newly = self.store.ingest_verified(verified);
            if newly.is_empty() {
                admitted.push(*wire_hash);
            } else {
                admitted.extend(newly.iter().copied());
                lifted.extend(newly);
            }
        }
        (admitted, lifted)
    }
}

async fn send_all<S: BareFrameIo>(
    stream: &mut S,
    messages: &[SyncMessage],
    outcome: &mut BareOutcome,
) -> Result<(), BareError> {
    for message in messages {
        stream.send_frame(&message.encode()).await?;
        outcome.frames_sent += 1;
    }
    Ok(())
}

/// A consistent (`EntrySource`, `Index`, root) triple over the music store —
/// built from exactly `Store::<MusicLang>::repair_records()`, so the index can
/// only ever advertise hashes the source can serve.
struct BareSource {
    /// entry hash -> (verbatim MUSIC wire bytes, causal predecessors)
    records: BTreeMap<EntryHash, (Vec<u8>, Vec<EntryHash>)>,
    index: hhhs_sync::reconciliation::Index,
    root: [u8; 32],
    salt: [u8; 16],
}

impl BareSource {
    fn capture(store: &Store<MusicLang>, salt: [u8; 16]) -> Result<Self, BareError> {
        let mut records: BTreeMap<EntryHash, (Vec<u8>, Vec<EntryHash>)> = BTreeMap::new();
        for (hash, (signed, predecessors)) in store.repair_records() {
            let bytes = signed
                .to_wire_bytes_in::<MusicLang>()
                .map_err(|error| BareError::Session(error.to_string()))?;
            records.insert(hash, (bytes, predecessors));
        }
        let mut source = Self {
            records,
            index: hhhs_sync::reconciliation::Index::new(salt),
            root: [0; 32],
            salt,
        };
        source.reindex();
        Ok(source)
    }

    /// Fold newly lifted entries in, keeping records, index, and root moving
    /// together.
    fn absorb(&mut self, store: &Store<MusicLang>, lifted: &[EntryHash]) -> Result<(), BareError> {
        let mut changed = false;
        for hash in lifted {
            if self.records.contains_key(hash) {
                continue;
            }
            let Some((signed, predecessors)) = store.repair_record(hash) else {
                continue;
            };
            let bytes = signed
                .to_wire_bytes_in::<MusicLang>()
                .map_err(|error| BareError::Session(error.to_string()))?;
            self.records.insert(*hash, (bytes, predecessors));
            changed = true;
        }
        if changed {
            self.reindex();
        }
        Ok(())
    }

    fn reindex(&mut self) {
        let mut index = hhhs_sync::reconciliation::Index::new(self.salt);
        for hash in self.records.keys() {
            index.insert(SortKey(hash.as_bytes().to_vec()), *hash);
        }
        self.index = index;
        self.root = sync_root_of(self.records.keys());
    }

    fn index(&self) -> hhhs_sync::reconciliation::Index {
        self.index.clone()
    }
}

impl EntrySource for BareSource {
    fn have(&self, hash: &EntryHash) -> bool {
        self.records.contains_key(hash)
    }

    /// The whole causal closure of `hash`, ancestors first, honoring the
    /// kernel's session-scoped `already_included` dedup; the requested hash is
    /// always in its own answer.
    fn bytes_with_closure(
        &self,
        hash: &EntryHash,
        already_included: &mut BTreeSet<EntryHash>,
    ) -> Vec<(EntryHash, Vec<u8>)> {
        if !self.records.contains_key(hash) {
            return Vec::new();
        }

        // Iterative post-order DFS, so ancestors precede descendants.
        let mut order: Vec<EntryHash> = Vec::new();
        let mut visited: BTreeSet<EntryHash> = BTreeSet::new();
        let mut stack: Vec<(EntryHash, bool)> = vec![(*hash, false)];
        while let Some((candidate, expanded)) = stack.pop() {
            if expanded {
                order.push(candidate);
                continue;
            }
            if already_included.contains(&candidate) || !visited.insert(candidate) {
                continue;
            }
            let Some((_, predecessors)) = self.records.get(&candidate) else {
                continue;
            };
            stack.push((candidate, true));
            for predecessor in predecessors {
                stack.push((*predecessor, false));
            }
        }

        let mut output = Vec::new();
        for candidate in order {
            if candidate == *hash || already_included.contains(&candidate) {
                continue;
            }
            let Some((bytes, _)) = self.records.get(&candidate) else {
                continue;
            };
            already_included.insert(candidate);
            output.push((candidate, bytes.clone()));
        }
        if !already_included.contains(hash)
            && let Some((bytes, _)) = self.records.get(hash)
        {
            already_included.insert(*hash);
            output.push((*hash, bytes.clone()));
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::signing_key_from_seed;
    use tutti_music::{MusicOp, TunedDegree, Tuning};

    const TOPIC: &str = "bare-music-peer-test";

    fn add_chain(peer: &mut BareMusicPeer, seed: &[u8; 32], count: u16) {
        let key = signing_key_from_seed(seed);
        let tuning = Tuning::twelve_tet();
        for offset in 0..count {
            peer.store.commit(
                &key,
                TOPIC,
                1 + u64::from(offset),
                MusicOp::AddDegree {
                    degree: TunedDegree::new(&tuning, offset % 12).unwrap(),
                },
            );
        }
    }

    #[test]
    fn capture_and_absorb_track_the_store_root() {
        let mut peer = BareMusicPeer::new();
        add_chain(&mut peer, &[7; 32], 5);
        let source = BareSource::capture(&peer.store, BARE_SESSION_SALT).unwrap();
        assert_eq!(source.root, peer.store.sync_root());
        assert_eq!(source.records.len(), peer.store.entry_hashes().len());

        // A delivery from another bare peer absorbs to the new store root.
        let mut donor = BareMusicPeer::new();
        add_chain(&mut donor, &[8; 32], 1);
        let pairs: Vec<(EntryHash, Vec<u8>)> = donor
            .store
            .repair_records()
            .into_iter()
            .map(|(hash, (signed, _))| {
                (hash, signed.to_wire_bytes_in::<MusicLang>().unwrap())
            })
            .collect();
        let (admitted, lifted) = peer.ingest_music(TOPIC, &pairs);
        assert_eq!(lifted.len(), 1);
        assert_eq!(admitted, lifted);
        assert_eq!(peer.domain_decode_failures, 0);
        let mut source = source;
        source.absorb(&peer.store, &lifted).unwrap();
        assert_eq!(source.root, peer.store.sync_root());
    }

    #[test]
    fn closure_is_whole_and_ancestors_first() {
        let mut peer = BareMusicPeer::new();
        add_chain(&mut peer, &[9; 32], 4);
        let source = BareSource::capture(&peer.store, BARE_SESSION_SALT).unwrap();
        let tip = *source
            .records
            .keys()
            .max_by_key(|hash| {
                let mut included = BTreeSet::new();
                source.bytes_with_closure(hash, &mut included).len()
            })
            .unwrap();
        let mut included = BTreeSet::new();
        let full = source.bytes_with_closure(&tip, &mut included);
        assert_eq!(full.len(), 4, "the whole causal chain ships");
        let mut emitted: BTreeSet<EntryHash> = BTreeSet::new();
        for (hash, _) in &full {
            for predecessor in &source.records[hash].1 {
                assert!(emitted.contains(predecessor), "ancestors precede descendants");
            }
            emitted.insert(*hash);
        }
    }

    #[test]
    fn a_foreign_frame_counts_as_a_domain_failure_and_never_enters() {
        let mut peer = BareMusicPeer::new();
        let garbage = vec![(
            EntryHash(hhhs_sync::Digest([0xAB; 32])),
            b"not a music frame at all".to_vec(),
        )];
        let (admitted, lifted) = peer.ingest_music(TOPIC, &garbage);
        assert!(admitted.is_empty() && lifted.is_empty());
        assert_eq!(peer.domain_decode_failures, 1);
        assert!(peer.store.is_empty());
    }
}
