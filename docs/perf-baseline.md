# Performance regression baseline

Regenerate: `nix develop --command cargo bench` (criterion micro-benchmarks +
the `reach_mem` dhat RAM harness). Criterion stores its own baseline under
`target/criterion/` and prints `change:` deltas on each run; this file is the
committed human-readable reference so shifts are visible, not silent.

**Captured:** 2026-08-08, commit `bc05368` (M3.0 bounded-window store), native
(release bench profile, per-crate crypto `opt-level=3`).

## Criterion medians (native)

| Benchmark | Median | Note |
|---|---|---|
| `ops/sign` | 28.5 µs | ed25519 sign |
| `ops/verify` | **58.5 µs** | ed25519 verify — the crypto opt-3 win holds (vs ~4755 µs size-optimized ≈ 80×) |
| `ops/encode` / `ops/decode` | 55 ns / 46 ns | CBOR envelope |
| `ops/ingest` 10 / 100 | 4.6 µs / 21.7 µs | lift + strict-deferral drain |
| `view/full` linear/forked 1000 | **2.01 ms / 2.05 ms** | production `view()` (lazy `Reach`) |
| `view/reach_only` linear/forked 1000 | **82.4 ms / 14.0 ms** | kernel `ReachIndex` path — the O(N²) blocker (~40× the lazy `view()`) |
| `view/pitches` 1000 | 2.0 ms | add-wins degree set |
| `view/pieces` 100 | 5.4 ms | shared-piece CRDT (heaviest — pairwise `is_ancestor`) |
| `view/registers` 100 | 1.2 ms | causal-maxima registers |
| `view/hot_key` a1×r10 / a50×r50 | 20.9 µs / 195.7 µs | query-heavy A·R `is_ancestor` |
| `rbsr/fingerprint` 100/1k/10k | 10.6 µs / 105 µs / 1.07 ms | ~linear |
| `rbsr/split` 100/1k/10k | 16.2 µs / 160 µs / 1.73 ms | |
| `rbsr/respond` | 279 µs | |

## `reach_mem` — RAM (dhat), the leaf-profile numbers

| Structure | N | retained | alloc |
|---|---|---|---|
| `ReachIndex` (kernel, O(N²)) linear | 1000 | **31.7 MB** | 32.8 MB |
| `ReachIndex` forked | 1000 | 7.97 MB | 9.06 MB |
| `view()` (lazy `Reach`) linear | 1000 | 7.9 KB | **1.78 MB** |
| `view/hot_key[100×100]` | 200 | 7.4 KB | 249 KB |

## Takeaways (why these numbers gate the roadmap)

- **Crypto is fast:** verify 58 µs (the per-crate `opt-level=3` override; ~80× vs
  the size-optimized 4755 µs). Signing/verify is not the bottleneck.
- **Lazy `Reach` vs kernel `ReachIndex`:** `view/full` 2 ms vs `view/reach_only`
  82 ms at N=1000 (~40×), and `view()` allocs 1.78 MB vs the kernel index's
  31.7 MB retained. This is exactly why the **kernel `ReachIndex` is the O(N²)
  leaf blocker** and the **M3 windowed store** matters: the ESP32 leaf cannot hold
  a 31.7 MB / N=1000 ancestor closure. M3.0 (bounded window) + M3.1 (compaction)
  replace it with an ~8 KB W×W bitset.
- **`view/pieces`** is the heaviest fold (5.4 ms @ N=100) — the shared-piece CRDT's
  pairwise `is_ancestor` over surviving adds; a candidate for a future combinator
  optimization.
- **RBSR scales ~linearly** in room size — healthy for the sync layer.
