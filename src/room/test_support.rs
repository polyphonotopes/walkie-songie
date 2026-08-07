//! Shared test scaffolding for the room data layer.
//!
//! Gated behind `#[cfg(any(test, feature = "test-support"))]` so it is reachable
//! both from `store.rs`'s in-crate unit tests and from the out-of-crate `tests/`
//! integration suite (which enables the `test-support` feature). It holds the
//! hand-authoring [`Peer`], the INDEPENDENT [`oracle`], and the [`entryhash_set`]
//! helper — everything a convergence assertion needs that is not itself part of
//! the production `RoomStore` surface.

use std::collections::{BTreeMap, BTreeSet};

use hhhs_core::{Digest, EntryHash, Header, Position, entry_hash};

use super::ops::WalkieOp::*;
use super::ops::{
    AuthorId, LogHead, OpId, SignedOp, SigningKey, VerifiedOp, WalkieOp,
    sign_op_for_topic_observing, signing_key_from_seed, verify_signed_op,
};
use super::store::{Piece, RoomStore, RoomView};
use crate::tuning::{TunedDegree, TunedPeriodicPitch, Tuning, TuningDefinition};

/// The room topic every test op is bound to.
pub const TOPIC: &str = "sunny-garden-melody";

/// Stable author seeds, so op hashes — and therefore every hash-order tiebreak —
/// are identical across runs.
pub const SEED_A: [u8; 32] = [1u8; 32];
pub const SEED_B: [u8; 32] = [2u8; 32];
pub const SEED_C: [u8; 32] = [3u8; 32];

pub fn tet_tuning() -> Tuning {
    Tuning::twelve_tet()
}

pub fn tet_definition() -> TuningDefinition {
    TuningDefinition::twelve_tet()
}

pub fn tet_degree(index: u16) -> TunedDegree {
    TunedDegree::new(&tet_tuning(), index).expect("test degree is valid")
}

/// Compatibility helper for the old MIDI-like fixture coordinates where 60
/// meant C4. The durable v3 value is an exact degree + signed period.
pub fn tet_pitch(absolute: i32) -> TunedPeriodicPitch {
    let relative = absolute - 60;
    TunedPeriodicPitch::new(
        &tet_tuning(),
        relative.rem_euclid(12) as u16,
        relative.div_euclid(12),
    )
    .expect("test pitch is valid")
}

pub fn tuning_with_step(step_cents: u16) -> TuningDefinition {
    TuningDefinition::new(
        format!("test tuning {step_cents}\n2\n{step_cents}.0\n1200.0\n"),
        None,
    )
    .expect("test tuning definition is valid")
}

/// Framing tag prefixed to the verbatim signed bytes when they become a kernel
/// entry payload. A private copy of `store::SIGNED_OP_FRAME_MAGIC`, kept here so
/// the oracle recomputes entry hashes fully INDEPENDENTLY of the store (never
/// borrowing the store's private framing helper).
const SIGNED_OP_FRAME_MAGIC: &[u8] = b"walkie.hhhs.signed-op/1";

/// Independent re-implementation of the store's op framing:
/// `MAGIC ++ len(header) ++ header ++ len(payload) ++ payload` (u64 little-endian
/// lengths). Kept byte-identical to `store::frame_signed` so the oracle's entry
/// hashes match the store's — the register tiebreak is defined over `EntryHash`.
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

/// A test author that chains ops onto its own log and lets us stamp an explicit
/// `observed` horizon, so we can build adversarial concurrency by hand (something
/// `RoomStore::commit`, which observes the frontier, cannot).
pub struct Peer {
    pub key: SigningKey,
    pub head: LogHead,
}

impl Peer {
    pub fn new(seed: &[u8; 32]) -> Self {
        Self {
            key: signing_key_from_seed(seed),
            head: LogHead::genesis(),
        }
    }
    pub fn author(&self) -> AuthorId {
        AuthorId(*self.key.verifying_key().as_bytes())
    }
    pub fn sign(&mut self, ts: u64, observed: Vec<[u8; 32]>, op: WalkieOp) -> VerifiedOp {
        let (signed, advanced) =
            sign_op_for_topic_observing(&self.key, &self.head, ts, TOPIC, observed, op);
        self.head = advanced;
        verify_signed_op(&signed).expect("signed op verifies")
    }
}

/// The lifted entries' hashes as hex, for cross-store identity comparison. Reads
/// the store's public `entry_hashes()` accessor (never a private field).
pub fn entryhash_set(store: &RoomStore) -> BTreeSet<String> {
    store.entry_hashes().iter().map(|e| e.to_hex()).collect()
}

// ---------------------------------------------------------------------
// The INDEPENDENT oracle. Derives ancestry from the OP GRAPH over `OpId`s
// (a different representation than `ReachIndex` over `EntryHash`es) and
// never calls `RoomStore`/`ReachIndex`. It re-derives entry hashes purely
// for the register tiebreak, which the kernel defines over `EntryHash`.
// ---------------------------------------------------------------------
pub fn oracle(ops: &[VerifiedOp]) -> RoomView {
    let by_id: BTreeMap<OpId, &VerifiedOp> = ops.iter().map(|o| (o.id(), o)).collect();

    // An op's causal parents: backlink ∪ observed, as OpIds.
    fn refs_of(op: &VerifiedOp) -> BTreeSet<OpId> {
        let mut refs = BTreeSet::new();
        if let Some(backlink) = op.backlink() {
            refs.insert(OpId(backlink));
        }
        for observed in op.observed() {
            refs.insert(OpId(*observed));
        }
        refs
    }

    // Strict-deferral mirror: an op is admissible iff every ref is admissible
    // (hence present). Fixpoint.
    let mut admissible: BTreeSet<OpId> = BTreeSet::new();
    loop {
        let mut changed = false;
        for (id, op) in &by_id {
            if admissible.contains(id) {
                continue;
            }
            if refs_of(op).iter().all(|r| admissible.contains(r)) {
                admissible.insert(*id);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let ids: Vec<OpId> = admissible.iter().copied().collect();
    let refs: BTreeMap<OpId, BTreeSet<OpId>> =
        ids.iter().map(|id| (*id, refs_of(by_id[id]))).collect();

    // Transitive closure over OpIds -> strict ancestors.
    let mut anc: BTreeMap<OpId, BTreeSet<OpId>> = refs.clone();
    loop {
        let mut changed = false;
        for id in &ids {
            let parents: Vec<OpId> = refs[id].iter().copied().collect();
            let mut extra: Vec<OpId> = Vec::new();
            for p in &parents {
                extra.extend(anc[p].iter().copied());
            }
            let set = anc.get_mut(id).expect("known id");
            for x in extra {
                if set.insert(x) {
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    let is_anc = |a: &OpId, b: &OpId| anc.get(b).is_some_and(|s| s.contains(a));

    // Independent entry-hash recomputation (same framing + kernel header),
    // recursive over the OpId graph. Used ONLY for the register tiebreak.
    fn entry_hash_of(
        id: OpId,
        by_id: &BTreeMap<OpId, &VerifiedOp>,
        refs: &BTreeMap<OpId, BTreeSet<OpId>>,
        memo: &mut BTreeMap<OpId, EntryHash>,
    ) -> EntryHash {
        if let Some(h) = memo.get(&id) {
            return *h;
        }
        let payload_digest = Digest::of(&frame_signed(&by_id[&id].signed()));
        let mut prevs = BTreeSet::new();
        for r in &refs[&id] {
            prevs.insert(entry_hash_of(*r, by_id, refs, memo));
        }
        let hash = entry_hash(&Header {
            payload_digest,
            prevs: Position(prevs),
        });
        memo.insert(id, hash);
        hash
    }
    let mut eh: BTreeMap<OpId, EntryHash> = BTreeMap::new();
    for id in &ids {
        entry_hash_of(*id, &by_id, &refs, &mut eh);
    }

    let resolve = |candidates: &BTreeSet<OpId>| -> Option<OpId> {
        candidates
            .iter()
            .copied()
            .filter(|c| !candidates.iter().any(|o| o != c && is_anc(c, o)))
            .max_by(|a, b| eh[a].as_bytes().cmp(eh[b].as_bytes()))
    };

    // Resolve tuning first: every musical projection is scoped to its winner.
    let tuning_candidates: BTreeSet<OpId> = ids
        .iter()
        .copied()
        .filter(|id| matches!(by_id[id].payload(), SetTuning { .. }))
        .collect();
    let tuning = resolve(&tuning_candidates)
        .map(|id| match by_id[&id].payload() {
            SetTuning { definition } => definition.clone(),
            _ => unreachable!(),
        })
        .or_else(|| Some(tet_definition()));
    let active_tuning = tuning
        .as_ref()
        .and_then(|definition| definition.validate("oracle tuning").ok());

    // --- Degrees: content-keyed add-wins, current tuning only ---
    let mut adds: BTreeMap<TunedDegree, Vec<OpId>> = BTreeMap::new();
    let mut removes: BTreeMap<TunedDegree, Vec<OpId>> = BTreeMap::new();
    for id in &ids {
        match by_id[id].payload() {
            AddDegree { pitch }
                if active_tuning
                    .as_ref()
                    .is_some_and(|tuning| pitch.validate(tuning).is_ok()) =>
            {
                adds.entry(*pitch).or_default().push(*id)
            }
            RemoveDegree { pitch }
                if active_tuning
                    .as_ref()
                    .is_some_and(|tuning| pitch.validate(tuning).is_ok()) =>
            {
                removes.entry(*pitch).or_default().push(*id)
            }
            _ => {}
        }
    }
    let mut pitches = BTreeSet::new();
    let mut pitch_authors = BTreeMap::new();
    for (key, add_ids) in &adds {
        let empty = Vec::new();
        let rem_ids = removes.get(key).unwrap_or(&empty);
        let mut authors = BTreeSet::new();
        for a in add_ids {
            if !rem_ids.iter().any(|r| is_anc(a, r)) {
                authors.insert(by_id[a].author());
            }
        }
        if !authors.is_empty() {
            pitches.insert(*key);
            pitch_authors.insert(*key, authors);
        }
    }

    // --- Config registers (resolved first: the lock gate reads them per op) ---
    let mut locked_c = BTreeSet::new();
    let mut emoji_c = BTreeSet::new();
    for id in &ids {
        match by_id[id].payload() {
            SetConfig {
                pieces_locked,
                available_emojis,
            } => {
                if pieces_locked.is_some() {
                    locked_c.insert(*id);
                }
                if available_emojis.is_some() {
                    emoji_c.insert(*id);
                }
            }
            _ => {}
        }
    }

    // --- Pieces: SHARED — cross-author observed-remove + causal position register.
    // Independent mirror of `RoomStore::with_pieces`: owner is attribution only,
    // `pieces_locked` (resolved over an op's causal past) is the consent gate. ---
    // put:      (piece_id, owner, emoji, put_pitch)
    let mut puts: Vec<(OpId, AuthorId, String, TunedPeriodicPitch)> = Vec::new();
    // move:     (move_id, target_piece, pitch)
    let mut moves: Vec<(OpId, OpId, TunedPeriodicPitch)> = Vec::new();
    // remove:   (remove_id, target_piece)
    let mut piece_removes: Vec<(OpId, OpId)> = Vec::new();
    // unremove: (unremove_id, target_remove)
    let mut unremoves: Vec<(OpId, OpId)> = Vec::new();
    for id in &ids {
        let op = by_id[id];
        match op.payload() {
            PutPiece { emoji, pitch }
                if active_tuning
                    .as_ref()
                    .is_some_and(|tuning| pitch.validate(tuning).is_ok()) =>
            {
                puts.push((*id, op.author(), emoji.clone(), *pitch))
            }
            MovePiece { piece, pitch }
                if active_tuning
                    .as_ref()
                    .is_some_and(|tuning| pitch.validate(tuning).is_ok()) =>
            {
                moves.push((*id, *piece, *pitch))
            }
            RemovePiece { piece } => piece_removes.push((*id, *piece)),
            UnremovePiece { remove } => unremoves.push((*id, *remove)),
            _ => {}
        }
    }

    // Whether the lock register, resolved over ONLY the causal ancestors of `op`,
    // reads `true` — the per-op consent gate (matches the store's `locked_as_of`).
    let locked_as_of = |op: &OpId| -> bool {
        let observed: BTreeSet<OpId> = locked_c
            .iter()
            .copied()
            .filter(|write| is_anc(write, op))
            .collect();
        resolve(&observed).is_some_and(|winner| {
            matches!(
                by_id[&winner].payload(),
                SetConfig {
                    pieces_locked: Some(true),
                    ..
                }
            )
        })
    };

    let mut pieces = BTreeMap::new();
    for (piece_id, owner, emoji, put_pitch) in &puts {
        // Effective removes: not lock-suppressed, not overridden by an observing
        // (and itself unlocked) unremove.
        let effective_removes: Vec<OpId> = piece_removes
            .iter()
            .filter(|(_, target)| target == piece_id)
            .filter(|(rem_id, _)| {
                if locked_as_of(rem_id) {
                    return false;
                }
                let overridden = unremoves.iter().any(|(un_id, target_rem)| {
                    target_rem == rem_id && is_anc(rem_id, un_id) && !locked_as_of(un_id)
                });
                !overridden
            })
            .map(|(rem_id, _)| *rem_id)
            .collect();

        // Adds = put + non-suppressed moves; survives iff no effective remove
        // observed it (add-wins).
        let survives = |add: &OpId| !effective_removes.iter().any(|rem| is_anc(add, rem));
        let mut surviving: BTreeSet<OpId> = BTreeSet::new();
        if survives(piece_id) {
            surviving.insert(*piece_id);
        }
        for (move_id, _, _) in moves.iter().filter(|(_, target, _)| target == piece_id) {
            if !locked_as_of(move_id) && survives(move_id) {
                surviving.insert(*move_id);
            }
        }
        if surviving.is_empty() {
            continue;
        }

        // Position = register winner over the surviving adds' pitches.
        let pitch = resolve(&surviving)
            .map(|winner| match by_id[&winner].payload() {
                PutPiece { pitch, .. } | MovePiece { pitch, .. } => *pitch,
                _ => unreachable!("a surviving add is a PutPiece or MovePiece"),
            })
            .unwrap_or(*put_pitch);
        pieces.insert(
            *piece_id,
            Piece {
                id: *piece_id,
                owner: *owner,
                emoji: emoji.clone(),
                pitch,
            },
        );
    }

    let pieces_locked = resolve(&locked_c)
        .map(|id| match by_id[&id].payload() {
            SetConfig {
                pieces_locked: Some(b),
                ..
            } => *b,
            _ => unreachable!(),
        })
        .unwrap_or(false);
    let available_emojis = resolve(&emoji_c).map(|id| match by_id[&id].payload() {
        SetConfig {
            available_emojis: Some(s),
            ..
        } => s.clone(),
        _ => unreachable!(),
    });

    RoomView {
        pitches,
        pitch_authors,
        pieces,
        tuning,
        pieces_locked,
        available_emojis,
    }
}
