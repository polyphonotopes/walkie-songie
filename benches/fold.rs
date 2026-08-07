//! Bench 1 — `view()` and its cost centers (the daily driver; H3).
//!
//! Produces the view-fold latency curve across N and history shape, plus an
//! attributable sub-fold breakdown. The paired RAM numbers (the Θ(N²) reach
//! closure) live in `reach_mem.rs`, which needs the dhat global allocator.
//!
//! Sub-folds (`with_pitches`/`with_pieces`/`with_registers`) are private, so
//! they are isolated by CONSTRUCTION: a corpus of one op family makes `view()`
//! spend its non-reach time in that family's fold. Subtract `reach_only` at the
//! same N to attribute the sub-fold share.
//!
//! Anchors: `store.rs:393-408`, `cover.rs:59-88`, `store.rs:435-450`.

mod support;

use criterion::{BenchmarkId, Criterion, black_box};
use hhhs_core::cover::ReachIndex;

/// N sweep for the full fold + reach. 10k is deliberately omitted: a linear log
/// at N=10k materializes ~50M ancestor hashes (~1.6 GB), which the budget
/// analysis (§3) already marks impossible — see `reach_mem.rs` for the curve.
const NS: [usize; 3] = [10, 100, 1_000];
const FORK_HEADS: usize = 4;

fn bench_view_full(c: &mut Criterion) {
    let mut group = c.benchmark_group("view/full");
    group.sample_size(20);
    for &n in &NS {
        let linear = support::store_from_ops(&support::linear_ops(n));
        group.bench_with_input(BenchmarkId::new("linear", n), &linear, |b, store| {
            b.iter(|| black_box(store.view()))
        });

        let forked = support::store_from_ops(&support::forked_ops(n, FORK_HEADS));
        group.bench_with_input(BenchmarkId::new("forked", n), &forked, |b, store| {
            b.iter(|| black_box(store.view()))
        });
    }
    group.finish();
}

fn bench_reach_only(c: &mut Criterion) {
    let mut group = c.benchmark_group("view/reach_only");
    group.sample_size(20);
    for &n in &NS {
        let linear = support::linear_dag(n);
        group.bench_with_input(BenchmarkId::new("linear", n), &linear, |b, dag| {
            b.iter(|| black_box(ReachIndex::new(dag)))
        });

        let forked = support::forked_dag(n, FORK_HEADS);
        group.bench_with_input(BenchmarkId::new("forked", n), &forked, |b, dag| {
            b.iter(|| black_box(ReachIndex::new(dag)))
        });
    }
    group.finish();
}

/// `view()` over a corpus dominated by AddDegree ops: non-reach time is
/// `with_pitches`. (Same as `view/full/linear`; kept named for attribution.)
fn bench_pitches(c: &mut Criterion) {
    let mut group = c.benchmark_group("view/pitches");
    group.sample_size(20);
    for &n in &NS {
        let store = support::store_from_ops(&support::linear_ops(n));
        group.bench_with_input(BenchmarkId::from_parameter(n), &store, |b, store| {
            b.iter(|| black_box(store.view()))
        });
    }
    group.finish();
}

/// `view()` over a piece-op corpus: non-reach time is `with_pieces`.
fn bench_pieces(c: &mut Criterion) {
    let mut group = c.benchmark_group("view/pieces");
    group.sample_size(20);
    for &p in &[10usize, 100] {
        let store = support::store_from_ops(&support::piece_ops(p));
        group.bench_with_input(BenchmarkId::from_parameter(p), &store, |b, store| {
            b.iter(|| black_box(store.view()))
        });
    }
    group.finish();
}

/// `view()` over a register-write corpus: non-reach time is `with_registers`
/// (`register::resolve`).
fn bench_registers(c: &mut Criterion) {
    let mut group = c.benchmark_group("view/registers");
    group.sample_size(20);
    for &n in &[10usize, 100] {
        let store = support::store_from_ops(&support::register_ops(n));
        group.bench_with_input(BenchmarkId::from_parameter(n), &store, |b, store| {
            b.iter(|| black_box(store.view()))
        });
    }
    group.finish();
}

/// One hot key with A adds × R removes, all mutually concurrent — every
/// liveness verdict runs (no short-circuit), so `view()` pays A·R
/// `is_ancestor` probes.
fn bench_hot_key(c: &mut Criterion) {
    let mut group = c.benchmark_group("view/hot_key");
    let cases = [(1usize, 10usize), (1, 100), (10, 10), (50, 50)];
    for &(adds, removes) in &cases {
        let store = support::store_from_ops(&support::hot_key_ops(adds, removes));
        let label = format!("a{adds}xr{removes}");
        group.bench_with_input(BenchmarkId::from_parameter(label), &store, |b, store| {
            b.iter(|| black_box(store.view()))
        });
    }
    group.finish();
}

fn main() {
    let mut c = Criterion::default().configure_from_args();
    bench_view_full(&mut c);
    bench_reach_only(&mut c);
    bench_pitches(&mut c);
    bench_pieces(&mut c);
    bench_registers(&mut c);
    bench_hot_key(&mut c);
    c.final_summary();
}
