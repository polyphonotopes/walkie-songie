//! Shared fixture generators for the M0 benchmark suite
//! (`docs/research/performance-benchmark-suite.md` §8).
//!
//! Everything here is built from walkie-songie's PUBLIC op/store surface plus
//! hhhs-core's kernel types — no `#[cfg(test)]` internals — so the benches
//! exercise exactly the embeddable core the device would run (fold + ops + RBSR,
//! no iroh/tokio/wasm). Fixtures are seeded and deterministic.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use hhhs_core::reconciliation::Index;
use hhhs_core::{Digest, Entry, EntryHash, MemDagStore, Position, SortKey};

use walkie_songie::room::ops::{
    LogHead, OpId, SignedOp, SigningKey, VerifiedOp, VersionedOp, WalkieOp,
    sign_op_for_topic_observing, signing_key_from_seed, verify_signed_op,
};
use walkie_songie::room::store::RoomStore;
use walkie_songie::tuning::{TunedDegree, TunedPeriodicPitch, Tuning, TuningDefinition};

/// The room topic every bench op is bound to.
pub const TOPIC: &str = "walkie-bench-room";

/// A fixed session salt for RBSR fixtures (keys the fingerprint monoid only).
pub const SALT: [u8; 16] = [0x5a; 16];

fn tuning() -> Tuning {
    Tuning::twelve_tet()
}

/// A valid 12-TET degree (index taken mod 12 so callers can pass a running
/// counter without tripping the tuning bound).
pub fn degree(i: u16) -> TunedDegree {
    TunedDegree::new(&tuning(), i % 12).expect("12-TET degree is valid")
}

/// A valid periodic pitch relative to the tuning's reference.
pub fn pitch(relative: i32) -> TunedPeriodicPitch {
    TunedPeriodicPitch::new(
        &tuning(),
        relative.rem_euclid(12) as u16,
        relative.div_euclid(12),
    )
    .expect("12-TET pitch is valid")
}

// ---------------------------------------------------------------------------
// A hand-authoring peer (a local re-implementation of the test-support `Peer`
// over the public signing API, so the benches need no test-only features).
// ---------------------------------------------------------------------------

pub struct Author {
    key: SigningKey,
    head: LogHead,
}

impl Author {
    /// A distinct author per `seed_byte`, so op/entry hashes are stable.
    pub fn new(seed_byte: u8) -> Self {
        Self {
            key: signing_key_from_seed(&[seed_byte; 32]),
            head: LogHead::genesis(),
        }
    }

    /// Sign, verify, and advance the log head. `observed` is the causal horizon
    /// stamped into the op (empty = a pure per-author chain via the backlink).
    pub fn sign(&mut self, ts: u64, observed: Vec<[u8; 32]>, op: WalkieOp) -> VerifiedOp {
        let (signed, advanced) =
            sign_op_for_topic_observing(&self.key, &self.head, ts, TOPIC, observed, op);
        self.head = advanced;
        verify_signed_op(&signed).expect("a just-signed op verifies")
    }
}

// ---------------------------------------------------------------------------
// Op-log fixtures (real VerifiedOps, ingestible into a RoomStore).
// ---------------------------------------------------------------------------

/// A single-author chain of `n` AddDegree ops. Each op's only prev is its own
/// backlink, so history depth H = N: this is the worst-case linear log the
/// ReachIndex closure is Θ(N²) over.
pub fn linear_ops(n: usize) -> Vec<VerifiedOp> {
    let mut a = Author::new(1);
    (0..n)
        .map(|i| {
            a.sign(
                1_000 + i as u64,
                vec![],
                WalkieOp::AddDegree {
                    pitch: degree(i as u16),
                },
            )
        })
        .collect()
}

/// `heads` disjoint author chains, round-robin, totalling `n` ops. Frontier
/// width is `heads` and each op's causal history is bounded by its chain length
/// (~N/heads), so Σ|ancestors| ≈ N²/(2·heads) — the "wide fan-out" shape that
/// isolates the H term of the Θ(N·H) reach cost.
pub fn forked_ops(n: usize, heads: usize) -> Vec<VerifiedOp> {
    let mut authors: Vec<Author> = (0..heads).map(|h| Author::new((h + 1) as u8)).collect();
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let h = i % heads;
        out.push(authors[h].sign(
            1_000 + i as u64,
            vec![],
            WalkieOp::AddDegree {
                pitch: degree(i as u16),
            },
        ));
    }
    out
}

/// `p` pieces put then moved once each by one owner — a corpus whose fold cost
/// is dominated by `with_pieces` (O(P·(R+M+U))).
pub fn piece_ops(p: usize) -> Vec<VerifiedOp> {
    let mut a = Author::new(1);
    let mut out = Vec::new();
    let mut piece_ids = Vec::new();
    for i in 0..p {
        let put = a.sign(
            1_000 + i as u64,
            vec![],
            WalkieOp::PutPiece {
                emoji: "🎵".into(),
                pitch: pitch(i as i32),
            },
        );
        piece_ids.push(put.id());
        out.push(put);
    }
    for (i, pid) in piece_ids.iter().enumerate() {
        out.push(a.sign(
            5_000 + i as u64,
            vec![],
            WalkieOp::MovePiece {
                piece: *pid,
                pitch: pitch(i as i32 + 1),
            },
        ));
    }
    out
}

/// `n` chained register writes (alternating SetConfig / SetTuning) — a corpus
/// whose fold cost is dominated by `with_registers` (`register::resolve`).
pub fn register_ops(n: usize) -> Vec<VerifiedOp> {
    let mut a = Author::new(1);
    (0..n)
        .map(|i| {
            if i % 2 == 0 {
                a.sign(
                    1_000 + i as u64,
                    vec![],
                    WalkieOp::SetConfig {
                        pieces_locked: Some(i % 4 == 0),
                        available_emojis: None,
                    },
                )
            } else {
                a.sign(
                    1_000 + i as u64,
                    vec![],
                    WalkieOp::SetTuning {
                        definition: TuningDefinition::twelve_tet(),
                    },
                )
            }
        })
        .collect()
}

/// One hot degree with `adds` concurrent adds and `removes` concurrent removes,
/// every op from a distinct author with an empty horizon so no remove is in any
/// add's causal past. `with_pitches` must then run all A·R `is_ancestor`
/// verdicts (none short-circuit), exposing the quadratic liveness cost.
/// Requires `adds + removes < 255`.
pub fn hot_key_ops(adds: usize, removes: usize) -> Vec<VerifiedOp> {
    assert!(adds + removes < 255, "distinct-author seeds are one byte");
    let d = degree(0);
    let mut out = Vec::with_capacity(adds + removes);
    for i in 0..adds {
        let mut a = Author::new((1 + i) as u8);
        out.push(a.sign(1_000 + i as u64, vec![], WalkieOp::AddDegree { pitch: d }));
    }
    for j in 0..removes {
        let mut r = Author::new((1 + adds + j) as u8);
        out.push(r.sign(2_000 + j as u64, vec![], WalkieOp::RemoveDegree { pitch: d }));
    }
    out
}

/// Ingest a fixture into a fresh store (creation order = a valid causal order,
/// so everything lifts, nothing parks).
pub fn store_from_ops(ops: &[VerifiedOp]) -> RoomStore {
    let mut store = RoomStore::new();
    for op in ops {
        store.ingest_verified(op.clone());
    }
    store
}

/// An op-id → VerifiedOp index, for the RBSR session driver's byte transfer.
pub fn by_id(ops: &[VerifiedOp]) -> BTreeMap<OpId, VerifiedOp> {
    ops.iter().map(|o| (o.id(), o.clone())).collect()
}

// ---------------------------------------------------------------------------
// Ops-bench fixtures.
// ---------------------------------------------------------------------------

fn wide_horizon(width: usize) -> Vec<[u8; 32]> {
    (0..width)
        .map(|i| {
            let mut b = [0u8; 32];
            b[..8].copy_from_slice(&(i as u64).to_le_bytes());
            b
        })
        .collect()
}

/// A representative unsigned AddDegree envelope with a frontier of `width`
/// observed entries (drives the 32 B/observed-entry wire inflation).
pub fn sample_versioned(width: usize) -> VersionedOp {
    VersionedOp::current_for_topic(
        WalkieOp::AddDegree { pitch: degree(4) },
        1_700_000_000_000_000,
        TOPIC,
    )
    .observing(wide_horizon(width))
}

/// A signed AddDegree op with a frontier of `width` observed entries.
pub fn sample_signed(width: usize) -> SignedOp {
    let key = signing_key_from_seed(&[1u8; 32]);
    let (signed, _) = sign_op_for_topic_observing(
        &key,
        &LogHead::genesis(),
        1_700_000_000_000_000,
        TOPIC,
        wide_horizon(width),
        WalkieOp::AddDegree { pitch: degree(4) },
    );
    signed
}

pub fn bench_signing_key() -> SigningKey {
    signing_key_from_seed(&[1u8; 32])
}

/// A store of `n` chained ops plus one more op, ready to lift immediately on
/// ingest (its causal past is already present) — the "past-complete" ingest.
pub fn ingest_fixture(n: usize) -> (RoomStore, VerifiedOp) {
    let mut a = Author::new(1);
    let mut store = RoomStore::new();
    for i in 0..n {
        let op = a.sign(
            1_000 + i as u64,
            vec![],
            WalkieOp::AddDegree {
                pitch: degree(i as u16),
            },
        );
        store.ingest_verified(op);
    }
    let next = a.sign(
        1_000 + n as u64,
        vec![],
        WalkieOp::AddDegree {
            pitch: degree(n as u16),
        },
    );
    (store, next)
}

// ---------------------------------------------------------------------------
// Synthetic DAG + index fixtures (reach isolation, RBSR primitives).
// ---------------------------------------------------------------------------

/// A synthetic linear DAG of `n` entries (present-only, distinct payloads).
/// `ReachIndex::new` over this stores Σ = N(N−1)/2 ancestor hashes — the
/// Θ(N²) closure, isolated from the rest of `view()`.
pub fn linear_dag(n: usize) -> MemDagStore {
    let dag = MemDagStore::new();
    let mut prev: Option<EntryHash> = None;
    for i in 0..n {
        let mut prevs = BTreeSet::new();
        if let Some(p) = prev {
            prevs.insert(p);
        }
        let entry = Entry::new((i as u64).to_le_bytes().to_vec(), Position(prevs));
        let hash = entry.hash();
        dag.append(&entry);
        prev = Some(hash);
    }
    dag
}

/// A synthetic forked DAG: `heads` disjoint chains, round-robin, `n` entries.
pub fn forked_dag(n: usize, heads: usize) -> MemDagStore {
    let dag = MemDagStore::new();
    let mut tips: Vec<Option<EntryHash>> = vec![None; heads];
    for i in 0..n {
        let h = i % heads;
        let mut prevs = BTreeSet::new();
        if let Some(p) = tips[h] {
            prevs.insert(p);
        }
        let entry = Entry::new((i as u64).to_le_bytes().to_vec(), Position(prevs));
        let hash = entry.hash();
        dag.append(&entry);
        tips[h] = Some(hash);
    }
    dag
}

/// `n` distinct synthetic entry hashes, for the RBSR primitive benches.
pub fn synthetic_hashes(n: usize) -> Vec<EntryHash> {
    (0..n)
        .map(|i| EntryHash(Digest::of(&(i as u64).to_le_bytes())))
        .collect()
}

/// Build an RBSR `Index` (`SortKey(entry_hash) -> EntryHash`) over `hashes`.
pub fn index_from_hashes(hashes: &[EntryHash]) -> Index {
    let mut idx = Index::new(SALT);
    for h in hashes {
        idx.insert(SortKey(h.as_bytes().to_vec()), *h);
    }
    idx
}

/// Build an RBSR `Index` over a store's lifted entry-hash set.
pub fn index_from_store(store: &RoomStore) -> Index {
    let mut idx = Index::new(SALT);
    for h in store.entry_hashes() {
        idx.insert(SortKey(h.as_bytes().to_vec()), h);
    }
    idx
}
