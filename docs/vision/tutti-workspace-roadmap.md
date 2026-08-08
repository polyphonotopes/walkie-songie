# The tutti workspace: consolidation specification and migration roadmap

**Status:** specification + migration plan, 2026-08-08. Companion to
`docs/vision/hhhs-reorg-spec-and-migration.md` (the executed kernel split this
plan mirrors in rigor and extends downward into), `docs/vision/
tutti-crate-architecture.md` (the Track-D extraction that produced today's
`crates/tutti-core` and established genericize-in-place → re-export → extract),
`docs/vision/windowed-store-design.md` (M3.0–M3.2, realized in
`tutti-core/src/windowed.rs`), and `docs/research/tutti-amy-esp32-leaf.md`
(the leaf grand challenge that drives every crate boundary below).

**Grounding.** Every claim was verified against source, not carried forward:

- **hhhs-rs** at `/laboratory/fe-stuff/hhhs-rs`, branch `reorg-crate-split`,
  HEAD `567ce23` ("Delete the hhhs-core shim; relocate its tests; bump 0.2.0"),
  workspace version 0.2.0 — the split family (`hhhs-dag`, `hhhs`, `hhhs-sync`,
  `hhhs-testkit`) exists and is the rev walkie pins at all sites
  (walkie `Cargo.toml:158-160,179`, `crates/tutti-core/Cargo.toml`).
- **walkie-songie** at HEAD: `crates/tutti-core/src/{lib,ops,store,windowed,
  merkle,retain}.rs` (3,138 lines) + its four test suites (3,556 lines);
  `src/room/{ops,store,streams}.rs`; `src/midi/{ledger,input,mod}.rs`;
  `src/tuning/{mod,scl,kbm}.rs` (1,364 lines); `src/web/midi.rs`;
  `docs/perf-baseline.md`.
- **tutti-amy** at `/laboratory/walkie-songie/tutti-amy` (workspace-excluded,
  root `Cargo.toml:111`): `src/{lib,music,main}.rs` (1,645 lines) +
  `tests/{envelope_converge,partition_rejoin}.rs`.

The target decomposition under specification:

| crate | repo | one line |
|---|---|---|
| `hhhs-dag` (grows) | hhhs-rs | the floor gains what leaked upward: `reach` (the `CausalPast` contract + lazy `Reach`) and `windowed` (`WindowedDag`, `WindowedReach`, `PackedSummary`) — the M3 leaf profile behind the `DagRead` seam |
| `tutti-core` | **new `tutti` workspace** | the collapsed essence: signed-op envelope + `OpLanguage` lift/fold seam + `Store<L>`/`WindowedStore<L>` + retention combinators (+ `merkle` feature) — a thin `hhhs-dag` consumer, ~2,200 lines |
| `tutti-music` | tutti | **the MIDI of tutti**: the music protocol — tuning-scoped degrees, pitch-sets, per-degree facets (envelopes), tuning registers — as one `OpLanguage` (reference wire + fold) plus the target-agnostic render-adapter surface |
| `tutti-midi` | tutti | reactive MIDI bridge over `tutti-music`: walkie's source-keyed `MidiLedger` promoted, plus reconcile-on-reconnect (state-first, no stuck notes) |
| `tutti-osc` | tutti | reactive OSC bridge, same shadow/reconcile kernel discipline, OSC address codec |
| `tutti-amy` | tutti | the AMY render leaf, slimmed to what it should have been: C FFI + the fold→AMY event compilers; the music domain it currently hosts moves OUT to `tutti-music` |
| `walkie-songie` | walkie | a thin consumer of `hhhs` + `tutti` via git pins — `WalkieLang` + tuning-UI + hosts; mirrors exactly how it consumes hhhs today |

Positioning, stated once: **tutti is the upstream of the polyphonotopes
ecosystem, not a sibling of it.** The `/laboratory` tree carries years of
music work — scale-graph theory, PCS formalizations, graph apps,
composition tools, color systems, pitch detection, prior P2P experiments —
and the bulk of it sits *downstream* of what this plan builds: those
projects will consume tutti; they are not members of this workspace and
this plan does not design their internals. The one deliberate exception is
the scattered music *theory*, which §A.7 surveys and routes into a
carefully-gated later consolidation — explicitly **not** a dependency of
the extraction in Part B, which stands on its own.

Byte-compatibility posture, stated up front: **no step in Part B changes a
wire byte, an entry hash, or a fold verdict.** The golden entry-hash vector,
the L0 convergence suite, `windowed_equiv`, and the tutti-amy audio-ledger
tests are unmodified gates on every step. The one genuine schema move this
plan identifies (embedding the music core into `WalkieOp`) is explicitly
gated and unscheduled (§A.3.4, §C.5 Q1).

---

# PART A — TARGET END-STATE

## A.0 Ground truth: the measured tree

### A.0.1 tutti-core today (3,138 lines)

| module | lines | production imports (verified `use` lines) | verdict (detail in §C.1) |
|---|---|---|---|
| `lib.rs` | 52 | re-exports; `pub use hhhs::EntryHash` (lib.rs:52) | stays |
| `ops.rs` | 721 | `p2panda_core`, `hhhs::EntryHash` (ops.rs:30), serde, thiserror | **genuinely tutti** — the envelope band STATUS.md excludes from the kernel |
| `store.rs` | 819 | `hhhs::{AppendOutcome, DagRead, Entry, EntryHash, MemDagStore, Position}` (store.rs:25) — **all floor symbols**; `hhhs::cover::ReachIndex` + `hhhs::register` only under `cfg(any(test, feature = "test-support"))` (store.rs:31-34) | split: lift/fold seam stays; `CausalPast` (store.rs:525-551) + `Reach` (store.rs:572-632) + the `ReachIndex` bridge (store.rs:639-648) **sink to hhhs-dag** |
| `windowed.rs` | 1,438 | `hhhs::{DagRead, Entry, EntryHash, GrowthEpoch, Position}` (windowed.rs:62) | split personality, cut in §A.1.2: the L-free half (~655 lines) sinks; `WindowedStore<L>` (~700 lines) stays |
| `merkle.rs` | 59 | `hhhs::EntryHash`, `radix_immutable` (merkle.rs:23-24) | stays, feature `merkle` (§C.2) |
| `retain.rs` | 49 | `hhhs::EntryHash`, `FoldCtx` (retain.rs:18-21) | stays (re-parameterized over `CausalPast`, §A.2.2) |

The load-bearing measurement: **tutti-core's production kernel imports are
100% floor (`hhhs-dag`) symbols.** The facts crate (`hhhs`) is reached only
by the cfg-gated reference oracle. After the sink, tutti-core's production
dependency is `hhhs-dag` alone — which is what makes the ESP32 leaf graph
(§A.6.4) carry zero lens/graph/void/query code.

### A.0.2 The music domain today: scattered and backwards

- **`tutti-amy/src/music.rs` (876 lines)** — the fully-fleshed `MusicLang`:
  `MusicOp::{AddDegree{pc}, RemoveDegree{pc}, SetEnvelope{pc, env}}`
  (music.rs:47-56), `MusicView{live, envelopes}` (music.rs:76-81), an
  add-wins + causal-maxima-register fold (music.rs:140-178), its own wire
  identity (`tutti.music.entry/1`, schema 2 — music.rs:91-94), and a full
  convergence test suite (partition→rejoin, order-independence, oracle
  parity — music.rs:654-876). **The protocol is trapped inside the
  peripheral render leaf**: `Envelope`/`Interp` — op *payload* types, i.e.
  wire schema — are defined in the AMY FFI crate's root (tutti-amy
  lib.rs:294-328), beside `unsafe extern "C"` blocks.
- **walkie `src/room/ops.rs` (436 lines)** — `WalkieLang`:
  `WalkieOp::{AddDegree{pitch: TunedDegree}, RemoveDegree, PutPiece,
  MovePiece, RemovePiece, UnremovePiece, SetTuning, SetConfig}` (ops.rs:59-89),
  wire identity `walkie.hhhs.signed-op/1` / schema 3 (ops.rs:49,103), fold
  delegated to `walkie_fold` (store.rs:40-52): staged registers →
  tuning-scoped add-wins degrees with per-key authors (store.rs:57-95) →
  shared pieces with lock-gating (store.rs:117-239).
- **The comparison, measured** (§A.3.1 draws the line): the two languages
  share an add-wins degree set and causal-maxima registers — the same two
  CRDT semantics over the same substrate — but differ in degree identity
  (walkie: `TunedDegree` scoped to an in-log `SetTuning` register; MusicLang:
  bare `u16` + a compile-time `EDO` const, music.rs:190), in facets (only
  MusicLang has envelopes), in objects (only walkie has pieces/config), and
  in wire identity (different magics, different schema versions — **they are
  two distinct protocols today; unification is a schema move, not a rename**).

### A.0.3 The bridge precedents (walkie already proved the discipline)

- **`src/midi/ledger.rs` (660 lines)** — `MidiLedger`: source-keyed voice
  ownership ("Sources, not pitches, are the unit of ownership" —
  ledger.rs:15-16), deterministic `MidiMessage` output checkable against a
  fake sink (mod.rs:3-5), balanced acquire/release in `set_source`
  (ledger.rs:244-281), `panic()` (ledger.rs:298), `change_tuning` = full
  balanced release then repopulate-from-projection (ledger.rs:285-296), MPE
  config for microtonal output (ledger.rs:100-119). Its entire import
  surface is substrate + music vocabulary: `room::ops::{AuthorId, OpId}`
  (tutti-core types) and `tuning::{TunedDegree, TunedPeriodicPitch, Tuning,
  TuningId}` (ledger.rs:5-8) — **it extracts to the tutti workspace with no
  domain surgery**.
- **`src/web/midi.rs:124`** — `sync_toggle_notes(&mut self, current)`:
  "send offs for removed, ons for added" — state-diff, not event replay.
- **`src/room/streams.rs`** — the yrs-era delta streams: state snapshot →
  diff → `PitchClassDelta{added, removed}` (streams.rs:27-40), and crucially
  "The first emitted delta represents the initial state (empty → current)"
  (streams.rs:120-122, 136-143) — **connect is already modeled as a
  reconcile from assumed-silence** in walkie's oldest bridge code.
- **`tutti-amy/src/lib.rs:252-273`** — `pitchset_to_amy_events(before,
  after)`: the pure diff seam, note-offs before note-ons for voice reuse,
  plus the end-to-end no-stuck-notes ledger (`AudioReport.stuck_oscs` must
  be empty after teardown, music.rs:332-349, 418-433).

## A.1 `hhhs-dag` grows: sinking the leaked floor

Two new modules. Both are code that exists in tutti-core **only because the
floor was too thin**, and both were pre-assigned to this seam by the reorg
spec itself (§A.6.3: the lazy reach "enters the kernel as a second
reachability implementation behind the same query shape"; the M3 windowed
store "lands as a new type in `hhhs-dag` (or an app crate) with zero trait
change — that is the point of specifying the seam now").

**The promotion gate is now met.** The reorg spec deferred the lazy reach at
n=1 (§C.4: "promotion gate at n=2"). This plan is the n=2 moment, twice
over: (i) `WindowedDag`/`WindowedReach` — floor-bound by the argument below —
must implement the ancestry contract, so the contract must live at or below
them; (ii) the standalone tutti workspace makes tutti-core a *second repo's*
consumer of the lazy reach, beside the anticipated hhhs-datalog trigger.

### A.1.1 `hhhs_dag::reach` — the ancestry contract + the lazy oracle

Moves from tutti-core, verbatim semantics:

- **`CausalPast`** (store.rs:525-551) — "the ONE causal question a domain
  projection asks of the DAG": strict, present-only `is_ancestor(a, b)`,
  plus the defaulted `resolve(candidates)` register rule (drop strict
  ancestors, break remaining concurrent maxima by max raw-bytes
  `EntryHash`) — stated in tutti-core as "the kernel rule expressed over
  `is_ancestor`, so a backend whose `is_ancestor` matches the kernel
  resolves registers identically" (store.rs:537-539). This is a *contract
  on `DagRead` implementations' reachability*, which is exactly the floor's
  membership test ("how a store is observed").
- **`Reach`** (store.rs:572-632) — the lazy oracle: `prevs` adjacency built
  in one pass over any `&impl DagRead` (O(N+E)), reverse-walk `is_ancestor`
  with a per-instance per-`b` memo, nothing Θ(N²) surviving the call
  (store.rs:553-571). This is the production fix for the measured leaf
  blocker: `ReachIndex` at N=1000 retains **31.7 MB** and costs 82 ms where
  the lazy path allocates 1.78 MB and costs 2 ms (`docs/perf-baseline.md`,
  "Takeaways": "the kernel `ReachIndex` is the O(N²) leaf blocker").
- **`impl CausalPast for ReachIndex`** (store.rs:639-648) moves to `hhhs`
  (the crate that owns `ReachIndex`; the impl is coherent there because
  `hhhs` depends on `hhhs-dag`): `is_ancestor` forwards to the kernel,
  `resolve` overrides the default to call the real
  `hhhs::register::resolve` — preserving "the reference projection is the
  genuine kernel behavior" (store.rs:634-638).
- **Conformance** moves to `hhhs-testkit`: the pairwise
  `Reach ≡ ReachIndex` assertion (tutti-core store.rs:806-817 smoke +
  the walkie `reach_equiv` shape) becomes
  `hhhs_testkit::conformance::causal_past_conformance(&impl DagRead)`, so
  every future `CausalPast` backend — including `WindowedReach` — is
  certified by the same oracle discipline.

**Why the floor and not `hhhs::cover` (amending reorg spec §A.6.3).** The
reorg spec sketched the promoted lazy reach as `hhhs::cover::Reach`. This
plan deliberately places it one band lower, for a mechanical reason the
spec's sketch predates: **`WindowedDag` sinks to `hhhs-dag` (§A.1.2) and its
`WindowedReach` must implement the shared ancestry contract** — if
`CausalPast` lived in `hhhs`, the floor would have to depend upward
(illegal) or the windowed floor piece couldn't share the seam. Secondary
gains: tutti-core's production graph drops to `hhhs-dag` alone (§A.0.1),
and the ESP32 leaf never compiles the facts band. `hhhs` re-exports
`reach::{CausalPast, Reach}` at its root so front-door consumers and
discoverability are unaffected; `cover::ReachIndex` remains where it is,
untouched, as the Θ(N²) oracle. The richer cover-query surface
(`observed_at`/`causal_cover`/`concurrent_cover`) is **not** part of
`CausalPast` and does not sink — if a lazy implementation of those queries
is ever wanted, that is the `hhhs::cover` follow-up the reorg spec sketched,
orthogonal to this plan. *This amendment needs hhhs co-review sign-off
(§C.5 Q3).*

One duplication resolved honestly: `CausalPast::resolve`'s default restates
`hhhs::register::resolve`'s rule at the floor. The two cannot drift silently
— the testkit conformance asserts `resolve ≡ register::resolve(…,
ReachIndex)` for every backend — and `register::resolve` remains the
facts-band spelling (it takes `&ReachIndex`-shaped reach args and is in
potluck-adjacent frozen surface; it does not move).

### A.1.2 `hhhs_dag::windowed` — the M3 leaf floor, and the exact cut

`windowed.rs` (1,438 lines) is two crates' worth of code sharing a file,
and its own doc says so: "Two types, mirroring the kernel's own
`MemDagStore` vs `Store<L>` split (design §6.1)" (windowed.rs:3-5). The cut
follows the `L` parameter exactly:

**Sinks to `hhhs_dag::windowed` (L-free, ~655 lines):**

| item | lines today | why floor |
|---|---|---|
| `BitRow` (+ `remap_row`) | windowed.rs:85-169 | the §3.3 index-compressed bitset closure — pure DAG math (Θ(W²) *bits*: 8 KB @ W=256 vs the 2 MB hash-set pricing, windowed.rs:79-84) |
| `WindowedReach` (+ backends) | windowed.rs:199-268 | a `CausalPast` implementation — the third backend beside `Reach` and `ReachIndex` (windowed.rs:176-180); needs the §A.1.1 trait at the floor |
| `WindowedDag` | windowed.rs:296-597 | **a `DagRead` implementation** (windowed.rs:514-534) — a bounded suffix window with dense indices, incremental bitset reach, `append_capped`/`insert`/`discard`, the completeness fence (`is_complete`, windowed.rs:354-361), and the bounded `appended_since` honoring the `DagDelta` `None`-past-the-window contract "designed for exactly this store" (windowed.rs:434-461, citing dag.rs:228-235). This is *literally* the §A.6.2 table's "bounded-window / leaf store (future, M3)" row, built — it was only ever app-side because the floor rev predated it |
| `PackedSummary` | windowed.rs:601-732 | the M3.2 bounded ancestry summary: dense retained index + strict-retained-ancestor `BitRow` closure + `discarded_reach` laggard rows, `lift`/`rebuild` — every field is `EntryHash`/`BitRow`; no `L` anywhere. It is the general answer to "how does any compacting `DagRead` host answer ancestry across its cut in O((|R|+W)²) bits", not a tutti fact |
| private `frontier_of`/`topo_of` | windowed.rs:539-597 | self-described duplicates: "Mirrors `hhhs::dag::frontier_of`" / "mirroring `hhhs::dag::topo_of`" (windowed.rs:536-538, 553-555) — in-crate after the sink, the duplication can be collapsed onto the kernel's own helpers |

Also lands here (small, new): the deferred `DagDelta` trait impl for
`WindowedDag` noted at windowed.rs:444-448 ("the trait impl is an M3.1
follow-up under the reorg's n=2 promotion gate") — the gate this plan
opens — behind whatever interior-mutability answer the `Growth: Send +
Sync` supertrait demands, or as the inherent method it is today; **not a
blocking item**.

**Stays in tutti-core (the tutti half, ~700 lines):**

| item | lines today | why tutti |
|---|---|---|
| `WindowedStore<L>` | windowed.rs:814-1418 | the leaf-profile sibling of `Store<L>`: lift/strict-deferral/drain over signed ops, `prepare_commit`/`commit`, the cut-scoped sync surface, and the fenced `view()` — all of it threaded by `OpLanguage`, which is tutti's seam |
| `Checkpoint` | windowed.rs:749-776 | compaction bookkeeping: owns the `PackedSummary` and the `merkle`-gated `pinned_cut_ops_root` (radix_immutable — must not enter the blake3-only floor, reorg spec §A.1.4) |
| `compact()` + auto-compaction | windowed.rs:1140-1205, 1019-1021 | **compaction is driven by `L::retain`** (windowed.rs:1151-1160) — the domain names the residue; the fold-equivalence theorem is conditional on the domain's retention soundness (windowed.rs:1132-1135). The policy is tutti; only the summary *representation* sinks |
| `Compaction` | windowed.rs:780-786 | the `compact()` result type |

The seam between the halves after the cut: `WindowedStore` drives
`WindowedDag::{insert, discard, append_capped, appended_since,
windowed_reach}` and `PackedSummary::{lift, rebuild}` — all already the
internal API (windowed.rs:1069-1096, 1183, 1195-1199); the sink promotes
them from `pub`-in-crate to floor API with the same signatures.

### A.1.3 Features, invariants, posture

- **Features:** none new. `reach` and `windowed` are unconditional modules
  (std-only, like the rest of the floor — reorg spec §A.1.3's honest
  posture unchanged). `serde` remains derive-only.
- **New floor invariants** (added to the §A.1.5 "laws" list):
  7. **Ancestry backends agree with the kernel.** Every `CausalPast`
     implementation answers strict, present-only `is_ancestor` equal to
     `ReachIndex::is_ancestor` for every present pair, and `resolve` equal
     to `register::resolve` — certified by testkit conformance, never
     assumed.
  8. **A truncated window never answers.** `WindowedDag` maintains reach
     structures only while complete and frees them on first truncation
     (windowed.rs:287-290); ancestry across a cut is answered only by an
     exact summary (`PackedSummary`) — "a wrong view, not an error" is the
     failure class this fences (windowed.rs:22-25).
- **Posture:** wasm-safe (no clock, no I/O, no threads in either module);
  `scripts/check-wasm.sh` extends to them automatically via `hhhs-dag`.
  std-only today, per the floor's existing stance; the ESP32-S3 leaf runs
  std over ESP-IDF, so this blocks nothing
  (`docs/research/tutti-amy-esp32-leaf.md` §5.2: "`no_std` is not the
  near-term gate").
- **Perf gates carried along:** `docs/perf-baseline.md`'s `reach_mem`
  numbers become the floor's regression reference for `Reach`; the
  M3.2 memory-bound instruments (`packed_summary_bytes` flat in N,
  windowed.rs:1358-1368; `courier_gap_entries` the honest O(N) residual,
  windowed.rs:1370-1380) move with the summary — instrument surface behind
  the same `test-support`-style cfg the kernel uses.

## A.2 `tutti-core` collapsed to its essence

### A.2.1 Module → contents map (end state, ~2,150–2,350 lines)

| module | contents | change from today |
|---|---|---|
| `ops` | `OpLanguage` (incl. `retain` default), `VersionedOpG`/`VerifiedOpG`/`SignedOp` + wire framing + size ladder, sign/verify, `AuthorId`/`OpId`/`LogHead` | unchanged (721 lines); imports `hhhs_dag::EntryHash` |
| `store` | `frame_signed`/`unframe_signed`, `sync_root_of`, `DecodedOp`, `Store<L>` (lift/deferral/commit/sync surface/`view`), `FoldCtx` (now over `Box<dyn hhhs_dag::CausalPast>`), `view_reference` (cfg-gated) | ~690 lines: `CausalPast` + `Reach` + `ReachIndex` bridge deleted in favor of floor re-exports |
| `windowed` | `WindowedStore<L>`, `Checkpoint`, `Compaction` | ~740 lines: the L-free half deleted in favor of `hhhs_dag::windowed` |
| `retain` | `causal_maxima`, re-parameterized `(&impl CausalPast, &BTreeSet<EntryHash>)` — it only reads `ctx.is_ancestor` today (retain.rs:36-49), so dropping the `L` parameter is a strict generalization; a `FoldCtx` convenience wrapper keeps call sites | 49 lines |
| `merkle` (feature) | `ops_trie`/`ops_root_of`/`prove_op` over `radix_immutable` | unchanged (59 lines) |
| `lib` | re-exports, including path-stability shims: `pub use hhhs_dag::{EntryHash, reach::{CausalPast, Reach}, windowed::{WindowedDag, WindowedReach}}` — every current `tutti_core::…` spelling keeps compiling | ~55 lines |

### A.2.2 Dependencies and features (the sharpened posture)

```toml
[dependencies]
p2panda-core = "0.7"
hhhs-dag = { git = "…/hhhs-rs.git", rev = "<pin>", features = ["serde"] }  # was: hhhs
blake3 = "=1.8.5"
serde = { version = "1.0", features = ["derive"] }
thiserror = "2.0"
radix_immutable = { …, optional = true }          # feature merkle, as today
hhhs = { git = "…", rev = "<pin>", optional = true }  # test-support ONLY

[features]
merkle = ["dep:radix_immutable"]
test-support = ["dep:hhhs"]     # exposes view_reference + the ReachIndex oracle

[dev-dependencies]
hhhs = { git = "…", rev = "<pin>" }
```

The headline: **`hhhs` (facts) becomes test-only.** Production tutti is a
floor consumer, which the import audit already proved (§A.0.1). The
`test-support` contract keeps its exact meaning ("drive the SAME fold on
the Θ(N²) kernel index and assert the cheap lazy `Reach` matches it" —
tutti-core/Cargo.toml comment).

Type identity across the swap is safe by construction: `hhhs::EntryHash`
*is* `hhhs_dag::EntryHash` (wholesale floor re-export, reorg spec §A.2.1),
so `hhhs-dag`-naming and `hhhs`-naming consumers unify as long as one rev
is pinned — the same property the hhhs split itself relied on.

### A.2.3 Invariants owned (unchanged, restated as the crate's laws)

1. **Byte-compatible lift.** Entry payload = `L::ENTRY_FRAME_MAGIC ++
   len(header) ++ header ++ len(payload) ++ payload`; a pure function of
   the signed op, hence order-independent entry hashes (store.rs:41-53).
   The golden entry-hash vector pins it.
2. **Verification is a capability.** `VerifiedOpG` fields are private; the
   only constructor is `verify_signed_op_in` (ops.rs:463-467).
3. **Strict deferral, defer-never-reject.** Never omit a prev
   (store.rs:334-336); parked ops are not materialized, not advertised.
4. **One fold, many backends.** `L::fold` reads ancestry only through
   `FoldCtx` over an erased `CausalPast`, so full-store, reference-oracle,
   and windowed projections are the same fold with a swapped backend
   (store.rs:655-660; windowed.rs:1274-1283) — equivalence is structural.
5. **The windowed fence.** `view()` refuses a hard-truncated uncompacted
   window; a compacted store is answerable iff retention was sound, which
   `windowed_equiv` falsifies adversarially (windowed.rs:1285-1302).

## A.3 `tutti-music` — the MIDI of tutti

The framing, made precise: **a music protocol** — the op alphabet and fold
semantics every tutti music peer speaks — **plus its reference
implementation** — the `OpLanguage` instantiation and the render-adapter
surface renderers consume. What MIDI standardized for event streams, this
crate standardizes for *convergent state*: not note-on/note-off pairs, but
the pitch-set, its tuning, and its facets as CRDT ops with pinned wire
bytes and pinned fold verdicts.

### A.3.1 The reconciliation verdict: same semantics, divergent protocols

Measured against the code (§A.0.2): `MusicLang` and `WalkieLang` are **the
same domain core wearing two incompatible wires**, plus disjoint
extensions:

| axis | MusicLang (tutti-amy/src/music.rs) | WalkieLang (src/room/ops.rs) | tutti-music resolution |
|---|---|---|---|
| degree identity | bare `u16` + const `EDO = 31` (music.rs:49, 190) | `TunedDegree` scoped to a `SetTuning` register (ops.rs:61-63, 81) | **walkie's shape wins** — tuning-scoped degrees + in-log tuning register; the const-EDO shortcut was the n=1 leaf simplification and MusicLang is young enough to bump (schema 2 → 3, its wire has no deployed rooms) |
| set semantics | add-wins observed-remove (music.rs:154-164) | identical rule + per-key author attribution (store.rs:78-93) | shared combinator, attribution included (authorship-as-channel is protocol) |
| registers | per-degree `SetEnvelope` causal-maxima (music.rs:166-175) | room-wide `SetTuning`/`SetConfig` causal-maxima (store.rs:243-293) | both: tuning register + per-degree facet registers are protocol |
| facets | `Envelope`/`Interp` (in tutti-amy lib.rs:294-328 — misplaced) | none yet | protocol (§A.3.2 `facets`) |
| objects | none | pieces graph + lock gate (store.rs:117-239) | **not protocol** — walkie app domain (emoji is not the MIDI of anything) |
| wire | `tutti.music.*`, schema 2 | `walkie.hhhs.*`, schema 3 | two wires (below) |

**The design: one semantics now, one wire later.**

1. **Now (no wire change anywhere):** tutti-music ships (a) the protocol
   *types* and *fold combinators* as plain functions over `FoldCtx` — the
   staged-fold-is-just-Rust posture from tutti-crate-architecture.md §3.2 —
   and (b) `MusicLang`, the canonical reference `OpLanguage` with its own
   wire consts, which is what *new* peers (the AMY leaf, OSC/MIDI test
   rigs, future rooms) speak. Walkie keeps `WalkieLang` byte-identical but
   re-expresses `walkie_fold`'s degree/register stages through the shared
   combinators — semantics unified, pinned by walkie's golden vector and
   oracle tests.
2. **Later (gated, §C.5 Q1):** `WalkieOp` embeds the core at its next
   schema move — `WalkieOp::{Music(tutti_music::MusicOp), Piece(…),
   Config(…)}` — a CBOR layout change, therefore a real `SCHEMA_VERSION`
   bump + entry-hash re-baseline + room migration story. The plan refuses
   to smuggle this in as an extraction step.

This mirrors MIDI's own reality: the protocol is what heterogeneous devices
speak; a DAW's internal model is a superset dialect. Cross-domain frame
separation already enforces the boundary mechanically:
`SignedOp::from_wire_bytes_in::<L>` rejects another domain's magic with
`WrongDomain` (ops.rs:353-367).

### A.3.2 Contents (module map)

| module | contents | provenance |
|---|---|---|
| `tuning` | `TuningDefinition`, `TuningId` (blake3 of canonical Scala bytes), `Tuning`, `TunedDegree`, `TunedPeriodicPitch`, `QuantizeResult`, SCL + KBM parsing | moved from walkie `src/tuning/{mod,scl,kbm}.rs` (1,364 lines); walkie keeps `crate::tuning` as a re-export shim. Rationale: for "the MIDI of tutti", tuning is *the* protocol floor — MIDI's most famous absence — and walkie's `SetTuning`-register + tuning-scoped-degree shape is the proven design. (Alternative — a separate `tutti-tuning` crate — rejected at n=1, §C.2) |
| `facets` | `Envelope`, `Interp`, `MAX_ENV_POINTS`, `MAX_ENV_LEVEL` | moved from tutti-amy lib.rs:294-328 — these are op-payload (wire schema) types and must live with the protocol, not the renderer |
| `ops` | `MusicOp::{AddDegree{degree: TunedDegree}, RemoveDegree, SetEnvelope{degree, env}, SetTuning{definition}}` + `validate_wire` bounds | reconciled from music.rs:47-124 + ops.rs:59-154 per §A.3.1. Deliberately **no note events as durable ops**: performance is presence-lease-shaped and never enters the log — the verdict the AMY research already reached ("the description is shared; the performance is local", tutti-amy-esp32-leaf.md §3.3) and MusicLang already embodies (state-first pitch-sets, not note streams). The generic lease envelope remains a parked tutti-core item (tutti-crate-architecture.md §2.1) |
| `fold` | the combinators as plain functions: `add_wins_degrees(ctx, classify) -> {live, holders}` (walkie's with_pitches shape, store.rs:57-95), `facet_registers(ctx, classify)` (music.rs:166-175), `tuning_register(ctx)` (store.rs:267-273); plus `MusicView{live, holders, envelopes, tuning}` | shared by `MusicLang::fold` and `walkie_fold` |
| `lang` | `MusicLang: OpLanguage` — the canonical wire (`ENTRY_FRAME_MAGIC = b"tutti.music.entry/2"` etc. — bumped once, now, for the degree-identity change; it has no deployed history to preserve) + `MusicLang::retain` override for the compacting leaf (survivor maxima per degree + register maxima via `causal_maxima`) — the first real `retain` implementation, new work gated by `windowed_equiv`'s adversarial suite | music.rs:85-178, generalized |
| `render` | the target-agnostic render-adapter surface: `PitchSetDiff{added, retracted}` + `diff(before, after)` (offs-before-ons ordering contract), `Pitch{degree, edo} → fractional MIDI` resolution (tutti-amy lib.rs:175-238's `Pitch`/`midi_note`, generalized over `Tuning`), facet lookup joins | the seam every renderer (AMY, MIDI, OSC, UI) consumes; extracted from tutti-amy's pure half |

Dependencies: `tutti-core` + `serde`. Nothing else — no AMY, no I/O, no UI,
wasm- and xtensa-clean. This crate is the leaf's second-largest dependency
and must stay boring.

### A.3.3 Invariants owned

1. **Wire discipline** (inherited, restated): append variants, never
   reorder, `#[serde(default)]` fields only, bump `SCHEMA_VERSION` on shape
   change (ops.rs:14-16).
2. **State-first.** The log stores descriptions (degrees, curves, tunings)
   — functions, never samples, never imperative deltas (music.rs:41-45;
   the framing-(b) post-mortem at tutti-amy-esp32-leaf.md §3.3 is this
   crate's founding document).
3. **Facet independence from liveness.** An envelope register persists past
   its degree's removal; re-add resumes under the converged curve
   (music.rs:68-74, pinned by `removing_a_degree_keeps_its_envelope_facet`,
   music.rs:861-875).
4. **Fold determinism** across arrival orders and backends — the existing
   order-independence + oracle-parity suites (music.rs:694-737, 819-842)
   move in as this crate's conformance gates.

## A.4 `tutti-amy` — the render leaf, slimmed

What remains after `music`/`facets`/`Pitch` move out (§A.3.2): the honest
render-target crate the ESP32 story needs.

- `ffi` + `Amy` guard + block render + `write_wav`/`rms`/`peak`
  (lib.rs:30-169) — unchanged.
- The **compilers** from render-surface values to AMY wire strings:
  `pitchset_to_amy_events` (consuming `tutti_music::render::diff`),
  `degrees_to_amy_events`, `envelope_to_amy` + `eg_type` mapping + the
  `Step`-staircase honesty (lib.rs:240-433) — these are AMY-specific and
  stay.
- The scenario/driver/acceptance material (`run_scenario`, `drive_amy`,
  `AudioReport` no-stuck-notes ledger, the wav bins and the two acceptance
  tests) — stays, now importing `tutti_music::{MusicLang, MusicOp, …}`.
- Dependencies: `tutti-core` (`test-support` for oracles), `tutti-music`,
  `serde`, `cc` (build). Not `merkle` — matching the leaf profile
  (tutti-amy/Cargo.toml comment).

Workspace posture: a **member** of the tutti workspace but excluded from
`default-members`, so `cargo test` needs no C toolchain while leaf CI can
still build the whole stack with one flag — the same courtesy walkie's root
manifest extends today (`Cargo.toml:108-111`).

## A.5 `tutti-midi` / `tutti-osc` — the reactive bridge crates

### A.5.1 The design principle, stated once

MIDI and OSC are event protocols, and event protocols fail at exactly one
place: **disconnection**. A dropped cable loses note-offs (stuck notes),
loses controller motion (stale state), and on reconnect a naive bridge
replays a gap that no longer describes reality. tutti is uniquely positioned
to fix this because tutti is **state-first**: the pitch-set + facets *are*
the convergent CRDT state, and events are a *derivable projection* of it.
So the bridge contract is:

> **Outbound MIDI/OSC is DERIVED from state, never queued from events.
> Reconnection is re-projection: diff the endpoint's assumed state against
> tutti's current view and emit exactly the reconciling events.**

This is the STATE/EVENTS/INTERPOLATION triad applied to the bridge — the
same verdict the AMY edge already reached ("reconciliation upstream, events
downstream", tutti-amy-esp32-leaf.md §3.1) and the same discipline walkie's
UI adapter enforces ("a render adapter that only the projection can
construct cannot show unbacked state", tutti-crate-architecture.md §3.5).
And it is not speculative: every piece has a shipped precedent (§A.0.3) —
`sync_toggle_notes`' state diff, `streams.rs`' connect-as-initial-delta,
`MidiLedger`'s balanced source-keyed ownership + `panic()`, and
`pitchset_to_amy_events`' offs-before-ons diff with the stuck-note ledger
asserting silence after teardown.

### A.5.2 Crate shape: two crates, one written discipline

Two crates, `tutti-midi` and `tutti-osc`, each **layered like hhhs-sync**:
a sans-io core (pure functions + a ledger state machine emitting message
values — no ports, no sockets, no runtime; usable verbatim on the ESP32
leaf and testable against a fake sink, exactly as `src/midi/mod.rs:3-5`
promises today) and an optional `reactive` feature (a thin adapter binding
the core to a `Signal<L::View>` / view stream in the futures-signals
discipline walkie's UI uses). No shared `tutti-bridge` kernel crate yet:
the voice-space models genuinely differ (MIDI: channel×note×bend with MPE
allocation; OSC: an address space of typed values with no voice scarcity),
so at n=2 the *shared thing* is the written contract below plus a common
conformance-test shape in the workspace — a kernel crate is extracted if a
third bridge (DMX, Ableton Link, …) makes the duplication real (§C.2).

**`tutti-midi` contents:**

- `ledger` — walkie's `MidiLedger` moved verbatim (it already speaks only
  tutti vocabulary, §A.0.3): `MidiSource` (generalized: the walkie-specific
  variants become a domain-supplied source key type or a small enum
  extension point), `MidiVoice`, `MidiMessage::to_bytes`,
  `MidiOutputConfig` (12-TET channel / MPE pool), `set_source` balanced
  acquire/release, `change_tuning` = panic + repopulate, `panic()`.
- `shadow` + `reconcile` — the new surface (§A.5.3).
- `input` — walkie's `MidiInputTracker`/`HeldInputAction`
  (src/midi/input.rs): inbound events → held/latched intent, which the app
  folds to `tutti_music` ops (held = presence-lease-shaped, latched =
  durable `AddDegree`/`RemoveDegree` commits — walkie's existing split).
- Dependencies: `tutti-core`, `tutti-music`; feature `reactive` adds
  `futures-signals`/`futures`. Port I/O (midir, Web MIDI) stays app-side —
  walkie's `src/midi/native.rs` and `src/web/midi.rs` become thin drivers.

**`tutti-osc` contents (greenfield, rudimentary by design):**

- `codec` — OSC 1.0 message/bundle encoding (hand-rolled or `rosc`,
  optional dep; the surface is small).
- `address` — the projection scheme, versioned like a wire magic:
  `/tutti/1/<topic>/degrees` (list or per-degree `/…/degree/<n>` with 0|1),
  `/…/tuning` (TuningId hex + optionally full SCL text), `/…/env/<n>`
  (breakpoint list + interp code), `/…/holders/<n>` (author count —
  authorship-as-channel, cheap and honest). Floats are fractional MIDI from
  `tutti_music::render` — microtonality survives where raw MIDI needs MPE.
- `shadow` + `reconcile` — same contract as MIDI, simpler voice model:
  OSC values are idempotent addresses, so reconcile = "send every address
  whose target differs from the shadow", and full-refresh is always legal.
- Liveness policy: OSC/UDP has no connection, so "disconnect" is defined by
  the driver (socket error, peer timeout, explicit `/tutti/1/hello`
  handshake) — the core only exposes `on_attach(assumed)` /
  `on_detach()` transitions and never guesses.

### A.5.3 The reconcile-on-reconnect contract (the shared discipline)

State per endpoint, owned privately by the bridge (single-writer, read-only
introspection out — the watertight rule):

```rust
struct EndpointShadow {
    /// What we believe the endpoint currently sounds/holds:
    /// MIDI: voice → owning sources (the MidiLedger already is this map);
    /// OSC:  address → last value sent.
    sounding: …,
    controls: …,
    epoch: u64,        // bumped per attach; stale async sends are dropped
}
enum Attachment { Detached { last: Option<EndpointShadow> }, Attached(EndpointShadow) }
```

Steady state (`on_view`, the walkie shape): project the view through
`tutti_music::render`, diff against the shadow, emit exactly the delta —
offs before ons (voice reuse, the `pitchset_to_amy_events` ordering,
lib.rs:245-248), then facet/control updates for changed values only.
Rollback needs no code: a reverted op re-projects the view and the diff
emits the corrective events — Tier-0 rollback-as-reprojection extended to
hardware.

Reconnect (`on_attach`), the load-bearing algorithm:

```text
on_attach(policy) -> Vec<Msg>:
  assumed = match policy {
    FreshEndpoint  => silence/defaults        // power-cycled synth, new OSC peer
    ResumedGlitch  => last shadow             // brief cable drop; endpoint kept state
    Unknowable     => silence, PREFIXED by panic (MIDI: per-channel All-Notes-Off
                                               + sustain-off; OSC: /…/panic or
                                               full-refresh) — fail to silence,
                                               then rebuild                    }
  target = project(current view)              // tutti's convergent truth, NOW —
                                              // not the state at disconnect time
  emit reconcile(assumed, target):
      note-offs  for assumed.sounding − target.sounding
      note-ons   for target.sounding − assumed.sounding      (offs first)
      ctrl/facet writes for every address where target ≠ assumed
  shadow = target; epoch += 1
```

Properties, each testable against a fake sink:

1. **No stuck notes, ever:** after any disconnect/reconnect interleaving,
   `shadow.sounding` equals the projection of the current view, and the
   emitted history is balanced (every on has a matching off or survives as
   currently-live) — the `AudioReport` ledger property (tutti-amy
   music.rs:344-349) promoted to a bridge conformance test.
2. **The gap is never replayed.** Events lost during detachment are
   *irrelevant by construction* — reconcile reads only (assumed, target).
   A note added *and* removed while detached emits nothing; a note added
   while detached emits one on. This is what an event-log bridge cannot
   say.
3. **Idempotent re-attach:** `on_attach` twice with no view change emits
   nothing the second time (Fresh→diff, then empty diff).
4. **Tuning change = panic + repopulate** (the `change_tuning` doctrine,
   ledger.rs:285-296) — a register flip mid-attachment is a controlled
   detach/attach on the same cable.

Inbound direction (both crates): events fold into state — the bridge never
mutates a view; it *commits ops* (or refreshes leases) through the app's
handle, and the outbound side then re-projects — so local hardware input
and remote peers converge through the identical path. Honest limitation,
stated: on reconnect the *inbound* controller's state is unknowable without
device-specific dumps (MIDI has none short of SysEx; OSC peers can be
queried if they implement it) — the contract therefore only promises
outbound correctness plus "inbound events from now on"; it never invents
controller state.

## A.6 The `tutti` workspace

### A.6.1 Repo and layout

`github.com/polyphonotopes/tutti` (the org already hosting walkie-songie —
walkie's `origin` remote). Layout:

```
tutti/
  Cargo.toml            # workspace; lockstep workspace.package.version = "0.1.0"
  tutti-core/
  tutti-music/
  tutti-midi/
  tutti-osc/
  tutti-amy/            # member, NOT in default-members (C toolchain)
  docs/                 # windowed-store-design.md, tutti-amy-esp32-leaf.md,
                        # this file's Part A → the workspace's own vision docs
  scripts/check-wasm.sh # tutti-core, tutti-music, tutti-midi, tutti-osc
```

`default-members = ["tutti-core", "tutti-music", "tutti-midi", "tutti-osc"]`.
The walkie test suites that pin tutti behavior travel with their crates
(tutti-core's four suites — 3,556 lines — and tutti-amy's two); the
walkie-domain gates (golden entry-hash vector, L0 convergence, walkie
`reach_equiv`) stay in walkie, exercising the pins.

Explicitly **out of scope for the initial workspace** (named so nobody
wonders): `tutti-net`/`tutti-net-iroh`, `tutti-reactive`, `tutti-testkit` —
the tutti-crate-architecture.md §2.2-2.3 satellites remain walkie-side
until their own extraction gates (UI-discipline landing, net-surface
quiescence — §5 there) are met. The workspace is born with the five crates
that have consumers today.

### A.6.2 The git-pin consumption pattern (mirroring hhhs exactly)

walkie's manifest after cutover — the same shape as its hhhs lines today
(`Cargo.toml:158-160`):

```toml
tutti-core  = { git = "https://github.com/polyphonotopes/tutti.git", rev = "<pin>", features = ["merkle"] }
tutti-music = { git = "…/tutti.git", rev = "<pin>" }
tutti-midi  = { git = "…/tutti.git", rev = "<pin>", features = ["reactive"] }
[dev-dependencies]
tutti-core  = { git = "…", rev = "<pin>", features = ["test-support"] }
```

Coordination rules inherited verbatim from the hhhs plan (reorg spec
§A.7.2, §B.3.1): release unit = rev; lockstep workspace version as a
legibility label; every rev classified `internal` vs `surface`; one rev in
flight at a time; breaking changes run expand → migrate → contract.
Publishing to crates.io is a later, bottom-up decision (`tutti-core` first)
with two named blockers: the `radix_immutable` path dep
(tutti-core/Cargo.toml:31-33) and the hhhs git pin itself — neither blocks
git-pinned consumption.

### A.6.3 MSRV / toolchain

Workspace `rust-version = "1.97.1"` (walkie's toolchain and tutti-core's
declared MSRV today). tutti-amy keeps edition 2021 or moves to 2024 with
the workspace — a build-only detail; the C side is vendored via `cc`.

### A.6.4 The ESP32 grand challenge as the boundary oracle

The target that disciplines every line above: **a self-hosted walkie peer
on an ESP32-S3 — hhhs-dag + tutti-core + tutti-music + AMY — converging
over a windowed store and making sound** (tutti-amy-esp32-leaf.md §5). The
leaf's dependency closure, end-state:

```
hhhs-dag (dag + reach + windowed)          ~ the entire kernel the leaf carries
tutti-core (no merkle, no test-support)    envelope + WindowedStore<MusicLang>
tutti-music                                the protocol + retain + render surface
tutti-amy                                  AMY C + the event compilers
p2panda-core / blake3 / serde              the crypto + encoding floor
```

Zero facts-band code, zero radix_immutable, zero tokio/iroh/UI — each
absence load-bearing for the ~300 KB usable-SRAM budget
(tutti-amy-esp32-leaf.md §5.3: window ≤64 KB target, AMY 15-40 KB,
summary flat-in-N by the M3.2 gate). Build reality (§5.2 there): the S3
target is std-over-ESP-IDF, so the floor's std-only posture blocks
nothing; `no_std` remains explicitly unearned (n=0). CI posture: the
existing wasm32 checks are the everyday proxy (no clock/io/threads); an
`xtensa-esp32s3-espidf` `cargo check` of the leaf closure becomes a gate
when tutti-amy's Stage-2 (on-device) work starts, not before — pinning an
espressif toolchain in CI for zero on-device consumers would be ceremony.
One honest unknown, named: `p2panda-core`'s dalek/getrandom chain has a
documented wasm carve-out (tutti-core/Cargo.toml:36-43); its espidf
behavior is unverified and is the first thing the Stage-2 check answers.

### A.6.5 End-state dependency graph, exhaustively

```
hhhs-rs workspace (gitlab.com/micahscopes/hhhs-rs):
  hhhs-dag      → blake3                    (+ serde opt)   [gains reach, windowed]
  hhhs          → hhhs-dag                                  [gains impl CausalPast for ReachIndex]
  hhhs-sync     → hhhs-dag
  hhhs-testkit  → hhhs, hhhs-sync                           [gains causal_past/windowed conformance]

tutti workspace (github.com/polyphonotopes/tutti):
  tutti-core    → hhhs-dag, p2panda-core, blake3, serde, thiserror
                  (merkle → radix_immutable; test-support → hhhs; dev → hhhs)
  tutti-music   → tutti-core, serde
  tutti-midi    → tutti-core, tutti-music   (reactive → futures, futures-signals)
  tutti-osc     → tutti-core, tutti-music   (codec → rosc opt; reactive → …)
  tutti-amy     → tutti-core(test-support), tutti-music, serde  (build: cc)

apps:
  walkie-songie → tutti-core, tutti-music, tutti-midi (git pins)
                  + hhhs (EntryHash spellings), hhhs-sync (the designated driver),
                    hhhs-reactive — all as today
  ESP32 leaf    → hhhs-dag + tutti-core + tutti-music + tutti-amy   (§A.6.4)
  potluck       → unaffected (consumes hhhs directly; no tutti anything —
                  reorg spec §C.5's verified fact stands)
```

Every arrow points downward; no crate in the tutti workspace names `hhhs`
(facts) in production; no bridge crate names a transport.

## A.7 The polyphonotopes ecosystem: downstream consumers, and the theory question

### A.7.1 Downstream: the ecosystem consumes tutti

The workspace stays tight (five crates, §A.6.1). Everything else in the
polyphonotopes constellation is a **consumer** of tutti's public API, in
its own repo, on its own schedule — named here only so the API freeze
knows who it serves:

- `musical-graphs` (+ `musical-graphs-app`, the Bevy visualizer in
  `polyphonotopes-2025`), `polyphonotopes-2023` (the earlier
  force-directed app), `composition-codes-polyphonotopes` (Vite/JS
  composition tool) — graph/scale UIs that would sit where walkie's UI
  sits: over `tutti-core` views + `tutti-music` vocabulary + the bridges.
- `polyphonotopic-colors` — the OKLCH palette source-of-truth walkie
  already consumes; it maps *onto* music state (a render concern), it
  never enters the protocol.
- `basic-pitch` (ML pitch detection) — an inbound edge: detected pitch →
  `tutti-music` ops / presence leases, the same fold-into-state path as
  `tutti-midi::input` (§A.5.3).
- `music-of-the-peers`, `musical-flows-from-peer-2-peer` — prior P2P-music
  experiments; prior art for the peer/bridge story, superseded as
  infrastructure by hhhs + tutti.
- `amy` (shorepine/amy, C) — already wrapped by `tutti-amy`.

Consequence for this plan: nothing. Consequence for the workspace's
steady state: tutti's public surfaces (`OpLanguage`, `FoldCtx`,
`tutti-music::render`, the bridge cores) are the ecosystem's substrate,
so their post-extraction change policy is the hhhs-style
classified-rev discipline of §A.6.2 — that is what "upstream" costs.

### A.7.2 The scattered theory: a grounded inventory (survey, not action)

The music *theory* is the one place the ecosystem overlaps tutti-music's
charter, and it is — the owner's own words — "very messy": emerging in
several crates, in two languages, with at least three independent
prime-form implementations. The inventory, read from source:

| where | what theory it holds | maturity / shape |
|---|---|---|
| `polyphonotopes-2025/polyphonotopes-rs` — `src/theory/` | **PCS mechanics, 12-EDO-fixed**: `ScaleBitset` (12-bit pitch-class-set with rotation/complement and prime-form normalization — "This is the 'prime form' in pitch class set theory", theory/bitset.rs:123), `pitch_class.rs` (0-11 naming + MIDI conversion), `scales.rs` (**the chord/scale constant DB**: "Common scale and chord patterns as ScaleBitset constants"), `solfege.rs` (diatonic mode detection) | shipped Rust lib (serde, wasm-capable), with a legacy-API layer marked for deprecation (lib.rs:38-41) |
| `polyphonotopes-2025/polyphonotopes-rs` — `src/graph.rs` + `src/cozodb.rs` + `src/queries/*.datalog` | **scale-relationship graphs** + CozoDB datalog queries (n-hop scale neighborhoods, cycles) — the "musical scale graph relationships" the workspace is named for | shipped; the cozo dep is feature-gated |
| `polyphonotopes-2025/polyphonotopes-math` | **the Lean 4 formalization**: `PCS/{BitOps, NormalForm, LinearAlgebra, Structures}.lean` — PCS as vectors in (Z₂)¹², XOR-as-voice-leading diff spaces, minimum-weight bases, canonical forms; `PCSData.lean` + `data/tonal-pcs.json` (**a second chord DB**: precomputed scales/chords/intervals with normal forms) | self-labeled EXPERIMENTAL (README.md:2) |
| tutti-core `tests/riffcat_lens.rs` (824 lines) | **a third prime-form implementation**: `pitch_set_to_pcs` (PS ↠ PCS octave collapse) + `pcs_to_set_class` (transposition+inversion prime form) as provable convergence-preserving lenses over `Store<RiffLang>` — arbitrary-EDO, unlike the 12-bit bitsets | test-only; the lens *discipline* is proven, the theory is re-derived in-file |
| `fe-stuff/riff-catalog` | **not music** — despite the name lineage: it is the compiler-artifact catalog (facet-relative content addressing for Solidity/Yul/EVM; its `lean/` and `cubical/` are digest-engine lockstep ports of *that* core). What tutti borrows from it is methodology only: the Rust/Lean lockstep-on-golden-corpus discipline (lean/README.md:1-8) — exactly the shape a future verified music-theory core would want |
| `musical-graphs` (JS `lib/`, `analyze_js_scales.js`) | a fourth, JS-side pocket of scale mechanics | legacy JS; downstream (§A.7.1), listed for completeness of the mess |

Overlap with this plan, stated precisely: `tutti-music` (§A.3) ships
*tuning* (arbitrary-EDO Scala/KBM, `TunedDegree`) and *facets*, and
deliberately does **not** ship set-class/scale-graph theory — the
`riffcat_lens` suite stays a test until the consolidation below. The two
vocabularies meet at one seam: at 12-EDO, `TunedDegree` collapses to
polyphonotopes' `pitch_class` 0-11 and a folded pitch-set projects onto a
`ScaleBitset` — a lens, which is precisely what `riffcat_lens.rs` already
demonstrates. The honest gap: the polyphonotopes theory is 12-bit /
12-EDO-fixed throughout, while tutti's tuning floor is arbitrary-EDO —
generalizing the bitset theory (or scoping the lens to 12-EDO honestly) is
a *design* decision, not a code move.

### A.7.3 Theory consolidation: a distinct, later, human-gated workstream

The pull is real — three-plus prime-form implementations, two chord DBs,
one experimental formalization — and the owner's instinct is the right
one: *"I'm reluctant to move too hastily on that."* So this plan's
position is explicit:

1. **The Part B extraction does not depend on theory consolidation in any
   step.** tutti-core, tutti-music, tutti-amy, and the bridges land whole
   without it; `tutti-music`'s protocol surface (degrees/tuning/facets)
   needs none of the set-class machinery to converge, render, or bridge.
2. **The consolidation, when it happens, is curation, not migration**: a
   possible `tutti-theory` crate (or a `theory` module grown inside
   `tutti-music`; or a promoted `polyphonotopes-theory` crate that tutti
   *depends on* rather than owns — three genuinely different shapes,
   §C.5 Q6) that unifies: one PCS/prime-form implementation
   (arbitrary-EDO-generalized or honestly 12-scoped), one chord/scale DB
   (reconciling `scales.rs` and `tonal-pcs.json`), the scale-graph
   relationship layer, and — the ambitious part — the Lean core held in
   riff-catalog-style golden-corpus lockstep with the Rust engine, so the
   theory the protocol leans on is the theory that is checked.
3. **Sequencing:** strictly after Phase III (the workspace exists and is
   consumed), as its own proposal with its own survey-to-spec pass. It is
   listed in Part B only as an unscheduled workstream marker (B.4), and
   nothing in Parts A–B may grow a dependency on it in the meantime — the
   one standing rule being that no *fourth* prime-form implementation gets
   written while the consolidation is pending.

---

# PART B — THE SEQUENCED MIGRATION

Ordering principle (third application of the walkie-proven pattern:
genericize in place → re-export → extract): every step is one rev in one
repo that leaves (a) that repo's suite green, (b) walkie at its current
pins compiling and green — trivially, since pins insulate it until it
chooses to re-pin — and (c) the standing byte gates unmodified: the golden
entry-hash vector, the L0 convergence suite, `windowed_equiv` (1,342
lines), `second_domain`/`channel_algebra`/`riffcat_lens`, walkie's
`reach_equiv`, and tutti-amy's convergence + no-stuck-notes audio tests.
None of these tests may change in Phases I–III; a step that needs to edit
one is a schema move wearing an extraction costume and must stop.

## B.0 Coordination mechanics

Two pinned upstreams are in play (hhhs-rs, then tutti) with walkie as the
consumer of both. Per step: push rev → re-pin walkie (all sites, one
commit) → walkie gates run → next step. One rev in flight at a time. Two
standing items ride Phase I, both inherited from the hhhs plan's own risk
register: the hhhs pin `567ce23` is the head of branch `reorg-crate-split`,
not master (merge it — reorg spec risk 6's recurrence), and every hhhs
change below is `internal`-class for potluck (nothing touches the facts
band's surface) but the §A.1.1 reorg-spec amendment gets flagged in the
co-review channel regardless (§C.5 Q3).

## B.1 Phase I — hhhs-rs: sink the floor pieces

**Step H1 — `hhhs_dag::reach`.**
Add `CausalPast` + `Reach` to `hhhs-dag` (new module, code moved verbatim
from tutti-core store.rs:516-632 minus the tutti doc references); add
`impl CausalPast for ReachIndex` in `hhhs` (store.rs:639-648's body);
re-export `reach::*` from `hhhs`'s root; add
`hhhs_testkit::conformance::causal_past_conformance` (pairwise
`≡ ReachIndex`, both `is_ancestor` and `resolve`, over the conformance
DAG shapes).
*Gate:* hhhs workspace suite + `check-wasm.sh`; the new conformance run
against `Reach` and `ReachIndex` itself.
*Downstream class:* internal-only (nothing consumes yet; walkie pin
untouched). *Rollback:* revert the rev.

**Step H2 — `hhhs_dag::windowed`.**
Move the §A.1.2 L-free half (windowed.rs:60-732 minus `WindowedStore`
references) into `hhhs-dag`: `BitRow`, `remap_row`, `WindowedReach` (+ its
`CausalPast` impl, now against the H1 trait), `WindowedDag`,
`PackedSummary`; collapse the private `frontier_of`/`topo_of` duplicates
onto the kernel's own helpers (or keep them private-in-module — either is
invisible). Promote the internal API (`insert`/`discard`/`append_capped`/
`windowed_reach`/`appended_since`; `PackedSummary::{lift, rebuild}`;
cfg-gated `summary_bytes`) to documented floor API with signatures
unchanged. Port the L-free property tests (window bitset ≡ `ReachIndex`
over a complete window; summary exactness across `rebuild`) into the
testkit/slice-tests.
*Gate:* hhhs suite; wasm check; `cargo tree -p hhhs-dag` still shows
blake3 (+serde) only.
*Downstream class:* internal-only. *Rollback:* revert.

**Step W1 — walkie re-pins; tutti-core hollows onto the floor.**
One walkie commit: bump the hhhs pin; in `crates/tutti-core` delete
`CausalPast`/`Reach`/the `ReachIndex` bridge and the windowed L-free half,
replacing them with re-exports (`pub use hhhs_dag::reach::{CausalPast,
Reach}`, `pub use hhhs_dag::windowed::{WindowedDag, WindowedReach}`) so
every `tutti_core::…` spelling — including `walkie::room::store`'s
re-exports (room/store.rs:28) — compiles unchanged; swap `hhhs = …` for
`hhhs-dag = …` in production deps, demoting `hhhs` to
dev + `test-support` (§A.2.2); `WindowedStore` re-targets its internals to
the floor types (same signatures — mechanical).
*Gate — the heavy one:* the **full** walkie matrix unmodified: golden
entry-hash vector, L0 suite, `reach_equiv`, all four tutti-core suites
(`windowed_equiv` especially — it drives `WindowedStore` + `WindowedDag` +
`PackedSummary` through adversarial shuffles against the kernel oracle),
tutti-amy's suites at its path dep, wasm `web-ui` build.
*Rollback:* revert the walkie commit (pins are insulation in both
directions).

## B.2 Phase II — walkie workspace: consolidate the music domain in place

**Step W2 — `crates/tutti-music` is born (path-dep member).**
Create the crate beside tutti-core; move in: walkie `src/tuning/` (walkie
keeps `crate::tuning` as `pub use tutti_music::tuning::*` — every walkie
spelling survives), tutti-amy's `Envelope`/`Interp`/consts (tutti-amy
re-exports them from `tutti_music::facets` for its own callers), and
tutti-amy's `MusicLang`/`MusicOp`/`MusicView`/fold (music.rs:34-212) with
the §A.3.1 degree-identity reconciliation (bare `u16`+EDO →
`TunedDegree` + `SetTuning`; wire magic bumped to `tutti.music.entry/2` —
legal precisely because MusicLang has no deployed rooms; tutti-amy's
scenario code updates in the same commit). Add `MusicLang::retain`
(§A.3.2) with `windowed_equiv`-style adversarial coverage in the new
crate's tests.
*Gate:* tutti-amy's convergence + envelope + audio-ledger tests green
against the moved types; walkie tuning tests unmodified; walkie wire gates
untouched (nothing walkie-wire-visible moved — `WalkieOp` still owns its
bytes).
*Note:* this step *does* change MusicLang's bytes — flagged as the one
sanctioned schema bump, taken now while its deployment count is zero.

**Step W3 — `walkie_fold` re-expressed over the shared combinators.**
Rewrite `with_pitches`/`with_registers` (store.rs:57-95, 243-293) as calls
into `tutti_music::fold::{add_wins_degrees, tuning_register, …}`; pieces
(store.rs:117-239) stay walkie-local. `WalkieOp`, magics, schema version:
byte-identical.
*Gate:* golden entry-hash vector + oracle-parity + permutation-convergence
+ L0 — all unmodified. Any diff in any projected view fails the step.

**Step W4 — `crates/tutti-midi` extracted; `tutti-osc` born.**
Move `src/midi/{ledger,input}.rs` into `crates/tutti-midi` (source-key
generalization per §A.5.2); implement `shadow`/`reconcile` + the four
conformance properties (§A.5.3) against a fake sink; walkie's
`src/web/midi.rs` + `src/midi/native.rs` become drivers over it
(`sync_toggle_notes`'s body becomes a `reconcile` call). Create
`crates/tutti-osc` (codec + address scheme + shadow/reconcile) with its
conformance suite; walkie need not consume it yet — its first consumer may
be a leaf/test rig, and that is fine for a workspace member (it is *not*
fine long-term: §C.4 tracks it).
*Gate:* walkie MIDI behavior unchanged (the ledger tests move with the
crate and stay green; walkie's midi integration paths compile against the
driver shim); new bridge conformance suites green.

## B.3 Phase III — stand up the tutti repo; walkie becomes a pin consumer

**Step T1 — repo cutover.**
`git mv` history-preserving export (or subtree split) of
`crates/tutti-core`, `crates/tutti-music`, `crates/tutti-midi`,
`crates/tutti-osc`, `tutti-amy/` into `polyphonotopes/tutti`; workspace
manifest per §A.6.1; CI = native suite + wasm checks + (non-default)
tutti-amy build. Docs that define the crates' contracts
(`windowed-store-design.md`, `tutti-amy-esp32-leaf.md`, Part A of this
file) copy into `tutti/docs/` as the workspace's own references.
*Gate:* tutti workspace suite green standalone — including tutti-core's
four suites and the bridge conformance suites, none of which may have
changed in the move.

**Step T2 — walkie re-homes onto pins.**
One walkie commit: delete the moved trees; replace path deps with the
§A.6.2 git pins; `members`/`exclude` cleanup in the root manifest
(`Cargo.toml:107-111`).
*Gate:* the full walkie matrix at the pin — the same list as W1. From this
commit, walkie consumes tutti exactly as it consumes hhhs, and the
per-rev classification discipline (§A.6.2) governs both.

**Step T3 — steady state + deferred items, each with its own gate:**
- `WalkieOp` embeds `MusicOp` — **gated on a deliberate walkie
  `SCHEMA_VERSION` 4 decision** (room migration/ALPN story required; the
  golden vector is *re-baselined*, not edited — a new vector beside the
  old). §C.5 Q1.
- ESP32 Stage-2: xtensa check-in-CI + the on-device M4 milestone
  (tutti-amy-esp32-leaf.md §7.1) — the first real exercise of
  `WindowedStore<MusicLang>` + `MusicLang::retain` off-desktop.
- `hhhs-datalog` adopting `hhhs_dag::reach::Reach` for scaled
  `GraphVoid`/`removers_of` walks — the reorg spec's own anticipated
  trigger, now a small PR instead of a promotion debate.
- tutti-net / tutti-reactive / tutti-testkit extractions per
  tutti-crate-architecture.md §5, into this workspace when their gates
  land.

## B.4 Unscheduled workstreams (markers only — nothing in B.1–B.3 waits on them)

- **Theory consolidation** (§A.7.3): its own future proposal, after T2,
  human-gated (§C.5 Q6). Until then: no new prime-form implementations
  anywhere in the workspace; `riffcat_lens` stays a test.
- **Downstream adoptions** (§A.7.1): each ecosystem project re-homes onto
  tutti pins in its own repo at its own pace; the only tutti-side
  obligation is the classified-rev discipline on public surfaces.

Sequencing invariants: H1/H2 are kernel-internal and revertible in
isolation; W1 is the only step that touches tutti-core's dependency shape
and it is one commit against one pin bump; W2 is the only step that
changes any wire byte (MusicLang's, zero-deployment); W3/W4 are
behavior-pinned refactors; T1/T2 are pure moves. At no point does walkie's
wire, walkie's entry hashes, or any kernel surface change — and no step
acquires a dependency on the B.4 workstreams.

---

# PART C — RATIONALE, ALTERNATIVES, RISKS, OPEN QUESTIONS

## C.1 Genuinely-tutti vs leaked-hhhs-plumbing, the full table

| item | where today | verdict | disposition |
|---|---|---|---|
| `CausalPast` trait | tutti-core store.rs:525-551 | **leaked floor contract** — "the ONE causal question" is a property of `DagRead` reachability, not of signed ops | → `hhhs_dag::reach` |
| `Reach` lazy oracle | store.rs:572-632 | **leaked floor plumbing** — exists *only* because `ReachIndex` is Θ(N²) (store.rs doc header:13-16; perf-baseline.md names it "the O(N²) leaf blocker"); it serves every `DagRead` consumer, not just tutti | → `hhhs_dag::reach` |
| `impl CausalPast for ReachIndex` | store.rs:639-648 | bridge to the kernel oracle | → `hhhs` |
| `WindowedDag`, `WindowedReach`, `BitRow`, `PackedSummary` | windowed.rs:60-732 | **leaked floor** — a `DagRead`/`DagDelta` implementation, the M3 leaf profile the reorg spec §A.6.2/§A.6.3 explicitly reserved a floor landing zone for; contains no `L` | → `hhhs_dag::windowed` |
| `frontier_of`/`topo_of` | windowed.rs:539-597 | leaked *duplicates* (self-confessed mirrors of kernel privates) | collapsed in the sink |
| signed-op envelope (`ops.rs`) | tutti-core | **genuinely tutti** — signatures/authorship/topic-binding/wire-framing, the exact band hhhs "deliberately does not implement" (STATUS.md via tutti-crate-architecture.md §1) | stays |
| `Store<L>` lift/deferral/heads | store.rs:163-487 | genuinely tutti — needs `OpId ↔ EntryHash`, p2panda heads, verified-op capability | stays |
| `OpLanguage` + `FoldCtx` + `DecodedOp` | ops.rs:86-155, store.rs:120-157, 661-741 | genuinely tutti — the domain seam itself | stays |
| `WindowedStore<L>` + `Checkpoint` + `compact` | windowed.rs:749-1438 | genuinely tutti — compaction is *driven by `L::retain`*; the soundness burden is the domain's | stays |
| `causal_maxima` | retain.rs:36-49 | floor-*shaped* (reads only `is_ancestor`) but semantically a retention lemma; n=1 | stays, re-parameterized over `CausalPast`; sinks later if a second consumer appears |
| `sync_root_of` | store.rs:104-111 | tutti (the RBSR cross-check digest) — with one named wart: its domain tag is the walkie-legacy literal `walkie.hhhs.sync-root/1` (store.rs:92) baked into a *generic* crate. Byte-pinned (Done cross-checks), so it cannot change silently; parameterizing it per-`L` is a wire change to schedule with the next generation bump, not now | stays, wart documented |
| `merkle` (`ops_root`/`prove_op`) | merkle.rs | tutti for now — entry-hash-set semantics are arguably kernel-band, but the radix_immutable dep must not enter the blake3-only floor, and n=1 (walkie) | stays behind `merkle`; a future `hhhs-merkle` satellite is the promotion path if potluck ever wants proofs |
| `MusicLang` + `Envelope`/`Interp` | tutti-amy | **backwards** — the protocol living in the render leaf; wire-payload types beside `extern "C"` | → `tutti-music` |
| `src/tuning/` core | walkie | protocol floor for the MIDI-of-tutti (tuning-scoped degree identity is the load-bearing walkie invention) | → `tutti-music::tuning`, walkie shims |
| `MidiLedger` + `MidiInputTracker` | walkie src/midi | already expressed in pure substrate+music vocabulary (ledger.rs:5-8) — an extraction the code did to itself | → `tutti-midi` |
| pieces fold, lock gate, emoji, `SetConfig`, presence body, UI, hosts | walkie | walkie's app domain, full stop | stay |

## C.2 Alternatives considered and rejected

- **Lazy `Reach` into `hhhs::cover` (the reorg spec's original sketch)
  instead of `hhhs-dag`.** Rejected on the dependency arrow: the windowed
  floor piece must implement the ancestry contract, and it lives in
  `hhhs-dag`; a trait in `hhhs` is unreachable from below. The front-door
  re-export preserves every discoverability property the sketch wanted.
  The cover-query extensions (`causal_cover` etc.) stay `hhhs`-side —
  nothing about band C moves down. (Needs co-review: §C.5 Q3.)
- **Keeping `PackedSummary` tutti-side** (floor gets only
  `WindowedDag`/`WindowedReach`). Viable — the summary is only *driven* by
  compaction, which is tutti — but it forces tutti to either reach into
  `WindowedDag` internals for `BitRow` or duplicate the bitset machinery,
  and it strands the M3.2 bounded-ancestry representation (a general
  answer to a general windowing problem) in a domain crate. The summary is
  `EntryHash`-pure; it sinks. The fallback is cheap if co-review balks:
  the cut moves one struct.
- **One shared wire now: walkie adopts `MusicLang` (or MusicOp embeds into
  WalkieOp) as part of this consolidation.** Rejected flat: it is a
  walkie-rooms schema break (magics, CBOR enum layout, golden vector) and
  extraction steps must never be schema moves. The two-wire/one-semantics
  design (§A.3.1) delivers the consolidation value now; the embedding is
  parked behind an explicit schema decision (§C.5 Q1).
- **A separate `tutti-tuning` crate.** n=1 over-splitting (the reorg
  spec's own refusal pattern, §C.2 there): tuning has exactly one consumer
  class today (the music protocol), 1,364 lines, and no independent change
  axis. If a non-music tutti domain ever wants Scala parsing, split then.
- **A shared `tutti-bridge` kernel crate under the MIDI/OSC bridges.**
  Deferred at n=2: the voice-space models diverge enough (MPE allocation
  vs idempotent addresses) that the shared thing is a written contract +
  a conformance-test shape, not code. Extract the kernel when a third
  bridge makes the duplication measurable. The opposite failure —
  one mega-`tutti-bridge` crate with feature-gated protocols — was also
  rejected: midir-class and rosc-class deps should never ride together.
- **Note events as durable protocol ops in tutti-music.** Rejected by the
  already-written verdict (tutti-amy-esp32-leaf.md §3.3's four-point
  post-mortem of framing (b)): event alphabets don't commute, performance
  state isn't a function of the op-set, and it re-imports the stuck note
  into the *log*. Ephemeral performance stays lease-shaped.
- **Absorbing the polyphonotopes theory (or `polyphonotopes-rs` itself)
  into `tutti-music` now.** Rejected as haste, per §A.7.3: the theory is
  scattered across three-plus implementations in two languages with a
  12-EDO-vs-arbitrary-EDO representation question unresolved; bolting any
  one of them into the protocol crate before curation would freeze the
  wrong vocabulary into tutti's most public surface. The extraction stands
  without it; the consolidation gets its own deliberate pass.
- **Folding the tutti crates into the hhhs-rs workspace** (one repo to
  rule them all). Rejected: different change cadences, different consumer
  sets (potluck consumes hhhs and must not inherit tutti's rev churn),
  different coordination partners. Two lockstep trains with one consumer
  (walkie) riding both is the honest shape.
- **Publishing to crates.io as part of this plan.** No design force; two
  hard blockers (radix path dep, hhhs git pin) resolve on their own
  schedules. Git pins are the proven pattern.

## C.3 Risks and mitigations

1. **The W1 hollowing is the riskiest single step** — it swaps tutti-core's
   kernel dependency and deletes ~790 lines in one commit. Mitigation: the
   step changes zero behavior by construction (re-exports + identical
   types), and the gate list is the heaviest in the plan (golden vector,
   L0, all four tutti suites, `windowed_equiv`'s adversarial shuffles,
   wasm build). Rollback is one revert + pin restore.
2. **The MusicLang degree-identity bump (W2) is a real wire change.**
   Mitigated by timing: MusicLang has zero deployed rooms — this is the
   last cheap moment; after the tutti workspace ships, its wire hardens.
   The bump is confined to one step, named, with the magic literally
   versioned (`tutti.music.entry/2`).
3. **Floor surface growth.** `hhhs-dag` gains ~800 lines and two modules;
   the floor's value is that it nearly never changes. Mitigation: both
   modules arrive *finished* (shipped, benchmarked, adversarially tested
   in walkie), the new invariants are added to the laws list, and the
   default-methods-only trait policy extends to `CausalPast`
   (`resolve` is already a defaulted method — the pattern holds).
4. **Two lockstep trains, one consumer.** walkie now re-pins two upstreams.
   Mitigation: the one-rev-in-flight rule spans both (B.0); tutti pins
   hhhs at the same rev walkie does, asserted by a workspace CI check
   (pin-equality grep), so the diamond (`walkie → tutti-core → hhhs-dag`
   and `walkie → hhhs`) cannot skew types.
5. **The bridges freeze an n=1 reactive surface.** `tutti-midi`'s core is
   proven (walkie's ledger); its `reactive` feature and all of `tutti-osc`
   are new. Mitigation: sans-io cores with conformance suites; the
   reactive layer is a feature, deletable without a major; tutti-osc ships
   rudimentary-by-design with a versioned address scheme (`/tutti/1/…`) so
   its wire can bump.
6. **Cross-repo test stranding.** Moving suites (T1) risks silently
   weakening gates. Mitigation: B's standing rule — no gate file may be
   *edited* in Phases I–III — plus the W1/T2 walkie-side matrix, which
   re-runs everything that ever pinned tutti behavior from the consumer's
   side.
7. **The espidf unknown** (§A.6.4): p2panda-core's crypto chain on xtensa
   is unverified. Contained: it gates only Stage-2 leaf work, not the
   workspace; the fallback (a leaf-local verify shim or an upstream
   getrandom feature pin) is a tutti-core `[target.'cfg(…)']` stanza, the
   same shape the wasm carve-out already uses.

## C.4 The n-count audit: which boundaries are earned

| boundary | n today (verified) | verdict |
|---|---|---|
| `reach` in the floor | tutti-core (prod) + `WindowedReach` (impl) + datalog (anticipated, reorg §A.6.3) + testkit oracle | **earned — the reorg spec's own n=2 gate, now met** |
| `windowed` in the floor | `WindowedStore<L>` (tutti) + the ESP32 leaf (the design driver) + the reorg spec's reserved landing zone | earned by the seam argument; consumer n=1 until the leaf ships — the honest caveat |
| `tutti-core` as a crate | walkie + tutti-amy + 5 `OpLanguage` instantiations across its suites (`WalkieLang`, `KvLang`, `WinLang`, `ChannelLang`, `RiffLang`, `MusicLang`) | earned — genericity demonstrated by use |
| `tutti-music` as a crate | walkie (fold semantics) + tutti-amy (wire + fold) + tutti-midi/osc (render surface) + the leaf | **earned — three consumer classes on day one; the whole point of the consolidation** |
| `tutti-midi` as a crate | walkie (web + native drivers) + the conformance rig | earned (the ledger already extracted itself) |
| `tutti-osc` as a crate | **n=0 consumers at birth** | *not yet earned by use* — justified as protocol-completeness of the bridge pair; tracked: if no consumer lands by the first workspace surface-rev, demote to an example |
| one wire for walkie+music | n=1 each side | **deferred — Q1** |
| `tutti-bridge` shared kernel | n=2 with divergent models | refused for now |
| `no_std` floor | n=0 | unchanged from reorg spec: not earned, explicitly deferred |

## C.5 Open questions for the human

**Q1 — The WalkieOp ⊇ MusicOp embedding: whether and when.** Part A ships
two wires with one semantics. Do you want walkie's next schema move
(v4: `WalkieOp::Music(MusicOp) | Piece(…) | Config(…)`) scheduled — which
buys "walkie rooms *are* tutti-music rooms plus pieces" and lets a bare
tutti-music peer (an AMY leaf!) join a walkie room's degree/envelope layer
— at the cost of a room-migration/ALPN story and a golden-vector
re-baseline? Or is walkie's wire frozen indefinitely with the combinator
layer as the only sharing? (The leaf's value proposition leans toward the
embedding: without it, an ESP32 MusicLang peer cannot join a walkie room.)

**Q2 — Tuning's degree of absorption, and MusicLang's degree type.** This
plan absorbs all of `src/tuning/` into `tutti-music` and bumps MusicLang's
degrees to walkie's `TunedDegree`+`SetTuning` shape (§A.3.1). The lighter
alternative keeps MusicLang's bare `u16`+EDO (leaf-simple, no Scala parser
on-device) and layers tuning-scoping as a walkie-only refinement — but then
"the MIDI of tutti" ships without tuning identity, which is the one thing
it should be famous for. Which weight do you want? (This decides W2's
scope and MusicLang's wire before it hardens.)

**Q3 — hhhs co-review of the floor amendment.** §A.1.1 deviates from the
reorg spec's sketch (`hhhs::cover::Reach`) by placing `CausalPast`+`Reach`
in `hhhs-dag`, and §A.1.2 adds ~800 lines to the floor. Both are argued
here, but the floor's change policy (reorg spec §A.7.1, co-signed with
potluck as Q3 there) makes this a co-review item, not a unilateral move.
Green-light needed before H1/H2.

**Q4 — Bridge reconnect default policy per endpoint class.** §A.5.3 defines
three attach policies (`FreshEndpoint` / `ResumedGlitch` / `Unknowable`).
The safe default is `Unknowable` (panic-then-rebuild) for MIDI hardware and
`FreshEndpoint` (full refresh, no panic) for OSC. Confirm, or name endpoint
classes where panic-on-attach is unacceptable (e.g. a MIDI endpoint shared
with another controller, where All-Notes-Off tramples a co-tenant).

**Q5 — tutti-amy's C build in workspace CI.** Member-but-not-default
(§A.4) means the AMY build is exercised only by leaf CI lanes. Acceptable,
or do you want the C toolchain mandatory in tutti CI so `tutti-music`
render-surface changes can never silently break the AMY compilers?

**Q6 — The theory consolidation's shape, when you're ready (explicitly not
now).** §A.7.2's inventory found the mess you remembered: PCS mechanics +
chord/scale constants + scale graphs in `polyphonotopes-rs`'s `theory`/
`graph`/`cozodb` modules, the experimental Lean formalization + JSON chord
DB in `polyphonotopes-math`, and a third prime-form implementation inside
tutti-core's `riffcat_lens` test — three shapes to choose among when the
time comes: (a) a new `tutti-theory` workspace member, (b) a `theory`
module inside `tutti-music`, or (c) a curated standalone
`polyphonotopes-theory` crate that tutti *depends on* (keeping the theory's
identity with the polyphonotopes name rather than the tutti name). Bundled
into the same decision: whether the bitset theory generalizes to
arbitrary EDO or the 12-EDO lens boundary is drawn honestly, and whether
the Lean core adopts the riff-catalog lockstep discipline. No Part B step
waits on this; it deserves its own survey-to-spec pass when you want it.

---

## Appendix: tutti-core line arithmetic (the ~3,000 → ~? answer)

| | today | sinks to hhhs-dag | stays |
|---|---|---|---|
| lib.rs | 52 | — | ~55 (re-export shims) |
| ops.rs | 721 | — | 721 |
| store.rs | 819 | ~130 (`CausalPast` 525-551, `Reach` 553-632, bridge 634-648 + imports) | ~690 |
| windowed.rs | 1,438 | ~655 (BitRow 85-169, WindowedReach 199-268, WindowedDag 270-597, PackedSummary 599-732) | ~740–780 |
| retain.rs | 49 | — | 49 |
| merkle.rs | 59 | — | 59 |
| **total** | **3,138** | **~790** | **~2,250–2,350** |

Plus the new `tutti-music` (~2,400: 1,364 tuning + ~450 lang/fold + ~200
facets + ~200 render + headroom) — net effect: the *substrate* crate
shrinks to a genuinely thin hhhs consumer while the domain gets a home
that is neither the app nor the FFI leaf.
