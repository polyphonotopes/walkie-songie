# Authenticated history-independent trees for walkie: MST vs prolly vs "geometric" — and whether one should replace RBSR

*Research report, August 2026. Follow-up to
[zk-provable-dag-snapshots.md](./zk-provable-dag-snapshots.md), which recommended adding a
Merkle/MST-style `ops_root` plus a canonical `state_root` (§1.5, §7 there). This report
picks the concrete structure — evaluating Merkle Search Trees, prolly trees, "geometric
search trees," and a Merkle-radix baseline — and answers whether the chosen tree should
**replace** the current RBSR sync (unify) or **complement** it (proofs/snapshot layer
only).*

---

## 1. Terminology: what "geometric search trees" means

The phrase has at least four referents in the literature; only one is relevant here.

1. **G-trees (the relevant one): "Geometric Search Trees"** — Carson Farmer (Textile) &
   Aljoscha Meyer (TU Berlin), [g-trees.github.io/g_trees](https://g-trees.github.io/g_trees/).
   A *family* of randomized, history-independent search trees in which every item gets a
   **rank drawn from a geometric distribution** — in practice "hash the item and count
   trailing zero bits." The name comes from the geometric distribution, not from
   computational geometry. The framework's point is unification: **zip trees, zip-zip
   trees, skip lists, Merkle Search Trees, and prolly trees are all instances** of the
   same construction, differing only in the rank distribution's parameter and in the
   per-rank "set data structure" (their k-ary variants recover B-tree-like nodes; they
   measured ~2× speedups for 32-ary over binary zip trees). Because ranks are a pure
   function of the items, "a tree of G-nodes is uniquely determined by the set of its
   items" — exactly the unique-representation property Merkle-ization needs, and the
   paper explicitly motivates set fingerprinting. Status: paper + experiment code
   ([g-trees org](https://github.com/g-trees/g_trees); the experiments repo is the paper's
   benchmark harness, not a library). **No production implementation exists.**
2. *Computational-geometry search structures* — kd-trees, range trees, priority search
   trees ("geometric searching" in the algorithms-textbook sense). Spatial indexes; not
   authenticated, not relevant.
3. *"The geometry of binary search trees"* (Demaine et al., SODA 2009) — a geometric
   reformulation of BST access sequences used in dynamic-optimality theory. Pure theory;
   not a data structure you deploy.
4. *G-Tree, the road-network index* (Zhong et al., TKDE 2015) — spatial KNN partitioning
   for maps. Unrelated.

Adjacent randomized-BST family worth naming because G-trees subsume them: **treaps**
(Seidel–Aragon) with priorities = hash(key) are the classic *uniquely represented* search
tree — this is the standard construction in the history-independence literature (Naor &
Teague, STOC 2001, "anti-persistence"); **zip trees** ([Tarjan, Levy, Timmel,
arXiv:1806.06726](https://arxiv.org/abs/1806.06726)) are the modern rank-based
formulation; a Merkle-ized hash-priority treap is a perfectly sound authenticated
canonical set structure. The catch for all of them: binary fanout (deep paths, more
hashes per proof) and **no maintained Rust library with an authenticated/proof API** —
you would build it from scratch. Meyer's own set-reconciliation paper notes the family is
usable for fingerprinting "in principle, but malicious actors can craft degenerate sets
that incur super-logarithmic computation times"
([RBSR, arXiv:2212.13567](https://arxiv.org/abs/2212.13567), §2) — a caveat that applies
to *every* hash-derived structure in this report (§6, risk 1).

**Verdict on the user's term:** read "geometric search trees" as Farmer & Meyer's
G-trees. They are the right *theory* — the lens that shows MSTs, prolly trees, and zip
trees as one design space — but they are a 2024-vintage paper with reference-grade code,
not an adoptable dependency in 2026.

---

## 2. What walkie actually needs the tree for (and the one fact that decides everything)

Grounding, from code read for this report:

- **The committed set is a set of blake3 hashes.** The RBSR index is keyed by the *bare
  op hash*: `RoomSyncSource::capture` inserts `SortKey(hash.as_bytes())`
  (`src/net/sync.rs`), and `sync_root` is blake3 over the sorted hash list
  (`src/room/store.rs::sync_root_of`). The op set is **grow-only** (ops are never
  removed; removal semantics live in the fold, not the set) until some future
  history-truncation feature.
- **The CRDT does not live in the tree.** Add-wins, seq registers, causal maxima are
  computed by the fold over the causal DAG (`src/room/ops.rs`, `store.rs`). The tree only
  has to *commit* to (a) the op set (`ops_root`) and (b) the folded view (`state_root`).
- **Incumbent sync #1 (Rust): RBSR** (`hhhs-rs/hhhs-core/src/reconciliation.rs`) —
  sans-io range fingerprinting, XOR of `blake3(entry_hash ‖ session_salt)`, per-session
  salt (the code's explicit defense against the "commutative-monoid adversary"), index
  **rebuilt per capture** into a `BTreeMap`, ephemeral recursion, no persistent tree, no
  proofs. The surrounding driver has accumulated real battle scars (frame-cap poisoning
  fixes, causal-closure budgets — see the comments in `src/net/sync.rs`).
- **Incumbent sync #2 (TS): frontier gossip** (`hhs3-ts/modules/sync/SPECS.md`) —
  broadcast frontier heads, BFS ancestry walk with `limits`, header/payload fetch.
  Excellent for live tails (transfer ∝ new ops); no set-difference bound for cold
  divergence, no commitment, no proofs.

**The deciding fact:** every sorted-search-tree candidate in this report (MST, prolly,
G-tree, treap) exists to solve one problem — *keeping a canonical balanced tree over
arbitrary, adversarially-or-sequentially distributed user keys*. Walkie's `ops_root` keys
are **already uniformly distributed 32-byte hashes**. The balancing problem these
structures solve does not exist here; a plain **canonical radix/Patricia trie over the
hash bytes** is automatically balanced w.h.p., history-independent by construction, and
gives membership *and* non-membership proofs with less machinery. That baseline
(Ethereum's [Merkle Patricia trie](https://ethereum.org/en/developers/docs/data-structures-and-encoding/patricia-merkle-trie/)
family; [Merklix trees](https://www.deadalnix.me/2016/09/24/introducing-merklix-tree-as-an-unordered-merkle-tree-on-steroid/)
as the set-flavored variant; [jmt](https://github.com/penumbra-zone/jmt) as the
production Rust incarnation) must therefore be beaten, not assumed away. Note Ethereum
itself is migrating the hexary MPT toward simple **binary** trees
([EIP-7864](https://eips.ethereum.org/EIPS/eip-7864)) for smaller proofs — the trend in
the flagship deployment is *toward* the minimal structure.

The `state_root` side is even less demanding: `RoomView` is small (hundreds of keys), so
any canonical Merkle over sorted `(section, key, value)` leaves suffices; no search-tree
machinery is warranted there at all.

---

## 3. The candidates, examined

### 3.1 Merkle Search Trees (Auvolat & Taïani, SRDS 2019)

[HAL hal-02303490](https://inria.hal.science/hal-02303490) /
[DOI 10.1109/SRDS47363.2019.00032](https://doi.org/10.1109/SRDS47363.2019.00032).
Structure: each key's **layer = number of leading zeros of hash(key) in base B**
(expected fanout B); a node ("page") holds the run of same-layer keys between
higher-layer separators, with child pointers to lower layers; pages are hashed →
Merkle root. Deterministic and history-independent given (hash, B): two peers with the
same key set hold byte-identical trees. Ordered by key, so it is a real search tree with
range structure — the paper's headline efficiency case is "updates on sets of sequential
keys."

**The "MST is a state-based CRDT" claim, critically.** The paper's abstract states the
thesis plainly: "pure state-based CRDTs can be efficiently implemented by encoding states
as specialized Merkle trees," demonstrated via "a distributed event store" with a 66%
bandwidth reduction over a vector-clock approach in low-update-rate large networks. Read
carefully, the contribution is narrower than the framing:

- **The CRDT is a grow-only set (plus an LWW-map built on it); the tree is its
  encoding.** Merge = set union, computed by simultaneous traversal that prunes shared
  subtree hashes; because the representation is canonical, `merge(A,B)` is independent of
  merge order — i.e., the MST is a *canonical representative* of the G-Set semilattice
  with an efficient delta computation, not a new merge semantics. All conflict semantics
  richer than union (walkie's add-wins, registers, owner-gating) still have to live in a
  layer above. **Removals are not part of the model** — deletion breaks the union
  semilattice; you need tombstones above the tree. For walkie this is harmless
  (`ops_root` commits a grow-only op set) but it demolishes any idea of the MST
  *replacing* walkie's CRDT layer.
- **The anti-entropy protocol is tree-shape-guided, and that is its weakness.** Meyer's
  RBSR paper evaluates [AT19] directly: "reconciliation messages are guided by the tree
  shape, so the number of recursion steps in each round is fixed, and the maximum number
  of rounds can degrade to **O(n) in the worst case**," versus RBSR's guaranteed
  logarithmic rounds with free choice of split points
  ([arXiv:2212.13567](https://arxiv.org/abs/2212.13567), §2 — while also crediting the
  MST paper's "promising experimental evaluation"). Notably, v2 of Meyer's paper
  *removed* its own claims about authenticated data structures
  ([submission history](https://arxiv.org/abs/2212.13567)) — sync and authentication kept
  cleanly separate even by the person who unified the theory.
- **Balance assumes honest keys; the paper targets "open networks."** The uniform-hash
  balancing argument is non-adversarial. The largest production deployment writes the
  warning into its spec: atproto notes "accounts can mine for sets of record keys with
  particular depths… which can cause… network amplification" and tells implementers to
  cap entries per node ([atproto repository spec](https://atproto.com/specs/repository)).
- **Production validation is single-writer.** Bluesky's MST (fanout 4: leading zeros of
  sha256(key) counted in 2-bit chunks; keys are record *paths*, not hashes; single
  signing key per repo; proof chains for firehose inclusion) is an **authenticated
  single-writer map** — it exercises the Merkle/proof half of the design and none of the
  multi-writer CRDT-merge half. The multi-writer story remains validated only by the
  paper's simulations (28 citations in seven years; no large multi-writer deployment we
  could find).

**Rust reality:** [domodwyer/merkle-search-tree](https://github.com/domodwyer/merkle-search-tree)
v0.8.0 is a clean, well-tested (fuzzing + property tests + snapshot tests) pure-Rust
implementation of the paper aimed squarely at anti-entropy: `root_hash()`,
`serialise_page_ranges()`, `diff()`. Fine print that matters for walkie: default hasher
is **SipHash-128 — explicitly "not of cryptographic quality"** and non-portable across
platforms/Rust versions (the docs say so); the `Hasher<N, T>` trait is generic, so blake3
drops in, but portability/security is on you. There is **no membership-proof API** (diff
page-ranges are not proofs), and **no delete** (upsert-only), which forecloses future
history truncation without a rebuild. Wasm: pure Rust, trivial dep tree — fine.

**Fit:** good — canonical, ordered by exactly walkie's RBSR sort order (hash order), one
credible crate. But its two distinctive strengths (sequential-key update locality;
tree-guided anti-entropy) are respectively irrelevant (keys are hashes) and inferior to
the RBSR walkie already has.

### 3.2 Prolly trees (Noms → Dolt)

[DoltHub's write-up](https://www.dolthub.com/blog/2024-03-03-prolly-trees/): a
"probabilistic B-tree" whose node boundaries are **content-defined** — a chunker walks
the sorted entries and declares a boundary when a hash condition fires. Deterministic and
history-independent given the chunker config; identical data ⇒ identical tree regardless
of insertion order; diff "in time proportional to the size of differences"; structural
sharing across versions. Dolt's engineering history is instructive: Noms' original
chunker produced geometrically distributed chunk sizes (many tiny, few huge); Dolt moved
to a size-controlled probability function targeting ~4 KB chunks, and simplified rolling
input to keys-only. Write amplification per edit ≈ chunk size × tree depth (their own
number: "4 KB multiplied by the depth of the tree"), with a small probability of
boundary shifts rippling into a neighbor chunk — bounded because "changes within a
subtree can never affect the subtrees to its right" (see also
[Joel Gustafson's analysis](https://joelgustafson.com/posts/2023-05-04/merklizing-the-key-value-store-for-fun-and-profit):
~7 nodes touched per random edit at 16.7 M entries, ~0.18 splits/merges per update).

**Fit:** prolly trees are *git-for-data* infrastructure — designed for large ordered
mutable maps with locality, block storage, version graphs, three-way merge. Every one of
those design pressures is absent in `ops_root` (small set, hash keys, no locality, no
value mutation, no version graph). The chunker is pure overhead relative to MST's
"layer = f(key)" — which is precisely the G-tree paper's point that both are instances of
one family; with uniform hash keys they converge in behavior anyway. Rust:
[prollytree](https://github.com/zhangfengcdt/prollytree) 0.4.1-beta *does* advertise
inclusion/absence proofs plus git-style branching, but it is a young (33 stars),
feature-sprawling crate (GlueSQL, agent-memory, RocksDB backends) — the wrong
dependency profile for a wasm plugin. **Not the pick.**

### 3.3 G-trees / zip trees / hash-priority treaps

Covered in §1. As *theory*, G-trees are the best account of the whole space, and their
k-ary instances would be a lovely basis for a from-scratch canonical tree. As
*engineering*, there is nothing to adopt: no library, no proof API, no deployment. If
walkie ever writes a bespoke ordered canonical tree, write it as a (k)-G-tree and cite
the paper; do not block on the family today. (A Merkle-ized hash-priority treap is the
minimal member — sound, but binary-depth proofs and fully bespoke.)

### 3.4 Merkle radix / Patricia tries (the baseline that competes)

For a set of fixed-length uniform hash keys, a compressed radix trie (binary or 4-bit) is
canonical trivially (structure = key bits; no chunker, no rank function, no rotation),
balanced w.h.p. (depth ≈ log₂ n + small constant for n uniform keys), supports **both**
membership and the cleanest **non-membership** proofs (a divergent-prefix node), and
handles **deletes** canonically (relevant the day history truncation arrives). Proof size
at n = 10⁴: binary ≈ 13–14 sibling hashes ≈ ~500–700 B; 4-bit radix ≈ 7 levels × 3
siblings ≈ ~800 B. Verification is a dozen blake3 calls — microseconds in wasm.
Production Rust: [jmt](https://github.com/penumbra-zone/jmt) (Diem's Jellyfish Merkle
Tree, maintained by Penumbra, used by several chains; inclusion + exclusion proofs) —
though its versioned-storage API is heavier than walkie needs; a bespoke in-memory trie
over 32-byte keys is a few hundred lines with no dependency risk, and — decisive for this
codebase — **a spec small enough to reimplement identically in TypeScript** for hhs3-ts
parity. Ethereum's own move from hexary MPT to binary trees
([EIP-7864](https://eips.ethereum.org/EIPS/eip-7864)) endorses the minimal shape.

Weaknesses: no range/order structure beyond lexicographic-on-hash (irrelevant for
`ops_root`, whose sort order *is* hash order — the trie's prefix order coincides with the
RBSR `SortKey` order); per-key insert rewrites a full root path (≈ depth × node size —
trivial at walkie scale); grinding shared prefixes deepens one path (birthday-bounded:
2^k work buys ~2k shared bits — a DoS nuisance, not a soundness break; cap depth).

### 3.5 The incumbents (what any tree must beat)

**RBSR** already gives walkie transfer ∝ difference in ~log rounds
([negentropy](https://github.com/hoytech/negentropy) measures ≈2.5 round trips at 10⁶
elements, b=16 — [logperiodic's analysis](https://logperiodic.com/rbsr.html)), with three
properties no canonical tree matches: (a) **stateless, storage-decoupled protocol** — no
tree to persist, snapshot, or keep consistent under concurrent sessions; (b) **free
choice of split points** — worst-case logarithmic rounds regardless of data shape
(Meyer's critique of tree-guided sync, §3.1); (c) **per-session salted fingerprints** —
the XOR monoid is trivially forgeable *unsalted* (logperiodic: crafting an XOR collision
takes "seconds"; even addition mod 2²⁵⁶ falls "within days"; negentropy hardens with
SHA-256 over the accumulator + count), and walkie's session salt rules the attack out
per-session. A Merkle root is unkeyed *by necessity* (it must be canonical), which is
fine for a collision-resistant commitment (blake3 concatenation, not a linear monoid) but
means: **you cannot replace the salted XOR fingerprints with cached unsalted monoid
labels without reopening the hole the salt closed.** The only cacheable *and* secure
range-fingerprint monoids are the homomorphic multiset hashes Meyer surveys — ECMH
([Maitin-Shepard et al.](https://arxiv.org/abs/1601.06502)) / LtHash
([2019/227](https://eprint.iacr.org/2019/227.pdf)) — at real CPU cost per update.

**Frontier gossip** (TS) stays regardless: it is the live-tail path (one broadcast, no
negotiation), and no tree replaces that job. Its gap is cold-divergence repair — which is
exactly where a shared `ops_root` helps it (§5).

---

## 4. Comparison table

Scale assumptions: n = 10³–10⁵ ops, keys = uniform blake3 hashes, browser/wasm32 peers.

| | 1. History-indep. / determinism | 2. Membership / non-membership proofs | 3. CRDT / concurrent-merge fit | 4. Incremental update (wasm) | 5. Diff / sync efficiency | 6. Browser & Rust maturity |
|---|---|---|---|---|---|---|
| **MST** (Auvolat–Taïani; atproto) | Yes — canonical given (hash, B); layer = leading zeros of hash(key) | Possible (page path) but **no API in the Rust crate**; non-membership via ordered adjacency; proof ≈ 3–4 pages ≈ 3–4 KB | Encodes **G-Set only**; merge = canonical union with subtree pruning; no removals; richer semantics must sit above (walkie's fold) | O(log n) expected; page rewrite + layer cascades; upsert-only crate (no delete) | Tree-guided diff, rounds ∝ depth, **worst-case O(n) rounds** (Meyer); transfer ∝ diff | [Crate v0.8.0](https://github.com/domodwyer/merkle-search-tree): pure Rust, fuzzed, wasm-fine; **default SipHash non-crypto/non-portable**, bring blake3 |
| **Prolly tree** (Noms/Dolt) | Yes — canonical given chunker config; content-defined boundaries | Yes in [prollytree](https://github.com/zhangfengcdt/prollytree) (0.4.1-**beta**); chunk-path proofs ≈ chunk × depth (KBs) | Same as MST semantically (canonical map encoding); shines at git-style three-way merge of *databases*, not op sets | Edit ≈ chunk × depth bytes rewritten (Dolt: "4 KB × depth"); boundary shifts bounded, small | Diff ∝ difference (excellent, block-granular); same tree-guided round-trip caveat | Beta crate, heavy feature surface; designed for disk/block stores, overkill in wasm |
| **G-trees** (Farmer–Meyer) / zip / hash-treap | Yes — unique representation from rank = f(hash(item)); the *theory* umbrella for the two rows above | In principle (Merkle-ize G-nodes); nothing implemented | Same encoding argument; no CRDT beyond canonical-set | O(log n) w.h.p.; k-ary variants ≈ B-tree behavior | Same class as MST | **Paper + experiment code only**; no library ([repo](https://github.com/g-trees/g_trees)) |
| **Merkle radix trie** (MPT/Merklix/jmt) | Yes — trivially (structure = key bits); no tunables beyond arity | **Best**: inclusion + exclusion native; ~0.5–1 KB at 10⁴ keys; µs verify; [jmt](https://github.com/penumbra-zone/jmt) ships both | Canonical-set encoding like the rest; deletes canonical (future truncation); union merge = insert diff | Path rewrite, ≈ depth nodes ≈ tens of blake3 calls; no cascades, no chunker | Root-compare + subtrie descent works (rounds ∝ depth); same worst-case caveat as any tree | jmt production-grade (heavier API); **bespoke ≈ 400 lines**, pure Rust + easy TS twin |
| **RBSR** (incumbent, Rust) | N/A — ephemeral recursion; **no commitment, no proofs** | None | N/A (transport, not state); already causal-closure-aware via `completion_plan` | Index rebuild per capture: O(n log n) + O(n) hashing ≈ ms–tens of ms at 10⁵ — a non-issue | **Best-in-class**: transfer ∝ diff, guaranteed ~log rounds, split-point freedom, salted vs Byzantine collisions, battle-tested framing | Already shipped and hardened in `hhhs-core` + `src/net/sync.rs` |
| **Frontier gossip** (incumbent, TS) | N/A | None | Causally shaped by construction | O(1) per new op (broadcast head) | Ideal for live tails; unbounded walks for cold divergence; no set-equality check other than frontier match | Already shipped in hhs3-ts |

---

## 5. Unify or complement? The clear position

**Complement. Keep RBSR as the wire protocol; add one canonical authenticated structure
as the commitment/proof layer; do not make the tree drive sync.** Three reasons, each
sufficient:

1. **The tree would be a worse sync protocol than the one walkie has.** This is not
   opinion; it is the published comparison by the author of RBSR itself: tree-guided
   reconciliation fixes recursion steps to tree shape and degrades to O(n) rounds in the
   worst case, while RBSR guarantees logarithmic rounds and leaves split points free
   (§3.1). RBSR is also stateless per session (no persistent tree to snapshot under
   concurrent syncs — logperiodic's "rigid tree structures require CoW snapshots"
   argument) and its per-session salt is a real Byzantine defense that canonical
   (necessarily unkeyed) tree labels cannot reproduce with a cheap monoid (§3.5). The
   deviation-hardened driver in `src/net/sync.rs` (frame-cap poisoning, closure budgets)
   would all have to be re-earned by a new protocol.
2. **The supposed unification savings don't exist at walkie's scale.** The only real cost
   RBSR pays that a persistent tree would amortize is `capture()`'s per-session index
   rebuild + O(n) salted hashing — milliseconds to tens of milliseconds at 10⁴–10⁵ ops in
   wasm, per session, and `absorb()` already makes intra-session updates O(lifted). The
   upgrade path *if this ever profiles hot* is known and incremental: keep the wire
   protocol, back the index with a persistent monoid tree (Meyer §4: any balanced tree
   with monoid labels answers arbitrary range fingerprints in O(log n), independent of
   tree shape), and either relabel per session (O(n) hashing, what capture does today) or
   switch the fingerprint monoid to ECMH/LtHash to make labels salt-free *and* secure.
   That is a contained follow-up, not a prerequisite.
3. **What the tree is uniquely good for is exactly what RBSR can't do** — and it slots
   into the snapshot plan already accepted in the prior report: `ops_root` in the
   snapshot message (Stage 0 there), O(log n) membership/exclusion proofs for verifiable
   snapshots, light/partial access, and `state_root`-leaf fraud-proof localization
   (Stage 1). One structure, purely additive, zero wire-protocol risk.

**Where unification *is* real: across the two stacks, not within the Rust one.** hhs3-ts
has no cold-divergence repair path. If both stacks compute the *same specified*
`ops_root`, the TS side gains (a) an O(1) "are we converged?" check stronger than
frontier comparison, and (b) a poor-man's repair path — root-compare, descend divergent
subtries, fetch — without implementing RBSR in TypeScript. This is the atproto/Joel-
Gustafson merkle-diff pattern, acceptable there because the TS deployment is small-room
and non-adversarial; the Rust stack keeps RBSR. One commitment, two consumers.

The same structure also subsumes `sync_root`: `ops_root` of the same entry set is a
strictly stronger digest (root equality ⟺ set equality, plus proofs), so `sync_root` can
be redefined as `ops_root` after a deprecation window, removing a redundant O(n) hash
pass from `capture`/`absorb`.

---

## 6. Recommendation

**Adopt: a bespoke canonical hash-keyed Merkle radix trie (4-bit radix suggested,
blake3, domain-separated leaf/node tags) for `ops_root`, ~300–500 lines of pure Rust +
a TS twin, specified byte-for-byte and pinned by shared golden vectors. For
`state_root`: a flat canonical Merkle over sorted `(section, key, value)` leaves of
`RoomView` — no search tree needed. Keep RBSR and frontier gossip exactly as they are.**

Among the three structures the question named, **the MST is the best fit** — canonical,
hash-ordered like the RBSR index, one credible fuzz-tested Rust crate — and it is the
recommended fallback if a dependency is preferred over bespoke code (use
`domodwyer/merkle-search-tree` with a blake3 `Hasher`, never the SipHash default, and
budget the missing pieces: a proof-extraction layer over its pages and no deletes).
But the trie beats it on every axis walkie actually exercises: proofs (native inclusion
*and* exclusion vs none in the crate), deletes (future history truncation), spec size
(the TS twin is the hidden cost of every fancier structure — atproto's TS MST exists but
is keyed on record paths with DAG-CBOR/CIDs, not reusable), and zero tunables to freeze
into a cross-stack canon. Prolly trees: no — database machinery with a beta crate,
solving locality/versioning problems `ops_root` doesn't have. G-trees: the right theory,
nothing to adopt; revisit if a library materializes. The MST-as-CRDT claim, assessed:
true only in the narrow sense that a canonical tree makes a G-Set's anti-entropy
efficient — the tree carries no merge semantics walkie needs, its multi-writer story has
never been validated in production (Bluesky is single-writer), and its sync protocol is
dominated by RBSR — so adopt the *commitment*, never the *protocol*, and keep the CRDT
in the fold where it lives.

**Effort** (fits the prior report's Stage 0, extends it by the proof layer):

| Work item | Estimate |
|---|---|
| Trie + `ops_root`/`state_root` + golden vectors (Rust) | ~1 week |
| Inclusion/exclusion proof gen + wasm verify | ~2–4 days |
| TS twin + cross-stack parity vectors | ~3–5 days |
| Snapshot-message integration (`F, ops_root, state_root, sig`) | in Stage 0 (prior report) |
| (Deferred, only if profiled) persistent monoid-labeled index for RBSR / ECMH labels | ~1–2 weeks |

**Top risks**

1. **Key-grinding shape attacks** — common to *every* hash-derived canonical structure
   (atproto documents the MST version in its spec; prolly chunkers have the giant-chunk
   analog; tries have birthday-bounded deep paths; Meyer flags degenerate sets for the
   whole history-independent family). DoS-grade, not soundness-grade. Mitigate: depth
   cap / per-node entry cap with deterministic overflow rule, written into the trie
   spec from day one (retrofitting changes every root).
2. **Protocol-unification temptation.** Replacing RBSR with tree-diff sync would trade a
   guaranteed-log-round, stateless, salted protocol for a worst-case-O(n)-round,
   stateful, unkeyed one — and silently reopen the fingerprint-forgery hole the session
   salt closed if XOR labels were ever cached unsalted. The unify-at-index-layer path
   (monoid labels / ECMH) exists and is compatible; take it only on profiling evidence.
3. **Commitment ≠ correctness, and drift.** `ops_root` proves *presence*, not fold
   validity — the trust ladder (co-signing, fraud proofs, zkVM) from the prior report is
   unchanged. And three artifacts now describe one entry set (`ops_root`, RBSR `Index`,
   legacy `sync_root`): they must be derived from a single source of truth
   (`RoomSyncSource`-style consistent capture) with invariant tests, or a skew becomes a
   permanent phantom divergence. Deleting `sync_root` after migration shrinks this
   surface.

---

## 7. Sources

- **G-trees / randomized family:** Farmer & Meyer, [Geometric Search Trees](https://g-trees.github.io/g_trees/) ·
  [g-trees/g_trees](https://github.com/g-trees/g_trees) + [experiments](https://github.com/g-trees/gtree_experiments) ·
  zip trees [arXiv:1806.06726](https://arxiv.org/abs/1806.06726) ·
  zip-zip trees [arXiv:2307.07660](https://arxiv.org/abs/2307.07660) ·
  Naor & Teague, anti-persistence/history-independence (STOC 2001) ·
  Demaine et al., geometry of BSTs (SODA 2009) — for disambiguation only
- **MST:** Auvolat & Taïani, SRDS 2019 — [HAL hal-02303490](https://inria.hal.science/hal-02303490),
  [DOI](https://doi.org/10.1109/SRDS47363.2019.00032) ·
  [atproto repository spec](https://atproto.com/specs/repository) (fanout-4 MST, key-mining warning, proof chains) ·
  [domodwyer/merkle-search-tree](https://github.com/domodwyer/merkle-search-tree) (v0.8.0; Hasher trait docs re SipHash)
- **Prolly trees:** [DoltHub: Prolly Trees](https://www.dolthub.com/blog/2024-03-03-prolly-trees/) ·
  [Dolt chunker redesign](https://www.dolthub.com/blog/2022-06-27-prolly-chunker/) ·
  [zhangfengcdt/prollytree](https://github.com/zhangfengcdt/prollytree) ·
  [Gustafson: Merklizing the key/value store](https://joelgustafson.com/posts/2023-05-04/merklizing-the-key-value-store-for-fun-and-profit) ·
  [canvasxyz/okra](https://github.com/canvasxyz/okra)
- **RBSR & fingerprint security:** Meyer, [Range-Based Set Reconciliation, arXiv:2212.13567](https://arxiv.org/abs/2212.13567)
  (monoid trees §4; MST critique §2; multiset-homomorphic-hash survey; v2 note re ADS claims) ·
  [hoytech/negentropy](https://github.com/hoytech/negentropy) ·
  [logperiodic: RBSR](https://logperiodic.com/rbsr.html) (XOR/addition attack costs, negentropy fingerprint) ·
  ECMH [arXiv:1601.06502](https://arxiv.org/abs/1601.06502) · LtHash [ePrint 2019/227](https://eprint.iacr.org/2019/227.pdf)
- **Radix-trie baseline:** [Ethereum MPT docs](https://ethereum.org/en/developers/docs/data-structures-and-encoding/patricia-merkle-trie/) ·
  [EIP-7864 (binary tree migration)](https://eips.ethereum.org/EIPS/eip-7864) ·
  [Merklix trees](https://www.deadalnix.me/2016/09/24/introducing-merklix-tree-as-an-unordered-merkle-tree-on-steroid/) ·
  [penumbra-zone/jmt](https://github.com/penumbra-zone/jmt)
- **Walkie code grounding:** `hhhs-rs/hhhs-core/src/reconciliation.rs` (salted XOR monoid, sans-io) ·
  `walkie-songie/src/net/sync.rs` (`RoomSyncSource::capture`/`absorb`, hash-order `SortKey`) ·
  `walkie-songie/src/room/store.rs` (`sync_root_of`) ·
  `hhs3-ts/modules/sync/SPECS.md` (frontier gossip) ·
  [prior report](./zk-provable-dag-snapshots.md) (§1.5, §7, Stage 0/1)
