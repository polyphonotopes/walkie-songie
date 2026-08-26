# Performance evidence

This file contains two different kinds of evidence. The Room-v4 native section
is a historical measurement which motivated the HHHS migration; its custom
store and benchmarks were deleted with Room v4 and it cannot be regenerated
against Room v5. The browser section measures the current v0.4.3 path. Do not
use the historical table as a current regression gate or revive the retired
store merely to keep an old benchmark name alive.

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

## Browser release path (v0.4.3)

### Initial release-path baseline

**Captured:** 2026-08-24 from a fresh `trunk build --release`, with two
independent Chromium profiles in one room on the same machine. The peers had an
open direct WebRTC data channel. The sample alternated one pitch on and off ten
times after connection and startup work had settled.

| Boundary | Median | Observed range |
|---|---:|---:|
| click → local pending state visible | 10.5 ms | 7.0–27.3 ms |
| click → durable record handed to networking | 84.9 ms | 55.5–111.6 ms |
| network handoff → peer receive | 24.5 ms | 7–140 ms |
| peer receive → verified, persisted, materialized, applied | 29.5 ms | 18–85 ms |
| applied peer state → peer DOM change | 26.0 ms | 12.1–62.6 ms |
| click → peer-visible state | **177.3 ms** | **104.2–357.6 ms** |

The sender's measured durable phases had medians of 2.0 ms for lane-specific
capability discovery, 5.3 ms for preparation/signing, 37.0 ms for commit (35.1
ms waiting at the IndexedDB transaction boundary), and 6.2 ms for
materialization plus application. Receiver admission had a 14.5 ms median (5.6
ms at the IndexedDB boundary), followed by 10.3 ms of
materialization/application. “IndexedDB boundary” includes browser transaction
scheduling and delivery of its completion callback; it is not serialization
time or a claim that the storage engine spent the entire interval writing
bytes. Per-phase medians do not sum exactly to the end-to-end median because
each median can come from a different sample.

This establishes two separate product paths. Local pending intent provides
gesture feedback near one frame, but the durable peer path crosses serial
persist-before-publish boundaries at both peers and is not a sub-15-ms music
lane. A future bounded session-capability protocol may carry authenticated
provisional intent and reconcile it against the durable Replica; v0.4.3 does
not add that second protocol.

Periodic health checking now sends a fixed-domain causal-frontier probe and
opens full HHHS repair only when frontiers differ. In the same browser run,
waiting beyond the 27-second repair interval produced no new long task; the old
unconditional repair produced 50–134 ms tasks even for synchronized peers.

### Storage-boundary refinement

Temporary instrumentation then separated encoding from browser transaction
completion. A normal music transaction was about 1.33 KiB: roughly 706–709
bytes of command entry payload, one 32-byte predecessor, 400 bytes of public
authority evidence, a 79-byte evidence-kind identifier, and about 111 bytes of
canonical framing/count/sequence/domain data. It contained no secret mutations
or checkpoints. HHHS encoding took 0.0–0.1 ms in the release WASM build, so the
large interval was not serialization.

Idle synthetic measurements for the same two-put transaction were about 2.4 ms
median. A 1.34 KiB sample using Walkie's actual `settings` object store was
about 4.85 ms median (2.4–18.3 ms observed). The live adapter's completion
callback was much more variable because its event was delivered into the same
busy browser event loop as UI, crypto, transport, and application work.

Walkie now calls `IDBTransaction.commit()` immediately after queuing the atomic
record-plus-count writes and still waits for the transaction `complete` event
before publishing the Replica record. This does not weaken atomicity or the
persist-before-publish boundary. In the controlled A/B run, receiver completion
fell from roughly 29 ms median to 7–8 ms; the ten-operation end-to-end median
improved from 177.3 ms to 136.4 ms (97.6–306 ms observed), about a 23% reduction.
Local pending visibility in that run was 13.9 ms median.

### UI projection cleanup

The final v0.4.3 code also removes redundant presentation work:

- pending intent and durable confirmation flow through one RoomProjection event
  path;
- equal Replica projections do not notify subscribers;
- `FullStateSync` projects into the keyboard once instead of once per pitch,
  voice, and full-sync branch;
- toggle overlays compare pitch classes and `data-key-overlay` values in one
  absolute-key coordinate system, so durable confirmation no longer removes and
  recreates unchanged overlays;
- unconditional RoomEvent logging and a leaked per-piece debug MutationObserver
  are gone.

A fresh release build verified direct WebRTC convergence across ten alternating
operations and observed exactly one local plus one remote overlay mutation per
logical operation. The machine was heavily saturated during that final
diagnostic run (about 2–3% CPU idle, with unrelated VM/build/scanning work), so
its wall-clock distribution is intentionally not recorded as a replacement
baseline. Re-capture p50/p95/p99 on a quiet host after the keyboard renderer is
upgraded; retain the one-mutation and convergence assertions regardless of host
speed.

The remaining durable path still performs serial sender and receiver durability
and is not the proposed sub-15-ms musical lane. A browser worker can isolate
Replica/IndexedDB callbacks from presentation contention, but does not eliminate
those two durability boundaries. The post-v0.4 session-capability lane remains
the design for immediate symmetrically authenticated peer feedback followed by
durable confirmation or correction.
