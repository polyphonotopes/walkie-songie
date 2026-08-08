# hhhs-core reorganization: architectural specification and downstream migration plan

**Status:** specification + migration plan, 2026-08-08. This document formalizes
and extends `docs/vision/hhhs-ecosystem-design.md` (the design recommendation)
into the durable reference for executing the split. Companion documents:
`docs/research/potluck-hhhs-coreview-questions.md` (the three open cross-repo
questions, referenced throughout as Q1/Q2/Q3) and
`docs/vision/tutti-crate-architecture.md` (the walkie-side extraction that
established the genericize-in-place → re-export → extract pattern this plan
reuses).

**Grounding.** Every claim below was re-verified against source, not carried
forward from the design doc:

- `hhhs-rs` at `/laboratory/fe-stuff/hhhs-rs`, branch `harden-sync-session`,
  HEAD `bd23d4e` ("Harden streamed SyncSession: bounded frames, O(|union|)
  transfer, sequenced acks") — the exact rev walkie and potluck both pin.
- walkie-songie at HEAD: `crates/tutti-core`, `src/room/**`, `src/net/sync.rs`,
  `tests/support/reconcile.rs`, `Cargo.toml:155-156,175`,
  `crates/tutti-core/Cargo.toml:27`.
- potluck at `/laboratory/dweb-camp-2026/potluck`, branch
  `migrate-hardened-hhhs`, HEAD `1e4e847` ("Re-pin hhhs kernel to bd23d4e"):
  `crates/potluck-hhhs/**`, `crates/potluck-wire/src/reconcile.rs`,
  `crates/potluck-hhhs/Cargo.toml:10-11,16`.

All three trees were accessible and read; **no row of the migration matrix is
unverified**. Where this re-audit *corrected* the prior design doc, the
correction is flagged inline with **[correction]** and collected in §C.1.4.

The target decomposition under specification:

| crate | one line |
|---|---|
| `hhhs-dag` | the contract floor: `dag`, `encoding`, `strategy`, `staged`, `rollback`; mandatory dep blake3 only |
| `hhhs` | the facts front door: `cover`, `register`, `canonical_index`, `lens`, `graph`, `query`, `void`; re-exports the floor, never the engine |
| `hhhs-sync` | the engine: `reconciliation`, `sync_session`; depends on `hhhs-dag` only |
| `hhhs-testkit` | conformance suites, `foreign_log`, `rng`, `replica` |
| `hhhs-core` | deprecated path-stable re-export shim over all of the above |

This plan changes **no code semantics**: no module or type is renamed, no wire
byte changes, no verdict or reconciliation behavior moves. Only crate
boundaries move, behind re-export shims.

---

# PART A — FULL SPECIFICATION

## A.0 Ground truth: the measured tree

`hhhs-core` today is 18 modules (counting `test_utils`'s two submodules),
8,321 lines:

| module | lines | production imports (verified from `use` lines) |
|---|---|---|
| `dag.rs` | 669 | `encoding` |
| `encoding.rs` | 95 | blake3 (the only external dep) |
| `strategy.rs` | 128 | `dag::EntryHash`, `encoding::Digest` |
| `staged.rs` | 124 | `dag` |
| `rollback.rs` | 55 | `dag` |
| `cover.rs` | 305 | `dag` |
| `register.rs` | 175 | `cover`, `dag` |
| `canonical_index.rs` | 163 | `dag`, `encoding`, `lens`, `strategy` |
| `lens.rs` | 108 | `dag`, `encoding` |
| `query.rs` | 132 | `dag` (incl. `MemDagStore`), `lens`, `strategy` |
| `graph.rs` | 641 | `dag` (incl. `MemDagStore`), `encoding`, `void` |
| `void.rs` | 346 | `dag` |
| `reconciliation.rs` | 384 | `dag`, `encoding`, `strategy` (`SortKey`, `StrategyId`) — "this module NEVER depends on a concrete strategy. It sees `SortKey` bytes only" (reconciliation.rs:3-4) |
| `sync_session.rs` | 3,867 | `dag::EntryHash`, `reconciliation`, `strategy::StrategyId` (sync_session.rs:82-84; its test module at 1829 uses the full `dag` surface) |
| `replica.rs` | 416 | `dag`, `lens`, `query`, `reconciliation`, `strategy` — the one module bridging both sides of the cut (replica.rs:10-16) |
| `rng.rs` | 38 | nothing |
| `test_utils/conformance.rs` | 438 | `dag`, `encoding`, **`graph`**, `strategy` (conformance.rs:7-10) |
| `test_utils/foreign_log.rs` | 178 | `dag`, `encoding` |

Reading that import graph confirms the proposed partition is a *cut with no
crossing edges* except the two the design already accounts for:

1. `replica.rs` imports both the read model (`lens`, `query`) and the engine
   (`reconciliation`) — which is exactly why it goes to the testkit, per its
   own doc: "it demonstrates their composition; it is not the storage,
   transport, or adapter abstraction downstream applications should implement"
   (replica.rs:1-8).
2. `test_utils/conformance.rs` imports `graph` (its
   `graph_liveness_conformance` suite, conformance.rs:184) — which is why
   `hhhs-testkit` must depend on `hhhs`, not only the floor.

Every other module lands wholly inside one target crate, and the target
crates' dependency edges (`hhhs → hhhs-dag`, `hhhs-sync → hhhs-dag`,
`hhhs-testkit → hhhs + hhhs-sync`) cover every arrow in the table.

The current feature surface being redistributed (hhhs-core/Cargo.toml):
`wire = ["dep:serde", "dep:postcard"]` — gating serde derives on `EntryHash`
(dag.rs:17), `Digest` (encoding.rs:12), `StrategyId` (strategy.rs:17),
`SortKey` (strategy.rs:59), the `reconciliation` wire types (`SessionHello`,
`KeyRange`, `FpBytes`, `Message`), and the `SyncMessage` codec
(`encode`/`decode`, sync_session.rs:1816-1821) — and `test_utils = []` gating
`pub mod test_utils` (lib.rs:25-26). Dev-deps: proptest. One test target,
`read_growth_conformance`, is gated on `required-features = ["test_utils"]`.

## A.1 `hhhs-dag` — the contract floor

### A.1.1 Contents (module → crate map)

Moves verbatim: `dag.rs`, `encoding.rs`, `strategy.rs`, `staged.rs`,
`rollback.rs`. No type or function is renamed; module paths inside the crate
are unchanged (`hhhs_dag::dag`, `hhhs_dag::encoding`, …), and the crate root
re-exports the same curated list `hhhs-core`'s lib.rs carries today for these
modules.

### A.1.2 Complete public API surface

From `dag.rs`:

- **Coordinates:** `EntryHash` (newtype over `Digest`; "the ONLY identity in
  the system", dag.rs:14-15), `Position` (a `BTreeSet<EntryHash>` of observed
  heads), `Header { payload_digest, prevs }`, `Entry { header, payload }` with
  `Entry::new`, `Entry::hash`, `Entry::payload_matches_digest` (the
  anti-poisoning check, dag.rs:89-94), `canonical_header`, and `entry_hash`
  ("the one and only identity function", dag.rs:110-113).
- **Read contract:** `DagRead` (dag.rs:117-136) — `entry`, `contains`,
  `frontier`, `entries_topo`, `all_hashes`, plus the *defaulted* `snapshot()`
  (the existing precedent for default-method-only evolution, §A.7).
- **Growth contract:** `Growth` (dag.rs:217-221; linearizable
  registration-vs-commit doctrine at dag.rs:213-216), `GrowthEpoch`,
  `GrowthSubscription` (drop-to-unsubscribe token).
- **Delta contract:** `DagDelta: DagRead + Growth` with
  `appended_since(since) -> Option<Vec<Entry>>` — `None` meaning "this store
  cannot answer — it has compacted or never retained that history — and the
  caller must fall back to a full recomputation" (dag.rs:228-235). This is
  the pre-built compaction/windowing escape hatch §A.6.3 leans on.
- **Write contract:** `DagStore: DagRead + Growth` with `append` and
  `missing_prevs` (dag.rs:252-269 — introduced precisely so consumers can be
  "generic over storage … rather than naming `MemDagStore` concretely, which
  is what every downstream currently has to do").
- **Admission vocabulary:** `AppendOutcome::{Appended, Duplicate,
  MissingPrevs(Vec<EntryHash>), BadDigest}` (defer-never-reject doctrine,
  dag.rs:348-360).
- **Reference impls:** `MemDagStore` (linearizable growth dispatch under one
  mutex, dag.rs:379-385), `DagSnapshot` (immutable horizon capture,
  `Arc`-shared, optional epoch).
- From `encoding.rs`: `Digest` (`Digest::of` = blake3), `Encoder` (the
  canonical length-prefixed byte writer every hash-visible value goes
  through, encoding.rs:1-8).
- From `strategy.rs`: `AddressStrategy` (the "determinism quarantine": a pure
  function of payload bytes + `EntryHash`, "no corpus, no horizon, no store
  handle in any signature", strategy.rs:5-9), `RefinementFamily`,
  `PrefixNestedKey`, `StrategyId`, `ClassId`, `ClassAddress`, `SortKey` (with
  the load-bearing invariant: "a sort key MUST end with the 32-byte op hash",
  strategy.rs:55-57, and `SortKey::op_suffix`), `WholeValue` (the built-in
  base strategy).
- From `staged.rs`: `StagedDag` (base ∪ unpublished extension; O(staged)
  frontier and no re-sort, staged.rs:8-16; `stage`/`staged_topo`/`abandon`/
  `into_staged`; `DagRead` impl).
- From `rollback.rs`: `Rollback::{AbandonStaged, Retract, Compensate}` and
  `rollback_for` (the published/unpublished discrimination doctrine,
  rollback.rs:7-10).

### A.1.3 Cargo features

- `serde` (new name; replaces the serde *half* of today's `wire` cfg): gates
  the `serde::{Serialize, Deserialize}` derives on `EntryHash`, `Digest`,
  `SortKey`, `StrategyId` — exactly the four floor types that currently carry
  `#[cfg_attr(feature = "wire", …)]`. Pure derive addition; additive under
  Cargo feature unification by construction.
- **No `std` feature yet.** Honest current state: nothing in the crate
  compiles without std (`MemDagStore` holds `Mutex` + uses
  `catch_unwind`/`resume_unwind`, dag.rs:11, 498-506; `DagSnapshot` uses
  `HashMap`; `Growth` hands out `Arc<dyn Fn + Send + Sync>`). A `std` feature
  that cannot actually be disabled invites downstream
  `default-features = false` breakage at a distance; it ships only in the
  same change that makes the alloc-only subset (coordinates + `entry_hash` +
  the traits + `SortKey`) genuinely compile, with a `--no-default-features`
  CI check beside `scripts/check-wasm.sh`. The embedded floor is n=0 today
  (§C.4); the crate graph's only obligation is not to preclude it.
- **Unification hazard inventory:** `serde` is the only feature, it is
  derive-only, and it changes no observable behavior — any consumer enabling
  it is unobservable to every other consumer in the build. That property is a
  standing rule for every future floor feature (never behavior-changing,
  never subtractive).

### A.1.4 Dependency edges, and why minimal

`blake3` only (mandatory); `serde` optional. blake3 is mandatory because
identity *is* blake3 (`Digest::of`, encoding.rs:17-19) — an optional hash dep
would make `entry_hash` conditionally-existent. blake3 is itself
no_std-capable when the alloc floor eventually lands. Nothing else: no
postcard (engine-only), no proptest at runtime (dev-only, and it stays with
whichever crate's tests use it).

### A.1.5 Load-bearing invariants (the crate's "laws")

These are what make `hhhs-dag` a contract crate rather than a types crate:

1. **One identity function.** `entry_hash(&header)` = blake3 over
   `canonical_header` (dag.rs:97-113). Strategies supply order and addresses,
   "never this" (dag.rs:14-15, strategy.rs:1-3).
2. **Defer-never-reject admission.** `AppendOutcome::MissingPrevs` is a
   normal answer, not an error (dag.rs:259-260, 355-357).
3. **Linearizable growth registration.** "A subscriber registered before a
   commit is selected for that commit" (dag.rs:213-216); `MemDagStore`
   implements it with the single-mutex + pending-queue dispatch
   (dag.rs:379-384).
4. **The compaction escape hatch.** `DagDelta::appended_since → None` keeps
   "nothing changed" distinguishable from "I don't know" (dag.rs:230-234).
5. **The sort-key suffix invariant.** Every `SortKey` ends with the 32-byte
   op hash so order is total over all payloads (strategy.rs:55-57).
6. **The strategy purity quarantine.** No corpus, horizon, or store handle in
   any `AddressStrategy` signature (strategy.rs:5-9), so nothing derived ever
   rides the wire.

### A.1.6 no_std / wasm / embedded posture

wasm-safe today (no clock, no thread spawn, no getrandom anywhere in the
crate; `scripts/check-wasm.sh` already gates `hhhs-core` on
`wasm32-unknown-unknown` and extends to `hhhs-dag` at extraction). Not
no_std (§A.1.3). The future leaf profile is a *new `DagRead`/`DagDelta`
implementation* (bounded window + compacted view), not a change to this
crate's surface — see §A.6.3.

## A.2 `hhhs` — the facts front door

### A.2.1 Contents (module → crate map)

Moves verbatim: `cover.rs`, `register.rs`, `canonical_index.rs`, `lens.rs`,
`graph.rs`, `query.rs`, `void.rs`. Plus: `pub use hhhs_dag::{dag, encoding,
strategy, staged, rollback};`-style wholesale module re-export at identical
paths, and the same curated root re-export list `hhhs-core/src/lib.rs:29-51`
carries today *minus* the engine lines (the `replica` and `sync_session`
re-exports at lib.rs:39 and 42-44 do NOT come along).

### A.2.2 Complete public API surface

Everything here is a pure algorithm over `&impl DagRead`:

- `cover::ReachIndex` — `new(&impl DagRead)`, `contains`, `ancestors`
  (strict, present-only), `is_ancestor` (strict relation), `observed_at`,
  `causal_cover` (hhs3-ts `findCoverWithFilter` semantics, cover.rs:139-143),
  `concurrent_cover` ("the add-wins primitive", cover.rs:157-160).
- `register::resolve(candidates, reach) -> Option<EntryHash>` — causal maxima
  first, then max raw-bytes hash tiebreak (register.rs:8-24; the deliberate
  raw-bytes-not-base64 deviation from hhs3-ts is documented at
  register.rs:20-24).
- `canonical_index::{CanonicalRow, CanonicalIndex::build, canonical_live_values}`
  — horizon-labelled, domain-separated result roots
  (`"hhhs.canonical-index/1"`, canonical_index.rs:13).
- `lens::{Op, encode_op, decode_op, live, live_values}` — the position-keyed
  observed-remove set (lens.rs:2-7).
- `graph::{GraphOp, GraphEdge, GraphFacts, GraphVoid, encode_op, decode_op,
  graph_facts, graph_facts_live}` plus the emit helpers (`emit`, `genesis`,
  `add_node`, `add_edge`, `remove`, `ref_advance`, graph.rs:145-198).
- `query::{AddressIndex, Revision, Subscription, subscribe}` — subject to the
  §A.2.4 wart decision.
- `void::{Polarity, VoidReason, Verdict, SeniorityKey, VoidPolicy, VoidStats,
  verdict, verdict_with_stats}` — the cache-free verdict engine with
  deny-on-cycle (void.rs:11-19) and the documented no-verdict-cache doctrine
  (void.rs:21-32, citing Chen & Warren, JACM 43(1) 1996).

### A.2.3 The engine anti-re-export rule (and why not even a feature)

`hhhs` never depends on or re-exports `hhhs-sync` — **not even behind an
off-by-default feature**. Two independent reasons, both mechanical:

1. **Cargo feature unification defeats the manifest audit.** Features unify
   across the whole build graph: if `hhhs` had a `sync` feature, any crate
   anywhere in an app's graph enabling `hhhs/sync` would make
   `hhhs::sync_session::*` paths compile in *every* crate of that build —
   including crates whose own `Cargo.toml` never asked for engine access. The
   discipline the split buys is "engine access is a deliberate, separate
   dependency edge, visible in the consumer's own manifest"; a feature-gated
   re-export makes that edge invisible precisely where the audit looks.
2. **It defeats the falsification grep (§B.1.3).** The standing CI check
   matches `hhhs_(core|sync)::(reconciliation|sync_session|replica)` outside
   an allowlist of designated drivers. With a feature-gated re-export the
   engine becomes reachable as `hhhs::sync_session::…`, so the pattern must
   widen to bare `hhhs::` — at which point the grep can no longer distinguish
   a sanctioned front-door import from an engine reach-past without also
   parsing feature resolution, i.e. it stops being a grep. The no-re-export
   rule is what keeps the check sound *and cheap*.

Same reasoning, in miniature, is why there is no `hhhs::prelude`: the curated
root re-export list already serves discoverability without hiding provenance.

### A.2.4 Warts fixed additively at extraction time

Two functions in this band name the concrete store, which the `DagStore`
trait (dag.rs:252-257) now makes unnecessary:

- The `graph` emit helpers take `&MemDagStore` (graph.rs:145-198).
  Generalize to `&impl DagStore` — call-site compatible for every current
  caller (`hhhs-slice-tests` d1/d2/d3, datalog tests, kernel tests).
- `query::subscribe(Rc<MemDagStore>)` (query.rs:93). Its only out-of-kernel
  consumer is `hhhs-slice-tests/tests/a4_reactive.rs`, and it is superseded
  in practice by `hhhs-reactive`'s generic `stream_view`/`signal_vec_view`
  (hhhs-reactive/src/lib.rs:10-15 describes itself as "the reactive analogue
  of `hhhs_core::query::subscribe` but GENERIC over an arbitrary view
  function"). Decision: **demote `subscribe` + `Subscription` to
  `hhhs-testkit`**; `AddressIndex` and `Revision` stay in `hhhs::query`
  (`AddressIndex` is production surface for `hhhs-reactive` and `replica`;
  `Revision` is the shape `hhhs_reactive::Revision` generalizes). Fallback
  if co-review objects: generalize `subscribe` over `Rc<impl DagStore>` and
  keep it — both options are additive.
- **[correction]** `hhhs-datalog/src/advisory.rs:449` uses `MemDagStore` in
  *production* as a default type parameter
  (`pub type ViewFn<D = MemDagStore> = Box<dyn Fn(&D, &Position) -> BTreeMap<Key, Row>>`),
  not merely in tests as the design doc's table said. No boundary impact —
  `MemDagStore` is floor API re-exported through `hhhs` — but the front-door
  export list must keep `MemDagStore` at the root (it does).

### A.2.5 Features, dependencies, invariants, posture

- Features: `serde = ["hhhs-dag/serde"]` (a pure forward so front-door
  consumers can enable floor serde without naming `hhhs-dag`; walkie's
  tutti-core currently gets its derives via `hhhs-core/wire` and re-pins to
  this). Nothing else. No `test_utils` feature here — conformance lives in
  the testkit (§A.4).
- Dependencies: `hhhs-dag` only. All of C+D is pure std code — verified: the
  seven modules import nothing external.
- Invariants owned by this crate: the no-verdict-cache doctrine
  (void.rs:21-32, register.rs:26-37, cover.rs:14-17 — stated three times in
  three modules; the purity/adversarial-reorder tests are the standing
  guard); deny-on-cycle determinism (void.rs:11-19); position-keyed element
  identity in `lens` (lens.rs:2-7) and `graph` (graph.rs:6-9);
  `graph_facts`'s current frontier-only honesty check (the
  `debug_assert_eq!(at, &store.frontier())` at graph.rs:237-241, which stays
  until Stage-2 horizon-honoring lands).
- Posture: wasm-safe (inherits the floor's properties; adds no I/O, clock,
  or thread dependency). `check-wasm.sh` extends to it.

## A.3 `hhhs-sync` — the engine

### A.3.1 Contents (module → crate map)

Moves verbatim: `reconciliation.rs`, `sync_session.rs`. Verified imports:
`reconciliation` uses `dag::{DagRead, Entry, EntryHash}`,
`encoding::Digest`, `strategy::{SortKey, StrategyId}`
(reconciliation.rs:21-23); `sync_session` uses `dag::EntryHash`,
`reconciliation::*`, `strategy::StrategyId` (sync_session.rs:82-84). **The
dependency on `hhhs-dag` alone is a measured fact, not an aspiration.**

`replica.rs` does NOT come along (it needs `lens` and `query`,
replica.rs:13-14 — it goes to the testkit, §A.4).

### A.3.2 Complete public API surface

- `reconciliation`: `SessionHello` (strategy + per-session salt,
  reconciliation.rs:27-31), `KeyRange` (with `is_well_formed` — the
  peer-controlled-bounds guard against `BTreeMap::range` panics,
  reconciliation.rs:57-64), `FpBytes`, `Message`, `Config`, `Index`
  (`new`/`insert`/`len_total`/`len`/`fingerprint`/`hashes_in`/`split`),
  `respond` (pure, stateless), `opening`, `completion_plan(&impl DagRead,
  received)` (the causal-completion planner — the engine's one use of
  `DagRead` beyond `EntryHash`).
- `sync_session`: `SyncSession` (`initiate`/`accept`/`with_budget`/
  `set_root`/`salt`/`role`/`strategy`/`status`/`is_finished`/`is_complete`/
  `root_divergence`/`is_aborted`/`on_message`/`resume_admitted`/`resume`),
  `SyncMessage` (+ `encode`/`decode` under `wire`), `EntrySource` (the single
  trait an app implements, sync_session.rs:16-24), `Role`, `SessionStatus`,
  `SessionOutput`, `SessionBudget`, `SessionError`.
- Re-exports of the floor types appearing in its own API — `EntryHash`,
  `Entry`, `SortKey`, `StrategyId` — so a sync driver can depend on
  `hhhs-sync` alone (the tutti-core re-export-hygiene rule,
  tutti-core/src/lib.rs:43-48, applied kernel-side).

### A.3.3 Features

`wire = ["dep:serde", "dep:postcard", "hhhs-dag/serde"]` — exactly today's
contract: "the core state machine is feature-independent; only encode/decode
lives here" (hhhs-core/Cargo.toml comment), and "potluck's no-serde build is
unaffected" keeps being literally true because potluck's manifest enables no
features (potluck-hhhs/Cargo.toml:10).

### A.3.4 Load-bearing invariants

1. **"SortKey bytes only."** The engine never depends on a concrete strategy
   (reconciliation.rs:3-6); strategy data never rides the wire (the A7
   invariant restated at sync_session.rs:18-24: entries are opaque app bytes,
   the hash re-derived from verified content).
2. **Append-only postcard variants; generation bumps are ALPN changes.**
   "Wire-evolution rule: append variants only — postcard tags are ordinal. A
   protocol generation bump belongs in the app's ALPN" (sync_session.rs:90-96).
   Walkie's driver already implements the coordination side
   (src/net/sync.rs:55-60: "a change here is an ALPN/mode change, never a
   silent reshape").
3. **No `Send`/`Sync` bounds.** "It must run on wasm's single thread as
   happily as inside a native tokio task" (sync_session.rs:5-7).
4. **The driver contract** (sync_session.rs:35-72): resume-admitted after
   every `Entries` frame including the empty final one; the session root is
   the reconciled snapshot's root, advanced solely via `resume_admitted`/
   `resume`; close on `status() != Exchanging`, never on `is_complete()`
   (root-divergent runs finish with `is_complete() == false` forever).
5. **Budgets scale together.** Raising size budgets requires raising
   `max_requested_hashes` in step (sync_session.rs:68-72; restated in
   walkie's `SyncLimits` doc, src/net/sync.rs:106-118).

These invariants are why this crate's surface hardens on a different cadence
from everything else: it absorbed the nine hardening passes (frame caps, ack
ledger, O(|union|) transfer, `Divergent` status — the `bd23d4e` commit
message itself) while the read model sat still.

### A.3.5 Posture

wasm-safe by design (invariant 3); no clock, no runtime, sans-io. The only
place time exists is the *driver* (walkie's `SyncTimer`,
src/net/sync.rs:96-104), which is exactly why the driver is app-side.

## A.4 `hhhs-testkit` — conformance and prototype hosts

### A.4.1 Contents

- `test_utils/conformance.rs` → `hhhs_testkit::conformance`:
  `dag_read_conformance`, `dag_read_growth_conformance`,
  `graph_liveness_conformance`, `default_payloads`,
  `address_strategy_conformance`, `address_strategy_conformance_with`,
  `refinement_prefix_conformance`, `refinement_direction`
  (conformance.rs:70-432). "Any implementation, from any crate, is validated
  by the same assertions. This is what makes the seams real"
  (conformance.rs:1-5).
- `test_utils/foreign_log.rs` → `hhhs_testkit::foreign_log`: `ForeignId`,
  `ForeignRecord`, `ForeignMirror` (`boot_replay`/`ingest`/`entry_hashes`),
  `IngestOutcome`, `RejectReason`, `lift_foreign` — the reference shape for
  the walkie/potluck "mirror a foreign signed log" pattern.
- `rng.rs` → `hhhs_testkit::rng`: the seeded xorshift64* PRNG ("tests never
  touch `thread_rng`/clock entropy", rng.rs:1-3).
- `replica.rs` → `hhhs_testkit::replica`: `Replica`, `Stats`, `reconcile`,
  `reconcile_to_fixpoint` — the self-described prototype host
  (replica.rs:1-8).
- Per §A.2.4: `query::subscribe` + `Subscription` (demoted here).

### A.4.2 Dependencies and rationale

Depends on `hhhs` + `hhhs-sync` (both needed: conformance needs `graph`;
replica needs `lens`/`query` and `reconciliation`). A separate crate rather
than the `test_utils` feature because: (i) a feature means the production
crate carries test scaffolding code paths and its test target needed
`required-features = ["test_utils"]` gymnastics (hhhs-core/Cargo.toml's
`read_growth_conformance` stanza); (ii) a dev-dependency cannot leak into a
production graph the way an accidentally-unified feature can; (iii) potluck
already consumes it as a de-facto dev-dep (potluck-hhhs/Cargo.toml:16
declares `hhhs-core` a *second time* under `[dev-dependencies]` just to turn
the feature on — the shape of the workaround is the argument for the crate).

Note for scope honesty: `hhhs-reactive/src/test_utils.rs`
(`reactive_live_set_conformance`) is a *production, unconditional* module of
`hhhs-reactive` (lib.rs:52) and stays there — the testkit does not absorb
satellite conformance, only kernel conformance.

## A.5 `hhhs-core` — the deprecated path-stable shim

End state: `hhhs-core` contains no source modules. It depends on `hhhs` and
`hhhs-sync` and re-exports both at today's exact paths:

```rust
// hhhs-core/src/lib.rs, end state (illustrative, complete in shape)
pub use hhhs::{canonical_index, cover, graph, lens, query, register, void};
pub use hhhs::{dag, encoding, rollback, staged, strategy};   // floor, via hhhs
pub use hhhs_sync::{reconciliation, sync_session};
#[cfg(feature = "test_utils")]
pub use hhhs_testkit as test_utils_crate;                    // plus path shims:
#[cfg(feature = "test_utils")]
pub mod test_utils { pub use hhhs_testkit::{conformance, foreign_log}; }
pub mod replica { pub use hhhs_testkit::replica::*; }        // path-stable
pub mod rng { pub use hhhs_testkit::rng::*; }
// … plus the identical root re-export list lib.rs:29-51 carries today.
```

Features forward: `wire = ["hhhs-sync/wire", "hhhs-dag/serde"]`;
`test_utils = ["dep:hhhs-testkit"]` (an optional dependency, so the shim's
default build stays exactly as light as today's). One deliberate wrinkle:
today's `replica`/`rng` are unconditional modules, so the shim keeps them
unconditional — which means the shim (unlike `hhhs`) does depend on
`hhhs-testkit` unconditionally, or alternatively keeps `replica`/`rng` gated
behind a default-on feature. Recommendation: unconditional dep in the shim.
The shim is deprecated-for-new-use; its cost is borne only by laggards, and
laggards want maximal path fidelity, not minimal deps.

Deprecation window: §B.4.4.

## A.6 The `DagRead` seam, formalized

`DagRead` (dag.rs:117-136) is *the* load-bearing interface of the ecosystem:
every band above the floor is generic over it, and every storage answer below
the floor is an implementation of it. This section specifies the contract, its
implementations, and how the two known performance threads slot in as
alternative implementations *behind* it rather than changes *to* it.

### A.6.1 The contract

```rust
pub trait DagRead {
    fn entry(&self, h: &EntryHash) -> Option<Entry>;
    fn contains(&self, h: &EntryHash) -> bool;
    fn frontier(&self) -> Position;          // heads: unreferenced entries
    fn entries_topo(&self) -> Vec<Entry>;    // deterministic topo, hash ties
    fn all_hashes(&self) -> Vec<EntryHash>;
    fn snapshot(&self) -> DagSnapshot { … }  // DEFAULTED (the evolution model)
}
```

Semantic obligations (what `hhhs-testkit::conformance::dag_read_conformance`
certifies): present-only answers, deterministic `entries_topo` (predecessors
before successors, ties by entry hash — the property `ReachIndex::new`'s
one-pass fill depends on, cover.rs:36-38), `frontier` = entries not referenced
as a prev, and `snapshot()` capturing all fields at one linearization point
for concurrent hosts (dag.rs:128-135). The companion contracts layer on top:
`Growth` (linearizable registration), `DagDelta` (`appended_since` with the
`None` escape), `DagStore` (admission).

### A.6.2 Below the seam: the implementation table

| impl | where (verified) | what it proves about the contract |
|---|---|---|
| `MemDagStore` | hhhs-dag | full in-memory history; linearizable growth dispatch under one mutex (dag.rs:379-385) |
| `DagSnapshot` | hhhs-dag (dag.rs:587-611) | immutable horizon capture; `snapshot()` as a default method costs static readers nothing |
| `StagedDag` | hhhs-dag (staged.rs:73-124) | overlay reads over base ∪ staged in O(staged) without re-sorting |
| `ForeignMirror` | hhhs-testkit (`foreign_log.rs`) | a DAG lifted from foreign signed records — the walkie/potluck ingestion shape, in miniature |
| tutti-core `Store<L>`'s inner dag | walkie (crates/tutti-core/src/store.rs:25 — wraps `MemDagStore`; exposed to tests via `test-support`) | a DAG assembled from verified per-author signed-op logs; the kernel never sees the envelope |
| `PotluckDagHost` | potluck (crates/potluck-hhhs/src/host.rs) | a Send-compatible forwarding host whose `Clone` is "intentionally a deep fork" for validate-before-commit (host.rs:8-12); certified by `dag_read_conformance` + `dag_read_growth_conformance` (host.rs:142) |
| a SQL/durable store | anticipated in-source (dag.rs:254-257: "an in-memory host, a staged overlay, a SQL-backed store") | persistence without kernel change |
| **bounded-window / leaf store** | future (M3) | a suffix window + compacted view; `appended_since → None` past the window |

### A.6.3 The Θ(N²) `ReachIndex` thread, and the M3 windowed store

**The problem, measured.** `ReachIndex::new` materializes every present
entry's full strict-ancestor `BTreeSet` (cover.rs:44-49) — Θ(N²) space and
time on deep histories. The fix walkie already shipped is the template:
`tutti_core::Reach` keeps only the `prevs` adjacency (O(N+E)) and answers
`is_ancestor` by lazy reverse walk with a per-instance memo that "lives only
for the `Store::view` call that owns the `Reach`" (tutti-core
store.rs:531-547), backed by a `CausalPast` bridge so equivalence tests can
"drive the SAME fold on the Θ(N²) kernel index and assert the cheap lazy
`Reach` matches it" (tutti-core/Cargo.toml `test-support` comment).

**The specification decision:** the fix enters the kernel as a *second
reachability implementation behind the same query shape*, not a rewrite of
`cover.rs`. Concretely, when promoted (promotion gate below): a
`hhhs::cover::Reach` (name TBD at promotion) exposing the same
`is_ancestor`/`observed_at`/`causal_cover`/`concurrent_cover` query surface,
constructed from any `&impl DagRead`, equivalence-tested against `ReachIndex`
as the oracle — the identical oracle-vs-accelerator discipline
`hhhs-datalog`'s advisory layer already institutionalizes ("the advisory
layer only ever reproduces the same keyed result faster and is checkable
against a from-scratch `eval` at every horizon", hhhs-datalog/src/lib.rs:23-27).
**Promotion gate: a second consumer.** Today the lazy reach is n=1 (walkie);
it stays app-side until the Datalog consumer needs it (scaled
`GraphVoid`/`removers_of` walks are the expected trigger). Nothing in this
split blocks or forces the promotion; `cover.rs` moving into `hhhs` is
orthogonal to it.

**The M3 bounded-window store** is the same seam exercised from below: a
`DagRead + DagDelta` implementation holding a bounded recent window plus a
compacted view, answering `appended_since` with `None` for anything past its
window (the contract already written for exactly this at dag.rs:228-235) and
`entries_topo` with the window's entries. Consumers above degrade per the
documented rule: full recompute, or on a leaf, "the window is the world."
This lands as a new type in `hhhs-dag` (or an app crate) with **zero trait
change** — that is the point of specifying the seam now. The one trait
evolution it will likely want — a streaming
`for_each_topo(&self, f: impl FnMut(&Entry))` so windowed and large stores
avoid `entries_topo()`'s full-history `Vec<Entry>` clone — enters as a
default method implemented over `entries_topo`, per §A.7.

### A.6.4 Above the seam: the consumer inventory (all verified call shapes)

`ReachIndex::new(&impl DagRead)`; `lens::live/live_values(&impl DagRead)`;
`graph_facts(&impl DagRead, at)` and `graph_facts_live`; `void::verdict(p,
atom, at, &impl DagRead)` and `VoidPolicy::deps(…, &impl DagRead)` (the only
place a policy sees a store, void.rs:90-93); `canonical_live_values(&impl
DagRead, strategy)`; `rollback_for(&impl DagRead, …)`;
`reconciliation::completion_plan(&impl DagRead, received)`; hhhs-datalog's
`EdbSource` backends (`GraphEdb`/`GraphVoidEdb` over `DagRead`,
edb.rs:16-17); hhhs-reactive's views (a view is "a pure function `(store,
frontier) -> BTreeMap<K, R>`" over `DagRead + Growth` hosts,
hhhs-reactive/src/lib.rs:17-19); potluck's mirror (`DagRead` + `DagSnapshot`
+ `Growth*`, potluck-hhhs/src/{lib,host}.rs); tutti-core's store and fold.
The engine and the read model share *only* the floor — which is what makes
the sibling topology of §1 of the design doc possible at all.

## A.7 The public-surface freeze contract

### A.7.1 Trait evolution: default-methods-only

The `DagRead`/`Growth`/`DagDelta`/`DagStore` family evolves **by defaulted
methods only**:

- Adding a defaulted method (the `snapshot()` precedent, dag.rs:128-135;
  the planned `for_each_topo`) = minor change. Existing impls — currently
  seven across three repos (§A.6.2) — inherit the default; big stores
  override.
- Adding a *required* method, changing a signature, or tightening a bound =
  major change that forks every impl in three trees simultaneously. Under
  git-pinning this is a coordinated flag-day (§B.3) and is treated as a
  last resort.
- The same policy applies to `AddressStrategy`/`RefinementFamily`/
  `PrefixNestedKey` (implemented by two strategy crates + `WholeValue`) and
  to `EntrySource` (implemented by walkie's `RoomSyncSource`, and by potluck
  only if Q1 answers "yes").
- This policy gets written into `hhhs-dag`'s crate docs as a stated rule at
  extraction time, and Q3 asks potluck to co-sign it.

### A.7.2 Semver posture for a git-pinned kernel

While both apps pin revs (walkie: Cargo.toml:155-156 + 175 +
crates/tutti-core/Cargo.toml:27; potluck: potluck-hhhs/Cargo.toml:10-11 +
16 — all five sites currently `bd23d4e`), the release unit is the rev and
versions are coordination *labels*, not resolver inputs. Policy:

1. **Lockstep workspace versioning** (`workspace.package.version = "0.1.0"`
   today) across all kernel crates until crates.io publishing actually
   happens. Per-crate independent versions pre-publishing add ceremony with
   zero benefit.
2. **Every rev classifies every crate it touches** as `internal` (no public
   surface diff — downstream sees only a rev bump) or `surface` (public API
   diff — downstream re-review required). The split's payoff is that this
   classification becomes per-crate: a sync hardening rev that touches only
   `hhhs-sync` is reviewable by potluck as "no facts-band diff" structurally,
   which the monolith cannot offer.
3. **Breaking changes run expand → migrate → contract** (§B.3.4): additive
   API beside deprecated old, both apps migrate at their own rev, removal in
   a later rev. No rev ever strands either app.
4. **`hhhs-dag` is a public dependency in the semver sense** — its types
   appear in `hhhs`'s and `hhhs-sync`'s public APIs via re-export, so a
   major bump of the floor forces a coordinated major of the whole family.
   Consequence: the floor carries the strictest change policy and, when
   publishing happens, reaches 1.0 *first* (§B.4.2).
5. **Wire compatibility is a separate axis with its own rules** (append-only
   postcard variants; ALPN generation bumps coordinated app-side,
   sync_session.rs:90-96, src/net/sync.rs:55-60). The crate split neither
   touches nor relaxes it.

---

# PART B — COMPLETE DOWNSTREAM MIGRATION PLAN

## B.1 The import/impact matrix

### B.1.1 Summary table

One row per consumer. "Bands": A coordinates / B read-store contract /
C read model / D domain-shaping / E engine. Every symbol listed was found in
a real `use` line (file:line cited in §B.1.2); nothing is predicted.

| consumer | imports today (bands) | target crate(s) | engine band? |
|---|---|---|---|
| `tutti-core` (walkie substrate) | A+B production; C oracle (cfg-gated) | `hhhs` | **no** (oracle imports are `cfg(any(test, feature = "test-support"))`) |
| walkie `src/room` + hosts | A only (`EntryHash`, + test-support A) | `hhhs` via tutti-core re-export | no |
| walkie `src/net/sync.rs` | A + D-vocabulary + **E** | `hhhs-sync` (sole production driver) | **yes — designated** |
| walkie `tests/support/reconcile.rs` | **E** (dev harness) | `hhhs-sync` dev-dep | **yes — dev-only [correction: not in the design doc's audit]** |
| `hhhs-datalog` | A+B+D(graph); prod `MemDagStore` | `hhhs` (+ testkit dev) | no (tests use `rng` → testkit) |
| `hhhs-reactive` | A+B+C(lens,query)+D(strategy) | `hhhs` (+ testkit dev) | no (tests use `Replica` → testkit) |
| `hhhs-strategy-toyfacet` | A + strategy contract | `hhhs-dag` (+ testkit dev) | no |
| `hhhs-strategy-riffcat` | A + strategy contract; **example uses E** | `hhhs-dag` (+ testkit dev for tests *and* example) | example-only [correction] |
| `hhhs-slice-tests` | everything incl. E | `hhhs` + `hhhs-sync` + `hhhs-dag` + `hhhs-testkit` (all dev) | yes — the integration harness (sanctioned) |
| `potluck-hhhs` | A+B+**C(canonical_index)**+D(void) prod; conformance dev | `hhhs` (+ testkit dev) | **no** |

### B.1.2 Per-consumer detail: exact imports → exact new imports

**`tutti-core`** (`/laboratory/walkie-songie/crates/tutti-core`, pins
`bd23d4e` + `wire`):

- store.rs:25 `use hhhs_core::{AppendOutcome, DagRead, Entry, EntryHash,
  MemDagStore, Position};` → `use hhhs::{…same list…};`
- store.rs:31-34, under `#[cfg(any(test, feature = "test-support"))]`:
  `use hhhs_core::cover::ReachIndex;` / `use hhhs_core::register;` →
  `use hhhs::cover::ReachIndex;` / `use hhhs::register;` (the Θ(N²) oracle
  for `Reach` equivalence tests — production `view()` never builds it,
  store.rs:27-30)
- merkle.rs:23 + lib.rs:48 (`pub use hhhs_core::EntryHash;`) → `hhhs::…`
- Cargo: `hhhs-core = { git, rev, features = ["wire"] }` →
  `hhhs = { git, rev, features = ["serde"] }` (tutti-core needs the derives
  on `EntryHash` for its own wire types; it never uses postcard or the sync
  codec, so `hhhs/serde → hhhs-dag/serde` is the honest replacement for its
  current `wire`).

**walkie app tree** (`src/room`, `src/web`):

- room/store.rs:19, web/browser_host.rs:31 `use hhhs_core::EntryHash;` →
  ideally `use tutti_core::EntryHash;` (the re-export already exists,
  tutti-core/src/lib.rs:43-48); otherwise `use hhhs::EntryHash;`
- room/store.rs:1443-1444 (test module: `cover::ReachIndex`, `{DagRead,
  EntryHash, register}`) and room/test_support.rs:12 (`{Digest, EntryHash,
  Header, Position, entry_hash}`) → `hhhs::…` as a dev-dependency
- Cargo.toml:155 `hhhs-core = { …, features = ["wire"] }` → walkie's direct
  production kernel deps become `hhhs = { git, rev }` (walkie names only
  `EntryHash` + test-support A-band directly; the `wire` feature it enables
  today exists for the benefit of `src/net/sync.rs`, which moves to a
  `hhhs-sync = { git, rev, features = ["wire"] }` line)

**walkie `src/net/sync.rs`** (the designated driver; → future `tutti-net`):

- lines 40-47:
  ```rust
  use hhhs_core::{
      EntryHash, SortKey,
      reconciliation::{Config, Index, SessionHello},
      strategy::StrategyId,
      sync_session::{EntrySource, SessionBudget, SessionError, SessionStatus, SyncMessage, SyncSession},
  };
  ```
  → `use hhhs_sync::{EntryHash, SortKey, reconciliation::{…}, strategy …
  sync_session::{…}};` — one crate, everything through `hhhs-sync`'s floor
  re-exports (§A.3.2). `StrategyId` comes along in that re-export set.
- test module 808-809 (`encoding::Digest`, `reconciliation::{KeyRange,
  Message}`) → `hhhs_sync::…` (`Digest` via re-export or `hhhs_dag::…`; the
  re-export list in §A.3.2 should include `Digest` for exactly this reason —
  ship it there).

**walkie `tests/support/reconcile.rs`** (dev harness, line 28:
`use hhhs_core::reconciliation::{self, Config, Index, Message};`) →
`use hhhs_sync::reconciliation::{…};` under the existing dev-dep. This is a
**[correction]** to the design doc's falsification narrative, which reported
walkie's only engine imports as `src/net/sync.rs`; the L0 harness's
hand-rolled fixpoint pump is a third site. It is dev-only and sanctioned
(sync_session.rs:28-30 itself describes it as the loop `SyncSession`
formalized), but the CI grep allowlist (§B.1.3) must name it.

**`hhhs-datalog`**:

- ast.rs:10-11 (`encoding::Encoder`, `EntryHash`), edb.rs:16-17
  (`graph::GraphFacts`, `{graph, DagRead, Position}`), query.rs:10
  (`{DagRead, Position}`), advisory.rs:68-69 (`encoding::{Digest, Encoder}`,
  `{DagRead, MemDagStore, Position}` — production, see §A.2.4) →
  `use hhhs::…` throughout, a pure crate-name rename.
- tests: generic_read.rs:1 (`{graph, DagRead, Entry, EntryHash, MemDagStore,
  Position}`) stays `hhhs`; datafrog_oracle.rs:10 / eval.rs:7 / advisory.rs:11
  / common/mod.rs:6 (`rng::Rng`) → `use hhhs_testkit::rng::Rng;` (new
  dev-dep on `hhhs-testkit`).

**`hhhs-reactive`**:

- lib.rs:45-50 (`lens`, `query::AddressIndex`, `strategy::{AddressStrategy,
  ClassAddress}`, `{DagRead, DagSnapshot, EntryHash, Growth, GrowthEpoch,
  GrowthSubscription, Position}`) and src/test_utils.rs:6 (`{encode_op,
  AppendOutcome, DagRead, Entry, EntryHash, Growth, Op}`) → `use hhhs::…`,
  pure rename (every symbol is front-door; `strategy` arrives via the floor
  re-export).
- tests/acceptance.rs:19-22 (`… MemDagStore, Op, Position, Replica`) →
  `Replica` becomes `use hhhs_testkit::replica::Replica;` (new dev-dep);
  the rest `hhhs::…`.

**`hhhs-strategy-toyfacet`** (lib.rs:14-18: `dag::EntryHash`,
`encoding::{Digest, Encoder}`, `strategy::{AddressStrategy, ClassAddress,
ClassId, PrefixNestedKey, RefinementFamily, SortKey, StrategyId}`) →
`use hhhs_dag::…`, pure rename. The floor's cleanest demonstration: a
strategy author never pulls the read model.

**`hhhs-strategy-riffcat`**:

- lib.rs:44-48 (`dag::EntryHash`, `strategy::{…seven items…}`,
  `Digest as HhDigest`) → `hhhs_dag::…`.
- lib.rs:632 (test module: `{encode_op, AppendOutcome, DagRead, Entry,
  MemDagStore, Op}` — `encode_op`/`Op` are `lens`, band C) → dev-dep on
  `hhhs` (or `hhhs-testkit`, which re-exposes what its suites need).
- **[correction]** examples/walking_skeleton.rs:6-7
  (`reconciliation::Config`, `{reconcile, Replica}`) touches the engine and
  the prototype host. Examples build against dev-deps, so: dev-deps gain
  `hhhs-testkit` (for `reconcile`/`Replica`) and the example's `Config`
  import becomes `hhhs_testkit`'s re-export or a direct `hhhs-sync` dev-dep.
  The design doc's claim "strategy crates import A + D₁ only" is true of
  their *libraries* but not their example surface.

**`hhhs-slice-tests`** (all dev-deps; 16 test files enumerated): imports
span every band — `test_utils::{conformance, foreign_log}` (a1, foreign_log,
riffcat), `rng::Rng` (8 files), `reconcile`/`reconcile_to_fixpoint`/
`Replica`/`Stats` (a2, a4, a6, a7, a8×2, d1-d3, riffcat),
`reconciliation::Config` (common, d1, d2, d3), `subscribe` (a4),
`graph`/`GraphOp` (d1-d3), `verdict`/void vocabulary (authority_retraction),
plus floor symbols throughout. → dev-deps become `hhhs`, `hhhs-dag`,
`hhhs-sync`, `hhhs-testkit`; imports split accordingly (`rng`/`reconcile`/
`Replica`/`Stats`/conformance/foreign_log/`subscribe` →
`hhhs_testkit::…`; `reconciliation::Config` → `hhhs_sync::…`; the rest →
`hhhs::…`). This crate is *supposed* to depend on everything: it keeps the
cross-crate integration role and gains the falsification harness (§B.2
step 0).

**potluck (`potluck-hhhs`)** — the only potluck crate naming hhhs:

- lib.rs:13 `use hhhs_core::{AppendOutcome, DagRead, DagSnapshot, Encoder};`
  → `use hhhs::…`
- lib.rs:14-18 `pub use hhhs_core::{CanonicalIndex, CanonicalRow, Digest as
  HhhsDigest, Entry as HhhsEntry, EntryHash as HhhsEntryHash, GrowthEpoch as
  HhhsGrowthEpoch, GrowthSubscription as HhhsGrowthSubscription, Position as
  HhhsPosition};` → `pub use hhhs::…` — note these are *re-exports into
  potluck's own public API*, and **[correction]** they include
  `CanonicalIndex`/`CanonicalRow` (band C): the design doc's import table
  showed potluck's C column empty, but
  `offering_universe_matching_commitment(index: &CanonicalIndex, …)`
  (lib.rs:72-81) is public potluck API taking a kernel C-band type. The
  front-door freeze (Q2) therefore covers `canonical_index` on potluck's
  behalf, not only `void`.
- lib.rs:26 `pub use hhhs_core::{verdict, Polarity, Verdict, VoidPolicy,
  VoidReason};` → `pub use hhhs::…` (re-exported "so callers of
  `PotluckHhhsMirror::void_policy` need not depend on `hhhs-core` directly",
  lib.rs:23-25)
- host.rs:3-6 `use hhhs_core::{AppendOutcome, DagRead, DagSnapshot, Entry,
  EntryHash, Growth, GrowthEpoch, GrowthSubscription, MemDagStore,
  Position};` → `use hhhs::…`
- void_policy.rs:64 `use hhhs_core::{DagRead, Polarity, Position, Verdict,
  VoidPolicy, VoidReason};` → `use hhhs::…`
- host.rs:142-143 (test) `use hhhs_core::test_utils::conformance::{
  dag_read_conformance, dag_read_growth_conformance};` →
  `use hhhs_testkit::conformance::{…};`
- lib.rs:19 + 177/241/259: `hhhs_reactive::{Revision, stream_view}` —
  unchanged (`hhhs-reactive` keeps its name; its internal re-target to
  `hhhs` is invisible to potluck).
- Cargo (potluck-hhhs/Cargo.toml): `[dependencies] hhhs-core = { git, rev }`
  → `hhhs = { git, rev }`; `[dev-dependencies] hhhs-core = { git, rev,
  features = ["test_utils"] }` → `hhhs-testkit = { git, rev }`.
  `hhhs-reactive` line unchanged (rev bump only).

**Engine verdict for potluck, stated flat:** potluck imports **zero** engine
symbols — no `sync_session`, no `reconciliation`, no `replica`, anywhere in
its tree (grep verified, §B.1.3). Its anti-entropy is its own inventory
protocol over signed source ops: `potluck-wire/src/reconcile.rs` — "Planning
one round of anti-entropy, with no I/O in it. Two peers reconcile by
comparing inventories of signed source ops … This is the volume that owns
them" (reconcile.rs:1-16), operating on
`potluck_core::trade_engine::HhhsSourceOpBytes`, not on hhhs entries. The
`sync_session.rs:5` claim that "walkie-songie and potluck both drive it" is
**not true of potluck's tree today** — that is exactly co-review Q1, and the
kernel doc comment should be corrected or the potluck adoption made explicit
when Q1 is answered (§B.3.3).

### B.1.3 The falsification test — run for real, result

Executed against all three trees (grep for
`hhhs_core::(reconciliation|sync_session|replica)` and module-path
equivalents, excluding doc comments):

- **walkie:** three hits. (1) `src/net/sync.rs:40-47` — the designated
  production driver; after extraction this is the sanctioned
  `tutti-net → hhhs-sync` edge, not a reach-past. (2) `src/net/sync.rs:808-809`
  — the same file's test module. (3) `tests/support/reconcile.rs:28` — the
  L0 dev harness pump. (`src/net/mod.rs:258` and `src/net/sync.rs:3` are
  doc-comment mentions, not imports.)
- **potluck:** **zero hits.**
- **hhhs-rs workspace:** `hhhs-slice-tests` (the integration harness —
  sanctioned by role) and `hhhs-strategy-riffcat/examples/walking_skeleton.rs`
  (example-only; re-targets to testkit dev-deps, §B.1.2).

**Verdict: the boundary passes.** Every read-side consumer across six
production crates in three repos lives entirely in A-D; the engine band has
exactly one production consumer (walkie's driver) plus dev/example harnesses.
The standing CI encoding (step 0, §B.2): a grep in `hhhs-slice-tests` (and
mirrored in walkie CI) asserting that outside the allowlist
`{src/net/sync.rs, tests/support/reconcile.rs, hhhs-slice-tests/**,
hhhs-strategy-riffcat/examples/**}` no file in either app tree matches
`hhhs_(core|sync)::(reconciliation|sync_session|replica)`. The §A.2.3
no-facade-re-export rule is what keeps this grep's pattern complete.

## B.2 The sequenced migration: additive, extract-in-place, all trees green

Ordering principle (inherited from the tutti extraction that walkie ran
three times: genericize in place → re-export → extract): every step is a
single hhhs-rs rev that leaves (a) the hhhs-rs workspace suite, (b) walkie at
its *current* pin, and (c) potluck at its *current* pin compiling and green —
(b) and (c) trivially, because git pins insulate them until they choose to
re-pin; the real gate is that the *new* rev is one they can re-pin to with
zero source diff until step 6.

**Step 0 — falsification check + surface doc (no code moves).**
Add to `hhhs-slice-tests`: the §B.1.3 grep as a test, and a
`front_door_surface.rs` doc-test or `#[cfg(any())]`-guarded file spelling the
proposed `hhhs` lib.rs (C+D modules + floor re-export, nothing else).
Gate: workspace `cargo test` (the grep passes against today's monolith —
§B.1.3 says so). Rollback: revert the rev; nothing depends on it.
Downstream class: **internal-only** (no downstream diff; not even a rev bump
required).

**Step 1 — extract `hhhs-dag`.**
Move `dag.rs`, `encoding.rs`, `strategy.rs`, `staged.rs`, `rollback.rs` into
`hhhs-dag`; `hhhs-core` depends on it and re-exports at identical paths
(`pub use hhhs_dag::dag;` etc. — every `hhhs_core::dag::EntryHash`-style
path stays valid, and the root re-export list keeps re-exporting the same
names). Introduce `hhhs-dag/serde`; `hhhs-core/wire` becomes
`["dep:serde", "dep:postcard", "hhhs-dag/serde"]` (serde derives on floor
types now come through the dependency; the postcard codec is still in-crate
until step 2).
Gate: workspace `cargo test` + `scripts/check-wasm.sh` (extended with
`hhhs-dag`); `cargo tree -p hhhs-dag` shows blake3 (+optional serde) only.
Rollback: revert; downstream pins unaffected.
Downstream class: **internal-only** (re-pin optional, zero source diff).

**Step 2 — extract `hhhs-sync`.**
Move `reconciliation.rs`, `sync_session.rs` into `hhhs-sync` (dep:
`hhhs-dag`; feature `wire = ["dep:serde", "dep:postcard", "hhhs-dag/serde"]`;
floor re-exports per §A.3.2). `hhhs-core` re-exports both modules
path-stably; `hhhs-core/wire` forwards to `hhhs-sync/wire`. In-repo rewiring:
`replica.rs` (still in hhhs-core at this step) imports
`hhhs_sync::reconciliation`.
Gate: workspace `cargo test` including the `sync_adversarial`/`sync_props`
suites and the proptest regressions *unmodified*; wire-bytes gate: the
`SyncMessage::encode` golden expectations in sync_session's tests are
byte-identical (nothing about postcard encoding may change in a move).
Rollback: revert rev.
Downstream class: **internal-only** mechanically; **policy-gated by Q1** —
this is the moment `hhhs-sync`'s surface becomes a named thing, so the answer
to "does potluck ever drive `SyncSession`?" decides whether its API freezes
hard now (potluck's `EntrySource` shapes join the review) or stays soft with
walkie as sole consumer (§B.3.3).

**Step 3 — extract `hhhs-testkit`.**
Move `test_utils/{conformance,foreign_log}.rs`, `rng.rs`, `replica.rs` into
`hhhs-testkit` (deps: `hhhs-core` at this step — becomes `hhhs` + `hhhs-sync`
after step 4; dev-deps carry proptest as needed). `hhhs-core` keeps
`test_utils = ["dep:hhhs-testkit"]` forwarding plus unconditional
`replica`/`rng` path shims (§A.5). In-repo rewiring: `hhhs-reactive`
tests (+`Replica`), `hhhs-datalog` tests (+`rng`), `hhhs-slice-tests`,
riffcat's example — all gain `hhhs-testkit` dev-deps with mechanical import
renames.
Gate: workspace `cargo test`; the `read_growth_conformance` required-features
stanza migrates to an ordinary testkit dev-dep.
Rollback: revert rev.
Downstream class: **internal-only for production** (potluck's production
build has no test_utils); **potluck CI-affecting** (its conformance dev-dep,
host.rs:142) — this is co-review Q3's forwarding-window question; the
`test_utils` feature forward keeps potluck's current CI compiling untouched
for as long as they want.

**Step 4 — introduce `hhhs`; hollow `hhhs-core` to a shim.**
Move `cover.rs`, `register.rs`, `canonical_index.rs`, `lens.rs`, `graph.rs`,
`query.rs`, `void.rs` into `hhhs`; `hhhs` re-exports the floor wholesale;
`hhhs-core` becomes the pure shim of §A.5. Ship the §A.2.4 additive
wart-fixes with the move (graph emit helpers over `&impl DagStore`;
`query::subscribe` demoted to testkit with an `hhhs-core::query::subscribe`
shim re-export preserved for path stability). Re-target `hhhs-testkit` to
`hhhs` + `hhhs-sync`.
Gate: workspace `cargo test`; `check-wasm.sh` now covering
`hhhs-dag`/`hhhs`/`hhhs-sync`; **the step-0 falsification check runs against
the real `hhhs/src/lib.rs`** and the front-door export list is diffed against
the step-0 mock — any drift is reviewed, not silently shipped.
Rollback: revert rev (the shim makes this a one-commit revert; no downstream
has re-pinned mid-step).
Downstream class: **coordination-gated by Q2** — this rev freezes the
front-door export list, which is part of potluck's own public API by proxy
(§B.1.2 potluck row: `CanonicalIndex`, `CanonicalRow`, `void` vocabulary all
`pub use`d). Potluck sign-off on the export list happens *before* this rev
merges. This is the first step that **needs** potluck sign-off (steps 2-3
want potluck's *answers*, but nothing in them can strand potluck).

**Step 5 — downstream migrations, at each repo's leisure** (the shim means
no flag day; each is a normal PR in its own repo against a single re-pinned
rev):

- 5a. hhhs-rs satellites re-target: `hhhs-datalog` → `hhhs` (+testkit dev);
  `hhhs-reactive` → `hhhs` (+testkit dev); `hhhs-strategy-toyfacet`/`-riffcat`
  → `hhhs-dag` (+`hhhs` or testkit dev, per §B.1.2); `hhhs-slice-tests` →
  the full new set. Gate: workspace suite. (Can land with step 4 or after.)
- 5b. walkie: tutti-core → `hhhs` (`serde` feature); app-tree `EntryHash`
  imports → `tutti_core::EntryHash` where possible, else `hhhs`;
  `src/net/sync.rs` + `tests/support/reconcile.rs` → `hhhs-sync/wire`.
  Gates: golden entry-hash vector, wire-frame round-trips, L0 convergence
  suite (`tests/l0_*`), `reach_equiv` oracle tests — all **unmodified**.
- 5c. potluck: the §B.1.2 potluck diff (two manifest lines + five import
  lines + one test import). Gates: potluck's conformance suites
  (`dag_read_conformance`, `dag_read_growth_conformance`) and its parity
  suites — unmodified.

**Step 6 — contract.** After both apps re-target (verified by grep for
`hhhs_core::` in both trees), `hhhs-core` gains a `#[deprecated]` crate-level
notice in docs and README; removal policy in §B.4.4.

Sequencing invariants: steps 1-3 are pure mechanical moves with re-export
nets, one rev each, revertible in isolation; step 4 is the only rev with a
review gate in front of it, which is where the gate belongs. No step touches
reconciliation/sync logic, wire bytes, verdict semantics, or any test
expectation — the walkie golden-vector/L0 gates and the kernel
conformance/adversarial suites are the proof each rev is an extraction, not
a change.

## B.3 Cross-repo coordination choreography

### B.3.1 The mechanics of one kernel rev

hhhs-rs is git-pinned by five manifest sites: walkie `Cargo.toml:155`
(hhhs-core+wire), `:156` (hhhs-reactive), `:175` (dev hhhs-core),
`crates/tutti-core/Cargo.toml:27` (hhhs-core+wire); potluck
`crates/potluck-hhhs/Cargo.toml:10` (hhhs-core), `:11` (hhhs-reactive), `:16`
(dev hhhs-core+test_utils). The dance per step:

1. **Push** the rev to `gitlab.com/micahscopes/hhhs-rs` (note: the current
   pin `bd23d4e` is the head of branch `harden-sync-session`, not master —
   merging that branch is an outstanding coordination item that predates
   this plan and should ride step 0 or 1).
2. **Re-pin walkie** (all four sites in one commit; walkie gates run).
3. **Co-review + re-pin potluck** (all three sites; potluck gates run).
4. Only after both apps are green at the new rev does the next step's rev
   start. One rev in flight at a time — the lockstep version train's whole
   point.

### B.3.2 Per-step classification

| step | downstream diff | class | potluck involvement |
|---|---|---|---|
| 0 | none | internal-only | notify |
| 1 | none (paths stable) | internal-only rev bump | notify |
| 2 | none (paths stable) | internal-only rev bump | **Q1 answer wanted before surface hardens** (blocking for the *freeze*, not the *rev*) |
| 3 | none prod; potluck CI dep shape eventually | internal-only + CI heads-up | **Q3: forwarding window agreed** |
| 4 | none (shim); export list freezes | **breaking-adjacent: sign-off required pre-merge** | **Q2: co-own the export list** |
| 5a-c | import renames per repo | each repo's own PR; no cross-repo lockstep needed (shim) | potluck merges 5c on its own schedule |
| 6 | shim deprecated | notify + agreed window | Q3's window governs |

### B.3.3 Where Q1/Q2/Q3 gate the sequence, precisely

- **Q1 (does potluck drive `SyncSession`?)** gates *the hardness of
  `hhhs-sync`'s surface freeze* at step 2, not the extraction itself. If
  "no": walkie's driver is the only consumer; `hhhs-sync` holds a soft
  surface (hardened internals, evolvable API) until tutti-net freezes it
  consumer-side, and the false claim at sync_session.rs:5 gets corrected in
  the same rev. If "yes"/"planned": potluck's `EntrySource` shape (its
  source-op records vs walkie's framed `SignedOp`) joins the step-2 review
  and the surface freezes hard immediately. Either answer is fine; the plan
  refuses to freeze a surface whose consumer count is unknown.
- **Q2 (front-door export list)** hard-gates step 4's merge. The measured
  sharpening this audit adds: the list potluck co-owns includes
  `canonical_index` (`CanonicalIndex`/`CanonicalRow` are in potluck's public
  API, §B.1.2), the full `void` vocabulary, `DagSnapshot`, `Growth*`, and
  `MemDagStore` — not just `void` as the co-review doc's framing suggests.
- **Q3 (trait evolution + testkit relocation)** gates the *durations*: the
  `test_utils` feature-forward window (step 3 → step 6) and the
  default-methods-only policy adoption (§A.7.1) — potluck implements the
  B-band traits on `PotluckDagHost` and certifies via the conformance
  suites, so both are potluck-CI-visible commitments.

### B.3.4 Breaking changes after the split (the standing recipe)

Unchanged from the design doc §6.3, now stated per-crate: land additive form
beside `#[deprecated]` old in the owning crate → walkie re-pins + migrates
(golden vector + L0 gates) → potluck co-reviews + re-pins + migrates
(conformance gates) → remove the deprecated form in a later rev. Wire
generation bumps remain app-ALPN-coordinated, orthogonal to crate versions.

## B.4 Publishing and versioning

### B.4.1 Now: git-pinned, lockstep

Release unit = rev; `workspace.package.version` stays lockstep across all
five kernel crates (`hhhs-dag`, `hhhs`, `hhhs-sync`, `hhhs-testkit`,
`hhhs-core`). Version numbers are labels on the train, bumped together when
a surface change lands, purely to make CHANGELOGs legible.

### B.4.2 Later: crates.io, bottom-up

When publishing happens (nothing in this plan forces it): publish
`hhhs-dag` → `hhhs` → `hhhs-sync` → `hhhs-testkit` → satellites, in that
order. `hhhs-dag` plays the `http`/`bytes` role — the shared-vocabulary
public dependency — and therefore reaches 1.0 first so the crates above can
iterate 0.x against stable vocabulary. Pre-1.0, cargo treats 0.x→0.(x+1) as
breaking; the lockstep train makes that legible. Publishing blockers that
simply stay out of the publish set: `hhhs-strategy-riffcat` and
`hhhs-slice-tests` carry a path dep on `riff-catalog-core`
(`../../riff-catalog/crates/riff-catalog-core`) outside the repo.
`hhhs-core` is published once with a deprecation notice, or never.

### B.4.3 MSRV

hhhs-rs currently declares none (edition 2021, no `rust-version`). walkie
pins Rust 1.97.1 (`rust-toolchain.toml`) and tutti-core declares
`rust-version = "1.97.1"`; potluck manages toolchains via its flake. At
step 1, set `workspace.package.rust-version` in hhhs-rs to the oldest
toolchain both apps run (today: 1.97.1 or older works — the kernel uses no
recent features), and treat MSRV raises as surface-class changes (co-review,
not silent).

### B.4.4 The `hhhs-core` deprecation window

Explicitly time-unbounded but state-bounded: the shim survives until (a)
both apps' trees grep clean of `hhhs_core::`, and (b) potluck confirms its
CI no longer needs the `test_utils` feature forward (Q3). After that, the
shim is retired from the workspace (or left permanently as a 20-line crate —
its carrying cost is near zero, and in a git-pinned world an unused shim
hurts nobody; removal is a tidiness decision, not a correctness one). The
shim must never gain new API: anything new lands in the real crates and is
*not* mirrored into the shim, so the shim's staleness is itself the nudge to
migrate.

---

# PART C — CONTEXT (for future architectural reference)

## C.1 Rationale

### C.1.1 The five-band decomposition

`hhhs-core`'s 18 modules sort into five kinds of thing, and the crate lines
follow the bands (full band table with symbols: design doc §0.1; §A.0 above
re-verifies the import edges):

- **A — Coordinates:** `dag`'s identity types + `entry_hash`; `encoding`.
- **B — Read/store contract:** `DagRead`/`Growth`/`DagDelta`/`DagStore`,
  `AppendOutcome`, `DagSnapshot`, `MemDagStore`, `StagedDag`, `rollback`.
- **C — Read model:** pure algorithms over any `DagRead` — `cover`,
  `register`, `canonical_index`, `lens` reads, `query`.
- **D — Domain-shaping contracts:** `strategy`, the `lens`/`graph` payload
  languages, `void`.
- **E — Engine:** `reconciliation`, `sync_session` (+ the `replica`
  prototype and `rng` as harness material).

Crate mapping: `hhhs-dag` = A + B + the strategy contract (D₁); `hhhs` =
C + rest-of-D; `hhhs-sync` = E-proper; `hhhs-testkit` = the harness band.
The bands are documentation structure; crates are coordination structure —
three production crates + a testkit is both the floor of what expresses the
real seams and the ceiling of what today's consumer count justifies.

### C.1.2 The facts | engine cut is a measurement

The empirical content of this whole design is §B.1's matrix: across six
production consumers in three repos, exactly one (walkie's designated sync
driver) imports band E, and it imports only E + coordinate/strategy
vocabulary. Every read-side consumer — two apps' substrates, a Datalog
engine, a reactive adapter, two strategy crates — lives entirely in A-D.
The crate lines are drawn where the imports already fall.

Two in-source facts show the kernel *anticipated* this cut:

- `graph.rs:213-214`: `GraphFacts` is "a neutral, Datalog-agnostic structure
  so `hhhs-core` need not know the query engine; **the datalog crate** maps
  these to `node`/`edge` relations." That crate exists (`hhhs-datalog`:
  stratified naive + semi-naive with negation, `GraphEdb`/`GraphVoidEdb`, an
  advisory incremental layer checked against the from-scratch oracle) — the
  kernel grew its second reference consumer before anyone designed for one.
- `void.rs:21-32` grounds retraction in well-founded semantics (Chen &
  Warren, JACM 43(1) 1996) as the reason there is no verdict cache — the
  canonical semantics of Datalog-with-negation, cited in the kernel's own
  retraction engine. The graph/void/negation bands are what a reactive
  Datalog graph DB stresses and music barely touches; `hhhs-datalog`'s
  `GraphVoidEdb` already routes query-layer stratified negation over
  kernel-layer (Remove) negation as "a SECOND, separate regime"
  (hhhs-datalog/src/lib.rs:12-14).

### C.1.3 Why `DagRead` anchors the floor

It has the most implementors (seven today across three repos, §A.6.2) and
the most generic consumers (a dozen call shapes, §A.6.4) of any item in the
system, so its home's version churn multiplies across the entire ecosystem.
The floor exists to give it a home with no other reason to change.

### C.1.4 Corrections this audit made to the prior design doc

Collected from the **[correction]** flags — all four sharpen rather than
overturn the design:

1. **Potluck consumes band C.** `CanonicalIndex`/`CanonicalRow` are
   `pub use`d into potluck's public API and appear in a public function
   signature (potluck-hhhs/src/lib.rs:14-18, 73-81). Design-doc table said
   C was empty for potluck. Consequence: Q2's freeze scope includes
   `canonical_index`.
2. **`hhhs-datalog` uses `MemDagStore` in production**
   (advisory.rs:449 `ViewFn<D = MemDagStore>` default type parameter), not
   tests-only. Consequence: `MemDagStore` stays in the front door's root
   re-exports (it does anyway).
3. **Walkie has a third engine-import site**: `tests/support/reconcile.rs:28`
   (the L0 harness pump), beside `src/net/sync.rs`'s production and test
   imports. Dev-only and sanctioned, but the falsification allowlist must
   name it.
4. **The strategy crates are floor-only in their libraries, not their
   examples**: `hhhs-strategy-riffcat/examples/walking_skeleton.rs` imports
   `reconciliation::Config` + `reconcile`/`Replica`. Consequence: testkit
   dev-deps for the strategy crates, and "strategy crates re-target to
   hhhs-dag" is true with that one asterisk.

## C.2 Alternatives considered and rejected

- **`hhhs-types` / `hhhs-primitives` as the floor.** The antipattern by
  name: a "types" crate advertises shape-without-law, imposes no membership
  test, accretes every cyclically-awkward struct, and turns every bump into
  a world rebuild. `hhhs-dag` is the opposite — law-dense (§A.1.5's six
  invariants), nearly-never-changing, with an enforceable membership test
  ("does this define what an entry is / how a store is observed / how bytes
  acquire order and addresses?"). The name carries the test.
- **Finer splits: `hhhs-void` / `hhhs-graph` / `hhhs-lens` / `hhhs-cover`.**
  Each would have one or two consumers; the modules are 108-641 lines; every
  extra crate in a git-pinned two-app workflow is another version to
  coordinate for zero independent change axis. n=1 over-splitting, refused.
  Likewise no separate `hhhs-strategy` contract crate: 128 lines whose two
  dependents both already sit on the floor.
- **`hhhs-engine` as the sync crate's name.** Says nothing ("engine of
  what?") and collides with the codebase's *other* engine — `void.rs:1`
  names itself "the cache-free verdict engine", and hhhs-datalog has two
  evaluation engines. `hhhs-sync` names the consumer's promise
  ("synchronize my store with a peer"); `reconciliation` stays as the
  mechanism-named module inside, where mechanism-naming is correct.
- **Feature-gated engine re-export from `hhhs`** (`hhhs/sync`). Rejected on
  the two mechanical grounds of §A.2.3: feature unification grants engine
  access build-wide without a manifest edge, and it forces the falsification
  grep to widen past soundness. Engine access must be a deliberate,
  separate, grep-visible dependency edge.
- **Two-crate fallback (fold `hhhs-dag` into `hhhs`).** Viable, and the
  named fallback if co-review balks at three crates — but then `hhhs-sync`
  depends on the full read model to spell `SortKey`, dragging
  lens/graph/void/query into every sync driver's build and review scope, and
  the future leaf floor loses its landing zone. The floor costs one
  Cargo.toml and buys the sibling topology.
- **Keeping `hhhs-core` as the permanent front door.** "-core" is
  position-naming, and after the split it would be false twice (the floor is
  `hhhs-dag`; the engine is outside). It survives only as the shim, which is
  the one job the name still fits.

## C.3 Risks and mitigations

1. **Coordination arithmetic.** Five kernel Cargo.tomls and a lockstep train
   where there was one crate; every contributor must learn which band a
   change belongs to. Mitigation: the membership tests (§A.1.5's "laws"
   rule, §C.2's floor test), lockstep versions, path-stable shims, and the
   one-rev-in-flight rule (§B.3.1).
2. **The engine's second consumer is unverified (Q1).** If potluck never
   drives `SyncSession`, `hhhs-sync` is n=1 and a hard early freeze would be
   premature. Mitigation: §B.3.3 makes freeze-hardness contingent on Q1's
   answer, and the extraction itself is Q1-independent.
3. **The export-list freeze freezing potluck by proxy (Q2).** Potluck
   `pub use`s kernel types into its own API (void + canonical_index).
   Mitigation: step 4 hard-gates on potluck sign-off; the step-0 mock makes
   the list reviewable before anything moves.
4. **Shim drift.** A shim that gains new API becomes a second front door.
   Mitigation: the never-new-API rule (§B.4.4) and the deprecation notice.
5. **Silent wire drift during the moves.** Any byte change during steps 1-4
   is a schema move wearing an extraction costume. Mitigation: the
   sync-test golden expectations + walkie's golden entry-hash vector + L0
   suite as unmodified gates on every rev; postcard's append-only rule is
   already policed in-source (sync_session.rs:90-96).
6. **The branch pin.** `bd23d4e` is the head of `harden-sync-session`, not
   master. A split executed on an unmerged branch multiplies rebase pain.
   Mitigation: merge the branch as part of step 0/1 (§B.3.1) — an item both
   the tutti doc and this plan flag.
7. **Θ(N²) `ReachIndex` promoted too early / too late.** Too early is n=1
   over-fit; too late starves the datalog consumer at scale. Mitigation: the
   explicit promotion gate + the oracle discipline (§A.6.3) — the seam is
   specified now so promotion is a small PR, not a redesign.

## C.4 The n-count audit: which boundaries are earned

| boundary | n today (verified) | verdict |
|---|---|---|
| facts \| engine (`hhhs` \| `hhhs-sync`) | 6 production consumers facts-only; 1 engine driver | **earned — a measurement** |
| floor (`hhhs-dag`) beneath both | engine's "SortKey bytes only" invariant + 2 strategy crates importing floor-only libraries + every B-band implementor | earned by dependency hygiene more than consumer count; two-crate fallback exists |
| testkit as a crate | potluck CI + walkie oracle patterns + 3 workspace crates' dev-deps | earned |
| engine surface hard-freeze | n=1 (walkie) until Q1 says otherwise | **deferred — Q1-contingent** |
| no_std/alloc floor shipping now | n=0 | **not earned; explicitly deferred** (§A.1.3) |
| lazy `Reach` promotion into `hhhs` | n=1 (walkie) | **not earned yet; promotion gate at n=2** (§A.6.3) |
| finer void/graph/lens splits | n=1 each | refused |

## C.5 The genericity story, honestly

The real evidence that the hhhs layer is generic is **multi-consumer at
`hhhs`**: four maximally-different read models already stand on the same
substrate — walkie's music fold (`OpLanguage::fold` over `FoldCtx`), the
Datalog engine (`GraphEdb`/`GraphVoidEdb` + stratified evaluation), the
reactive adapter (view functions over `DagRead + Growth`), and potluck's
trade-engine projections (its mirror + `PotluckVoidPolicy`). That is
genericity demonstrated by use, not asserted by abstraction.

One premise from earlier planning was investigated and is **wrong**: the
idea that potluck would adopt tutti-core's `OpLanguage` seam. Potluck does
not consume tutti anything — it consumes **hhhs directly** through its own
mirror crate (`potluck-hhhs`, "Experimental HHHS-compatible read mirror for
Potluck … preserve the exact signed source op bytes, derive a deterministic
secondary entry id with `hhhs-core`", lib.rs:1-7), keeps its own signed-op
model (`potluck-ops`), and runs its own anti-entropy (`potluck-wire`).
walkie and potluck are *parallel* instantiations of the same kernel, not
layers of one stack. The corollary: `tutti-core`'s genericity is still n=1
(walkie) and is argued in the tutti doc on its own terms; the kernel's
genericity is n≥4 and is the load-bearing fact here.

Where this design still extrapolates from walkie alone — the lazy reach, the
leaf floor, the engine surface freeze — it defers (§C.4).

## C.6 How this enables "various query/lens/db engines on top"

The front door is, deliberately, an engine-author's toolkit. The exact
primitives a query/lens/DB engine composes, with their real signatures:

- **Fact extraction:** `graph::graph_facts_live(&impl DagRead, at) ->
  GraphFacts { nodes: BTreeMap<EntryHash, Vec<u8>>, edges:
  BTreeMap<EntryHash, GraphEdge> }` — void-filtered node/edge relations
  (Remove retracts; a node's retraction transitively aborts incident edges,
  graph.rs:16-20). This is precisely the EDB `hhhs-datalog` maps to
  `node`/`edge` (edb.rs), by the kernel's own stated intent (graph.rs:213-214).
- **Set semantics:** `lens::live(&impl DagRead)` / `lens::live_values` — the
  position-keyed observed-remove set as a pure fold.
- **Value semantics:** `register::resolve(candidates, &ReachIndex)` — causal
  maxima then raw-bytes-hash tiebreak; the LWW read every replica computes
  identically.
- **Causality:** `cover::ReachIndex` — `is_ancestor`, `observed_at`,
  `causal_cover` (currently-winning matches), `concurrent_cover` ("the
  add-wins primitive", cover.rs:157-160) — the building blocks walkie's
  add-wins fold and any custom CRDT-shaped read are made of.
- **Commitment:** `canonical_index::CanonicalIndex::build(at, rows)` —
  byte-ordered, domain-separated, horizon-labelled result roots; "graph/
  void/Datalog consumers should build rows from their own horizon-pinned
  view and call `CanonicalIndex::build`" (canonical_index.rs:74-76). Potluck
  already does exactly this for its matching-universe commitments.
- **Reactivity:** the `Growth`/`GrowthEpoch` epoch + `query::AddressIndex`
  for equivalence queries, with `hhhs-reactive`'s `stream_view`/
  `signal_vec_view` as the generic recompute-on-growth adapters (and
  `hhhs-datalog`'s advisory layer as the template for making that fast
  without changing observable results).
- **Negation/retraction:** `void::verdict(policy, atom, at, &impl DagRead)`
  under a domain `VoidPolicy` — well-founded, deny-on-cycle, cache-free
  liveness with attributable `VoidReason`s. Potluck's `PotluckVoidPolicy`
  shows the shape: state the domain's rules as polarity edges and let the
  engine do the propagation (void_policy.rs:8-14).

**The honest boundary:** consumers bring their own evaluator. The kernel
supplies verified facts, causality, retraction verdicts, canonical roots,
and change notification; it deliberately contains no rule language, no
query planner, no materialization strategy. `hhhs-datalog` brings stratified
naive/semi-naive evaluation; walkie brings a staged music fold; potluck
brings trade-engine projections and a matcher. That division — facts below,
evaluators above, `DagRead` in between — is the same line drawn twice more
in this ecosystem (oracle-vs-advisory in datalog, `ReachIndex`-vs-`Reach` in
tutti), and it is the line this crate split makes structural.

---

## Appendix: end-state dependency graph, exhaustively

```
hhhs-dag         → blake3            (feature serde: dep:serde)
hhhs             → hhhs-dag          (re-exported wholesale; feature serde → hhhs-dag/serde)
hhhs-sync        → hhhs-dag          (feature wire: dep:serde, dep:postcard, hhhs-dag/serde)
hhhs-testkit     → hhhs, hhhs-sync
hhhs-core (shim) → hhhs, hhhs-sync, hhhs-testkit
                   (wire → hhhs-sync/wire + hhhs-dag/serde; test_utils → testkit paths)

hhhs-reactive    → hhhs, futures, futures-signals        (+dev: hhhs-testkit)
hhhs-datalog     → hhhs                                  (+dev: hhhs-testkit, datafrog)
hhhs-strategy-toyfacet → hhhs-dag                        (+dev: hhhs-testkit)
hhhs-strategy-riffcat  → hhhs-dag, hhhs-strategy-toyfacet, riff-catalog-core,
                          serde, serde_json              (+dev: hhhs-testkit)
hhhs-slice-tests → (dev) hhhs, hhhs-dag, hhhs-sync, hhhs-testkit, satellites
                   — integration + falsification harness

tutti-core       → hhhs (feature serde)                  (+dev oracle via test-support)
walkie src/net (→ tutti-net) → hhhs-sync (feature wire)
walkie-songie    → tutti-core (+ hhhs only for EntryHash until re-export cutover)
potluck-hhhs     → hhhs, hhhs-reactive                   (+dev: hhhs-testkit)
potluck (rest)   → potluck-hhhs — no other potluck crate names a kernel crate
```


