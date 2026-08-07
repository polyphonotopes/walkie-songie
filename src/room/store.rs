//! `RoomStore`: lift verified p2panda ops into an hhhs-core causal DAG and
//! materialize the room read model HHHS-natively.
//!
//! Every [`VerifiedOp`] is deterministically lifted to a kernel [`Entry`] whose
//! payload is the **verbatim framed signed bytes** of the op, and whose `prevs`
//! are the entries that lift the op's `backlink` and each of its `observed`
//! op ids. Because the payload and the prev set are both pure functions of the
//! signed op, the resulting [`EntryHash`] is identical on every peer regardless
//! of the order ops arrive — which is what makes cross-peer convergence hold.
//!
//! The read model ([`RoomView`]) is then computed HHHS-natively: pitches are a
//! content-keyed add-wins set resolved by causal ancestry
//! ([`ReachIndex`](hhhs_core::cover::ReachIndex)), voice is a per-author
//! seq-register, pieces are owner-gated per-owner seq-registers, and
//! tuning/config are cross-author registers resolved by
//! [`register::resolve`](hhhs_core::register::resolve).
//!
//! Signature verification happens once, at ingest, against a [`VerifiedOp`];
//! reads never re-verify.

use std::collections::{BTreeMap, BTreeSet};

use hhhs_core::cover::ReachIndex;
use hhhs_core::register;
use hhhs_core::{AppendOutcome, DagRead, Entry, EntryHash, MemDagStore, Position};

use super::ops::{
    AuthorId, LogHead, OpId, SignedOp, SigningKey, VerifiedOp, WalkieOp,
    sign_op_for_topic_observing, verify_signed_op,
};
use crate::tuning::{TunedDegree, TunedPeriodicPitch, TuningDefinition};

/// Framing tag prefixed to the verbatim signed bytes when they become a kernel
/// entry payload. Bumping this changes every [`EntryHash`], so it is a schema
/// pin: a golden-vector test asserts a concrete hash against it.
const SIGNED_OP_FRAME_MAGIC: &[u8] = b"walkie.hhhs.signed-op/1";

/// Deterministically frame a signed op into an entry payload:
/// `MAGIC ++ len(header) ++ header ++ len(payload) ++ payload` (u64 little-endian
/// lengths). A pure function of the signed op — never of any decoded record — so
/// the entry hash matches byte-for-byte across peers.
fn frame_signed(signed: &SignedOp) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        SIGNED_OP_FRAME_MAGIC.len() + 16 + signed.header.len() + signed.payload.len(),
    );
    out.extend_from_slice(SIGNED_OP_FRAME_MAGIC);
    out.extend_from_slice(&(signed.header.len() as u64).to_le_bytes());
    out.extend_from_slice(&signed.header);
    out.extend_from_slice(&(signed.payload.len() as u64).to_le_bytes());
    out.extend_from_slice(&signed.payload);
    out
}

/// Inverse of [`frame_signed`]: recover the verbatim [`SignedOp`] from a lifted
/// entry's payload. A total inverse of the deterministic framing above, so a
/// round-trip through the DAG payload is lossless — this is what lets the store
/// re-emit the exact bytes an author signed for anti-entropy transfer.
fn unframe_signed(bytes: &[u8]) -> SignedOp {
    let mut pos = SIGNED_OP_FRAME_MAGIC.len();
    let read_len = |bytes: &[u8], pos: usize| -> usize {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&bytes[pos..pos + 8]);
        u64::from_le_bytes(buf) as usize
    };
    let header_len = read_len(bytes, pos);
    pos += 8;
    let header = bytes[pos..pos + header_len].to_vec();
    pos += header_len;
    let payload_len = read_len(bytes, pos);
    pos += 8;
    let payload = bytes[pos..pos + payload_len].to_vec();
    SignedOp { header, payload }
}

/// Domain tag for [`sync_root_of`], so a convergence digest can never be
/// confused with an entry hash or an op frame.
const SYNC_ROOT_MAGIC: &[u8] = b"walkie.hhhs.sync-root/1";

/// The canonical convergence digest over an entry-hash identity set.
///
/// `hashes` MUST be in ascending order — every caller feeds it a `BTreeMap`/
/// `BTreeSet` iterator, which is. The digest is over the identity set alone, so
/// two peers agree iff they hold exactly the same lifted entries, independent of
/// arrival order or anything parked.
///
/// One definition, used by both [`RoomStore::sync_root`] and the sync layer's
/// snapshot, so the value a peer cross-checks on `Done` cannot drift from the
/// value the local store would compute.
pub fn sync_root_of<'a>(hashes: impl IntoIterator<Item = &'a EntryHash>) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(SYNC_ROOT_MAGIC);
    for hash in hashes {
        hasher.update(hash.as_bytes());
    }
    *hasher.finalize().as_bytes()
}

/// The op contents kept alongside a lifted entry, so reads never re-verify a
/// signature or re-decode a payload.
#[derive(Clone, Debug)]
struct DecodedOp {
    author: AuthorId,
    op: WalkieOp,
    /// Author-stamped time (display / last-resort tiebreak only; unused by the
    /// causal view but kept per the store contract).
    #[allow(dead_code)]
    ts_ms: u64,
    seq: u64,
}

/// The causal-DAG mirror of a room's signed op log plus everything reads need.
#[derive(Default)]
pub struct RoomStore {
    /// The opaque-payload causal DAG. Identity ([`EntryHash`]) is fixed here.
    dag: MemDagStore,
    /// p2panda op id -> the entry that lifts it. The resolution table for prevs.
    source_to_entry: BTreeMap<OpId, EntryHash>,
    /// entry -> p2panda op id (inverse of `source_to_entry`).
    entry_to_source: BTreeMap<EntryHash, OpId>,
    /// entry -> decoded op contents (author, payload, ts, seq).
    decoded: BTreeMap<EntryHash, DecodedOp>,
    /// Per-author log head, so the local author can chain new commits.
    heads: BTreeMap<AuthorId, LogHead>,
    /// Ops whose `backlink`/`observed` are not all lifted yet — parked until
    /// their full causal past arrives (strict deferral), then drained.
    pending: Vec<VerifiedOp>,
}

impl RoomStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of lifted (materialized) ops.
    pub fn len(&self) -> usize {
        self.source_to_entry.len()
    }

    pub fn is_empty(&self) -> bool {
        self.source_to_entry.is_empty()
    }

    /// The entry hashes of every lifted (materialized) op. The RBSR anti-entropy
    /// index is built from exactly this set, and it is the cross-peer identity set
    /// convergence is asserted over. Permanent public API: the sync layer needs it.
    pub fn entry_hashes(&self) -> BTreeSet<EntryHash> {
        self.entry_to_source.keys().copied().collect()
    }

    /// The number of ops parked awaiting their causal past (strict deferral). Zero
    /// after quiescence is the liveness invariant: nothing is stuck behind a
    /// predecessor that already arrived. Permanent public API.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Whether an operation is already lifted or is waiting on causal
    /// predecessors. Persistence and repair use this to avoid journal growth
    /// from duplicate gossip frames.
    pub fn knows_op(&self, id: OpId) -> bool {
        self.source_to_entry.contains_key(&id)
            || self.pending.iter().any(|pending| pending.id() == id)
    }

    /// The entry hash lifting op `id`, if that op is already materialized.
    ///
    /// `None` for parked and unknown ops alike — a parked op cannot resolve its
    /// `prevs` yet, so it has no entry hash to report. The sync layer uses this
    /// to name an already-lifted duplicate delivery in its `admitted` set by
    /// the hash the store derived, never the hash the wire claimed.
    pub fn lifted_entry(&self, id: OpId) -> Option<EntryHash> {
        self.source_to_entry.get(&id).copied()
    }

    /// The verbatim signed bytes of every lifted op, keyed by the entry hash that
    /// lifts it. Recovered losslessly from the DAG payloads, so it is exactly the
    /// bytes each author signed — what an anti-entropy transfer re-ingests on the
    /// far side. Permanent public API for the sync/reconcile layer.
    pub fn signed_ops(&self) -> BTreeMap<EntryHash, SignedOp> {
        self.dag
            .entries_topo()
            .into_iter()
            .map(|entry| (entry.hash(), unframe_signed(&entry.payload)))
            .collect()
    }

    /// A convergence digest over this store's entry-hash identity set.
    ///
    /// Carried on `Done` so the two halves cross-check that they actually agree
    /// (`SessionOutput::root_mismatch`). Without it a session that pruned a
    /// divergent range — a fingerprint collision, a peer that advertised what it
    /// could not serve — reports success while the peers have not converged, and
    /// nothing anywhere notices.
    pub fn sync_root(&self) -> [u8; 32] {
        sync_root_of(self.entry_to_source.keys())
    }

    /// Signed bytes plus causal predecessors for ONE lifted entry.
    ///
    /// Exists so the sync layer can fold newly-lifted entries into its
    /// `(EntrySource, Index)` pair in O(lifted) instead of rebuilding the whole
    /// snapshot — [`Self::repair_records`] topo-sorts and re-serializes the
    /// entire DAG, and the driver used to call it once per `Entries` frame.
    pub fn repair_record(&self, hash: &EntryHash) -> Option<(SignedOp, Vec<EntryHash>)> {
        let entry = self.dag.entry(hash)?;
        Some((
            unframe_signed(&entry.payload),
            entry.header.prevs.0.iter().copied().collect(),
        ))
    }

    /// Signed bytes plus causal-entry predecessors for a transport-neutral
    /// repair snapshot.
    pub fn repair_records(&self) -> BTreeMap<EntryHash, (SignedOp, Vec<EntryHash>)> {
        self.dag
            .entries_topo()
            .into_iter()
            .map(|entry| {
                (
                    entry.hash(),
                    (
                        unframe_signed(&entry.payload),
                        entry.header.prevs.0.iter().copied().collect(),
                    ),
                )
            })
            .collect()
    }

    /// Which p2panda op id each lifted entry hash lifts. The RBSR index advertises
    /// entry hashes; this resolves an advertised hash back to its op id (and thence
    /// its causal predecessors) without re-verifying. Permanent public API for the
    /// sync/reconcile layer.
    pub fn lifted_op_ids(&self) -> BTreeMap<EntryHash, OpId> {
        self.entry_to_source.clone()
    }

    /// Lift a verified op into the DAG. Deduplicates, advances the author's head,
    /// and — via strict deferral — parks the op if any referenced op id is not
    /// yet lifted, draining the pending set after every successful lift.
    ///
    /// Returns the entries this call newly LIFTED: the op itself if its causal
    /// past was complete, plus everything it unblocked. An empty return means the
    /// op parked. Callers must not treat "accepted" as "materialized" — a parked
    /// op is not in [`Self::entry_hashes`], is not advertised to peers, and
    /// cannot be served, so counting it as ingested overstates progress.
    pub fn ingest_verified(&mut self, op: VerifiedOp) -> Vec<EntryHash> {
        let id = op.id();
        if self.source_to_entry.contains_key(&id) {
            return Vec::new();
        }
        if self.pending.iter().any(|p| p.id() == id) {
            return Vec::new();
        }
        self.advance_head(&op);
        self.pending.push(op);
        self.drain_pending()
    }

    /// Advance (never regress) the author's tracked head to the greatest seq seen.
    fn advance_head(&mut self, op: &VerifiedOp) {
        let advanced = op.advanced_head();
        let slot = self
            .heads
            .entry(op.author())
            .or_insert_with(LogHead::genesis);
        if advanced.next_seq > slot.next_seq {
            *slot = advanced;
        }
    }

    /// Resolve an op's `prevs` = `{ lift(backlink) } ∪ { lift(o) : o in observed }`.
    /// Returns `None` (defer) if ANY referenced op id is not yet lifted — never
    /// omit a prev, or the entry hash would depend on arrival order.
    fn resolve_prevs(&self, op: &VerifiedOp) -> Option<BTreeSet<EntryHash>> {
        let mut prevs = BTreeSet::new();
        if let Some(backlink) = op.backlink() {
            prevs.insert(*self.source_to_entry.get(&OpId(backlink))?);
        }
        for observed in op.observed() {
            prevs.insert(*self.source_to_entry.get(&OpId(*observed))?);
        }
        Some(prevs)
    }

    /// Try to lift one op. Returns the lifted entry hash iff it was appended (or
    /// already present); `None` (with no mutation) if its causal past is
    /// incomplete.
    fn try_lift(&mut self, op: &VerifiedOp) -> Option<EntryHash> {
        let prevs = self.resolve_prevs(op)?;
        let entry = Entry::new(frame_signed(&op.signed()), Position(prevs));
        let entry_hash = entry.hash();
        match self.dag.append(&entry) {
            AppendOutcome::Appended | AppendOutcome::Duplicate => {}
            // Unreachable: every prev was resolved from `source_to_entry`, so it is
            // present in the DAG, and the payload hashes to its own digest.
            other => {
                debug_assert!(false, "unexpected append outcome: {other:?}");
                return None;
            }
        }
        let id = op.id();
        self.source_to_entry.insert(id, entry_hash);
        self.entry_to_source.insert(entry_hash, id);
        self.decoded.insert(
            entry_hash,
            DecodedOp {
                author: op.author(),
                op: op.payload().clone(),
                ts_ms: op.timestamp_ms(),
                seq: op.seq_num(),
            },
        );
        Some(entry_hash)
    }

    /// Repeatedly attempt to lift parked ops until a full pass makes no progress,
    /// returning every entry lifted along the way.
    fn drain_pending(&mut self) -> Vec<EntryHash> {
        let mut lifted = Vec::new();
        loop {
            let parked = std::mem::take(&mut self.pending);
            let mut still_pending = Vec::with_capacity(parked.len());
            let mut progressed = false;
            for op in parked {
                if let Some(hash) = self.try_lift(&op) {
                    lifted.push(hash);
                    progressed = true;
                } else {
                    still_pending.push(op);
                }
            }
            self.pending = still_pending;
            if !progressed {
                break;
            }
        }
        lifted
    }

    /// The op ids of the current DAG frontier — the causal horizon a new local op
    /// should stamp into its `observed`. Deterministic (ascending entry-hash order).
    pub fn observed_frontier(&self) -> Vec<[u8; 32]> {
        self.dag
            .frontier()
            .0
            .iter()
            .filter_map(|entry| self.entry_to_source.get(entry).map(|id| id.0))
            .collect()
    }

    /// Author and sign a local op without mutating the in-memory projection.
    ///
    /// Durable runtimes use this two-phase surface to fsync the signed bytes
    /// before ingestion, so a storage failure cannot leave a visible but
    /// unrecoverable operation.
    pub fn prepare_commit(
        &self,
        key: &SigningKey,
        topic: &str,
        ts_micros: u64,
        op: WalkieOp,
    ) -> SignedOp {
        let author = AuthorId(*key.verifying_key().as_bytes());
        let head = self
            .heads
            .get(&author)
            .copied()
            .unwrap_or_else(LogHead::genesis);
        let observed = self.observed_frontier();
        let (signed, _advanced) =
            sign_op_for_topic_observing(key, &head, ts_micros, topic, observed, op);
        signed
    }

    /// Author, sign, verify, and ingest a new local op, returning the signed bytes
    /// for gossip. In-memory/test callers use this convenience wrapper; durable
    /// runtimes should call [`Self::prepare_commit`], persist, then ingest.
    pub fn commit(
        &mut self,
        key: &SigningKey,
        topic: &str,
        ts_micros: u64,
        op: WalkieOp,
    ) -> SignedOp {
        let signed = self.prepare_commit(key, topic, ts_micros, op);
        let verified = verify_signed_op(&signed).expect("a just-signed op verifies");
        self.ingest_verified(verified);
        signed
    }

    /// Materialize the room read model from the current DAG.
    pub fn view(&self) -> RoomView {
        let snapshot = self.dag.snapshot();
        let reach = ReachIndex::new(&snapshot);

        RoomView {
            pitches: BTreeSet::new(),
            pitch_authors: BTreeMap::new(),
            pieces: BTreeMap::new(),
            tuning: Some(TuningDefinition::twelve_tet()),
            pieces_locked: false,
            available_emojis: None,
        }
        .with_registers(self, &reach)
        .with_pitches(self, &reach)
        .with_pieces(self)
    }
}

impl RoomView {
    /// Pitches: content-keyed ADD-WINS. An add is live iff no same-key remove
    /// causally observed it (`is_ancestor(add, remove)`).
    fn with_pitches(mut self, store: &RoomStore, reach: &ReachIndex) -> Self {
        let Some(active_tuning) = self
            .tuning
            .as_ref()
            .and_then(|definition| definition.validate("active room tuning").ok())
        else {
            return self;
        };
        let mut adds: BTreeMap<TunedDegree, Vec<EntryHash>> = BTreeMap::new();
        let mut removes: BTreeMap<TunedDegree, Vec<EntryHash>> = BTreeMap::new();
        for (entry, decoded) in &store.decoded {
            match &decoded.op {
                WalkieOp::AddDegree { pitch } if pitch.validate(&active_tuning).is_ok() => {
                    adds.entry(*pitch).or_default().push(*entry)
                }
                WalkieOp::RemoveDegree { pitch } if pitch.validate(&active_tuning).is_ok() => {
                    removes.entry(*pitch).or_default().push(*entry)
                }
                _ => {}
            }
        }
        for (key, add_entries) in &adds {
            let key_removes = removes.get(key).map(Vec::as_slice).unwrap_or(&[]);
            let mut authors: BTreeSet<AuthorId> = BTreeSet::new();
            for add in add_entries {
                let killed = key_removes
                    .iter()
                    .any(|remove| reach.is_ancestor(add, remove));
                if !killed {
                    authors.insert(store.decoded[add].author);
                }
            }
            if !authors.is_empty() {
                self.pitches.insert(*key);
                self.pitch_authors.insert(*key, authors);
            }
        }
        self
    }

    /// Pieces: owner-gated, per-owner seq register. Only the owner's ops affect a
    /// piece; the greatest-seq lifecycle op decides liveness; the greatest-seq
    /// move decides position; emoji comes from the PutPiece.
    fn with_pieces(mut self, store: &RoomStore) -> Self {
        let Some(active_tuning) = self
            .tuning
            .as_ref()
            .and_then(|definition| definition.validate("active room tuning").ok())
        else {
            return self;
        };
        // (piece_id, owner, seq, emoji, pitch)
        let mut puts: Vec<(OpId, AuthorId, u64, String, TunedPeriodicPitch)> = Vec::new();
        // (remove_op_id, target_piece, author, seq)
        let mut removes: Vec<(OpId, OpId, AuthorId, u64)> = Vec::new();
        // (target_remove, author, seq)
        let mut unremoves: Vec<(OpId, AuthorId, u64)> = Vec::new();
        // (target_piece, author, seq, pitch)
        let mut moves: Vec<(OpId, AuthorId, u64, TunedPeriodicPitch)> = Vec::new();

        for (entry, decoded) in &store.decoded {
            let op_id = store.entry_to_source[entry];
            match &decoded.op {
                WalkieOp::PutPiece { emoji, pitch } if pitch.validate(&active_tuning).is_ok() => {
                    puts.push((op_id, decoded.author, decoded.seq, emoji.clone(), *pitch))
                }
                WalkieOp::RemovePiece { piece } => {
                    removes.push((op_id, *piece, decoded.author, decoded.seq))
                }
                WalkieOp::UnremovePiece { remove } => {
                    unremoves.push((*remove, decoded.author, decoded.seq))
                }
                WalkieOp::MovePiece { piece, pitch } if pitch.validate(&active_tuning).is_ok() => {
                    moves.push((*piece, decoded.author, decoded.seq, *pitch))
                }
                _ => {}
            }
        }

        for put in &puts {
            let piece_id = put.0;
            let owner = put.1;
            let put_seq = put.2;

            // The owner's removes of THIS piece, and the ids that unremoves may target.
            let owner_remove_ids: BTreeSet<OpId> = removes
                .iter()
                .filter(|r| r.2 == owner && r.1 == piece_id)
                .map(|r| r.0)
                .collect();

            // Greatest-seq lifecycle event decides liveness. Within one author's log
            // seqs are unique, so the max is unambiguous.
            let mut best_seq = put_seq;
            let mut alive = true;
            for r in removes.iter().filter(|r| r.2 == owner && r.1 == piece_id) {
                if r.3 > best_seq {
                    best_seq = r.3;
                    alive = false;
                }
            }
            for u in unremoves
                .iter()
                .filter(|u| u.1 == owner && owner_remove_ids.contains(&u.0))
            {
                if u.2 > best_seq {
                    best_seq = u.2;
                    alive = true;
                }
            }
            if !alive {
                continue;
            }

            // Position from the owner's greatest-seq valid move of this piece.
            let mut best_move: Option<(u64, TunedPeriodicPitch)> = None;
            for m in moves.iter().filter(|m| m.1 == owner && m.0 == piece_id) {
                if best_move.is_none_or(|(seq, _)| m.2 > seq) {
                    best_move = Some((m.2, m.3));
                }
            }
            let pitch = match best_move {
                Some((_, pitch)) => pitch,
                None => put.4,
            };

            self.pieces.insert(
                piece_id,
                Piece {
                    id: piece_id,
                    owner,
                    emoji: put.3.clone(),
                    pitch,
                },
            );
        }
        self
    }

    /// Tuning / config: cross-author registers resolved by causal maxima then
    /// max raw-bytes entry hash. Each config field is resolved independently.
    fn with_registers(mut self, store: &RoomStore, reach: &ReachIndex) -> Self {
        let mut tuning_writes: BTreeSet<EntryHash> = BTreeSet::new();
        let mut locked_writes: BTreeSet<EntryHash> = BTreeSet::new();
        let mut emoji_writes: BTreeSet<EntryHash> = BTreeSet::new();
        for (entry, decoded) in &store.decoded {
            match &decoded.op {
                WalkieOp::SetTuning { .. } => {
                    tuning_writes.insert(*entry);
                }
                WalkieOp::SetConfig {
                    pieces_locked,
                    available_emojis,
                } => {
                    if pieces_locked.is_some() {
                        locked_writes.insert(*entry);
                    }
                    if available_emojis.is_some() {
                        emoji_writes.insert(*entry);
                    }
                }
                _ => {}
            }
        }

        self.tuning = register::resolve(&tuning_writes, reach)
            .map(|winner| match &store.decoded[&winner].op {
                WalkieOp::SetTuning { definition } => definition.clone(),
                _ => unreachable!("tuning candidate is a SetTuning"),
            })
            .or_else(|| Some(TuningDefinition::twelve_tet()));
        self.pieces_locked = register::resolve(&locked_writes, reach)
            .map(|winner| match &store.decoded[&winner].op {
                WalkieOp::SetConfig {
                    pieces_locked: Some(locked),
                    ..
                } => *locked,
                _ => unreachable!("locked candidate carries pieces_locked"),
            })
            .unwrap_or(false);
        self.available_emojis = register::resolve(&emoji_writes, reach).map(|winner| match &store
            .decoded[&winner]
            .op
        {
            WalkieOp::SetConfig {
                available_emojis: Some(emojis),
                ..
            } => emojis.clone(),
            _ => unreachable!("emoji candidate carries available_emojis"),
        });
        self
    }
}

/// A live emoji piece.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Piece {
    pub id: OpId,
    pub owner: AuthorId,
    pub emoji: String,
    pub pitch: TunedPeriodicPitch,
}

/// The materialized room read model.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoomView {
    /// Live tuning-scoped degree keys.
    pub pitches: BTreeSet<TunedDegree>,
    /// For each live key, the authors that hold a live add of it.
    pub pitch_authors: BTreeMap<TunedDegree, BTreeSet<AuthorId>>,
    /// Live pieces keyed by their PutPiece op id.
    pub pieces: BTreeMap<OpId, Piece>,
    /// Resolved canonical room tuning. Built-in 12-TET is the default.
    pub tuning: Option<TuningDefinition>,
    /// Whether pieces are locked (default false).
    pub pieces_locked: bool,
    /// Room-wide available-emoji palette, if set.
    pub available_emojis: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::WalkieOp::*;
    use super::*;

    use super::super::ops::{signing_key_from_seed, verify_signed_op_for_topic};
    use super::super::test_support::{
        Peer, SEED_A, SEED_B, SEED_C, TOPIC, entryhash_set, oracle, tet_definition, tet_degree,
        tet_pitch, tuning_with_step,
    };

    fn ingest_in_order(base: &[VerifiedOp], order: &[usize]) -> RoomStore {
        let mut store = RoomStore::new();
        for &i in order {
            store.ingest_verified(base[i].clone());
        }
        store
    }

    #[test]
    fn prepared_commit_is_invisible_until_persistence_boundary_ingests_it() {
        let mut store = RoomStore::new();
        let key = signing_key_from_seed(&SEED_A);
        let signed = store.prepare_commit(
            &key,
            TOPIC,
            1,
            AddDegree {
                pitch: tet_degree(4),
            },
        );
        assert!(store.is_empty());
        assert!(store.view().pitches.is_empty());

        let verified = verify_signed_op_for_topic(&signed, TOPIC).unwrap();
        store.ingest_verified(verified);
        assert!(store.view().pitches.contains(&tet_degree(4)));
    }

    fn ingest(ops: &[VerifiedOp]) -> RoomStore {
        let order: Vec<usize> = (0..ops.len()).collect();
        ingest_in_order(ops, &order)
    }

    fn assert_parity(ops: &[VerifiedOp]) {
        assert_eq!(ingest(ops).view(), oracle(ops));
    }

    // ---------------------------------------------------------------------
    // Scenario builders
    // ---------------------------------------------------------------------

    /// Two concurrent adds of the same key + a remove observing only one.
    /// Add-wins: the unobserved add survives, so the key stays live.
    fn concurrent_add_remove() -> (Vec<VerifiedOp>, AuthorId) {
        let mut a = Peer::new(&SEED_A);
        let mut b = Peer::new(&SEED_B);
        let survivor = b.author();
        let a0 = a.sign(
            1,
            vec![],
            AddDegree {
                pitch: tet_degree(5),
            },
        );
        let b0 = b.sign(
            2,
            vec![],
            AddDegree {
                pitch: tet_degree(5),
            },
        );
        // remove: backlink = a0 (observes only its own add), never sees b0.
        let a1 = a.sign(
            3,
            vec![],
            RemoveDegree {
                pitch: tet_degree(5),
            },
        );
        (vec![a0, b0, a1], survivor)
    }

    fn rich_history() -> Vec<VerifiedOp> {
        let mut a = Peer::new(&SEED_A);
        let mut b = Peer::new(&SEED_B);
        let mut c = Peer::new(&SEED_C);

        // Degrees: 0 add-wins keeps B; 7 is fully removed.
        let a_add0 = a.sign(
            1,
            vec![],
            AddDegree {
                pitch: tet_degree(0),
            },
        );
        let b_add0 = b.sign(
            2,
            vec![],
            AddDegree {
                pitch: tet_degree(0),
            },
        );
        let a_rem0 = a.sign(
            3,
            vec![],
            RemoveDegree {
                pitch: tet_degree(0),
            },
        );
        let c_add7 = c.sign(
            4,
            vec![],
            AddDegree {
                pitch: tet_degree(7),
            },
        );
        let a_add7 = a.sign(
            5,
            vec![c_add7.hash()],
            AddDegree {
                pitch: tet_degree(7),
            },
        );
        // remove7: backlink = c_add7, observes a_add7 -> both adds in its past.
        let c_rem7 = c.sign(
            6,
            vec![a_add7.hash()],
            RemoveDegree {
                pitch: tet_degree(7),
            },
        );

        // Pieces: A owns; move by A wins position; a non-owner move by B ignored;
        // remove then unremove by A -> alive.
        let a_put = a.sign(
            7,
            vec![],
            PutPiece {
                emoji: "🌵".into(),
                pitch: tet_pitch(60),
            },
        );
        let piece = a_put.id();
        let a_mov = a.sign(
            8,
            vec![],
            MovePiece {
                piece,
                pitch: tet_pitch(72),
            },
        );
        let b_mov = b.sign(
            9,
            vec![],
            MovePiece {
                piece,
                pitch: tet_pitch(61),
            },
        );
        let a_rem_p = a.sign(10, vec![], RemovePiece { piece });
        let a_unrem = a.sign(
            11,
            vec![],
            UnremovePiece {
                remove: a_rem_p.id(),
            },
        );

        // Tuning: two concurrent writers -> register tiebreak.
        let a_tune = a.sign(
            12,
            vec![],
            SetTuning {
                definition: tet_definition(),
            },
        );
        let b_tune = b.sign(
            13,
            vec![],
            SetTuning {
                definition: tet_definition(),
            },
        );

        // Config: independent fields from different authors.
        let a_cfg = a.sign(
            14,
            vec![],
            SetConfig {
                pieces_locked: Some(true),
                available_emojis: None,
            },
        );
        let c_cfg = c.sign(
            15,
            vec![],
            SetConfig {
                pieces_locked: None,
                available_emojis: Some("🌵🎵".into()),
            },
        );

        vec![
            a_add0, b_add0, a_rem0, c_add7, a_add7, c_rem7, a_put, a_mov, b_mov, a_rem_p, a_unrem,
            a_tune, b_tune, a_cfg, c_cfg,
        ]
    }

    // ---------------------------------------------------------------------
    // Parity tests over adversarial histories
    // ---------------------------------------------------------------------

    #[test]
    fn add_wins_two_concurrent_adds_and_a_remove() {
        let (ops, survivor) = concurrent_add_remove();
        let view = ingest(&ops).view();
        assert!(
            view.pitches.contains(&tet_degree(5)),
            "add-wins keeps the key live"
        );
        assert_eq!(
            view.pitch_authors[&tet_degree(5)],
            BTreeSet::from([survivor])
        );
        assert_parity(&ops);
    }

    #[test]
    fn retract_then_recreate_is_live() {
        let mut a = Peer::new(&SEED_A);
        let author = a.author();
        let add0 = a.sign(
            1,
            vec![],
            AddDegree {
                pitch: tet_degree(3),
            },
        );
        let rem = a.sign(
            2,
            vec![],
            RemoveDegree {
                pitch: tet_degree(3),
            },
        );
        let add1 = a.sign(
            3,
            vec![],
            AddDegree {
                pitch: tet_degree(3),
            },
        );
        let ops = vec![add0, rem, add1];
        let view = ingest(&ops).view();
        assert!(
            view.pitches.contains(&tet_degree(3)),
            "re-add after remove resurrects"
        );
        assert_eq!(view.pitch_authors[&tet_degree(3)], BTreeSet::from([author]));
        assert_parity(&ops);
    }

    #[test]
    fn concurrent_remove_does_not_kill_add() {
        // A adds; B removes WITHOUT observing A's add (no backlink, no observed).
        let mut a = Peer::new(&SEED_A);
        let mut b = Peer::new(&SEED_B);
        let add = a.sign(
            1,
            vec![],
            AddDegree {
                pitch: tet_degree(9),
            },
        );
        let rem = b.sign(
            2,
            vec![],
            RemoveDegree {
                pitch: tet_degree(9),
            },
        );
        let ops = vec![add, rem];
        let view = ingest(&ops).view();
        assert!(
            view.pitches.contains(&tet_degree(9)),
            "a concurrent remove cannot kill the add"
        );
        assert_parity(&ops);
    }

    #[test]
    fn tuning_change_hides_old_contributions_without_reinterpreting_them() {
        let mut a = Peer::new(&SEED_A);
        let old = a.sign(
            1,
            vec![],
            AddDegree {
                pitch: tet_degree(7),
            },
        );
        let definition = tuning_with_step(700);
        let tuning = definition.validate("test").unwrap();
        let set = a.sign(
            2,
            vec![],
            SetTuning {
                definition: definition.clone(),
            },
        );
        let new_pitch = TunedDegree::new(&tuning, 1).unwrap();
        let new = a.sign(3, vec![], AddDegree { pitch: new_pitch });
        let ops = vec![old, set, new];
        let view = ingest(&ops).view();
        assert_eq!(view.tuning, Some(definition));
        assert!(!view.pitches.contains(&tet_degree(7)));
        assert!(view.pitches.contains(&new_pitch));
        assert_parity(&ops);
    }

    #[test]
    fn piece_move_remove_unremove_by_owner() {
        let mut a = Peer::new(&SEED_A);
        let owner = a.author();
        let put = a.sign(
            1,
            vec![],
            PutPiece {
                emoji: "🎵".into(),
                pitch: tet_pitch(60),
            },
        );
        let piece = put.id();
        let mov = a.sign(
            2,
            vec![],
            MovePiece {
                piece,
                pitch: tet_pitch(72),
            },
        );
        let rem = a.sign(3, vec![], RemovePiece { piece });
        let unrem = a.sign(4, vec![], UnremovePiece { remove: rem.id() });
        let ops = vec![put, mov, rem, unrem];
        let view = ingest(&ops).view();
        let p = &view.pieces[&piece];
        assert_eq!(p.owner, owner);
        assert_eq!(p.emoji, "🎵");
        assert_eq!(p.pitch, tet_pitch(72), "greatest-seq move sets position");
        assert_parity(&ops);
    }

    #[test]
    fn non_owner_piece_ops_are_ignored() {
        let mut a = Peer::new(&SEED_A);
        let mut b = Peer::new(&SEED_B);
        let put = a.sign(
            1,
            vec![],
            PutPiece {
                emoji: "🌵".into(),
                pitch: tet_pitch(60),
            },
        );
        let piece = put.id();
        // B (not the owner) tries to move and remove A's piece.
        let b_mov = b.sign(
            2,
            vec![put.hash()],
            MovePiece {
                piece,
                pitch: tet_pitch(72),
            },
        );
        let b_rem = b.sign(3, vec![put.hash()], RemovePiece { piece });
        let ops = vec![put, b_mov, b_rem];
        let view = ingest(&ops).view();
        let p = &view.pieces[&piece];
        assert_eq!(p.pitch, tet_pitch(60), "non-owner move ignored");
        assert!(view.pieces.contains_key(&piece), "non-owner remove ignored");
        assert_parity(&ops);
    }

    /// W17 — the drag-divergence scenario at the data layer: A owns a piece,
    /// non-owner B moves it, and BOTH peers ingest BOTH ops (in opposite
    /// orders). Every store — including B's, whose own view never showed the
    /// move — converges on the owner's position. So any UI that displayed B's
    /// move was showing state without data; the data was never the bug
    /// (docs/research/reactive-effectful-ui-adapter-design.md §1, §6.1).
    #[test]
    fn w17_non_owner_move_converges_to_owner_position() {
        let mut a = Peer::new(&SEED_A);
        let mut b = Peer::new(&SEED_B);
        let put = a.sign(
            1,
            vec![],
            PutPiece {
                emoji: "🌵".into(),
                pitch: tet_pitch(60),
            },
        );
        let piece = put.id();
        // B is not the owner; it moves A's piece while observing the put.
        let b_mov = b.sign(
            2,
            vec![put.hash()],
            MovePiece {
                piece,
                pitch: tet_pitch(64),
            },
        );
        let ops = vec![put, b_mov];

        // Two independent peers ingest the same ops in opposite orders (the
        // reversed order also exercises strict deferral of the move).
        let store_a = ingest_in_order(&ops, &[0, 1]);
        let store_b = ingest_in_order(&ops, &[1, 0]);

        assert_eq!(store_a.view(), store_b.view(), "peers converge");
        assert_eq!(entryhash_set(&store_a), entryhash_set(&store_b));
        for (name, store) in [("A", &store_a), ("B", &store_b)] {
            let view = store.view();
            let held = &view.pieces[&piece];
            assert_eq!(
                held.pitch,
                tet_pitch(60),
                "{name} holds the owner's position, not B's move"
            );
        }
        assert_parity(&ops);
    }

    /// The mechanism behind the drag divergence: an owner-gated-rejected op
    /// produces ZERO view delta, so a diff-driven projection has no correction
    /// to emit. Any snap-back must therefore come from *rendering the
    /// projection*, never from a view event that will never fire
    /// (docs/research/reactive-effectful-ui-adapter-design.md §1.3, §6.2).
    #[test]
    fn non_owner_move_produces_no_view_delta() {
        // B's store ingests A's put, then commits B's own (non-owner) move.
        let mut a = Peer::new(&SEED_A);
        let put = a.sign(
            1,
            vec![],
            PutPiece {
                emoji: "🌵".into(),
                pitch: tet_pitch(60),
            },
        );
        let piece = put.id();

        let mut store = RoomStore::new();
        store.ingest_verified(put);
        let before = store.view();

        let b_key = signing_key_from_seed(&SEED_B);
        store.commit(
            &b_key,
            TOPIC,
            2,
            MovePiece {
                piece,
                pitch: tet_pitch(64),
            },
        );

        assert_eq!(
            store.view(),
            before,
            "inert op => zero delta => a diff-driven projection has nothing to \
             say; snap-back must come from rendering the projection"
        );
    }

    #[test]
    fn concurrent_set_tuning_uses_register_rule() {
        let mut a = Peer::new(&SEED_A);
        let mut b = Peer::new(&SEED_B);
        let a_tune = a.sign(
            1,
            vec![],
            SetTuning {
                definition: tuning_with_step(600),
            },
        );
        let b_tune = b.sign(
            2,
            vec![],
            SetTuning {
                definition: tuning_with_step(700),
            },
        );
        let ops = vec![a_tune, b_tune];
        let view = ingest(&ops).view();
        assert!(view.tuning.is_some(), "some concurrent writer wins");
        assert_parity(&ops);
    }

    #[test]
    fn set_config_fields_resolve_independently() {
        let mut a = Peer::new(&SEED_A);
        let mut b = Peer::new(&SEED_B);
        let locked = a.sign(
            1,
            vec![],
            SetConfig {
                pieces_locked: Some(true),
                available_emojis: None,
            },
        );
        let emojis = b.sign(
            2,
            vec![],
            SetConfig {
                pieces_locked: None,
                available_emojis: Some("🎹".into()),
            },
        );
        let ops = vec![locked, emojis];
        let view = ingest(&ops).view();
        assert!(view.pieces_locked, "locked field comes from A");
        assert_eq!(
            view.available_emojis.as_deref(),
            Some("🎹"),
            "emoji field comes from B"
        );
        assert_parity(&ops);
    }

    #[test]
    fn rich_history_matches_oracle_with_expected_values() {
        let ops = rich_history();
        let store = ingest(&ops);
        let view = store.view();

        assert_eq!(
            view.pitches,
            BTreeSet::from([tet_degree(0)]),
            "degree 7 is fully removed and degree 0 survives add-wins"
        );
        let b_author = Peer::new(&SEED_B).author();
        assert_eq!(
            view.pitch_authors[&tet_degree(0)],
            BTreeSet::from([b_author])
        );
        assert_eq!(view.pieces.len(), 1);
        let piece = view.pieces.values().next().unwrap();
        assert_eq!(piece.emoji, "🌵");
        assert_eq!(
            piece.pitch,
            tet_pitch(72),
            "owner move wins over non-owner move"
        );
        assert!(view.tuning.is_some());
        assert!(view.pieces_locked);
        assert_eq!(view.available_emojis.as_deref(), Some("🌵🎵"));

        assert_eq!(view, oracle(&ops));
    }

    // ---------------------------------------------------------------------
    // Out-of-order convergence
    // ---------------------------------------------------------------------

    #[test]
    fn out_of_order_ingest_converges_and_is_deterministic() {
        let base = rich_history();
        let n = base.len();
        let expected = oracle(&base);

        let identity: Vec<usize> = (0..n).collect();
        let reversed: Vec<usize> = (0..n).rev().collect();
        let mut interleave: Vec<usize> = (0..n).step_by(2).collect();
        interleave.extend((1..n).step_by(2));

        let baseline = ingest_in_order(&base, &identity);
        let baseline_view = baseline.view();
        let baseline_hashes = entryhash_set(&baseline);
        assert_eq!(baseline_view, expected);
        assert!(baseline.pending.is_empty(), "everything drains");

        for order in [identity.clone(), reversed, interleave] {
            let store = ingest_in_order(&base, &order);
            assert!(store.pending.is_empty(), "order {order:?} must fully drain");
            assert_eq!(
                store.view(),
                baseline_view,
                "view differs for order {order:?}"
            );
            assert_eq!(
                entryhash_set(&store),
                baseline_hashes,
                "entry-hash identity must be order-independent for {order:?}"
            );
        }
    }

    // ---------------------------------------------------------------------
    // Golden vector: pins the framing/lift so a format change is caught.
    // ---------------------------------------------------------------------

    #[test]
    fn golden_vector_entry_hash_and_pitches() {
        let key = signing_key_from_seed(&SEED_A);
        let mut store = RoomStore::new();
        let signed0 = store.commit(
            &key,
            TOPIC,
            1_700_000_000_000_000,
            AddDegree {
                pitch: tet_degree(0),
            },
        );
        store.commit(
            &key,
            TOPIC,
            1_700_000_000_000_001,
            AddDegree {
                pitch: tet_degree(4),
            },
        );

        let id0 = verify_signed_op(&signed0).unwrap().id();
        let eh0 = store.source_to_entry[&id0];
        assert_eq!(
            eh0.to_hex(),
            "9e217937915d7f0969a214c904ab6adb00da97c873d89407d82b7e5bf0bf3568",
            "golden entry hash for the first committed op",
        );
        assert_eq!(
            store.view().pitches,
            BTreeSet::from([tet_degree(0), tet_degree(4)])
        );
    }

    // ---------------------------------------------------------------------
    // Mutation test: prove the parity check has teeth.
    // ---------------------------------------------------------------------

    /// A DELIBERATELY WRONG oracle: pitches follow remove-wins (any remove kills
    /// the key), which disagrees with add-wins on a concurrent remove.
    fn wrong_oracle_remove_wins(ops: &[VerifiedOp]) -> RoomView {
        let mut view = oracle(ops);
        let mut adds: BTreeMap<TunedDegree, BTreeSet<AuthorId>> = BTreeMap::new();
        let mut has_remove: BTreeSet<TunedDegree> = BTreeSet::new();
        for op in ops {
            match op.payload() {
                AddDegree { pitch } => {
                    adds.entry(*pitch).or_default().insert(op.author());
                }
                RemoveDegree { pitch } => {
                    has_remove.insert(*pitch);
                }
                _ => {}
            }
        }
        view.pitches = BTreeSet::new();
        view.pitch_authors = BTreeMap::new();
        for (key, authors) in adds {
            if !has_remove.contains(&key) {
                view.pitches.insert(key);
                view.pitch_authors.insert(key, authors);
            }
        }
        view
    }

    #[test]
    fn parity_check_detects_a_broken_rule() {
        let (ops, _) = concurrent_add_remove();
        let real = ingest(&ops).view();

        // The correct oracle agrees with the real view (add-wins keeps the key).
        assert_eq!(real, oracle(&ops));
        assert!(real.pitches.contains(&tet_degree(5)));

        // The broken oracle drops the key -> it MUST disagree, proving the parity
        // assertion is not vacuously true.
        let wrong = wrong_oracle_remove_wins(&ops);
        assert!(!wrong.pitches.contains(&tet_degree(5)));
        assert_ne!(real, wrong, "a broken pitch rule must be caught by parity");
    }
}
