# The hhhs crate ecosystem: floor, front door, engine

**Status:** design recommendation, 2026-08-08. Companion to
`docs/vision/tutti-crate-architecture.md` (the walkie-side stack this kernel
carries) and `docs/vision/eventually-consistent-pitchsets.md`. Grounded in
`hhhs-rs @ bd23d4e` (the rev walkie and potluck both pin — walkie
`Cargo.toml`, `potluck/crates/potluck-hhhs/Cargo.toml:10`), read module by
module; in `crates/tutti-core` at HEAD; and in potluck at
`/laboratory/dweb-camp-2026/potluck`. No code changed; this is a crate-graph
design only. The engine is load-bearing (the sync layer's nine hardening
passes live in `sync_session.rs`/`reconciliation.rs`); everything proposed
here is additive re-structuring — moves and re-exports, never rewrites.

The question under design: `hhhs-core` is one crate wearing five hats. As
tutti/walkie, potluck, and the in-repo satellites (`hhhs-datalog`,
`hhhs-reactive`, the strategy crates) each grab a different subset of it, what
is the right crate decomposition — the one that names what consumers stand
on, keeps the two-app co-review workflow cheap, and doesn't fracture a
hardened kernel into semver confetti?

---

## 0. What the code already says (evidence before design)

### 0.1 The five bands, verified in-source

`hhhs-core`'s 18 modules sort into five kinds of thing, not "types + engine":

| band | contents (module: symbols) | depends on bands |
|---|---|---|
| **A — Coordinates** | `dag`: `EntryHash`, `Position`, `Header`, `Entry`, `canonical_header`, `entry_hash` (dag.rs:111 — "the one and only identity function"); `encoding`: `Digest`, `Encoder` | — |
| **B — Read/store contract** | `dag`: `DagRead` (dag.rs:117), `Growth`/`GrowthEpoch`/`GrowthSubscription` (dag.rs:217), `DagDelta` (dag.rs:228), `DagStore` (dag.rs:261), `AppendOutcome`, `DagSnapshot`; plus the reference impl `MemDagStore` and the overlay impl `staged::StagedDag`; `rollback`: the `Rollback` taxonomy (imports only `dag`) | A |
| **C — Read model** (algorithms over any `DagRead`) | `cover::ReachIndex`, `register::resolve`, `canonical_index::{CanonicalIndex, CanonicalRow, canonical_live_values}`, `lens::{live, live_values}`, `query::{AddressIndex, subscribe, Revision}` | A, B |
| **D — Domain-shaping contracts** | `strategy::{AddressStrategy, RefinementFamily, PrefixNestedKey, StrategyId, ClassAddress, SortKey, WholeValue}` (imports only `dag::EntryHash` + `encoding::Digest`); `lens::Op` + codec; `graph::{GraphOp, GraphFacts, GraphVoid, graph_facts, graph_facts_live}`; `void::{VoidPolicy, Polarity, Verdict, verdict}` | A, B |
| **E — Engine + sync** | `reconciliation::{Index, Message, KeyRange, respond, opening, completion_plan, SessionHello}`, `sync_session::{SyncSession, SyncMessage, EntrySource, SessionBudget, …}`, `replica` (self-described "prototype host", replica.rs:1-8), `rng` (test PRNG) | A, B, **D-strategy only** (`SortKey`/`StrategyId` bytes; reconciliation.rs:3-6: "this module NEVER depends on a concrete strategy") |

Two structural facts the split must honor, both stated in-source:

1. **The seam the code commits to is fact-substrate | query-layer.**
   `graph.rs:213-214`: `GraphFacts` is "a neutral, Datalog-agnostic structure
   so `hhhs-core` need not know the query engine; the datalog crate maps
   these to `node`/`edge` relations." That crate is not hypothetical — the
   workspace already contains `hhhs-datalog` (stratified naive + semi-naive
   evaluation with negation, `GraphEdb`/`GraphVoidEdb`, an advisory
   incremental layer checked against the from-scratch oracle). walkie's fold
   (now `tutti-core`'s `OpLanguage::fold`) does the same job for music. A
   Datalog engine and a music fold are **both consumers above the same
   line** — the kernel grew its second reference consumer before anyone went
   looking for one.
2. **`void.rs` grounds retraction in well-founded semantics** (the module doc
   cites Chen & Warren, JACM 43(1) 1996 — Datalog-with-negation's canonical
   semantics — as the reason the verdict engine carries no memo). The
   graph/void/negation bands are exactly what a reactive Datalog graph DB
   stresses and what music barely touches; `hhhs-datalog`'s `GraphVoidEdb`
   already routes query-layer negation over kernel-layer (Remove) negation.

### 0.2 The empirical import table (the falsification data, already real)

What each real consumer actually imports from `hhhs_core` today, from grep
over all four trees:

| consumer | A | B | C | D | E |
|---|---|---|---|---|---|
| `tutti-core` (walkie's substrate; `crates/tutti-core/src/store.rs`) | `EntryHash`, `Digest`, `Header`, `Position`, `entry_hash` | `DagRead`, `Entry`, `AppendOutcome`, `MemDagStore` | `cover::ReachIndex`, `register` (test oracle) | — | **none** |
| walkie's sync driver (`src/net/sync.rs:40-47` → future `tutti-net`) | `EntryHash` | — | — | `SortKey`, `StrategyId` | `reconciliation::{Config, Index, SessionHello}`, `sync_session::{EntrySource, SessionBudget, SyncSession, …}` |
| `hhhs-datalog` (`src/edb.rs`) | `EntryHash`, `Digest`, `Encoder` | `DagRead`, `Position` (+`MemDagStore` in tests) | — | `graph::GraphFacts`, `graph` | **none** |
| `hhhs-reactive` | `EntryHash`, `Position`, `DagSnapshot` | `DagRead`, `Growth`, `GrowthEpoch`, `GrowthSubscription` | `lens`, `query::AddressIndex` | `strategy::{AddressStrategy, ClassAddress}` | **none** |
| `hhhs-strategy-{toyfacet,riffcat}` | `EntryHash`, `Digest`, `Encoder` | — | — | `strategy::*` | **none** |
| `potluck-hhhs` (`src/{lib,host,void_policy}.rs`) | `Digest`, `Header`, `Encoder` | `DagRead`, `DagSnapshot`, `AppendOutcome` + `test_utils::conformance` | — | `void::{VoidPolicy, verdict, Polarity, Verdict, VoidReason}` | **none** |

Read that table twice. Across six real consumers, **exactly one** — the
designated sync driver inside walkie's net band — touches band E, and it
touches *only* E plus the coordinate/strategy vocabulary. Every read-side
consumer (two apps' substrates, a Datalog engine, a reactive adapter, two
strategy implementations) lives entirely in A–D. Potluck does not use the
hhhs engine at all: its anti-entropy is its own inventory protocol over
signed source ops (`potluck-wire/src/reconcile.rs`). One discrepancy to
carry into co-review: `sync_session.rs:6-8` claims "walkie-songie and
potluck both drive it," but no `hhhs_core::sync_session` import exists in
potluck's tree today — either that is planned or the doc is aspirational,
and which one changes how hard the `hhhs-sync` surface must be frozen (§6).

So the proposed boundary is not a prediction; it is a measurement. The
design job is to draw crate lines where the imports already fall.

---

## 1. The recommended crate graph

```
                         ┌──────────────────────────────┐
                         │           hhhs-dag           │   the floor
                         │  A + B + strategy contract   │   (deps: blake3)
                         │  dag, encoding, strategy,    │
                         │  staged, rollback            │
                         └──────────┬───────────┬───────┘
                                    │           │
                 ┌──────────────────┴──┐   ┌────┴─────────────────┐
                 │        hhhs         │   │      hhhs-sync       │  siblings,
                 │  the facts front    │   │  the reconciliation  │  no edge
                 │  door: C + rest-of-D│   │  engine: E           │  between
                 │  cover, register,   │   │  reconciliation,     │  them
                 │  canonical_index,   │   │  sync_session        │
                 │  lens, graph, query,│   │  (feature: wire)     │
                 │  void               │   └────────┬─────────────┘
                 └──┬────────┬─────┬───┘            │
                    │        │     │                │
        ┌───────────┴──┐ ┌───┴─────┴────┐   ┌───────┴────────┐
        │ hhhs-datalog │ │ hhhs-reactive│   │  tutti-net /   │
        │ (graph DB)   │ │ (signals)    │   │  app sync      │
        └──────┬───────┘ └──────┬───────┘   │  drivers       │
               │                │           └───────┬────────┘
   ┌───────────┴───┐   ┌────────┴─────┐             │
   │ docs/knowledge│   │ tutti-core → │◄────────────┘
   │ apps (future) │   │ walkie;      │
   └───────────────┘   │ potluck-hhhs │
                       └──────────────┘

   hhhs-testkit (replica prototype host, rng, test_utils conformance
   suites, foreign_log) — dev-dependency of everything, depends on
   hhhs + hhhs-sync. hhhs-slice-tests stays the cross-crate
   integration harness. hhhs-strategy-* re-target to hhhs-dag.
   hhhs-core remains, as a deprecated re-export shim (§7).
```

### 1.1 `hhhs-dag` — the contract floor (A + B + the strategy contract)

**Contents:** `dag.rs` whole (coordinates, `entry_hash`, the `DagRead` /
`Growth` / `DagDelta` / `DagStore` traits, `AppendOutcome`, `DagSnapshot`,
`MemDagStore` as the reference implementation), `encoding.rs`,
`strategy.rs`, `staged.rs`, `rollback.rs`. Mandatory external deps: blake3.
Optional: serde (derives currently under `wire` cfg on `EntryHash`,
`SortKey`, `StrategyId`, `Digest` — renamed to a `serde` feature here).

**Why this is a boundary.** Membership test: *needed by both siblings, and
depends on nothing but coordinates.* Both `hhhs` (the read model) and
`hhhs-sync` (the engine) consume `DagRead` and the strategy vocabulary —
the engine's own invariant is that it sees "`SortKey` bytes only"
(reconciliation.rs:3-6), and `strategy.rs` imports exactly
`dag::EntryHash` + `encoding::Digest` and nothing else. Without a floor
crate, the engine would have to depend on the whole read model (lens,
graph, void, Datalog-adjacent code) to spell a sort key, and the ESP-32
leaf floor (§4) would have no crate to stand on. `staged`/`rollback` ride
along because they import only `dag` and are storage-discipline, not
read-model, code.

**Why it is not the types-crate antipattern.** A parasitic types crate is
shape without law: it exists to break a cycle, carries no behavior, and
accretes everyone's structs until every version bump is a world rebuild.
`hhhs-dag` is the opposite — it holds the system's *laws*: the one identity
function (`entry_hash`, dag.rs:110-113 — "the ONLY identity in the
system"), the admission semantics (`AppendOutcome`'s defer-never-reject
doctrine), the linearizability contract on `Growth` registration
(dag.rs:214-216), the compaction escape hatch (`DagDelta::appended_since →
None`, dag.rs:228-236), and the sort-key invariant (the 32-byte op-hash
suffix, strategy.rs:56-58). That is a genuine contract layer: small, law-
dense, nearly-never-changing, and the thing independent implementations
implement. The accretion risk is managed by the membership test above —
anything that is not "what an entry is / how a store is observed / how
payload bytes acquire order and addresses" is refused.

### 1.2 `hhhs` — the facts front door (C + the rest of D)

**Contents:** `cover.rs`, `register.rs`, `canonical_index.rs`, `lens.rs`,
`graph.rs`, `query.rs`, `void.rs`. Depends on `hhhs-dag` and **re-exports
it wholesale at the same paths** (`pub use hhhs_dag::dag;` etc., plus the
curated root re-export list `hhhs-core/src/lib.rs:29-51` carries today).
External deps: none beyond the floor. This is the crate whose name answers
"what do consumers stand on": *verifiable reactive facts with retraction,
plus a graph read-model* — `graph_facts_live` filtered through
well-founded-semantics verdicts, live lens values, causal covers,
LWW registers, canonical horizon-labelled result roots.

**Why this is a boundary.** Everything in it is a pure algorithm over
`&impl DagRead` that a *reader* needs and the *engine* never does — the E
column of §0.2's table is empty for every consumer of this band. It
changes when read semantics grow (Stage-2 horizon-honoring `graph_facts`,
a lazy reach, a value/seniority read when `VoidPolicy::value` un-HELDs);
none of those changes should force a re-pin on a peer's sync driver, and
no sync hardening pass should force a re-review of verdict semantics.
Note one deliberate asymmetry: `hhhs` does **not** re-export or depend on
`hhhs-sync`, not even behind a feature — see §2.3.

**Wart to fix at extraction time (additively):** two functions in this band
name the concrete store — `query::subscribe(Rc<MemDagStore>)` and the
`graph::emit`/`add_node`/… helpers (`&MemDagStore`). The `DagStore` trait
now exists precisely so downstreams need not name `MemDagStore`
(dag.rs:252-258); generalizing these parameters to `&impl DagStore` is
call-site-compatible for every current caller and should land with the
move. `query::subscribe` is in practice superseded by `hhhs-reactive`'s
generic adapters and is a candidate for demotion to the testkit instead.

### 1.3 `hhhs-sync` — the engine (E)

**Contents:** `reconciliation.rs`, `sync_session.rs`. Depends on
`hhhs-dag` **only** — verified: the two modules import `dag`, `encoding`,
and `strategy` symbols exclusively. Re-exports the floor types that appear
in its own API (`EntryHash`, `Entry`, `SortKey`, `StrategyId`) so a sync
driver can depend on `hhhs-sync` alone. Feature `wire` = serde + postcard
+ `hhhs-dag/serde`, exactly today's semantics ("the core state machine is
feature-independent; only encode/decode lives here" —
hhhs-core/Cargo.toml:18-20).

**Why this is a boundary.** This is the single highest-leverage line in the
design, and it is justified three ways: (i) empirically — five of six real
consumers never import it (§0.2); (ii) by change cadence — this band
absorbed ~9 dual-review hardening passes while the read model sat still,
and its wire rules (append-only postcard variants, ALPN generation bumps,
sync_session.rs:90-96) have a different evolution discipline than any API
in A–D; (iii) by the co-review surface — a potluck re-pin should never
need to reason about RBSR budget changes it does not consume. The engine
being a *sibling* of `hhhs` rather than a layer above or below it is what
the strategy-contract placement in the floor buys.

`replica.rs` does **not** come along: it is the one module that bridges
both siblings (it imports `lens`, `query`, `strategy`, *and*
`reconciliation`), and its own doc says it is a prototype demonstrating
composition, "not the storage, transport, or adapter abstraction
downstream applications should implement" (replica.rs:1-8). It moves to
the testkit with `rng`, where its purpose is honest.

### 1.4 `hhhs-testkit` — conformance and prototype hosts

**Contents:** today's `test_utils` (the `dag_read_conformance` /
`dag_read_growth_conformance` suites potluck runs against its own host,
`foreign_log`), `rng`, and `replica` (with `reconcile` /
`reconcile_to_fixpoint` / `Stats`). Depends on `hhhs` + `hhhs-sync`.
Conformance kits ship beside the contracts they certify, but as a separate
crate rather than a feature, because `test_utils = []` as an in-crate
feature means the production crate carries test scaffolding code paths and
because a dev-dependency cannot leak into a production graph the way an
accidentally-unified feature can. During migration the existing
`hhhs-core/test_utils` feature keeps forwarding (§7).

### 1.5 Unchanged satellites

`hhhs-datalog` and `hhhs-reactive` re-target their single dependency from
`hhhs-core` to `hhhs` (pure rename — every symbol they use is in the front
door). `hhhs-strategy-toyfacet`/`-riffcat` re-target to `hhhs-dag` (their
imports are A + strategy only; their tests take a dev-dep on `hhhs` or the
testkit for `MemDagStore`), demonstrating the floor's value: a strategy
author never pulls the read model. `hhhs-slice-tests` keeps depending on
everything; it becomes the home of the falsification check (§8).

**What is deliberately NOT split** (the over-splitting refusals): no
`hhhs-void`, `hhhs-graph`, `hhhs-lens`, or `hhhs-cover` crates — each
would have one or two consumers, the modules are 100-650 lines, and every
extra crate in a git-pinned two-app workflow is another version to
coordinate for zero independent change axis. No separate `hhhs-strategy`
contract crate either: 128 lines with two dependents that both already
need the floor. The bands are documentation structure; crates are
coordination structure. Three production crates + a testkit is the floor
of what expresses the real seams, and also the ceiling of what today's
consumer count justifies.

---

## 2. Naming: what consumers stand on, not what the floor is made of

Three naming calls, each with the rejected alternatives and why.

### 2.1 The front door: `hhhs` (bare facade), not `hhhs-facts`, not `hhhs-core`

**Pick: `hhhs`.** The bare-name facade is the strongest pattern open-source
Rust has for a family's front door (`serde`, `tokio`, `http`): it is what a
new consumer finds first, its docs.rs page is the ecosystem's landing page,
and it grants the maintainer re-export freedom — internals can reshuffle
beneath it without any consumer noticing. Crucially, the facade here is not
an empty shell over sub-crates: `hhhs` *is* the facts layer (C + D live in
it as source), re-exporting only the floor. A facade-over-one-crate
(`hhhs` → `hhhs-facts`) was considered and rejected as pure over-split: it
adds a hop with no independent change axis.

- **Rejected: `hhhs-facts` as the front-door name.** It is the most honest
  descriptive candidate — "facts" is literally the in-code vocabulary
  (`GraphFacts`, `graph_facts_live`) and exactly the Datalog consumer's EDB
  view of the world. But hyphenated names read as *members of* a family,
  not as *the* family, and the bare `hhhs` name would then sit unused or,
  worse, get claimed later by a kitchen-sink facade. The "facts" vocabulary
  is preserved where it does the work: `hhhs::graph::GraphFacts`,
  `hhhs::lens::live_values`.
- **Rejected: keeping `hhhs-core` as the permanent front door.** "-core" is
  floor-naming — it describes the crate's position in a stack, not what a
  consumer gets from it, and after the split it would be false twice over
  (the actual core/floor is `hhhs-dag`, and the engine is no longer
  inside). It survives as a compatibility shim (§7), which is the only job
  the name is still fit for.
- **Rejected: `hhhs-model` / `hhhs-read`.** "Model" is the vaguest word in
  software; "read" describes a *direction*, and the front door also carries
  the write-discipline surface (`StagedDag`, `Rollback`, the emit helpers)
  via re-export.

### 2.2 The floor: `hhhs-dag`, not `hhhs-types`

**Pick: `hhhs-dag`.** It names the thing the crate defines: the causal
op-DAG — what an entry *is* (`Entry`/`Header`/`entry_hash`), what a
position in one is (`Position`), and how one is observed and grown
(`DagRead`/`Growth`/`DagStore`). A consumer that depends on it directly
(a storage backend, a strategy author, the engine, one day a leaf device)
is precisely a consumer implementing or addressing the DAG contract.

- **Rejected: `hhhs-types` (and `hhhs-primitives`).** This is the
  antipattern name. A "types" crate advertises itself as a dumping ground —
  the name imposes no membership test, so every cyclically-awkward struct
  in the workspace eventually migrates into it, and because everything
  depends on it, every change to it is a world rebuild and a world re-pin.
  §1.1 argues the floor is a *contract* crate (laws, not shapes); the name
  should say what the contract is over. The membership test "does this
  define what a DAG entry is or how a store is observed?" is enforceable in
  review precisely because `dag` is in the name.
- **Rejected: `hhhs-kernel`.** The whole finding of §0 is that "the kernel"
  is five bands, three of which live in the front door. Calling the floor
  "kernel" would imply `hhhs` and `hhhs-sync` are peripheral; they are the
  product.
- **Rejected: folding the floor into `hhhs` (two-crate split).** Viable —
  and if the co-review (§6) balks at three crates, it is the fallback. The
  cost: `hhhs-sync` would depend on the full read model to spell `SortKey`,
  dragging lens/graph/void/query into every sync driver's build and
  re-review scope, and the future leaf/no_std floor would have no home. The
  floor crate costs one Cargo.toml and buys the sibling topology; it is the
  cheapest crate in the design.

### 2.3 The engine: `hhhs-sync`, not `hhhs-engine`, not `hhhs-rbsr`

**Pick: `hhhs-sync`.** A consumer's reason to depend on it is "synchronize
my store with a peer over any byte stream" — `SyncSession` + `EntrySource`
+ budgets + the wire codec. Name the promise, not the mechanism.

- **Rejected: `hhhs-engine`.** Says nothing ("engine of what?"), and the
  codebase already uses "engine" for the *void verdict engine*
  (void.rs:1 — "the cache-free verdict engine") — the collision would be
  actively misleading about which side of the boundary a reader is on.
- **Rejected: `hhhs-rbsr` / `hhhs-reconcile`.** Algorithm names. RBSR is
  how the crate currently does its job; the crate's contract (sessions,
  budgets, causal completion, the driver rules in sync_session.rs:36-72)
  would survive a second protocol. `reconciliation` stays as the *module*
  name inside, where mechanism-naming is correct.

**And one anti-feature, stated as a naming rule:** `hhhs` never re-exports
`hhhs-sync`, not even behind an off-by-default `sync` feature. Cargo
feature unification means one crate anywhere in an app's graph enabling
`hhhs/sync` would silently grant engine access to every `hhhs` consumer in
that build — exactly the reach-past the §8 falsification check exists to
catch, made invisible to grep. Engine access must be a deliberate,
separate dependency edge. This is the rare case where one-dep ergonomic
convenience loses to boundary enforcement, and it is also why there is no
`hhhs::prelude`: the curated root re-export list (inherited from
hhhs-core/src/lib.rs:29-51) already serves discoverability without hiding
provenance.

---

## 3. The `DagRead` seam

`DagRead` (dag.rs:117-136) lands in `hhhs-dag`, and it is worth being
precise about why it is *the* boundary object rather than one trait among
several: every band above the floor is generic over it, and every storage
answer below the floor is an implementation of it.

**Below the seam — implementations, present and planned:**

| impl | where | what it proves about the trait |
|---|---|---|
| `MemDagStore` | hhhs-dag | full in-memory history, linearizable growth |
| `DagSnapshot` | hhhs-dag | immutable horizon capture; `snapshot()` is a default method so static readers get it free (dag.rs:128-135) |
| `StagedDag` | hhhs-dag | overlay: base ∪ unpublished extension, O(staged) reads (staged.rs:1-16) |
| tutti-core's lifted store | walkie | a DAG assembled from foreign signed-op logs; kernel never sees the envelope |
| potluck-hhhs host | potluck | forwarding host, certified by `dag_read_conformance` |
| a SQL/durable store | anticipated by the `DagStore` doc itself (dag.rs:254-257: "an in-memory host, a staged overlay, a SQL-backed store") | persistence without kernel change |
| **windowed / leaf store** | future (ESP-32 floor) | bounded suffix + compacted view; answers `DagDelta::appended_since` with `None` past its window — the "I don't know, recompute" escape hatch is *already designed in* (dag.rs:230-235) |

**Above the seam — every consumer algorithm:** `ReachIndex::new(&impl
DagRead)`, `lens::live_values(&impl DagRead)`, `graph_facts(&impl DagRead,
at)`, `void::verdict(policy, atom, at, &impl DagRead)`,
`VoidPolicy::deps(…, &impl DagRead)`, hhhs-datalog's `EdbSource` backends,
hhhs-reactive's views ("generic over `DagRead + Growth`", STATUS.md), and
even the engine's causal completion (`completion_plan(&impl DagRead, …)`).
The engine and the read model share *only* the floor — the seam is where
the sibling topology becomes possible.

**How the three open threads converge on this one seam:**

- **The Θ(N²) reach thread.** `ReachIndex` materializes every entry's full
  strict-ancestor `BTreeSet` — quadratic space/time in the worst case.
  walkie has already built the escape: `tutti-core` carries a cheap lazy
  `Reach` with a `CausalPast` backend, equivalence-tested against the
  kernel `ReachIndex` as the Θ(N²) oracle (tutti-core/Cargo.toml,
  `test-support` feature comment). That is the same oracle-vs-accelerator
  discipline as hhhs-datalog's advisory layer. The fix is therefore not a
  rewrite of `cover.rs` but a second reachability *impl behind the same
  query shape* — and once a second consumer needs it (the Datalog engine
  will, for `GraphVoid`'s `removers_of` scans at scale), the `Reach`
  seam gets promoted from tutti-core into `hhhs` beside `ReachIndex`.
  Until then it stays app-side: promoting it at n=1 is the exact over-fit
  this doc keeps warning about.
- **The embedded-leaf thread.** A leaf is a `DagRead` impl with a window,
  not a fork of the kernel. What it needs from the crate graph is (i) a
  floor crate that can eventually drop `std` (§4) and (ii) the
  `DagDelta::appended_since → None` contract so consumers above degrade to
  full recompute — or, on a leaf, to "the window is the world." Nothing
  about the leaf requires touching `hhhs` or `hhhs-sync` crate boundaries.
- **The perf/streaming thread.** `entries_topo()` returns `Vec<Entry>` —
  every read clones full history, which windowed and large stores cannot
  afford. The evolution rule that keeps the trait semver-stable: extend by
  **default methods only** (the pattern `snapshot()` already set) — e.g. a
  streaming `for_each_topo(&self, f: impl FnMut(&Entry))` default-
  implemented over `entries_topo`, which big stores override and existing
  impls inherit. Adding a defaulted method is a minor bump; adding a
  required one is a major bump that forks every impl in three trees. This
  rule should be written into `hhhs-dag`'s crate docs as policy.

The reason `DagRead` must live in the *smallest, most stable* crate is now
mechanical: it has the most implementors (six today, more planned) and the
most generic consumers of any item in the system, so its home's version
churn multiplies across the entire ecosystem. The floor exists to give it
a home that has no other reason to change.

---

## 4. Feature and dependency strategy

### 4.1 The dependency floor, defended as a feature

The entire mandatory third-party surface of the three production crates is
**blake3**. That is worth stating as an invariant, not an accident:

- `hhhs-dag`: blake3 (identity *is* blake3 — making it optional would make
  `entry_hash` conditionally-existent, a non-additive absurdity; blake3 is
  itself no_std-capable when the time comes). Optional: serde.
- `hhhs`: nothing beyond `hhhs-dag`. All of C+D is pure std code.
- `hhhs-sync`: nothing beyond `hhhs-dag`. Optional: serde + postcard
  (`wire`).
- Heavy/ecosystem deps stay in the satellites where they already are:
  futures/futures-signals in `hhhs-reactive`, datafrog as a *dev-only*
  oracle in `hhhs-datalog`, `radix_immutable` in tutti-core (feature
  `merkle`, off by default) — none of them ever enter the kernel crates.

### 4.2 Feature rules (all additive, by construction)

1. **`hhhs-dag/serde`** — replaces the serde half of today's `wire` cfg on
   `EntryHash`/`SortKey`/`StrategyId`/`Digest`. Pure derive addition;
   additive under unification by definition.
2. **`hhhs-sync/wire`** = `dep:serde`, `dep:postcard`, `hhhs-dag/serde` —
   exactly today's contract: the state machine is feature-independent,
   only the codec is gated, "potluck's no-serde build is unaffected"
   (hhhs-core/Cargo.toml:9-10 keeps being true).
3. **`hhhs-dag/std`, default-on — but only when it stops being a lie.** The
   honest current state: nothing in the crate compiles without std
   (`MemDagStore` needs `Mutex` + `catch_unwind`; `DagSnapshot` uses
   `HashMap`; `Growth` hands out `Arc<dyn Fn + Send + Sync>`). Do **not**
   ship a `std` feature preemptively — a feature that cannot actually be
   disabled is worse than none, because downstreams will write
   `default-features = false` against it and break at a distance later.
   Add it in the same change that makes the alloc-only subset (coordinates
   + `entry_hash` + the traits + `SortKey`) genuinely compile, with a
   `--no-default-features` check in CI beside the existing
   `scripts/check-wasm.sh` gate. The embedded floor is currently **n=0**
   (no real leaf consumer exists); the crate graph's only obligation today
   is not to preclude it, which the floor crate satisfies.
4. **Never a `no-std` feature, never behavior-changing features.** Features
   subtract nothing and change no observable semantics — a future
   `windowed-store` helper may *add* types, never alter `MemDagStore`.
   This is what keeps Cargo's feature unification harmless: any consumer
   turning a kernel feature on must be unobservable to every other
   consumer in the build.
5. **wasm-safety is a property of the default build**, not a feature:
   `hhhs-sync` deliberately carries no `Send`/`Sync` bounds so it runs on
   wasm's single thread (sync_session.rs:4-8), and the kernel has no
   getrandom/clock/thread dependency anywhere (`rng` is a seeded test
   xorshift, and it leaves for the testkit anyway). The existing
   `check-wasm.sh` gate extends to all three crates.

### 4.3 Unification and pinning hazards to clean up

- **The exact blake3 pin.** tutti-core pins `blake3 = "=1.8.5"` while the
  hhhs workspace wants `blake3 = "1"`. Exact cross-crate pins on a shared
  dep are a resolver time bomb: the day `hhhs-dag` needs `>=1.9`, the two
  requirements are unsatisfiable and walkie's build breaks in the
  resolver, not in code. Either relax tutti's pin to caret, or better,
  have downstreams hash through the re-exported `hhhs_dag::Digest::of` so
  most of them never name blake3 at all (tutti still needs it directly for
  its own Merkle roots; it alone carries the caret requirement).
- **Re-export hygiene as the public-dependency rule.** Every crate
  re-exports precisely the foreign types that appear in its own public
  API: tutti-core already models this (`pub use hhhs_core::EntryHash` —
  "so a downstream domain names it through tutti_core and never takes a
  direct, rev-pinned hhhs-core dependency purely to spell it",
  tutti-core/src/lib.rs:44-48). Applied kernel-side: `hhhs` re-exports the
  floor wholesale; `hhhs-sync` re-exports `EntryHash`/`Entry`/`SortKey`/
  `StrategyId`/`SessionHello`-adjacent floor types. The test: a consumer
  should be able to write its integration against exactly one kernel crate
  name per band it participates in.

---

## 5. Reference consumers: who imports what, and what n justifies

### 5.1 walkie (music), via tutti-core

- `tutti-core` → **`hhhs` only** (through re-exports it names: `EntryHash`,
  `Digest`, `Header`, `Position`, `entry_hash`, `DagRead`, `Entry`,
  `AppendOutcome`, `MemDagStore`, `cover::ReachIndex`, `register`). Its
  fold seam (`OpLanguage::fold` over `FoldCtx`) is the music-domain
  counterpart of hhhs-datalog's rule evaluation: both are pure functions
  from a `DagRead`-shaped fact substrate to a materialized view.
- `tutti-net` (once extracted; today `src/net/sync.rs`) → **`hhhs-sync`**
  (`SyncSession`, `EntrySource`, `SessionBudget`, `Config`, `Index`,
  `SessionHello`) plus the floor vocabulary (`EntryHash`, `SortKey`,
  `StrategyId`) via `hhhs-sync`'s re-exports.
- walkie the app never names a kernel crate directly; it stands on tutti.
- Planned consumption that stays front-door: `StagedDag` + `rollback_for`
  for the draft/intent lifecycle (tutti architecture §3.3) — both land in
  `hhhs-dag`, reachable through `hhhs`.

### 5.2 The Datalog graph DB (docs/knowledge apps)

`hhhs-datalog` → **`hhhs` only** (`DagRead`, `Position`, `EntryHash`,
`Digest`, `Encoder`, `graph::GraphFacts`, `graph`). It exercises the bands
music barely touches: `GraphOp`/`GraphFacts` extraction, `GraphVoidEdb`
routing kernel Remove-negation under query-layer stratified negation, and
recompute-on-growth reactivity with an advisory incremental layer. This is
the maximally-different reference consumer §0.1 describes — and note the
direction of the dependency arrows: the kernel's `graph.rs` was written to
be Datalog-agnostic *so that* this crate could exist without the kernel
knowing it. The boundary predates this document; the crate split just
stops pretending otherwise.

### 5.3 potluck (the real n=2)

`potluck-hhhs` → **`hhhs` only** (`DagRead`, `DagSnapshot`,
`AppendOutcome`, `Digest`, `Header`, `Encoder`, and — uniquely among
consumers — the full `void` surface: `VoidPolicy`, `verdict`, `Polarity`,
`Verdict`, `VoidReason`, which it `pub use`s into *its own* public API),
plus `hhhs-reactive`, plus the conformance suites from what becomes
`hhhs-testkit`. Potluck consumes **no** engine band at all; its
anti-entropy is its own inventory protocol (`potluck-wire/reconcile.rs`).
Potluck is therefore living proof that the front door must be consumable
without the engine — an `hhhs-core` that hard-wired reconciliation deps
into every consumer would be taxing potluck for machinery it deliberately
does not use.

### 5.4 The n-count audit: which boundaries are earned

| proposed boundary | justified by | verdict |
|---|---|---|
| `hhhs` \| `hhhs-sync` (facts \| engine) | ≥5 consumers on the facts side that never touch E; 1 designated driver that touches only E + vocabulary (§0.2) | **earned — this is a measurement** |
| `hhhs-dag` floor beneath both | the engine's own "SortKey bytes only" invariant + strategy crates importing A+D₁ only; conformance-suite consumers implementing B | earned, but by dependency hygiene rather than by consumer count — the fallback (fold floor into `hhhs`, §2.2) exists if co-review prefers two crates |
| testkit as a crate | potluck + in-repo tests both consume conformance; replica's prototype status is self-declared | earned |
| no_std/alloc floor *shipping now* | **n=0** — no real leaf exists | **not earned; explicitly deferred** (§4.2 rule 3) |
| promoting tutti's lazy `Reach` into `hhhs` | n=1 (walkie) | **not earned yet**; promote when the Datalog consumer needs it (§3) |
| any finer split (void/graph/lens crates) | n=1 each | refused (§1.5) |

The n=1 over-fit risk the tutti doc names for itself applies here with the
sign flipped: hhhs's risk is not generalizing from music — it is
generalizing from *walkie's consumption pattern*. The reason this design
holds anyway is that its load-bearing boundary is confirmed by three
independent non-music consumers (datalog, potluck, reactive), two of which
were written by/with the other project. Where only walkie's shape argues
for something (lazy reach, leaf floor), the design defers it.

---

## 6. Versioning, publishing, and the potluck co-review

### 6.1 Versioning while git-pinned (now)

Today the release unit is the rev: both apps pin `bd23d4e`, and "release"
means push → re-pin → co-review. Keep **lockstep workspace versioning**
(`workspace.package.version`, as now) across all kernel crates until
publishing actually happens — per-crate independent versions add semver
ceremony with zero benefit while the rev is the coordination token. What
the split changes immediately is not versioning but *blast radius*: a
sync-hardening rev that touches only `hhhs-sync` is reviewable by potluck
as "no facts-band diff — rubber-stamp," which the current monolith cannot
offer structurally.

### 6.2 Publishing (later), in the right order

When crates.io happens (tutti doc §6.5 defers it; nothing here forces it):

- **Publish bottom-up:** `hhhs-dag` → `hhhs` → `hhhs-sync` → satellites.
- **`hhhs-dag` plays the `http`/`bytes` role**: the shared-vocabulary crate
  whose types appear in everyone's public APIs. That makes it a *public
  dependency* in the semver sense — a major bump of `hhhs-dag` forces a
  coordinated major of every crate that re-exports its types, i.e. the
  whole ecosystem. Consequence: `hhhs-dag` gets the strictest change
  policy (default-method-only trait evolution, §3) and should reach 1.0
  *first*, precisely so the crates above can iterate 0.x freely against a
  stable vocabulary.
- Pre-1.0 semver reminder for downstreams: cargo treats 0.x→0.(x+1) as
  breaking; the lockstep train makes that legible.
- **Publishing blockers to not trip over:** `hhhs-strategy-riffcat` and
  `hhhs-slice-tests` carry path deps on `riff-catalog-core` outside the
  repo — they are unpublishable until that resolves and simply stay out of
  the publish set. The `hhhs-core` shim (§7) is published once with a
  deprecation notice, or never.

### 6.3 Staging a breaking change across two pinned apps

The recipe, generalizing what the tutti extraction already practices
(expand → migrate → contract), and matching the wire layer's own
append-only doctrine:

1. Land the **additive** form in hhhs-rs (new API beside old; old marked
   `#[deprecated]`). Both apps still compile at the new rev.
2. Re-pin walkie; migrate it; run its gates (golden entry-hash vector, L0
   convergence).
3. Co-review against potluck's shapes; re-pin potluck; migrate it; its
   conformance suites are the compatibility instrument.
4. Remove the deprecated form in a later rev. No rev ever strands either
   app.

Wire compatibility is a separate axis with its own rules already written
down: postcard variants append-only, generation bumps travel as app ALPN
changes coordinated between walkie and potluck (sync_session.rs:90-96) —
the crate split does not touch this and must not.

### 6.4 What must be co-reviewed with potluck before any surface freezes

1. **The `hhhs` front-door export list** (the §8 mock). Potluck `pub use`s
   kernel `void` types in its own public API — the front door's root
   re-exports are potluck's API surface too. Freezing `hhhs`'s lib.rs
   without their sign-off would freeze part of potluck by proxy.
2. **The `DagRead`-family trait set + conformance relocation.** Potluck
   implements the B-band traits on its own host and certifies with
   `test_utils::conformance` under the `test_utils` feature — both the
   trait-evolution policy (default-methods-only) and the testkit move
   (feature-forwarding vs. dev-dep migration) change their CI. Decide the
   forwarding window together.
3. **Whether potluck will drive `hhhs-sync` at all.** sync_session.rs:6-8
   claims both apps drive it; potluck's tree today shows no such import
   (§0.2). If potluck intends to adopt `SyncSession`, its `EntrySource`
   shapes (source-op records vs. walkie's framed `SignedOp`) belong in the
   surface review *now*; if not, walkie's driver is the engine's only
   consumer, the doc claim should be corrected, and `hhhs-sync`'s surface
   can stay softer for longer. Either answer is fine; not knowing which is
   the risk.

---

## 7. Migration path: additive, extract-in-place, three trees green at every step

The same pattern walkie just ran three times on tutti-core (genericize in
place → re-export → extract), applied to the kernel. Every step leaves
hhhs-rs' workspace suite, walkie, and potluck building; steps marked
[coord] need potluck in the loop, the rest are notify-only rev bumps.

1. **Land the falsification check + surface doc (no code moves).** Add the
   §8 front-door mock and the import-audit grep to `hhhs-slice-tests`; it
   passes against today's monolith (§0.2 says so) and becomes the standing
   guard for every later step. Zero risk, immediately useful.
2. **Extract `hhhs-dag`.** Move `dag.rs`, `encoding.rs`, `strategy.rs`,
   `staged.rs`, `rollback.rs` into the new crate; `hhhs-core` depends on it
   and re-exports at identical paths (`pub use hhhs_dag::dag;` …), keeping
   every `hhhs_core::dag::EntryHash`-style path valid. Split the serde half
   of `wire` into `hhhs-dag/serde`; `hhhs-core`'s `wire` forwards. No
   downstream diff beyond a rev bump.
3. **Extract `hhhs-sync`.** Move `reconciliation.rs`, `sync_session.rs`;
   `hhhs-core` re-exports; `wire` forwards to `hhhs-sync/wire`. The one
   in-repo consumer to re-wire is `replica` (dev path). [coord — item
   6.4-3: this is the moment to settle whether potluck is an engine
   consumer, because this crate's surface hardens here.]
4. **Extract `hhhs-testkit`** (`test_utils` + `foreign_log` + `rng` +
   `replica`). `hhhs-core` keeps `test_utils = []` forwarding to testkit
   re-exports for as long as potluck wants. [coord — item 6.4-2.]
5. **Introduce `hhhs` and hollow the shim.** Move C+D sources (`cover`,
   `register`, `canonical_index`, `lens`, `graph`, `query`, `void`) into
   `hhhs`; `hhhs-core` becomes a pure re-export shim over `hhhs` +
   `hhhs-sync` (+ feature forwards), documented as deprecated-for-new-use.
   Ship the additive wart-fixes with the move: `graph::emit`-family and
   `query::subscribe` generalized over `&impl DagStore` (call-site
   compatible), or `subscribe` demoted to the testkit. [coord — item
   6.4-1: the front-door export list freezes here; run the §8 check
   against the real lib.rs before merging.]
6. **Apps migrate at leisure.** tutti-core → `hhhs`; walkie's net band
   (→ tutti-net) → `hhhs-sync`; potluck-hhhs → `hhhs`;
   `hhhs-datalog`/`hhhs-reactive` → `hhhs`; strategy crates → `hhhs-dag`.
   The shim keeps every laggard compiling; there is no flag day.

Sequencing notes: steps 2-4 are pure mechanical moves with re-export nets
and can land in one rev each; step 5 is the only one with a review gate in
front of it, which is exactly where the gate belongs. Nothing here touches
reconciliation/sync logic, wire bytes, verdict semantics, or any test
expectation — the L0/golden-vector gates on the walkie side and the
conformance/adversarial suites kernel-side are the proof each step is an
extraction and not a change.

---

## 8. The falsification test: the two-consumer front door

Before step 5 freezes anything, run this concretely — it is cheap and it
has teeth.

**(a) Mock the front door.** Write the proposed `hhhs/src/lib.rs` as a doc
or a `#[cfg(any())]`-guarded file: the C+D modules plus the floor
re-export, and *nothing else*. This is the complete answer to "what is
hhhs?" — if writing it requires reaching for a reconciliation symbol, stop.

**(b) List both consumers' imports against it, side by side.**

| symbol needed | walkie (tutti-core) | datalog (hhhs-datalog) | in mock front door? |
|---|---|---|---|
| `EntryHash`, `Digest`, `Position`, `Header`, `entry_hash`, `Encoder` | yes | yes | yes (floor re-export) |
| `DagRead` (+`Entry`, `AppendOutcome`, `MemDagStore`) | yes | yes | yes (floor re-export) |
| `cover::ReachIndex`, `register::resolve` | yes | not yet (future: scaled `GraphVoid`) | yes |
| `graph::{GraphFacts, GraphOp, graph_facts_live}` | no (music has its own alphabet) | yes | yes |
| `void::{VoidPolicy, verdict, …}` | not yet (potluck: yes, today) | via `GraphVoidEdb` | yes |
| `strategy::{AddressStrategy, SortKey, …}` | no (walkie's order comes from its envelope) | no | yes (floor re-export; hhhs-reactive needs it) |
| `reconciliation::*`, `sync_session::*`, `replica::*` | **no** | **no** | **no** |

**(c) The pass/fail rule.** If either front-door consumer needs to reach
*past* the front door into `reconciliation`/`sync_session`/`replica`, the
boundary is drawn wrong — either the symbol is misfiled (move it down/over)
or the consumer is doing engine work in the wrong band (fix the consumer,
which is what the table would be telling you). The check ran against
today's tree during this design: **it passes** — the only engine imports
anywhere are `src/net/sync.rs:40-47` (the designated driver; after
extraction, tutti-net depending on `hhhs-sync`, which is the sanctioned
edge, not a reach-past) and that same file's test module
(`src/net/sync.rs:808-809`), which becomes an ordinary dev-dependency.
Notably `tutti-core` — the crate nearest the temptation — imports zero
engine symbols even in tests; its oracle imports are `cover::ReachIndex` +
`register` (store.rs:1443-1444), both front-door.

**(d) Keep it running.** Encode (c) as a CI grep in `hhhs-slice-tests`
(step 1 of §7): outside an allowlist of designated sync drivers, no crate
in either app tree may match `hhhs_(core|sync)::(reconciliation|
sync_session|replica)`. The §2.3 no-facade-re-export rule is what keeps
this grep sound — there is no feature flag that grants engine access
invisibly.

---

## 9. Honesty: tradeoffs, over-splitting, and where this extrapolates

- **The split's real cost is coordination arithmetic.** Three production
  crates + testkit means four `Cargo.toml`s and a lockstep version train
  where there was one crate. The mitigations are structural (lockstep
  versions, path-stable re-exports, the shim), but the cost is not zero:
  every future kernel contributor must learn which band a change belongs
  to. The §1.1/§2.2 membership tests exist to make that a lookup, not a
  debate.
- **The floor crate is the least-earned boundary** (§5.4) — it is justified
  by dependency hygiene and one invariant comment in reconciliation.rs,
  not by a consumer that depends on the floor *alone* in production
  (strategy crates come closest). If the co-review finds three crates too
  many, folding `hhhs-dag` into `hhhs` loses the sibling topology and the
  future leaf home but breaks nothing else in this design; the
  facts|engine line is the one worth defending to the mat.
- **The engine's second consumer is unverified** (§6.4-3). If potluck never
  drives `SyncSession`, then `hhhs-sync`'s surface is n=1 (walkie's
  driver) and should be held looser — hardened internals, soft API — until
  tutti-net freezes it from the consumer side.
- **The leaf/no_std floor and the lazy-reach promotion are deliberately
  deferred** at n=0 and n=1 respectively. The crate graph's obligation is
  only to leave them landing zones (`hhhs-dag`'s future `std` feature; a
  `Reach` seam beside `ReachIndex`), and it does.
- **What this design does not do:** it does not touch verdict semantics,
  reconciliation behavior, wire bytes, the no-cache doctrine, or any
  hardened invariant; it does not propose publishing; it does not rename
  any module or type — only crates, and even then behind a shim. The
  monolith's contents were found to be well-factored *internally*; this
  design's claim is narrower than "restructure the kernel" — it is "the
  crate lines should fall where the module lines and the consumer imports
  already agree they are."

---

## Appendix: end-state dependency edges, exhaustively

```
hhhs-dag        → blake3 (+serde optional)
hhhs            → hhhs-dag (re-exported wholesale)
hhhs-sync       → hhhs-dag (+serde, postcard under `wire`)
hhhs-testkit    → hhhs, hhhs-sync
hhhs-reactive   → hhhs, futures, futures-signals
hhhs-datalog    → hhhs (+datafrog dev-only)
hhhs-strategy-* → hhhs-dag (+dev: hhhs-testkit)
hhhs-slice-tests→ all of the above (integration + falsification harness)
hhhs-core       → hhhs, hhhs-sync (deprecated shim; feature forwards:
                  wire → hhhs-sync/wire + hhhs-dag/serde,
                  test_utils → hhhs-testkit re-exports)

tutti-core      → hhhs
tutti-net       → hhhs-sync (floor vocabulary via its re-exports)
walkie-songie   → tutti-* (never a kernel crate directly)
potluck-hhhs    → hhhs, hhhs-reactive (+dev: hhhs-testkit)
```


