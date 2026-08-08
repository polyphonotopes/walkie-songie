# The M3 windowed store: a bounded-window `DagRead` with a compacted checkpoint

**Status:** design, 2026-08-08. No code changed. This is the specification for the
one unbuilt load-bearing piece the reorg spec names (`docs/vision/
hhhs-reorg-spec-and-migration.md` §A.6.3, deferred at n=0 in §C.4) and the AMY
leaf ranks as its number-one risk (`docs/research/tutti-amy-esp32-leaf.md` §7.2:
"M3 is the long pole"). Companions: the reorg spec (the `DagRead` seam this store
is an implementation of), `docs/research/tutti-amy-esp32-leaf.md` §2/§5.3 (the
concrete RAM envelope), `docs/research/performance-benchmark-suite.md` §2/§3/§7.2
(the ≤ 64 KB @ W≤256 gate and the Θ(N²) measurements that make windowing
mandatory), and `docs/research/zk-provable-dag-snapshots.md` (the checkpoint-
adoption trust question, §4.4 here).

**Grounding.** Every type named below was re-read from source for this design:

- `crates/tutti-core/src/store.rs` — `Store<L>`, `FoldCtx<L>` (store.rs:639-689),
  `CausalPast` (503-529), the lazy `Reach` (550-610), `view()` (439-441), the
  lift/strict-deferral path (`ingest_verified`/`resolve_prevs`/`drain_pending`,
  287-379), `observed_frontier` (383-390), `sync_root` (235-237), `ops_root`/
  `prove_op` (481-491).
- `crates/tutti-core/src/ops.rs` — the signed envelope (`VersionedOpG.observed`,
  181-183; `MAX_OBSERVED_OPS` = 4096, :46; "NOT log continuity" verification
  honesty, 626-630) and `crates/tutti-core/src/merkle.rs`.
- hhhs-core @ `bd23d4e`: `dag.rs` (`DagRead` 117-136, `DagDelta::appended_since →
  None` 228-235, `DagStore` 261-269, `AppendOutcome` 349-360, `MemDagStore`,
  present-only `topo_of` 627-669), `cover.rs` (`ReachIndex` Θ(N²) closure 59-88,
  present-only doctrine 8-17), `register.rs` (`resolve` 53-64), `void.rs`
  (well-founded negation, deny-on-cycle, the no-verdict-cache doctrine 21-32),
  `canonical_index.rs`.
- The real domain fold: `src/room/store.rs` (`walkie_fold` 40-52, `with_pitches`
  57-95, `with_pieces` 117-239 incl. `locked_as_of` 161-176 and the
  `UnremovePiece` override 188-194, `with_registers` 243-294) and
  `src/room/ops.rs` (`WalkieOp`, 57-89).
- The adversarial test style to be matched: `crates/tutti-core/tests/
  second_domain.rs` (seeded SplitMix64 permutation convergence, `view()` ≡
  `view_reference()` oracle) and the `smoke` module (store.rs:691-767).

**The problem in one paragraph.** Today every peer holds the full signed-op DAG
and folds all of it: `Store<L>` wraps a `MemDagStore` of full history, `view()`
rebuilds a `FoldCtx` over the complete decoded map, and reachability is either
the kernel's Θ(N²) `ReachIndex` closure or walkie's lazy `Reach` — which fixes
the space blowup per query but still requires full history resident. For an
ESP32-S3 leaf (~300 KB usable RAM, `tutti-amy-esp32-leaf.md` §5.3), any
long-lived room, and any incremental query engine, unbounded history is
disqualifying: `ReachIndex` @ N=1k ≈ 16 MB, the `view()` snapshot clone @ N=1k ≈
400 KB *per read* (`performance-benchmark-suite.md` §3). The windowed store must
hold a bounded suffix window (W ≤ 256 ops, ≤ ~64 KB) plus a compacted checkpoint
such that **fold(checkpoint ⊕ window) is provably identical to fold(full
history)** under every admissible future ingest, convergence still holds across
peers with different windows, and the fate of verifiability is stated honestly.

**Contents:** §1 what the fold consumes (the interface the window must satisfy)
· §2 the checkpoint/compaction model and the soundness invariant · §3
`is_ancestor` across the boundary · §4 convergence + verifiability · §5 the leaf
RAM/CPU budget, concretely · §6 landing as a `DagRead` impl + the correctness
gate · §7 staging · §8 honesty inventory.

---

## 1. What the fold consumes — the contract the window must satisfy

### 1.1 The fold's actual read surface

A domain fold is `L::fold(&FoldCtx<'_, L>)` (ops.rs:119, store.rs:439-441), and
`FoldCtx` exposes exactly four things (store.rs:639-689):

| accessor | type | who uses it |
|---|---|---|
| `decoded()` | `&BTreeMap<EntryHash, DecodedOp<L>>` | every fold iterates it; `DecodedOp` carries `author` + the decoded `L::Op` |
| `op_id(entry)` | `EntryHash → OpId` | object identity (walkie pieces: `ctx.op_id(entry)`, room/store.rs:138) |
| `is_ancestor(a, b)` | strict, present-only causal ancestry | the ONE reachability question (store.rs:494-507); add-wins kills, per-op lock reads |
| `resolve(candidates)` | causal maxima + max-raw-bytes tiebreak | registers; a pure function of `is_ancestor` + `EntryHash` bytes (store.rs:518-528, kernel-identical per register.rs:53-64) |

There is no `at`/`from` parameter: the production fold reads at the full
current set. There is no ancestor *enumeration* and no cover call in the fold
path — `causal_cover`/`concurrent_cover`/`observed_at` back only the kernel's
own read models, not `OpLanguage` folds. So the windowed backend owes the fold:
a (smaller) decoded map, the entry↔op-id binding for what it retains, and an
`is_ancestor`/`resolve` oracle that answers **exactly what the full store would
answer** for every pair the fold can ask about — including pairs where one or
both endpoints predate the window (§3).

### 1.2 The store machinery's read surface (beyond the fold)

The windowed store is also a store, so it must answer:

- **`resolve_prevs`** (store.rs:315-324): map an incoming op's `backlink` +
  `observed` op ids to entry hashes. Post-compaction the map covers window ∪
  retained residue; anything below the cut defers (§4.5).
- **`observed_frontier`** (store.rs:383-390): what a local commit stamps as
  `observed`. Windowed answer = the frontier of the retained set — which is the
  window frontier once the window is non-empty, and the cut frontier `F_C`
  right after compaction. Narrow by construction, which the leaf independently
  wants (`performance-benchmark-suite.md` §3 point 3: frontier width inflates
  every authored op by 32 B/entry).
- **Per-author `LogHead`s** (store.rs:301-310): the leaf's own signing
  continuity must survive compaction — the own-author head is checkpoint state.
  Other authors' heads are advisory (verification deliberately does not check
  log continuity, ops.rs:626-630) and may be dropped.
- **`signed_ops` / `repair_record` / `entry_hashes` / `sync_root`** (store.rs:
  189-268): the sync layer's surface. Windowed answers are scoped to the
  retained set; §4.5 defines the cut-scoped session that makes this coherent.

### 1.3 The `DagRead` family, method by method

Per the reorg spec (§A.6.3) this is an implementation *behind* the seam with
**zero trait change**. The windowed impl's answers:

| method | windowed semantics |
|---|---|
| `entry(h)` / `contains(h)` | window ∪ residue; absent otherwise (present-only is already the kernel doctrine) |
| `frontier()` | maximal retained entries — the leaf's commit horizon |
| `entries_topo()` | residue-then-window, deterministic topo with the kernel's tie rule; `topo_of` (dag.rs:627-669) already counts only *present* prevs, so cut-dangling `prevs` are legal today |
| `all_hashes()` | retained hashes |
| `snapshot()` | the defaulted capture works unchanged |
| `DagDelta::appended_since(since)` | `Some(window suffix)` within the window; **`None` past it** — the escape hatch designed for exactly this (dag.rs:228-235) |
| `DagStore::append` / `missing_prevs` | admission over window ∪ residue; below-cut refs report `MissingPrevs` (defer-never-reject, dag.rs:349-360) |

One kernel gift worth naming: the present-only discipline (cover.rs:8-17,
dag.rs `topo_of`) means a truncated DAG is already a *legal* `DagRead` value —
nothing panics, nothing rejects. And one matching foot-gun: a truncated DAG fed
to plain `Reach::new`/`ReachIndex::new` silently computes `is_ancestor = false`
for every cross-cut fact, which is a **wrong view**, not an error. The windowed
store must therefore fence its own `view()` so the fold only ever runs against
the boundary-aware oracle of §3, never a naive reach over the window.

---

## 2. The checkpoint / compaction model

### 2.1 The cut

A **cut** `C` is a causally-closed (downward-closed) subset of the lifted entry
set: `C = past*(F_C)` for a chosen frontier `F_C` (normally the store's whole
frontier at compaction time). Causal closure is free: strict deferral
(`resolve_prevs` returns `None` until every referenced op is lifted,
store.rs:315-324) guarantees every lifted entry's full past is lifted, so any
`past*` of present entries is closed. The **window** is everything the store
lifts after the cut; W caps its size and a full window triggers the next
compaction (new cut = current frontier).

### 2.2 What a checkpoint IS

Both of the candidate answers, with a precise division of labor:

1. **A retained-op residue** — the subset of `C` whose contribution to *future*
   folds is not yet decided, kept as real decoded ops (entry hash, op id,
   author, `L::Op`). The fold code does not change: it iterates a decoded map
   that happens to be residue ∪ window.
2. **An ancestry summary** — per retained op, which `F_C` elements dominate it
   (a cut mask), plus the strict-ancestry relation among retained ops (§3).
3. **Pinned commitments** — the `ops_root`/`sync_root`/`state_root` at the cut,
   for the verifiability story (§4.3-4.4).
4. **Optionally, a cached materialized `L::View`** — a boot accelerator only,
   never identity-bearing: the view is always recomputable from residue ∪
   window, and the equivalence theorem (§2.6) is stated about that
   recomputation, not about the cache.

Deliberately **not** a checkpoint design: "checkpoint = the folded `L::View`
alone, with an incremental fold on top." `L::fold` is an arbitrary pure
function, not a monoid homomorphism; a view-only checkpoint cannot answer the
queries a future op forces (is this old add in the new remove's past? which
old register writes are visible below this op's horizon?). The residue-of-ops
model keeps the fold unchanged and puts all the intelligence into *what to
retain* — which is where the soundness argument actually lives.

### 2.3 Why "drop ops older than W" is wrong, precisely

Three concrete counterexamples from the real alphabet (`WalkieOp`,
src/room/ops.rs:57-89):

- **An old add killed by a future remove.** `AddDegree{k}` at op 3;
  `RemoveDegree{k}` arrives at op 900 from a peer that observed op 3. The full
  fold kills the add (`is_ancestor(add, remove)`, room/store.rs:81-87). A leaf
  that dropped op 3 either shows k live forever (wrong) or cannot evaluate the
  remove at all.
- **A remove resurrected by a future re-add — and by `UnremovePiece`.** A
  killed piece is resurrected when a later `UnremovePiece` observes the
  `RemovePiece` (room/store.rs:188-194); a degree re-appears when any author
  re-adds it. Naive dropping of "dead" piece ops makes resurrection
  unanswerable.
- **A register write concurrent with the cut.** `SetTuning` by a laggard whose
  horizon predates the cut: the full fold's `resolve` weighs it against the
  historical maxima (register.rs:53-64). If the leaf kept only the *winner*,
  the laggard's write would resolve against the wrong candidate set.

So compaction must be **semantic, not temporal**: an op leaves memory only when
its effect on every future fold is provably shadowed by what remains.

### 2.4 The compaction-soundness invariant

Two definitions, then the invariant.

**Fixed-at-lift facts.** Because of strict deferral, when op `x` lifts, its
entire causal past is materialized, and no op ingested later is ever in
`past(x)`. Hence any predicate of `x`'s own past — which adds a remove
observed, which lock-register value governed it (`locked_as_of`,
room/store.rs:161-176), which writes it supersedes — is computed once, at lift,
and **never changes**. This is the kernel invariant the whole design leans on.

**Shadowed contribution.** Op `x ∈ C` is *shadowed* by the retained set `R` iff
for every admissible future op-set extension `S′ ⊇ S`, every fold predicate
that consumes `x` evaluates identically whether `x` is present or not, given
`R` and the ancestry summary. (Formally: `fold(S′) = fold((S′ \ discard) )`
computed with the boundary oracle, for all `S′`.)

> **The invariant.** An op may be discarded at a causally-closed cut iff its
> fold contribution is already shadowed by retained ops in every admissible
> future — which holds exactly when every predicate consuming it is a
> **monotone consequence of causal facts fixed at lift** (a kill by an
> unconditional remove, a supersession by a retained later write), and never a
> consequence of the **continued absence of a future op** (an unoverridden
> remove, an unsuperseded register winner used as a value, an unremoved node
> in void semantics). Whatever is not yet shadowed is retained as residue —
> candidates, not verdicts — together with its cut mask.

The "absence of a future op" clause is the honest heart of it. In an open,
leaderless system **no op is ever causally stable in the strict sense**: any
key can sign tomorrow with `observed = []`, concurrent with all history
(`VersionedOpG.observed` is author-chosen, ops.rs:181-183; there is no
membership set to quiesce over). So the invariant deliberately does not appeal
to "no concurrent frontier can still arrive" — that never happens. It appeals
instead to monotonicity: facts that future concurrency *cannot un-make*.

### 2.5 The retention rules, per combinator, with shadowing lemmas

These are the concrete instantiations for the combinator algebra tutti actually
uses (`with_registers` / `with_pitches` / `with_pieces`, and the kernel's
`register`/`cover`/`void` semantics). Each rule comes with the lemma that makes
its discards sound; the lemmas are the things the §6 property gate falsifies.

**R — full-horizon registers (tuning, config).** Retain, per register, the
**causal maxima of its write set** — every write with no retained later write
superseding it. Discard the rest.

*Lemma R (supersession is permanent).* If `d < m` (strict ancestor) and `m` is
retained, `d` can never resolve as winner in any extension: rule 1 of `resolve`
drops `d` whenever `m` is in the candidate set (register.rs:56-60), the log is
grow-only so `m` never leaves, and the maxima of the full write set equal the
maxima of the retained set, so rule 2's tiebreak sees the identical set.
∎(sketch)

**R′ — sub-horizon register reads (the `pieces_locked` gate).** `locked_as_of`
resolves the lock register over `{writes ≤ op}` — a *past-restricted* candidate
set (room/store.rs:161-176). Here maxima-only retention is **unsound** when
`|F_C| > 1`: a discarded write `d` may be maximal inside `past(b₁)` for one cut
element `b₁` even though a later write `m > d` exists elsewhere, and a future
laggard observing only `b₁` would (in the full fold) resolve over a set
containing `d` but not `m`. The sound rule: retain, per register, the union of
**per-evaluation-point maxima** `⋃_e maxima(writes ∩ past(e))` where `e` ranges
over the cut-frontier elements `F_C` *and* every retained op that performs a
sub-horizon read (piece removes/unremoves/moves in the residue).

*Lemma R′.* Any future op's visible candidate set is `writes ∩ ⋃_{b∈B} past(b)`
for its cut-contact set `B ⊆ F_C` (plus window writes). A write maximal in that
union is maximal in each `past(b)` that contains it, hence retained; a
discarded write in the union has a retained superseder inside the union (chase
the supersession chain upward to a union-maximum); so `resolve` returns the
same winner over the retained subset. The same argument covers retained
residue readers with `e` = the reader. ∎(sketch)

**A — add-wins observed-remove (degrees).** Retain, per key and per author, the
**causal maxima of the surviving adds** (adds not observed by any effective
remove). Discard: all killed adds, all non-maximal surviving adds, and **all
degree removes**.

*Lemma A1 (kills are final).* A remove's kill set is fixed at lift: its full
past is materialized then, later ops are never in its past, and `RemoveDegree`
has no override op in the alphabet, so remove-effectiveness is unconditional.
An add observed by a remove is dead in every extension. *Lemma A2 (removes are
fully consumed).* A remove kills only adds in its own past; every future op is
outside that past; and window adds are outside `C ⊇ past(remove)` — so once a
cut-side remove's kills are recorded (by discarding its victims), the remove
itself contributes nothing further. *Lemma A3 (survivor dominance).* If
`a₁ < a₂` are surviving adds of the same key, any remove killing `a₂` observed
`a₂` and hence `a₁` (transitivity), so `a₁`'s survival always implies `a₂`'s:
`a₁` can never be the *only* survivor, and (per author) `pitch_authors` is
unchanged by dropping it. Note the lemma is stated causally, not via seq
numbers: an equivocating author who forks their log (verification does not
check cross-op continuity, ops.rs:626-630) simply yields an antichain of
per-author maxima instead of a singleton — soundness is unaffected. ∎(sketch)

**P — object graphs with resurrection (pieces).** Non-monotone: `UnremovePiece`
makes remove-effectiveness flip from effective to overridden (overriddenness
itself is then permanent — the unremove never leaves — and its
lock-suppression is fixed at lift). A killed piece add can therefore resurrect
in the future, forever: there is no point at which "this piece is settled" is
true in the open model. Retention: per piece whose `PutPiece` is not itself
provably shadowed, retain **the put, the causal maxima of its moves, all its
removes, and all its unremoves**.

*Lemma M (move supersession).* If `m₁ < m₂` are moves of the same piece, every
remove observing `m₂` observed `m₁`, so `m₁` surviving implies `m₂` surviving
at every future point; `m₁` can then never win the position register (rule 1
drops it against `m₂` among survivors) and never changes `surviving.is_empty()`.
Discarding non-maximal moves is sound; discarding removes/unremoves is not —
each remove's kill set and each unremove's override target remain live
dependencies of future resurrection arithmetic. ∎(sketch)

The piece residue is the honest bound-breaker: it grows with remove/unremove
wars on a single piece. §5 budgets it and §8 lists the policy knobs (a
domain-versioned cap, or excluding pieces from a leaf's interest set) — the
kernel design does not paper over it.

**V — void / well-founded negation (hhhs `graph` + `void`).** The general
remove-of-remove chain (`void.rs` resurrection tests, deny-on-cycle) is the
unbounded generalization of P: any node can acquire a new retractor forever,
and a verdict is a non-monotone function of the whole dependency subgraph. A
windowed void domain is sound only with residue = the dependency closure of
every atom whose verdict the fold still reads — which a domain must bound by
policy (e.g. finality declarations) that does not exist yet. Stated honestly:
**M3 does not claim to window the void engine**; it windows the combinator
algebra tutti's domains actually fold with (R/R′/A/P). The void doctrine's own
no-cache argument (void.rs:21-32) is respected: nothing here memoizes a
verdict — residue ops are *candidates*, and every verdict is recomputed by the
unchanged fold.

### 2.6 The equivalence theorem

> **Theorem (windowed-fold equivalence).** Let `S` be the lifted op set, `C =
> past*(F_C)` a cut, `R ⊆ C` the retained residue per §2.5, `D = C \ R` the
> discards. For every future extension `S′ ⊇ S` reachable by ingest (each new
> op lifting only when its past is present or summarized), the unchanged
> `L::fold` over decoded(`R ∪ (S′ \ C)`) with the §3 boundary oracle equals
> `L::fold` over decoded(`S′`) with the full oracle — for every `L` whose fold
> is composed of the R/R′/A/P combinators with retention per §2.5.

*Proof sketch.* Induction on the ingest sequence extending `S`.

*Base.* At the cut itself: each discard is covered by exactly one shadowing
lemma (R, R′, A1-A3, M); each lemma shows the fold predicates evaluate
identically on the retained subset, using the fact that all cut-side pairwise
ancestry queries were answerable at compaction time (full history still
present) and their *residue-relevant* projection is retained in the summary.

*Step.* A new op `f` lifts. Its past is fixed and equals (window part) ∪
`⋃_{b ∈ B(f)} past(b)` for its cut-contact set `B(f)` (§3.2). Every
`is_ancestor(x, f)` the fold asks with `x` retained is answered exactly by the
cut mask (§3 lemma); every `is_ancestor(x, f)` with `x` discarded is never
asked, because each lemma shows the predicate consuming `x` is already decided
without it: a discarded killed add stays killed in the full fold too (A1); a
discarded remove's kills are already reflected in the discarded/retained
partition and it can kill nothing in `S′ \ S` (A2); a discarded non-maximal
survivor/move is dropped by rule 1 in the full fold against its retained
dominator (A3/M); a discarded register write is dropped by rule 1 in every
candidate set the full fold can assemble (R/R′). Register `resolve` among
retained candidates uses the retained residue-ancestry relation and identical
raw-byte hashes, so rule 2 agrees. Hence the two folds compute identical
predicates op-by-op, and identical views. ∎(sketch — the §6 property gate is
the mechanical check of exactly this statement, including its inductive step
under adversarial laggards.)

Two corollaries worth stating now: **(i)** compaction is *idempotent and
composable* — compacting a compacted store at a later cut is compacting the
same fold-equivalent object; **(ii)** the theorem never mentions W or the cut
choice, which is what makes §4.1's convergence argument one line.

---

## 3. `is_ancestor` across the boundary

### 3.1 The three query classes

With retained set = residue `R` ∪ window `Wnd`, the oracle behind
`CausalPast::is_ancestor` (store.rs:503-507) partitions into:

1. **window × window** — answered inside the window (§3.3), stopping at the
   cut.
2. **residue × window (and residue × future ops)** — the crossing class,
   answered by cut masks (§3.2).
3. **residue × residue** — needed by `resolve` among retained register
   candidates and by `locked_as_of` recomputation; answered by a retained
   pairwise relation (§3.4).

The fourth class, discarded × anything, is never asked — that is exactly what
the shadowing lemmas of §2.5 established, and the fold cannot name a discarded
entry because it no longer appears in `decoded()`.

### 3.2 The crossing class: cut-contact sets and cut masks

For every retained op `x ∈ R`, the checkpoint stores a **cut mask**
`mask(x) ⊆ F_C`: the set of cut-frontier elements `b` with `x ≤ b` (or
`x = b`). For every window op `w`, the store maintains incrementally at lift a
**cut-contact set** `B(w) ⊆ F_C ∪ R`: the checkpoint entry points reachable
from `w`'s prevs — `B(w) = ⋃ B(prev) ∪ (prevs ∩ (F_C ∪ R))`, an O(|prevs|)
bitwise OR per lift.

> **Boundary lemma.** For `x ∈ R` and window op `w`:
> `is_ancestor(x, w) ⟺ ∃ e ∈ B(w): x = e ∨ x ≤ e`, where `x ≤ e` for
> `e ∈ F_C` is `e ∈ mask(x)` and for `e ∈ R` is the §3.4 relation.

*Soundness (⇐):* `x ≤ e ≤ w` by transitivity. *Completeness (⇒):* any `prevs`
path from `w` down to `x` enters the checkpoint region at some first node `e`;
`e` is directly referenced by a window op, so admission (§4.5) resolved it in
`F_C ∪ R`; hence `e ∈ B(w)` and `x ≤ e`. Present-only semantics are preserved
*with respect to the full store* — which is the correctness standard; the
window's own present-set is deliberately not the standard (§1.3's foot-gun).

The reverse direction `is_ancestor(w, x)` for `x ∈ R` is constant `false`:
`past(x) ⊆ C` (causal closure) and `w ∉ C`.

**Why masks and not the alternatives.** (a) *Per-author version vectors*
((author, seq) summaries) would be O(authors) and exact for honest per-author
chains — but only under the log-prefix assumption, which equivocation breaks,
and the substrate deliberately trusts nothing seq-shaped for ordering
(store.rs:119-123 "seqs … no longer decide anything"). Rejected. (b) *Bloom
filters / accumulators over ancestor sets* are probabilistic: a false positive
flips a fold verdict on one replica and not another — a convergence break of
exactly the class the no-verdict-cache doctrine exists to forbid (void.rs:
21-32). Rejected. (c) *Interval/chain labels* (DAG chain decompositions) are
exact and compact but require global relabeling as the DAG grows; the cut mask
is a frozen label over a frozen region, which is the same idea specialized to
the one boundary that never changes after the cut. Chosen: masks.

**Size bound.** `|F_C|` is the leaf's own frontier width at cut time — narrow
by construction (§1.2). Budget `|F_C| ≤ 64` (typical ≤ 8): `mask(x)` is one
u64 per retained op; `B(w)` is one u64 (cut part) + a small residue-ref set
(usually empty) per window op. Total: 8 B × (|R| + W) ≈ **4 KB at
|R| = W = 256**.

### 3.3 Inside the window: index-compressed bitset reach

The perf suite's scare number — "reach RAM ≈ Θ(W²) ≈ ~2 MB at W = 256"
(`performance-benchmark-suite.md` §3 point 1) — priced the closure as 32-byte
hashes in `BTreeSet`s (the `Reach` memo shape, store.rs:553-557). The windowed
store should not use that shape. Give every window op a dense index
`0..W`; the ancestor closure of the whole window is then a W×W bit matrix
maintained incrementally at lift (`row(op) = OR of row(prev) | bit(prev)`), and
`is_ancestor` is one bit test.

- W = 256: 256 × 32 B = **8 KB total**, O(1) query, O(W/8) per-lift cost.
- W = 128: **2 KB**.

Θ(W²) *bits* is affordable where Θ(W²) *hashes* was not; the same trick gives
the residue relation below. The lazy `Reach` stays the right shape for
unbounded full peers; the leaf's window justifies the dense closure.

### 3.4 Residue × residue

An |R|×|R| strict-ancestry bit matrix, computed once at compaction time (full
history still present, so it is exact) and frozen. |R| = 256 → 8 KB worst;
typical residues (§5) are ~100 ops → ~1.3 KB. It exists because register
resolution among retained candidates (rule 1) and `locked_as_of` recomputation
genuinely compare residue pairs; nothing else reads it.

### 3.5 The oracle as a `CausalPast` impl

The three classes compose into one backend:

```text
WindowedReach {
    window_rows: [BitRow; W],       // §3.3, incremental
    contact:     [CutSet; W],       // §3.2 B(w), incremental
    resid_rows:  [BitRow; |R|],     // §3.4, frozen at cut
    masks:       [u64; |R|],        // §3.2, frozen at cut
}
impl CausalPast for WindowedReach { … }   // store.rs:503 — already public, already dyn-erased
```

`FoldCtx` already erases its backend behind `Box<dyn CausalPast>`
(store.rs:642), so the *same* domain fold runs unchanged — the exact seam the
`Reach`/`ReachIndex` equivalence machinery was built around, exercised by a
third backend. `resolve` inherits correctness from `is_ancestor` + raw-byte
tiebreak via the trait default (store.rs:518-528).

---

## 4. Convergence and verifiability

### 4.1 Different windows, same view

Two leaves with different W, different compaction cadences, different cuts, but
the same ingested op-set: each satisfies the §2.6 theorem independently, so
each's windowed fold equals the full-history fold — hence they equal each
other. Convergence needs no coordination of cuts **because the checkpoint is
not part of the replicated object**: it is a local acceleration/GC structure,
and the theorem quantifies over every cut. The property gate (§6.3) asserts
this directly by running two windowed replicas with adversarially different
compaction schedules against one shuffled op soup.

The casualty is honest and already accepted upstream: **on-device time-travel
below the cut is forfeited** (`performance-benchmark-suite.md` §3 point 2). The
vision's `view(at, from)` parameterization (eventually-consistent-pitchsets.md
§6.1) degrades on a leaf to "at ⊇ cut"; scrubbing below the cut is a full-peer
affordance.

### 4.2 What happens to `sync_root`

`sync_root` digests the entry-hash identity set (store.rs:96-103); a windowed
leaf's retained set is a strict subset of a full peer's, so global
`sync_root` equality is structurally gone. The replacement is **cut-scoped
identity**: for a session anchored at cut frontier `F_C`, both sides digest
`{entries} \ past*(F_C)` with the *same* `sync_root_of` definition. The leaf's
side of that set is exactly its window (residue ⊆ `past*(F_C)` is excluded);
the full peer computes the complement of the cut's causal closure, which it
can, since it holds full history. Set-convergence remains checkable — relative
to a declared cut.

### 4.3 What happens to `ops_root` / `state_root` / `prove_op`

- **`state_root` survives windowing completely intact.** It commits to the
  folded `L::View` (room/store.rs:328-330), the windowed view equals the full
  view (§2.6), so a windowed leaf and a full peer produce the **same
  `state_root`** and can cross-check it. This is the load-bearing convergence
  check for leaves, and it is stronger in practice than it sounds: it verifies
  the *music*, not the ledger.
- **`ops_root` bifurcates.** The leaf can still compute an `ops_root` over its
  retained set (merkle.rs:46-48 is set-agnostic), but it commits to a
  different set than a full peer's. Two honest artifacts replace the one
  global root: (i) the **pinned cut `ops_root`** — computed over full history
  at the moment of compaction and retained in the checkpoint; (ii) a
  **window `ops_root`** over the suffix, comparable cut-scoped as in §4.2.
- **`prove_op`:** the leaf can *produce* inclusion proofs only for retained
  entries (against its window root). It can still *verify* full peers' proofs
  for discarded ops — against its own pinned cut root, which it computed
  itself while it held the history. Verification capability degrades from
  "can re-derive everything" to "can check proofs against a commitment I
  personally made" — a meaningful but honest downgrade.

### 4.4 The trust ledger: self-compacted vs adopted checkpoints

Two boot modes with different trust:

- **Mode A — grown-then-compacted.** The leaf verified every signature at
  ingress and computed its own checkpoint. Nothing was ever trusted. Residual
  verifiability is §4.3's: proofs against self-made pinned roots.
- **Mode B — checkpoint adoption.** A late-joining leaf receives (residue +
  masks + pinned roots + cut frontier) from a peer instead of replaying
  history. It can verify the *signatures* of residue ops (they are verbatim
  signed ops) and the internal consistency of the summary, but it **cannot
  verify that the summary is faithful to the discarded history** — that the
  retained maxima are really maximal, that the discarded adds were really
  killed. Mode B is trust-in-the-checkpoint-producer, mitigated by (i) the
  producer signing the checkpoint (attributable, slashable socially), (ii)
  cross-checking `state_root` and the pinned roots against multiple
  independent peers, and (iii) in the limit, the succinct-proof program of
  `docs/research/zk-provable-dag-snapshots.md`, whose "snapshot at frontier F
  + proof, then ordinary event sync after F" is *exactly this checkpoint* with
  the trust removed. M3 specifies the seam so that a proof can later ride
  along; it does not build it.

**Who authorizes a checkpoint in a leaderless room: nobody, and that is the
design.** Compaction is unilateral local GC — it changes no replicated state,
no wire byte, no other peer's view (§4.1). The only moment "authorization"
means anything is Mode B adoption, and there it is explicitly a *trust
decision by the adopting device*, named as such, with the mitigations above —
never a room-level consensus artifact. An installation runs ≥1 archival full
peer (the vision's standing operational commitment,
eventually-consistent-pitchsets.md §11; tutti-amy §6.2) and leaves adopt from
it.

### 4.5 Syncing a windowed leaf with a full peer (RBSR)

The session shape, using only existing machinery plus one app-level field:

1. **Anchor.** The leaf's session hello (the app-side wrapper in
   `src/net/sync.rs`, which already owns mode negotiation as an ALPN concern,
   sync.rs:55-60) carries its cut frontier `F_C` (≤ 64 hashes; one-time,
   bounded).
2. **Universe restriction.** Both sides build their RBSR `Index` over
   `{entries} \ past*(F_C)` — the leaf: its window, verbatim; the full peer:
   everything not causally below `F_C` (computable from its reach; for a full
   peer this is one `observed_at`-style sweep). `hhhs-sync` itself is
   untouched: the `Index`/`EntrySource` pair is app-constructed
   (sync.rs:153-246), and this is a different set fed to the same engine.
3. **Reconcile.** Standard RBSR over the restricted universe; `Done`
   cross-checks the cut-scoped root of §4.2. New leaf ops flow up; suffix ops
   the leaf lacks flow down; nothing below the cut ever rides the wire.
4. **Deep laggards.** A suffix op can still *reference* below-cut ops the leaf
   discarded (a peer offline since before the cut, `observed` pointing deep).
   The leaf's `resolve_prevs` fails; `AppendOutcome::MissingPrevs` defers it —
   defer-never-reject already the doctrine (dag.rs:349-360). Resolution
   options, both specified:
   - **Courier-assisted admission (default):** the full peer supplies, per
     missing prev, the `(OpId → EntryHash)` binding plus that op's cut mask.
     The leaf verifies entry-hash membership against its **pinned cut
     `ops_root`** (Mode A: a proof against its own commitment); the binding
     and the mask are *trusted* — proving them would require the discarded
     history (the binding is `blake3(header)` of bytes the leaf no longer
     holds, and the lift is recursive). This is the one place windowed fold
     correctness rests on courier honesty, bounded to deep-laggard ops only.
   - **Refusal (paranoid knob):** the leaf leaves the op parked. Safety is
     preserved (its view is the fold of a causally-closed subset — exactly the
     "window is the world" honesty, hhhs-ecosystem-design §3); convergence
     with full peers lags until the op is either courier-resolved or the room
     moves on. A deep-laggard *remove* deferred this way means the leaf may
     show a degree live that full peers have killed — a liveness gap in
     convergence, never a divergent fold over the same admitted set.

   Frequency argument, honestly: deep laggards require an author silent since
   before the leaf's cut; at musical op rates with W = 128-256 that is
   hours-to-days of absence, and the courier (the leaf's fuller peer,
   transport doc §7) is already in the loop by topology.

---

## 5. The leaf RAM/CPU budget, concretely

Against the AMY-leaf envelope (`tutti-amy-esp32-leaf.md` §5.3: ESP32-S3, ~300 KB
usable internal RAM after IDF + radio; the tutti line item targeted **≤ 64 KB at
W ≤ 256** per `performance-benchmark-suite.md` §2).

### 5.1 Window bytes — two layouts, honestly

The naive layout is today's `Store<L>` shape scaled down: three `BTreeMap`s
(`source_to_entry`, `entry_to_source`, `decoded`), hash-keyed, node overhead
≈ 1.5×. The packed layout is what a leaf should build: one flat table indexed
by a dense `u16` window index; hashes stored once; edges as indices; authors
interned.

| per window op | naive (BTreeMaps) | packed (dense index) |
|---|---|---|
| EntryHash + OpId | 64 B × 2 maps + node overhead ≈ 190 B | 64 B, stored once |
| prevs | `Vec<EntryHash>` ≈ 24 + 64 B | `u16` indices ≈ 5 B |
| decoded op (AddDegree-class) | author 32 + op ~8 + ts/seq 16 ≈ 56 B | op ~8 + author idx 1 (+ shed dead ts/seq — store.rs:116-123 marks them `#[allow(dead_code)]`) ≈ 12 B |
| cut-contact `B(w)` | 8 B | 8 B |
| closure row (§3.3) | 32 B @ W=256 | 32 B @ W=256 |
| **total/op** | **≈ 370-420 B** | **≈ 120-140 B** |
| **window @ W=256** | **≈ 95-105 KB — misses the gate ~2×** | **≈ 32-36 KB** |
| **window @ W=128** | ≈ 48-53 KB | ≈ 16-18 KB (closure row 16 B) |

Verbatim signed bytes (~350 B/op for degree ops) do **not** live in RAM: they
go to the flash journal (256 × ~350 B ≈ 90 KB flash, trivial against 2-8 MB
parts; wear at musical op rates is noise against 10⁴-10⁵ cycles/sector) and
are re-read only to serve RBSR `Fetch`es.

### 5.2 Checkpoint bytes (typical musical room, walkie alphabet)

| component | sizing | bytes |
|---|---|---|
| cut frontier `F_C` | ≤ 64 × 32 B (typical 8) | 0.3-2 KB |
| pinned roots (`ops_root`, `sync_root`, `state_root`) | 3 × 32 B | 96 B |
| degree residue (A3 maxima) | ~48 live keys × ~2 authors × ~88 B | ≈ 8.5 KB |
| register residue (R/R′) | ~30 writes × ~120 B; **large values (a 4096-degree `.scl`) live in flash behind a digest** | ≈ 3.5 KB + flash |
| piece residue (P/M) | 16 pieces × ~6 ops × ~100 B | ≈ 9.5 KB |
| residue reach matrix (§3.4) | ~220² bits (restrictable to the ~130 register+piece ops → 2 KB) | 2-6 KB |
| cut masks | 8 B × |R| | ≈ 1.8 KB |
| cached `RoomView` + own `LogHead` | bitfield 512 B + authors ~1 KB + 40 B | ≈ 1.6 KB |
| **checkpoint total** | | **≈ 20-27 KB** |

### 5.3 The verdict against the gate

| configuration | window | closure | checkpoint | scratch | total |
|---|---|---|---|---|---|
| packed, W=128 | 17 KB | 2 KB | ~23 KB | ~5 KB | **≈ 47 KB ✓** |
| packed, W=256 | 34 KB | 8 KB | ~23 KB | ~5 KB | **≈ 70 KB — ~10% over** |
| naive, W=256 | 100 KB | 8 KB | ~30 KB | ~10 KB | ≈ 150 KB ✗ |

Honest reading: **the ≤ 64 KB gate is met at W = 128 with the packed layout,
and missed by ~10% at W = 256** (still far inside the S3's ~300 KB envelope —
the combined AMY stack table in tutti-amy §5.3 holds either way). Recommend the
leaf default W = 128 and keep 64/128/256 as the bench axis the suite already
names (`performance-benchmark-suite.md` §4); either the gate is restated as
"≤ 64 KB at W ≤ 128, ≤ 80 KB at W = 256" or the packed layout is pushed
harder (interned hashes for residue too). These are estimates from struct
arithmetic, not measurements — M4's on-device probe replaces this table.

### 5.4 Per-op maintenance cost as ops stream in

- **Ingress verify:** Ed25519, modeled < 6 ms/op on 240 MHz silicon
  (perf suite §2, unmeasured) — protocol core, never the audio core.
- **Lift:** prev resolution O(|prevs| · log) over the packed index; closure
  row OR (W/8 = 16-32 B); cut-contact OR (8 B). Microseconds-class.
- **Refold (revision rate, not block rate):** the unchanged fold over
  |R| + W ≈ 350-500 decoded ops with O(1) `is_ancestor` — low-ms-class @
  240 MHz *(model; the `windowed::fold` bench, perf suite §7.2, is the
  measurement)*.
- **Compaction event (window full):** run `L::retain` over residue ∪ window,
  rebuild the two bit matrices and masks, pin roots, drop discards, journal
  the checkpoint to flash. One O((|R|+W)²)-bit pass — ms-class; and it is the
  *only* moment the leaf does more than O(W) work.
- **Catch-up burst (rejoin):** bounded by `SessionBudget` frames; verify
  dominates (≈ 0.6 s per 100 ops, background) — unchanged from tutti-amy
  §5.3's analysis.

---

## 6. How it lands, and the correctness gate

### 6.1 Crate placement — the seam exercised as specified

Two types, mirroring the kernel's own `MemDagStore`-vs-`Store<L>` split:

- **`WindowedDag`** — the L-free kernel piece: window entries with dense
  indices, cut frontier, cut masks, the bitset reach, implementing
  `DagRead + DagDelta + DagStore` exactly per §1.3. Zero hhhs trait change —
  the reorg spec's stated point of the seam (§A.6.3: "a new type … with zero
  trait change"). It starts life in walkie's tree beside tutti-core and is
  promoted into `hhhs-dag` under the standing **n = 2 promotion gate** (reorg
  §A.6.3's discipline for the lazy reach) when the second consumer (potluck,
  or the datalog engine's incremental EDB) asks for it.
- **`WindowedStore<L>`** — the domain piece in `tutti-core`: checkpoint
  ownership, retention, `view()` construction with the §3 oracle, the
  cut-scoped sync surface of §1.2. It is the leaf-profile sibling of
  `Store<L>`, not a replacement.

### 6.2 The kernel deltas this design actually wants (candidate hhhs hardening / potluck co-review)

Enumerated flat, because finding these was half the point:

1. **`FoldCtx` construction is hardwired to `&Store<L>`** (store.rs:648-661;
   `with_reach` is private). Wanted: a public constructor over the parts —
   `FoldCtx::over(decoded, entry_to_source, Box<dyn CausalPast>)` — so a
   second store type can drive the same fold. Additive, small.
2. **`OpLanguage` gains one defaulted method**:
   `fn retain(ctx: &FoldCtx<'_, Self>, cut: &BTreeSet<EntryHash>) ->
   BTreeSet<EntryHash> { cut.clone() }` — the domain names its residue;
   the default retains everything (compaction off, trivially sound).
   Default-method-only evolution, exactly the §A.7.1 freeze contract.
   Alternative shape if co-review prefers `OpLanguage` frozen: a separate
   `Compact<L>` trait. Either is additive; the law is the same (§2.6).
3. **Retention combinators beside the fold combinators** in tutti-core:
   `retain_register_maxima`, `retain_register_subhorizon` (R′),
   `retain_addwins_survivor_maxima` (A1-A3), `retain_object_residue` (P/M) —
   each shipping with its shadowing-lemma property suite, so a domain
   composes retention the same way it composes its fold.
4. **`for_each_topo` streaming default on `DagRead`** — already planned
   (reorg §A.6.3); windowed and large stores need `entries_topo`'s full
   `Vec<Entry>` clone gone. Unchanged ask, seconded.
5. **The `Reach` memo shape is wrong for a leaf** (BTreeSet-of-hashes per
   queried `b`, store.rs:553-557 — fine for full peers, Θ(queried·W·32 B) on
   a window). The window closure is bitsets (§3.3). No kernel change — a
   second `CausalPast` backend, which is what `CausalPast` is *for*.
6. **A `view()` fence on the windowed type**: constructing a plain `Reach`
   over a truncated window type-checks today and silently mis-answers (§1.3).
   `WindowedStore::view()` must be the only fold entry point; the raw
   `WindowedDag` should not leak into a `FoldCtx` without the boundary
   oracle. API shape, not trait change.
7. **`DecodedOp` carries 16 dead bytes/op** (`ts_ms`, `seq`, store.rs:116-123)
   — sheddable behind a leaf feature; noted, not required.
8. **Driver-side (no `hhhs-sync` change):** the cut-anchored hello field, the
   universe restriction, and the courier prev-resolution frames of §4.5 are
   walkie `src/net/sync.rs` / ALPN-versioned app wire, per the standing
   "a change here is an ALPN/mode change" rule (sync.rs:55-60).
9. **Envelope note:** `MAX_OBSERVED_OPS` = 4096 (ops.rs:46) admits a single
   op whose `observed` alone is 128 KB — legal, and bigger than the leaf's
   whole window budget. Leaf admission should cap accepted frontier width
   well below the envelope cap (a `SessionBudget`-class local bound, not a
   protocol change).

### 6.3 The correctness gate: `windowed_equiv`

A property test in tutti-core's test tree, in the exact adversarial style of
`tests/second_domain.rs` (seeded SplitMix64, shuffled ingest, oracle
cross-check) and the `reach_equiv`-style backend comparison
(store.rs:742-766):

- **Test language:** a fourth `OpLanguage` (`WinLang`) exercising every
  combinator the retention rules cover: content-keyed add/remove
  (degrees-shaped), object put/move/remove/unremove (pieces-shaped, with
  resurrection), a full-horizon register, and a sub-horizon-gated remove
  (lock-shaped) — so every lemma of §2.5 has teeth in one alphabet.
- **Drive:** N ≈ 200-500 ops, 3-5 authors committing through producer stores;
  **laggard injection** — authors periodically sign against deliberately
  stale frontiers (frozen earlier producer states), so removes/unremoves/
  register writes straddle every cut both causally and temporally;
  **equivocation injection** — a forked author log, since verification admits
  it (ops.rs:626-630).
- **Assert, after every ingest step, across shuffled arrival orders:**
  `windowed.view() == full.view() == full.view_reference()` (the kernel
  `ReachIndex` oracle stays the root of trust); boundary-oracle equality —
  for every retained × window pair, `WindowedReach::is_ancestor` ≡
  `ReachIndex::is_ancestor` on the full store; `state_root` equality.
- **Compaction schedules as an adversarial axis:** two windowed replicas with
  different W and different (randomized) compaction points over the same
  shuffled soup → equal views at every quiescent point (§4.1's claim,
  falsified directly). Compact-twice idempotence. Compact-at-every-prefix
  sweep for small N (exhaustive cuts).
- **Targeted vectors:** old-add-killed-by-post-cut-remove;
  killed-piece-resurrected-by-post-cut-unremove whose target remove is
  pre-cut; register write concurrent with the cut read by a narrow-horizon
  laggard (the R′ hazard); deep-laggard deferral and courier-resolved
  admission equivalence.

The gate for M3.1 (§7) is this suite green under `cargo test` and under the
perf suite's `windowed::fold` / `windowed::leaf_ram` benches meeting §5.3's
numbers.

---

## 7. Staging

**M3.0 — the bounded window, no compaction.** `WindowedDag` + dense bitset
reach + the `FoldCtx` constructor (delta 1) + `DagDelta::appended_since`
(`None` past the window) + the cut-scoped sync plumbing — with `retain` left at
its retain-everything default, so nothing is ever discarded and the theorem is
trivially true. While a room's life fits the window (N ≤ W) this is *exactly*
correct and *exactly* bounded; when it doesn't, memory grows (residue =
everything) and the store degrades gracefully to "small full store." Gate: the
§6.3 suite with compaction disabled + the leaf-RAM bench. **This is what the
AMY verifying leaf (tutti-amy §7.1 experiment 5) actually needs**: a jam
session is a few hundred ops, "the window is the world," and every *(model)*
number in the leaf column becomes measurable (M4). It also already serves the
incremental-query consumers: `appended_since` within the window is the delta
`hhhs-reactive`-class engines fall back from today (dag.rs:228-235).

**M3.1 — stable-verdict compaction.** `L::retain` (delta 2) + the R/R′/A
retention combinators + checkpoint build/persist + the full §6.3 gate with
adversarial cuts. Pieces retained wholesale (sound, just unpruned) or excluded
from the leaf's interest set. Unblocks: long-lived rooms on the leaf; bounded
steady-state for the browser leaf too (a real deployment today, perf suite §5).

**M3.2 — the full model.** Piece residue with move-supersession (P/M),
checkpoint adoption (Mode B boot + producer signature), courier-assisted
deep-laggard admission, and the zk-attestation seam left open for
`zk-provable-dag-snapshots.md`. Unblocks: late-joining leaves, multi-day
installations, and checkpoint-based cold start for *every* peer class.

Dependency: M3.0 ⊂ M3.1 ⊂ M3.2, each independently shippable and
independently gated; M4 (on-device measurement) needs only M3.0.

---

## 8. Honesty inventory — the hard parts, named

1. **There is no causal stability in an open system.** Any key can sign
   tomorrow concurrent with all history. The soundness invariant (§2.4) is
   monotone-shadowing, *not* quiescence, and every retention rule was derived
   under that assumption. Anything resembling "settled after time T" in a
   future design is a semantic change requiring a version bump, not a tuning
   knob.
2. **The piece residue is unbounded in the adversarial limit** (remove/
   unremove wars on one piece). Bounding it is a *domain policy decision* —
   a per-piece op cap or coalescing rule (schema-versioned), or leaf-side
   interest filtering — and this design deliberately does not smuggle one in.
3. **The void/WFN engine is not windowed by this design** (§2.5-V). Its
   verdicts are non-monotone over an unbounded retractor surface; windowing it
   requires a finality policy that doesn't exist. M3 windows the R/R′/A/P
   algebra, which is what tutti domains fold with today.
4. **Verifiability degrades, in two named ways:** an adopted checkpoint (Mode
   B) is trust in its producer until the zk-snapshot work lands; and
   courier-assisted deep-laggard admission trusts the courier for
   `OpId → EntryHash` bindings and cut masks (membership stays provable
   against the leaf's own pinned root). Everything else survives: signatures
   verify forever, `state_root` cross-checks exactly, self-compacted leaves
   verify proofs against their own pinned commitments.
5. **On-device time-travel below the cut is forfeited** — by design, already
   accepted in the perf suite (§3 point 2) and the vision's leaf profile.
6. **The 64 KB @ W ≤ 256 gate does not hold with today's data-structure
   shapes.** It holds at W = 128 packed, misses by ~10% at W = 256 packed,
   and misses ~2× naive (§5.3). All §5 numbers are struct arithmetic, not
   measurements; M4 exists to replace them.
7. **Sub-horizon registers were the trap.** The R′ hazard (§2.5) — maxima-only
   retention is unsound for past-restricted reads when the cut frontier is
   wide — is exactly the kind of bug the §6.3 gate's narrow-horizon-laggard
   vector exists to catch, and the reason retention combinators ship with
   per-lemma property suites rather than as folklore.
8. **Equivocation is tolerated, not solved:** forked author logs degrade
   per-author singleton maxima to antichains (§2.5-A3) and inflate residue;
   they never break soundness. A future equivocation-evidence policy would
   shrink the residue, not change this design.
9. **Extrapolation flags:** `|F_C| ≤ 64` is an assumption about leaf-authored
   frontier width, not an envelope guarantee (the envelope admits 4096);
   residue typicals assume musical workloads; the deep-laggard frequency
   argument is topological, not adversarial. Each is bench- or
   measurement-backed before it is load-bearing.

---

## 9. Summary

The windowed store is a retained-residue model, not a view-snapshot model: a
causally-closed cut, a domain-declared residue of not-yet-shadowed ops
(register maxima per evaluation point, surviving-add maxima, object residues),
a frozen ancestry summary (cut masks + two bit matrices, ~10 KB), and pinned
roots — under one invariant: **discard only what is monotonically shadowed by
causal facts fixed at lift; never discard anything whose meaning depends on
the continued absence of a future op.** The fold does not change; the
`CausalPast` seam and the present-only kernel doctrine already accommodate the
boundary; `DagDelta::appended_since → None` was designed for this store before
it existed. Convergence holds because the checkpoint is local GC, not
replicated state; `state_root` survives intact as the leaf's convergence
check; `ops_root`/proofs degrade to cut-scoped and pinned-commitment forms,
honestly. The smallest correct increment — the no-compaction bounded window —
is also the one the AMY leaf needs first, and the property gate that makes the
whole design falsifiable is one suite: windowed fold ≡ full fold ≡ kernel
oracle, under shuffled arrival, adversarial cuts, laggards, equivocation, and
resurrection races that straddle the compaction point.
