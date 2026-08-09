//! Paired memory harness for Bench 1 — pins the Θ(N²) `ReachIndex` closure and
//! the `view()` whole-log clone to real bytes (§3: ~158 KB @ N=100,
//! ~16 MB @ N=1k, linear).
//!
//! This is NOT a criterion bench: it installs `dhat::Alloc` as the global
//! allocator and reads `dhat::HeapStats` around each construction, so it must be
//! its own binary. Run with:
//!
//! ```text
//! cargo bench --bench reach_mem
//! ```
//!
//! Columns:
//! * `retained_B`  — live heap the call LEAVES behind (curr-bytes delta). For
//!   `ReachIndex` this is the structure's own RAM (the number §3 predicts).
//! * `alloc_B`     — total bytes the call allocated (transient + retained). For
//!   `view()`, whose result is tiny, this captures the whole-log clone + reach
//!   that H3 is about.
//! * `allocs`      — allocation count.
//! * `peakΔ_B`     — global heap high-water above this call's baseline (with the
//!   N sweep ascending, this call sets the new peak, so it reads as the call's
//!   transient peak).
//!
//! Anchors: `store.rs:393-408`, `dag.rs:307-322`, `cover.rs:44-88`.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

mod support;

use hhhs::cover::ReachIndex;

const NS: [usize; 3] = [10, 100, 1_000];
const FORK_HEADS: usize = 4;

/// Run `build` bracketed by heap snapshots and print the row.
fn row<T>(label: &str, n: usize, build: impl FnOnce() -> T) {
    let before = dhat::HeapStats::get();
    let value = build();
    let after = dhat::HeapStats::get();
    let retained = after.curr_bytes as i64 - before.curr_bytes as i64;
    let alloc_bytes = after.total_bytes - before.total_bytes;
    let allocs = after.total_blocks - before.total_blocks;
    let peak_delta = after.max_bytes as i64 - before.curr_bytes as i64;
    println!(
        "{label:<18} N={n:<6} retained_B={retained:>12}  alloc_B={alloc_bytes:>12}  allocs={allocs:>9}  peakΔ_B={peak_delta:>12}"
    );
    // Keep the structure alive across the second snapshot, then drop.
    std::hint::black_box(&value);
    drop(value);
}

fn main() {
    let _profiler = dhat::Profiler::builder().testing().build();

    println!("\n=== reach_mem — ReachIndex closure RAM (retained_B is the §3 number) ===");
    for &n in &NS {
        // Build the DAG BEFORE the snapshot so only ReachIndex::new is measured.
        let dag = support::linear_dag(n);
        row("reach/linear", n, || ReachIndex::new(&dag));
    }
    for &n in &NS {
        let dag = support::forked_dag(n, FORK_HEADS);
        row("reach/forked", n, || ReachIndex::new(&dag));
    }

    println!("\n=== reach_mem — full view() (alloc_B / peakΔ_B capture the whole-log clone + reach) ===");
    for &n in &NS {
        let store = support::store_from_ops(&support::linear_ops(n));
        row("view/linear", n, || store.view());
    }

    // Query-heavy view(): one hot key with A adds and R removes, every op from a
    // distinct author on an empty horizon, so NONE short-circuit — with_pitches
    // runs all A·R `is_ancestor` verdicts. This is the path the new lazy `Reach`
    // must serve; it materializes no persistent ancestor closure even here.
    println!("\n=== reach_mem — query-heavy view() (hot key: every A·R is_ancestor verdict runs) ===");
    for &(adds, removes) in &[(32usize, 32usize), (64, 64), (100, 100)] {
        let store = support::store_from_ops(&support::hot_key_ops(adds, removes));
        row(
            &format!("view/hot_key[{adds}x{removes}]"),
            adds + removes,
            || store.view(),
        );
    }
    println!();
}
