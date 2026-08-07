# A performance benchmarking suite for walkie-songie / `tutti` — embedded-first

Turning the tutti "leaf profile" claim (`docs/vision/eventually-consistent-pitchsets.md:805-868`) — that the
coordination-free substrate fits on an ESP-32 — into measured, regression-tracked
numbers, and driving perf work from them.

## 0. Thesis and the claim under test

The vision asserts an ESP-32 "leaf profile" is viable *because* the model is
coordination-free with tiny per-node state (`eventually-consistent-pitchsets.md:805-868`).
The architecture doc downgrades this to a present *constraint*: tutti-core's dep
set is wasm-safe and tokio-free but **not `no_std`**, `MemDagStore` holds full
history, and a real leaf needs a windowed store that does not exist yet
(`tutti-crate-architecture.md:700-713`). This suite exists to replace both the
optimism and the hand-waving with numbers.

The load-bearing hypotheses this suite must confirm or falsify:

- **H1 — per-op work is flat and cheap.** Sign, verify, lift, and gossip-encode
  one op are O(op size), independent of log depth N, and land in the low-µs to
  low-ms range on device.
- **H2 — steady-state RAM for a bounded-window leaf is bounded.** A leaf that
  holds a recent suffix + a compacted view fits its working set in a few tens of
  KB, well under an ESP-32's usable heap.
- **H3 — the daily-driver read (`view()`) is the scaling risk, not the network.**
  Because reads recompute from scratch under the no-verdict-cache doctrine
  (`store.rs:392-408`, `eventually-consistent-pitchsets.md:196,1205`), and because
  the causal reachability index materializes a full transitive closure
  (`cover.rs:59-88`), view cost is **super-linear in N** and is the first thing
  that breaks on constrained hardware. **The single most important number this
  suite produces is the N at which `view()` stops being affordable on a leaf.**
- **H4 — sync moves data proportional to disagreement.** RBSR bytes and
  roundtrips scale with symmetric difference, not room size.

Everything below is organized to measure exactly these four, native first (where
`criterion` gives statistical rigor), wasm second (the browser is a shipped
first-class target), and embedded third (a feasibility profile plus a
cross-compiled budget, since no on-device runtime exists yet).

## 1. Hot-path inventory (measured surfaces, with file:line)

Every row is a real code path with its measured/expected cost model. `N` = lifted
op count, `E` = edge count (Σ prev-set sizes), `H` = mean causal-history depth,
`F` = frontier width (concurrent heads), `S` = symmetric difference between two
peers. All hhhs-core refs are the pinned rev `bd23d4e` (`Cargo.toml:103-104`).

### 1.1 View fold / projection — the daily driver, and the scaling risk

`RoomStore::view()` (`store.rs:393-408`) rebuilds the entire read model on every
call. No verdict is cached; this is doctrine, not an oversight
(`eventually-consistent-pitchsets.md:196,1205`; `reactive-rollback-api-design.md:306`).
One `view()` does, in order:

1. `dag.snapshot()` — clones **every** entry (payload = the full framed signed op
   bytes) into a `HashMap`, then builds `topo` (a second Vec of all entries) and
   `frontier` (`dag.rs:307-322`). Cost: O(N) allocations of full op bytes + O(N+E)
   topo sort. At ~400 B/op this clones ~N·400 B per read.
2. `ReachIndex::new(&snapshot)` (`cover.rs:59-88`) — one topo pass that stores, for
   **every** entry, its **full transitive-closure ancestor set** as a
   `BTreeSet<EntryHash>` (`cover.rs:44-47`). This is the cost center: memory is
   Θ(Σ|ancestors(h)|) = **Θ(N·H)**, which for a deep/near-linear author log is
   **Θ(N²)**. See §3 for the concrete blow-up.
3. `with_registers` (`store.rs:555-605`) — 3 passes over `decoded` (O(N)) +
   `register::resolve` per config field (`register.rs:53-64`), each O(k²) in
   candidates k via `is_ancestor`.
4. `with_pitches` (`store.rs:414-451`) — buckets adds/removes by key, then for each
   live-check runs `reach.is_ancestor(add, remove)` (the liveness *verdict*,
   `store.rs:435-450`). Worst case one hot key with A adds, R removes: A·R ancestor
   probes, each a `BTreeSet::contains` over an O(H) set (`cover.rs:109-113`).
5. `with_pieces` (`store.rs:457-551`) — nested Vec scans over puts × {removes,
   moves, unremoves}: O(P·(R+M+U)), quadratic in piece-op count.

The convergence digest `sync_root()` (`store.rs:194-196` → `sync_root_of`,
`store.rs:89-96`) is a separate O(N) blake3 pass over the entry-hash set, carried on
sync `Done` to cross-check agreement.

### 1.2 Op ingest, sign, and verify (ed25519)

- **Sign**: `sign_versioned_op` (`ops.rs:452-484`) — CBOR-encode the `VersionedOp`,
  build a p2panda `Header`, `header.sign(key)` (one ed25519-dalek signature),
  `header.hash()` (blake3). Backed by `ed25519-dalek` / `curve25519-dalek`
  (`Cargo.lock:1661,1239`).
- **Verify**: `verify_signed_op` (`ops.rs:537-592`) — CBOR-decode header,
  `validate_operation` (ed25519 verify + payload-hash/size + seq/backlink), CBOR-decode
  payload, domain `validate_wire`. Runs at **every** peer's ingress, once per op; reads
  never re-verify (`store.rs:18-20`). This is the per-op CPU floor on device.
- **Lift / dedup**: `ingest_verified` (`store.rs:247-258`) → `try_lift`
  (`store.rs:289-315`) frames the verbatim signed bytes, resolves prevs, appends to
  the DAG (`dag.rs:411-453`), and `drain_pending` (`store.rs:319-339`) retries parked
  ops. Dedup is a `BTreeMap` probe; the DAG append is a `HashMap` insert + a growth
  dispatch.

### 1.3 Reconciliation (RBSR) — sans-io, in hhhs-core

Walkie drives sessions with `Config::default()` (`sync.rs:542,638`): `leaf_threshold=4`,
`split_ways=2`, `max_items_per_message=1024` (`reconciliation.rs:157-167`). The pure
step is `respond` (`reconciliation.rs:273-319`):

- **Fingerprint** (`reconciliation.rs:221-228`): XOR monoid over 32-byte hashes in a
  range — O(range size) per call, salted (`add`, `reconciliation.rs:91-102`).
- **Split** (`reconciliation.rs:239-268`): partition a range into ≤`split_ways`
  sub-ranges by cardinality, clamped to `2..=MAX_SPLIT_WAYS` where `MAX_SPLIT_WAYS=8`
  (`reconciliation.rs:132`). 8-way is the *cap*; walkie splits 2-way.
- **Roundtrips ≈ log_{split_ways}(N)** of tree depth; **peak outstanding questions**
  and **bytes** scale with S. hhhs already has measured numbers for the fan-out
  tradeoff: on a two-sided 100k divergence, peak outstanding = 0.111·S at 2-way,
  0.257·S at 8-way (`reconciliation.rs:124-131`). Entry frames are chunked to
  `MAX_SYNC_FRAME_BYTES` by `chunk_entries` (`sync_session.rs:573-604`), ~48 B
  overhead/pair (`sync_session.rs:558`).

### 1.4 Merkle root (M2 target — currently unused)

`radix_immutable` is **not yet a dependency** (no refs in `src/` or `Cargo.lock`); it
lives at `/laboratory/radix_immutable` and the merkle layer it needs is itemized in
`radix-immutable-merkle-audit.md:274-287`. The M2 `state_root`/`ops_root` plan
(`tutti-crate-architecture.md:412`) needs `merkle_root()` + per-node hashing. Expected
cost from the audit: incremental root update after one insert re-hashes ~10 KB
(3-node path, 256-ary) — microseconds; inclusion proof ~9.5 KB flat or ~0.7-0.9 KB
with the per-node binary commitment at N=10⁴ (`radix-immutable-merkle-audit.md:192-218`).
Benchmarking it requires first doing that wiring — see §8.

### 1.5 Reactive projection diff

`apply_room_view` (`browser_host.rs:909-...`) and `replace_native_projection`
(`app.rs:466`) diff a fresh `RoomView` against the prior projected snapshot to emit
minimal `AppEvent`s. Cost: O(pitches + pieces + voices) set/vec comparisons per view
delta — cheap relative to the fold that produced the view, but on the per-growth hot
path, so it belongs in the suite as a guard against accidental O(N²) diffs.

### 1.6 Gossip / wire sizes

- One signed op on the wire: `to_wire_bytes` (`ops.rs:281`) = MAGIC(19) + 8 +
  header + payload. Header carries a 32 B verifying key + 64 B signature + hashes
  (~130-240 B fixed). Payload (`VersionedOp` CBOR) carries the op + topic +
  **`observed` frontier at 32 B/entry**, capped at `MAX_OBSERVED_OPS=4096`
  (`ops.rs:65,570-573`) → op size grows with frontier width F, a leaf-relevant knob.
- Caps: `MAX_SIGNED_OP_WIRE_BYTES` ≈ 1.07 MB (`ops.rs:77-78`), `MAX_GOSSIP_MESSAGE_BYTES=1.2 MB`
  (`iroh_common.rs:35`), `MAX_SYNC_FRAME_BYTES=2 MiB` (`sync.rs:73`),
  `MAX_PRESENCE_BODY_BYTES=8 KiB` with a 1.5 s default lease (`presence.rs:16,18`).

### 1.7 Inventory summary

| # | Hot path | Anchor | Cost model | Embedded concern |
|---|----------|--------|-----------|------------------|
| A | `view()` full fold | `store.rs:393-408` | O(N) clone + **Θ(N·H)** reach | **Primary** — super-linear RAM+CPU |
| B | `ReachIndex::new` | `cover.rs:59-88` | Θ(Σ\|anc\|)=Θ(N·H)→Θ(N²) | **Primary** — the RAM blow-up |
| C | `verify_signed_op` | `ops.rs:537-592` | O(op) ed25519 | per-op CPU floor (ms-class) |
| D | `sign_versioned_op` | `ops.rs:452-484` | O(op) ed25519 | per-op CPU floor (ms-class) |
| E | `ingest_verified`/lift | `store.rs:247-315` | O(prev) + map ops | cheap, per-op |
| F | RBSR `respond`+`split`+`fp` | `reconciliation.rs:221-319` | O(S) bytes, ~log N RT | bandwidth ∝ disagreement |
| G | `merkle_root` (M2) | audit `:192-218` | ~10 KB rehash/insert | µs; proof size is the knob |
| H | projection diff | `browser_host.rs:909` | O(view size) | keep off the N² list |
| I | op wire encode | `ops.rs:281` | O(op)+32B·F | frontier width F drives size |

## 2. Metrics and targets that matter for embedded

Targets are stated as **native (desktop x86, the CI baseline)** and **ESP-32-class
(240 MHz Xtensa LX6, the aspiration)**. Native numbers are what CI gates on; ESP-32
numbers are the budget the design must fit, cross-checked by §3 and §5.3. Where a
device number can't be measured yet (no on-device runtime), it is marked *(model)*
and must be replaced by a real measurement at M4.

| Metric | What it measures | Native target | ESP-32 target | Anchor |
|--------|------------------|--------------|--------------|--------|
| **op sign** | `sign_versioned_op` latency | < 30 µs | < 3 ms *(model; dalek pure-Rust)* | `ops.rs:452-484` |
| **op verify** | `verify_signed_op` latency | < 60 µs | < 6 ms *(model)* | `ops.rs:537-592` |
| **op ingest/lift** | `ingest_verified` (past complete) | < 5 µs | < 100 µs | `store.rs:247-315` |
| **op wire size** | `to_wire_bytes` typical AddDegree, F≤4 | measure; ~350-650 B | same (wire is portable) | `ops.rs:281` |
| **per-op RAM** | bytes retained per lifted op in store | measure; target ≤ 1 KB/op | ≤ 512 B/op | `store.rs:113-127` |
| **view latency @N=100** | full fold | < 200 µs | < 20 ms | `store.rs:393-408` |
| **view latency @N=1k** | full fold | < 5 ms | *(flag if > 200 ms)* | `store.rs:393-408` |
| **reach RAM @N=1k linear** | `ReachIndex` closure footprint | measure (≈ 16 MB, see §3) | **must window** | `cover.rs:59-88` |
| **steady-state leaf RAM** | bounded-window working set (W≤256) | n/a | **≤ 64 KB** | §6 windowed store |
| **RBSR bytes** | wire bytes to reconcile S=10 into N=1k | ~ (S·op + O(log N)·fp) | same | `reconciliation.rs:273-319` |
| **RBSR roundtrips** | frames to converge, S=10, N=1k | ≤ ~12 (2-way) | same | `reconciliation.rs:239-268` |
| **merkle root update** | one insert re-hash (M2) | < 50 µs | < 1 ms | audit `:202-203` |
| **projection diff** | `apply_room_view` per delta | < 50 µs | < 2 ms | `browser_host.rs:909` |

**The three numbers that decide the leaf profile** (elevate these on the dashboard):

1. **N_view_leaf** — the largest N for which a full `view()` fits an ESP-32 frame
   budget (say ≤ 50 ms so a leaf can re-fold within a musical event). If this is
   small (§3 suggests low hundreds *without* windowing), the windowed store is not
   optional, it is the gate.
2. **RAM(N)** — total store + reach bytes as a function of N and history shape.
   The Θ(N·H) reach term is what must be capped by a window W.
3. **op verify µs on device** — the per-op CPU floor; sets the max sustainable op
   ingest rate and the radio duty cycle.

Secondary metrics worth tracking but not gating: fold breakdown (reach vs. pitches vs.
pieces vs. registers share), snapshot clone bytes, `sync_root` recompute cost,
allocations/op (via `dhat` on native), and wasm-vs-native slowdown ratio.

## 3. The ESP-32 budget analysis

**The device.** Classic ESP-32: 520 KB on-chip SRAM, but the memory map + IRAM
carve-outs + WiFi/BLE stack leave realistically **~200-290 KB usable heap**. Dual
Xtensa LX6 @ 240 MHz, no FPU-heavy crypto accel for curve25519 (some variants
accelerate the SHA side only). External SPI flash (~4 MB typical), wear-limited
(~10⁴-10⁵ erase cycles/sector). RISC-V siblings for reference: ESP32-C3 (single-core
RISC-V @ 160 MHz, ~400 KB SRAM), ESP32-S3 (512 KB SRAM + optional slow external PSRAM).
Budget everything against **~300 KB usable RAM / 240 MHz**.

**Where each hot path sits against that budget:**

| Hot path | Cost on ESP-32 | Fits ~300 KB / musical latency? |
|----------|----------------|-------------------------------|
| ed25519 sign/verify | low-ms *(model)* | Yes — holdings change at musical rates (`:849-852`) |
| One op resident | ~few hundred B CBOR + decoded record | Yes — static-buffer friendly (`:823-826`) |
| Rendered pitch-class set | ≤ 4096 degrees → 512 B bitfield (`scl.rs:12`) | Yes — trivially |
| **`ReachIndex` @ N=100 linear** | ~4,950 hashes × 32 B ≈ **158 KB** (+ BTree overhead) | **Marginal** |
| **`ReachIndex` @ N=1k linear** | ~500k hashes × 32 B ≈ **16 MB** | **NO — 50× over budget** |
| **`view()` snapshot clone @ N=1k** | ~1k × ~400 B ≈ **400 KB** cloned per read | **NO — exceeds heap on its own** |
| Full op log resident | N × ~400 B (grows without bound) | **NO — flash + RAM pressure** |
| RBSR session state | ∝ S (tiny for a leaf) | Yes |

**The decisive finding.** The reachability index materializes the *entire transitive
closure* — `ancestors[h]` is the full causal history of every node (`cover.rs:44-47,59-88`).
For a mostly-linear author log this is **Θ(N²)** memory: node k stores k−1 ancestors,
Σ = N(N−1)/2 hashes. Concretely, counting only the 32-byte payload (ignoring BTreeSet
node overhead, which roughly triples it):

- N=100 → ~4,950 hashes → **~158 KB** (already marginal on a leaf)
- N=1,000 → ~500k hashes → **~16 MB** (impossible)
- N=10,000 → ~50M hashes → **~1.6 GB** (absurd)

This is not a micro-optimization target; it is the structural reason the vision's
"bounded recent suffix + compacted current view" (`:837-847`) is **mandatory, not an
optimization**, for any leaf that lives more than a few hundred ops. The whole-DAG
`view()` recompute compounds it: even ignoring reach, cloning the log on every read
(`dag.rs:307-322`) is Θ(N) allocation of full op bytes per fold.

**What the budget implies for the design (each is a bench that proves or refutes it):**

1. A leaf **must** cap its resident window to W ops (suffix) and fold over W, not N.
   With W ≤ 256 and H bounded by W, reach RAM ≈ Θ(W²) ≈ ~2 MB at W=256 *(still too
   big!)* — so the leaf also needs the **compacted view** (a materialized pitch-set
   bitfield + per-author register digests), not a live `ReachIndex` at all past the
   window. **Bench target: steady-state leaf RAM ≤ 64 KB (§2).**
2. Ops must be **pruned to flash-checkpointed compaction**, forfeiting on-device
   time-travel by design (`:853-856`) — flash wear budget favors durable structural
   ops + RAM-resident leases for gesture (`presence.rs`, `:857-859`).
3. The frontier width F a leaf stamps into `observed` directly inflates every op it
   authors (32 B/entry, `ops.rs:65`); a leaf wants a **narrow** frontier — another
   argument for the bounded window.

## 4. Scaling axes

Every bench sweeps a subset of these. A shared fixture generator (see §8, lives in
`benches/support/`) produces reproducible op logs at each point using fixed seeds so
criterion comparisons are apples-to-apples.

| Axis | Points | Drives |
|------|--------|--------|
| **Set size N** (lifted ops) | 10, 100, 1k, 10k | view, reach, ingest, sync_root |
| **History shape H** | linear chain, wide fan-out (F heads), realistic mix | reach RAM (the Θ(N·H) term) |
| **Hot-key contention** | 1 add / hot key with R∈{1,10,100} removes | `with_pitches` A·R verdicts |
| **Piece-op count P** | 0, 10, 100 | `with_pieces` O(P··) |
| **Frontier width F** | 1, 4, 32, 256, 4096(cap) | op wire size, `observed` |
| **Peer count** | 2, 4, 8 | sync fan-in, gossip amplification |
| **Divergence S** | 1, 10, 100, 1k | RBSR bytes + roundtrips |
| **Window W** (leaf) | 64, 128, 256 | bounded-window RAM + fold |
| **Op payload** | AddDegree, PutPiece, SetTuning(large SCL) | verify + wire size spread |

The two axes that matter most for the leaf verdict are **N × H** (reach RAM) and
**S** (sync bandwidth). Hold everything else at a realistic default when sweeping one.

## 5. Harness architecture — native / wasm / embedded

Three tiers, in priority order. The native tier is the statistical backbone and the
CI gate; wasm is the shipped-target reality check; embedded is a feasibility profile
plus a cross-compiled budget until a real device runtime exists.

### 5.1 Native — `criterion` micro-benchmarks (`benches/`)

There is **no bench infra today** (`benches/` is empty; `criterion` is absent from
`Cargo.toml`/`Cargo.lock`). Add it:

- `criterion` as a `[dev-dependencies]`; one `[[bench]]` target per hot-path family
  with `harness = false`.
- **Crucially, the fold + ops path compiles native with no features** — `room` and
  `net` are unconditional modules (`lib.rs:12,14`), `room::store`/`room::ops` depend
  only on `hhhs-core` + `p2panda-core` (always-on deps), and hhhs-core's
  reconciliation types are pure sans-io. So `cargo bench` runs with **default
  features, no wasm, no iroh, no tokio** — the benches exercise exactly the
  embeddable core (§6). This is the whole reason native benches are trustworthy for
  the embedded story: they bench the same code the device would run.
- Bench targets (files under `benches/`):
  `fold.rs` (view + sub-folds + reach), `ops.rs` (sign/verify/encode/decode),
  `ingest.rs` (lift/dedup/drain), `rbsr.rs` (fingerprint/split/respond + a full
  driven session), `merkle.rs` (M2, gated on the radix wiring), `projection.rs`
  (apply_room_view diff).
- **RAM/allocation instrumentation** (the metric that actually decides the leaf
  profile is memory, which criterion does *not* measure): add a `dhat-rs`-based
  memory harness (a plain `cargo test --release --features dhat-heap` binary, not a
  criterion bench) that reports peak bytes + allocation count for
  `ReachIndex::new(N,H)` and `view(N)`. This is where the Θ(N²) reach number gets
  pinned. Alternatively snapshot `#[global_allocator]` counters around the call.

### 5.2 Wasm — the browser is a shipped target

The app ships as wasm (`lib.rs:18-19`, `web-ui`). Two complementary paths:

- **`performance.now()` timing harness** via `wasm-bindgen-test` in a headless
  browser (`wasm-pack test --headless --chrome`). A thin `bench!(name, iters, closure)`
  macro brackets a warm loop with `performance.now()` and logs median/p95. Reuses the
  *same* fixture generators as native so N-sweeps line up. This is the honest number
  for the browser leaf (which is a real deployment, not a stand-in).
- **criterion-on-wasm** is possible but heavier; prefer the `performance.now()` path
  first and only reach for criterion-wasm if variance analysis is needed.
- Report the **wasm/native slowdown ratio** per hot path — it is the best available
  proxy for "how much does a non-x86, cache-poor target cost us" ahead of real device
  numbers, and it stress-tests the same allocation patterns that hurt on an MCU.

### 5.3 Embedded feasibility profile (no device runtime yet — a budget, not a bench)

No code runs on an ESP-32 today (`tutti-crate-architecture.md:700-713`). Until M4,
"embedded benchmarking" means three deliverables:

1. **Cross-compile size + static RAM estimate.** Build the *essential path only*
   (§6) for `riscv32imc-unknown-none-elf` (ESP32-C3 class) and/or the
   `xtensa-esp32-none-elf` toolchain (via `esp-rs`), measuring `.text`/`.rodata`/`.bss`
   with `cargo size`/`esp-size`. This forces the `no_std` question and produces a
   real flash/RAM footprint for the fold + ed25519 + a minimal reconcile.
2. **Dependency embeddability audit** (the gate before any device work) —

   | Component | Embeddable? | Why |
   |-----------|-------------|-----|
   | `RoomStore` fold logic | Needs port | pure algorithm, but uses `std` `BTreeMap`/`HashMap` (alloc-OK, not `no_std` today) |
   | `hhhs-core` (dag/cover/register/reconcile) | Needs port | std crate; `MemDagStore` holds full history (`dag.rs:386`) — needs a windowed `DagRead` impl |
   | `p2panda-core` (sign/verify) | Needs port | std crate; ed25519-dalek itself is `no_std`-capable |
   | `blake3` | **Yes** | pure-Rust, `no_std` |
   | `ed25519-dalek`/`curve25519-dalek` | **Yes** (feature-gated) | `no_std`+`alloc` supported |
   | `serde`/CBOR (`ciborium`) | **Yes** (`alloc`) | |
   | `radix_immutable` | Mostly | wasm-verified; `no_std` needs `portable-atomic`+`race::OnceBox` (audit `:261-267`) |
   | **tokio** | **No** | drop entirely on-device |
   | **iroh full stack / quinn / QUIC** | **No** | the leaf talks to a fuller peer, never runs the transport (§6) |

3. **On-device micro-probe (M4).** When the windowed store exists, flash a minimal
   firmware that signs/verifies/ingests/folds a synthetic W-op window and reports
   cycle counts (`esp_timer_get_time`) + free-heap high-water (`esp_get_free_heap_size`).
   This replaces every *(model)* number in §2/§3 with a measured one.

## 6. The embeddable-core question (honest verdict)

**Verdict: the *algorithm* is embeddable; the *current implementation* is not, and
one specific data structure (the full-closure `ReachIndex`) plus the full-history
`MemDagStore` are the blockers — not crypto, not the fold rules, not sync.**

What genuinely runs on an ESP-32, today's logic ported to `no_std`+`alloc`:

- **Signing your own ops** (a) — ed25519-dalek is `no_std`; sign is the leaf's only
  authorship act (`ops.rs:452-484`, `:818`).
- **Verifying inbound ops** at ingress (`ops.rs:537-592`) — low-ms/op, fine at
  musical rates.
- **The fold rules themselves** — add-wins pitches (`store.rs:414-451`), owner-gated
  pieces (`store.rs:457-551`), causal registers (`register.rs:53-64`) are plain
  arithmetic over small sets **once the reachability facts are available cheaply**.
- **A minimal reconcile** — the vision explicitly allows a leaf to skip RBSR and just
  exchange "my ops since your last visit" with a fuller peer (`:829-831`,
  `eventually-consistent-pitchsets.md:699` swappable). `respond`/`fingerprint` are
  pure and tiny if kept.

What must **stay off-device**, delegated to a full node (phone/laptop/room server):

- **The iroh/tokio transport** — QUIC, hole-punching, gossip, mDNS
  (`Cargo.toml` native-net/browser-net tables). The leaf speaks a thin framed
  protocol to one fuller peer; it never runs the endpoint. This is stated design,
  not a limitation (`:841-847`).
- **Deep history, `getView(at, ·)` for arbitrary horizons, long-range repair** — the
  archive tier. The leaf prunes its *copy*, never the network's log (`:844-847`).

What must be **rebuilt** for the leaf (the real work, and what the suite exists to
size):

1. **A windowed `DagRead`** — a bounded-suffix store (W recent ops) that implements
   the trait (`dag.rs:117-126`) without holding full history, replacing `MemDagStore`
   on-device. This is the `TuttiSubstrate`-scoped-to-a-window idea
   (`tutti-crate-architecture.md:707-713`).
2. **A compacted view instead of a live `ReachIndex`** — past the window, the leaf
   keeps a materialized pitch-set bitfield + per-author register digests, not the
   Θ(N²) ancestor closure. Reachability is only computed *within* the window (Θ(W·H_W)).
   **This is the single most important thing the budget in §3 proves is necessary.**
3. **A pruning/checkpoint contract** flushed to flash with wear awareness.

The minimal **`tutti-core`-on-embedded** shape, then, is: `{ ed25519 sign/verify,
CBOR op codec, a windowed DagRead, in-window reachability, the fold combinators, a
compacted-view checkpoint }` — everything else (transport, archive, arbitrary-`at`
reads, RBSR-if-you-want-it) is a full-node concern. That set maps cleanly onto the
proposed `tutti-core` crate boundary (`tutti-crate-architecture.md:91-118`), which is
why the extraction plan there "just has to not preclude it" (`:712-713`) — and this
suite is how we'll know whether it actually doesn't.

## 7. Regression tracking and the milestone ladder

### 7.1 Baselines and the CI gate

- **criterion baselines**: `cargo bench -- --save-baseline main` on the default
  branch; PR CI runs `--baseline main` and **fails on > X% regression** on the gated
  hot paths. Start lenient (X=20%, noise-dominated on shared runners) and tighten to
  10% once variance is characterized. Gate only the stable, feature-free native
  benches (fold, ops, ingest, rbsr) — wasm/device numbers are tracked, not gated
  (too much runner variance).
- **Memory is gated separately**: the `dhat`/allocator harness (§5.1) asserts hard
  ceilings, not deltas — e.g. `reach_ram(N=100) < 256 KB`, `per_op_resident < 1 KB`.
  A memory regression is a correctness-of-budget failure, so it fails the build
  outright rather than on a percentage.
- **Track, don't gate**: op wire size, RBSR bytes/roundtrips, wasm slowdown ratio,
  fold breakdown — recorded to a checked-in `benches/BASELINE.md` table updated on
  release, so drift is visible in review even when it's under the gate threshold.

### 7.2 Tie to the milestone ladder

| Milestone | Perf obligation | Benches that enforce it |
|-----------|-----------------|-------------------------|
| **M0 — baseline** | Establish honest numbers for the *current* stack. Pin the Θ(N²) reach curve and N_view_leaf. First three benches (§8) land + saved baseline. | fold, ops, ingest + reach-RAM memory harness |
| **M1 — must-not-regress** | Any fold/ops/sync change holds the M0 baseline within threshold. RBSR bench added; wire-size table tracked. | + rbsr, projection; CI gate armed |
| **M2 — merkle/state_root** | Wire `radix_immutable` (audit `:274-287`), then `merkle_root` update + proof-size benches meet §2 targets before `state_root` ships. | + merkle |
| **M3 — windowed store** | The bounded-window `DagRead` + compacted view land; **steady-state leaf RAM ≤ 64 KB** bench passes; N_view_leaf becomes "N irrelevant, W-bounded". | + windowed-fold, leaf-RAM memory harness |
| **M4 — embedded validation** | Cross-compile size fits flash; on-device micro-probe replaces every *(model)* number in §2/§3 with a measured cycle/heap figure. | esp32 firmware probe + `cargo size` |

The ladder makes the dependency explicit: **M3 (the windowed store) is the gate for
M4**, because §3 proves the current full-history fold cannot run on-device at any N
past low hundreds. M0-M2 harden the shared substrate the leaf and the full node both
depend on.

## 8. Ordered implementation plan — the first three benches

Smallest set that yields a trustworthy embedded budget. All three are native,
feature-free, and bench the exact embeddable core (§5.1). Ship them with a saved
`main` baseline as M0.

**Prep (once):** add `criterion` + `dhat` dev-deps; create `benches/support/mod.rs`
with fixture generators — `linear_log(n)`, `forked_log(n, heads)`, `hot_key(adds,
removes)`, `piece_log(p)` — each returning `Vec<VerifiedOp>` via `RoomStore::commit`
/ signed helpers (`store.rs:379-390`, `ops.rs:426-448`) from fixed seeds. These are
shared by native and wasm.

### Bench 1 — `fold.rs` : `view()` and its cost centers (the daily driver, H3)

*This is the most important bench in the suite* — it produces N_view_leaf and the
Θ(N²) reach curve. Groups:

- `view/full` — `RoomStore::view()` at N ∈ {10,100,1k,10k} × shape ∈ {linear, forked}
  (`store.rs:393-408`).
- `view/reach_only` — `ReachIndex::new(&snapshot)` isolated (`cover.rs:59-88`).
- `view/pitches`, `view/pieces`, `view/registers` — each sub-fold isolated so the
  breakdown is attributable (`store.rs:414-451,457-551,555-605`).
- `view/hot_key` — sweep R removes on one key to expose the A·R verdict cost
  (`store.rs:435-450`).
- **Paired memory harness** (`dhat`): peak bytes + alloc count for `ReachIndex::new`
  and `view()` at each N/shape. **This is where §3's 158 KB / 16 MB numbers get
  pinned to reality.** Assert the ceilings from §7.1.

Anchors: `store.rs:393-408`, `cover.rs:59-88`, `store.rs:435-450`.

### Bench 2 — `ops.rs` : sign / verify / encode / decode (the per-op floor, H1)

The other daily-driver path; sets the device CPU floor and the max ingest rate.
Groups:

- `ops/sign` — `sign_versioned_op` (`ops.rs:452-484`).
- `ops/verify` — `verify_signed_op` (`ops.rs:537-592`), the every-ingress cost.
- `ops/encode`, `ops/decode` — `to_wire_bytes`/`from_wire_bytes` (`ops.rs:281,294`).
- `ops/ingest` — `ingest_verified` past-complete vs. parked+drained
  (`store.rs:247-315`).
- `ops/wire_size` — *report bytes, not time*: typical AddDegree, and the sweep over
  frontier width F ∈ {1,4,32,256,4096} to show the 32 B/observed-entry inflation
  (`ops.rs:65,570-573`). Feeds the wire-size tracking table (§7.1).

Anchors: `ops.rs:452-484,537-592,281`.

### Bench 3 — `rbsr.rs` : reconciliation bytes + roundtrips (H4)

Proves sync is bandwidth-∝-disagreement, and exercises the pure hhhs-core primitives
+ a driven session. Groups:

- `rbsr/fingerprint` — `Index::fingerprint` over a range at N ∈ {100,1k,10k}
  (`reconciliation.rs:221-228`).
- `rbsr/split` — `Index::split` fan-out (`reconciliation.rs:239-268`).
- `rbsr/respond` — one `respond` step (`reconciliation.rs:273-319`).
- `rbsr/session` — a full loopback session (walkie's `Config::default()`,
  `sync.rs:542`) between two stores diverging by S ∈ {1,10,100,1k}; **report wire
  bytes + roundtrip count + peak outstanding**, not just wall time. Cross-check
  against hhhs's own 0.111·S / 0.257·S peak-outstanding figures
  (`reconciliation.rs:124-131`).

Anchors: `reconciliation.rs:221-319`, `sync.rs:542`.

**Why these three first:** Bench 1 answers the question the whole leaf profile hinges
on (does the fold fit?), and its memory harness surfaces the single blocking finding
(§3). Bench 2 is the other path touched on literally every op and sets the device CPU
budget. Bench 3 confirms the one thing the vision *assumes* about sync so it can be
taken off the worry list. Everything after (merkle, projection, windowed-fold,
device probe) builds on the baseline these three establish.

## 9. Appendix — full bench catalog

Layout under `benches/` (native, `harness = false`) unless noted. `[M]` = paired
memory (`dhat`/allocator) assertion. `[T]` = reports a size/byte number, tracked not
timed. `[W]` = also run in the wasm `performance.now()` harness.

| File / group | Measures | Anchor | Milestone |
|--------------|----------|--------|-----------|
| `fold::view/full` `[M][W]` | full `view()` × N × shape | `store.rs:393-408` | M0 |
| `fold::view/reach_only` `[M]` | `ReachIndex::new` closure | `cover.rs:59-88` | M0 |
| `fold::view/{pitches,pieces,registers}` | sub-fold breakdown | `store.rs:414-605` | M0 |
| `fold::view/hot_key` | A·R liveness verdicts | `store.rs:435-450` | M0 |
| `fold::sync_root` | convergence digest recompute | `store.rs:89-96,194` | M1 |
| `ops::sign` `[W]` | ed25519 sign | `ops.rs:452-484` | M0 |
| `ops::verify` `[W]` | ed25519 verify at ingress | `ops.rs:537-592` | M0 |
| `ops::{encode,decode}` | wire round-trip | `ops.rs:281,294` | M0 |
| `ops::ingest` | lift/dedup/drain | `store.rs:247-315` | M0 |
| `ops::wire_size` `[T]` | op bytes vs. frontier F | `ops.rs:65,281` | M0 |
| `rbsr::fingerprint` | XOR monoid over range | `reconciliation.rs:221-228` | M1 |
| `rbsr::split` | fan-out partition | `reconciliation.rs:239-268` | M1 |
| `rbsr::respond` | one pure step | `reconciliation.rs:273-319` | M1 |
| `rbsr::session` `[T]` | bytes+RT+peak vs. S | `sync.rs:542`, `sync_session.rs:573` | M1 |
| `projection::apply` `[W]` | view-delta diff | `browser_host.rs:909` | M1 |
| `merkle::root_update` | one-insert re-hash | audit `:202` | M2 |
| `merkle::proof_size` `[T]` | inclusion proof bytes | audit `:192-218` | M2 |
| `windowed::fold` `[M]` | fold over W-suffix store | new (M3) | M3 |
| `windowed::leaf_ram` `[M]` | steady-state ≤ 64 KB | §3, §6 | M3 |
| `esp32::probe` (firmware) | cycles + free-heap on device | §5.3 | M4 |

**Open perf questions (each becomes a bench or a design task):**

1. Can `ReachIndex` be replaced by an incremental / windowed ancestor structure that
   is not Θ(N²) (e.g. interval labels, a bounded-depth closure, or the compacted-view
   digest of §6.2)? The fold bench quantifies the prize.
2. Does the no-verdict-cache doctrine (`:1205`) survive contact with the device
   budget, or does the leaf need a *coalesced* recompute (`reactive-rollback-api-design.md:439`)
   — recompute once per growth batch, not per op?
3. What is the real ed25519 sign/verify cost on Xtensa vs. RISC-V ESP variants, and
   does any variant's SHA accel help the blake3/hash side enough to matter?
4. Flash-wear budget: at what op rate does durable-op checkpointing threaten sector
   lifetime, and where exactly is the RAM-lease vs. durable-op line (`:857-859`)?

---

## References

- Vision, leaf profile: `docs/vision/eventually-consistent-pitchsets.md:805-868`;
  no-cache doctrine `:196,1205`.
- Crate architecture, leaf-honesty + `tutti-core` shape:
  `docs/vision/tutti-crate-architecture.md:91-118,412,700-713`.
- Fold: `src/room/store.rs:89-96,194-196,247-315,393-605`.
- Ops / sign / verify / wire: `src/room/ops.rs:64-78,281-294,452-484,537-592`.
- hhhs-core (rev `bd23d4e`, `Cargo.toml:103-104`): `cover.rs:44-113`,
  `register.rs:53-64`, `dag.rs:117-126,307-322,411-453`,
  `reconciliation.rs:124-167,221-319`, `sync_session.rs:552-604`.
- Sync / gossip caps: `src/net/sync.rs:73,542,638`, `src/net/iroh_common.rs:35`,
  `src/room/presence.rs:16-18`, `src/tuning/scl.rs:12`.
- Merkle audit (radix_immutable, not yet a dep):
  `docs/research/radix-immutable-merkle-audit.md:192-218,261-287`.
