//! Bench 2 — sign / verify / encode / decode / ingest (the per-op floor; H1).
//!
//! The path touched on literally every op; sets the device CPU budget and the
//! max ingest rate. `wire_size` is REPORTED (bytes, not time): the frontier
//! width F a leaf stamps into `observed` inflates every op it authors at 32 B
//! per entry.
//!
//! Anchors: `ops.rs:452-484,537-592,281,294`, `store.rs:247-315`, `ops.rs:65`.

mod support;

use criterion::{BatchSize, BenchmarkId, Criterion, black_box};

use walkie_songie::room::ops::{LogHead, SignedOp, sign_versioned_op, verify_signed_op};

/// Typical frontier width for the timed sign/verify/encode benches.
const TYPICAL_F: usize = 4;

fn bench_sign(c: &mut Criterion) {
    let key = support::bench_signing_key();
    let versioned = support::sample_versioned(TYPICAL_F);
    c.bench_function("ops/sign", |b| {
        b.iter(|| {
            let (signed, _head) =
                sign_versioned_op(black_box(&key), &LogHead::genesis(), versioned.clone());
            black_box(signed)
        })
    });
}

fn bench_verify(c: &mut Criterion) {
    let signed = support::sample_signed(TYPICAL_F);
    c.bench_function("ops/verify", |b| {
        b.iter(|| black_box(verify_signed_op(black_box(&signed)).expect("verifies")))
    });
}

fn bench_encode(c: &mut Criterion) {
    let signed = support::sample_signed(TYPICAL_F);
    c.bench_function("ops/encode", |b| {
        b.iter(|| black_box(signed.to_wire_bytes().expect("encodes")))
    });
}

fn bench_decode(c: &mut Criterion) {
    let bytes = support::sample_signed(TYPICAL_F)
        .to_wire_bytes()
        .expect("encodes");
    c.bench_function("ops/decode", |b| {
        b.iter(|| black_box(SignedOp::from_wire_bytes(black_box(&bytes)).expect("decodes")))
    });
}

/// Past-complete ingest: one op whose causal past is already lifted, so it lifts
/// immediately (no parking). Rebuilt fresh each batch since `RoomStore` is not
/// `Clone` and `ingest_verified` mutates.
fn bench_ingest(c: &mut Criterion) {
    let mut group = c.benchmark_group("ops/ingest");
    // N sweep is small on purpose: `iter_batched` rebuilds the whole store per
    // sample, so the wall time is dominated by fixture setup, not the timed
    // one-op ingest. 10→100 already shows the O(log N) map-insert + cache trend.
    group.sample_size(30);
    for &n in &[10usize, 100] {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_batched(
                || support::ingest_fixture(n),
                |(mut store, op)| black_box(store.ingest_verified(op)),
                BatchSize::SmallInput,
            )
        });
    }
    group.finish();
}

/// REPORTED, not timed: op wire size vs. frontier width F.
fn report_wire_sizes() {
    eprintln!("\n=== ops/wire_size (bytes) — AddDegree, topic-scoped ===");
    eprintln!(
        "{:>6}  {:>10}  {:>10}  {:>10}",
        "F", "header", "payload", "total"
    );
    for &f in &[1usize, 4, 32, 256, 4096] {
        let signed = support::sample_signed(f);
        let wire = signed.to_wire_bytes().expect("encodes");
        eprintln!(
            "{:>6}  {:>10}  {:>10}  {:>10}",
            f,
            signed.header.len(),
            signed.payload.len(),
            wire.len()
        );
    }
    eprintln!();
}

fn main() {
    report_wire_sizes();
    let mut c = Criterion::default().configure_from_args();
    bench_sign(&mut c);
    bench_verify(&mut c);
    bench_encode(&mut c);
    bench_decode(&mut c);
    bench_ingest(&mut c);
    c.final_summary();
}
