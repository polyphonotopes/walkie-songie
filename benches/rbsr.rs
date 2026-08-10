//! Bench 3 — reconciliation bytes + roundtrips (H4).
//!
//! Proves sync is bandwidth-∝-disagreement. The pure hhhs-sync primitives
//! (`fingerprint`/`split`/`respond`) are timed with criterion; a full driven
//! loopback session is REPORTED (wire bytes + roundtrips + peak outstanding vs.
//! divergence S), cross-checked against hhhs's own peak-outstanding figures.
//!
//! Anchors: `reconciliation.rs:221-319`, `src/net/sync.rs:542`,
//! `tests/support/reconcile.rs`.

mod support;

use std::collections::{BTreeMap, BTreeSet};

use criterion::{BenchmarkId, Criterion, black_box};
use hhhs_sync::reconciliation::{Config, KeyRange, Message, opening, respond};

use walkie_songie::room::ops::{OpId, VerifiedOp, WalkieOp};
use walkie_songie::room::store::RoomStore;

// ---------------------------------------------------------------------------
// Primitive micro-benches.
// ---------------------------------------------------------------------------

fn bench_fingerprint(c: &mut Criterion) {
    let mut group = c.benchmark_group("rbsr/fingerprint");
    for &n in &[100usize, 1_000, 10_000] {
        let idx = support::index_from_hashes(&support::synthetic_hashes(n));
        group.bench_with_input(BenchmarkId::from_parameter(n), &idx, |b, idx| {
            b.iter(|| black_box(idx.fingerprint(&KeyRange::full())))
        });
    }
    group.finish();
}

fn bench_split(c: &mut Criterion) {
    let mut group = c.benchmark_group("rbsr/split");
    for &n in &[100usize, 1_000, 10_000] {
        let idx = support::index_from_hashes(&support::synthetic_hashes(n));
        group.bench_with_input(BenchmarkId::from_parameter(n), &idx, |b, idx| {
            b.iter(|| black_box(idx.split(&KeyRange::full(), 2)))
        });
    }
    group.finish();
}

/// One `respond` step against a full-range fingerprint that disagrees (peer
/// holds a strict subset), so the step actually descends (splits).
fn bench_respond(c: &mut Criterion) {
    let cfg = Config::default();
    let hashes = support::synthetic_hashes(1_000);
    let mine = support::index_from_hashes(&hashes);
    let peer = support::index_from_hashes(&hashes[..900]);
    let msg = opening(&peer);
    c.bench_function("rbsr/respond", |b| {
        b.iter(|| black_box(respond(black_box(&mine), black_box(&msg), &cfg)))
    });
}

// ---------------------------------------------------------------------------
// Driven loopback session — reported, not timed.
// ---------------------------------------------------------------------------

#[derive(Default, Debug)]
struct SessionStats {
    rounds: usize,
    control_bytes: usize,
    transfer_bytes: usize,
    transfer_ops: usize,
    peak_outstanding: usize,
}

/// Collect `root` plus its full causal past over `by_id`, skipping ids already
/// seen this session — so every transferred op lifts on the receiver.
fn collect_with_past(
    root: OpId,
    by_id: &BTreeMap<OpId, VerifiedOp>,
    seen: &mut BTreeSet<OpId>,
    out: &mut Vec<VerifiedOp>,
) {
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        let Some(v) = by_id.get(&id) else {
            continue;
        };
        if let Some(backlink) = v.backlink() {
            stack.push(OpId(backlink));
        }
        for observed in v.observed() {
            stack.push(OpId(*observed));
        }
        out.push(v.clone());
    }
}

/// Drive one sans-io RBSR session between two stores to a fixpoint (`a`
/// initiates), transferring verbatim ops on terminal `Items`. Mirrors the L0
/// `tests/support/reconcile.rs` driver, instrumented for bytes/rounds/peak.
fn run_session(
    a: &mut RoomStore,
    b: &mut RoomStore,
    by_id: &BTreeMap<OpId, VerifiedOp>,
) -> SessionStats {
    let cfg = Config::default();
    let mut idx_a = support::index_from_store(a);
    let mut idx_b = support::index_from_store(b);

    let mut msgs = vec![opening(&idx_a)]; // opening full-range fp, A → B
    let mut to_b = true;
    let mut stats = SessionStats::default();
    let mut guard = 0usize;

    while !msgs.is_empty() {
        guard += 1;
        assert!(guard < 100_000, "reconcile failed to terminate");
        stats.rounds += 1;

        let outstanding: usize = msgs
            .iter()
            .map(|m| match m {
                Message::Ranges(pairs) => pairs.len(),
                Message::Items(_, _) => 1,
                Message::Done => 0,
            })
            .sum();
        stats.peak_outstanding = stats.peak_outstanding.max(outstanding);
        for m in &msgs {
            stats.control_bytes += postcard::to_allocvec(m).expect("Message encodes").len();
        }

        let (recv, send): (&mut RoomStore, &RoomStore) =
            if to_b { (&mut *b, &*a) } else { (&mut *a, &*b) };

        // Transfer entries the receiver lacks for each terminal Items message.
        let hash_to_id = send.lifted_op_ids();
        let have = recv.entry_hashes();
        let mut collected: Vec<VerifiedOp> = Vec::new();
        let mut seen: BTreeSet<OpId> = BTreeSet::new();
        for m in &msgs {
            if let Message::Items(_range, ids) = m {
                for id in ids {
                    if have.contains(id) {
                        continue;
                    }
                    if let Some(op_id) = hash_to_id.get(id) {
                        collect_with_past(*op_id, by_id, &mut seen, &mut collected);
                    }
                }
            }
        }
        drop(have);
        for v in &collected {
            if let Ok(bytes) = v.signed().to_wire_bytes() {
                stats.transfer_bytes += bytes.len();
            }
            stats.transfer_ops += 1;
            recv.ingest_verified(v.clone());
        }

        // Respond against the receiver's CURRENT index (post-transfer).
        let recv_idx = if to_b { &mut idx_b } else { &mut idx_a };
        *recv_idx = support::index_from_store(recv);
        let mut replies = Vec::new();
        for m in &msgs {
            replies.extend(respond(recv_idx, m, &cfg));
        }
        msgs = replies;
        to_b = !to_b;
    }
    stats
}

fn extra_chain(seed: u8, count: usize) -> Vec<VerifiedOp> {
    let mut author = support::Author::new(seed);
    (0..count)
        .map(|i| {
            author.sign(
                9_000 + i as u64,
                vec![],
                WalkieOp::AddDegree {
                    pitch: support::degree(i as u16),
                },
            )
        })
        .collect()
}

/// Reported S-sweep: two peers sharing N common ops, each holding S/2 extra
/// (two-sided divergence S). Cross-check `peak/S` against hhhs's 0.111·S
/// (2-way) figure (`reconciliation.rs:124-131`).
fn report_session_sweep() {
    const N_COMMON: usize = 1_000;
    let common = support::forked_ops(N_COMMON, 4);

    eprintln!("\n=== rbsr/session — two-sided divergence, N_common={N_COMMON}, 2-way ===");
    eprintln!(
        "{:>6}  {:>7}  {:>10}  {:>9}  {:>12}  {:>8}  {:>9}",
        "S", "rounds", "ctrl_bytes", "xfer_ops", "xfer_bytes", "peak", "peak/S"
    );
    for &s in &[1usize, 10, 100, 1_000] {
        let a_extra = s / 2;
        let b_extra = s - a_extra;
        let extra_a = extra_chain(200, a_extra);
        let extra_b = extra_chain(201, b_extra);

        let mut ops_a = common.clone();
        ops_a.extend(extra_a.iter().cloned());
        let mut ops_b = common.clone();
        ops_b.extend(extra_b.iter().cloned());

        let mut all = common.clone();
        all.extend(extra_a);
        all.extend(extra_b);
        let by_id = support::by_id(&all);

        let mut store_a = support::store_from_ops(&ops_a);
        let mut store_b = support::store_from_ops(&ops_b);

        let stats = run_session(&mut store_a, &mut store_b, &by_id);

        assert_eq!(
            store_a.entry_hashes(),
            store_b.entry_hashes(),
            "peers must converge after the session (S={s})"
        );

        eprintln!(
            "{:>6}  {:>7}  {:>10}  {:>9}  {:>12}  {:>8}  {:>9.3}",
            s,
            stats.rounds,
            stats.control_bytes,
            stats.transfer_ops,
            stats.transfer_bytes,
            stats.peak_outstanding,
            stats.peak_outstanding as f64 / s as f64,
        );
    }
    eprintln!();
}

fn main() {
    report_session_sweep();
    let mut c = Criterion::default().configure_from_args();
    bench_fingerprint(&mut c);
    bench_split(&mut c);
    bench_respond(&mut c);
    c.final_summary();
}
