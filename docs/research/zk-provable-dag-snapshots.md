# Verifiable DAG snapshots: succinct proofs of the CRDT fold vs. authenticated snapshots

*Research report, August 2026. Investigates whether walkie-songie can hand a cold-start or
long-absent joiner a **state snapshot plus a proof** instead of streaming the full causal
closure of signed events — and what "proof" should mean at each cost/trust level.*

**Scope note.** This is an optimization for cold start and long absence, not a replacement
for event sync. Live collaboration, fine-grained merge, and add-wins semantics still need
the ops themselves; the anti-entropy layer (RBSR over `entry_hashes()`, causal-closure
serving) stays. A snapshot is a *checkpoint baseline*: joiner state = snapshot at frontier
F, then ordinary event sync for everything after F.

---

## 1. The system, and the exact statement a proof must cover

### 1.1 What exists today (grounded in the code)

- **Ops** (`src/room/ops.rs`): eight small domain ops — `AddDegree`, `RemoveDegree`,
  `PutPiece`, `MovePiece`, `RemovePiece`, `UnremovePiece`, `SetTuning`, `SetConfig` —
  wrapped in a `VersionedOp` envelope (schema version, author timestamp, room topic
  binding, and `observed`: the causal horizon of up to 4096 op ids), CBOR-encoded as a
  p2panda `Body`, and signed via `Header::sign` — **Ed25519 over the CBOR header bytes**,
  with the op id = **blake3(header bytes)**. Verification (`verify_signed_op`) checks the
  signature, payload-hash binding, schema version, topic, and domain bounds.
- **DAG lift** (`src/room/store.rs`): each verified op becomes an hhhs `Entry` whose
  payload is the verbatim framed signed bytes and whose `prevs` =
  `{lift(backlink)} ∪ {lift(o) : o ∈ observed}`. Entry hash is a pure function of the
  signed bytes + prev set, so identity is arrival-order independent. Strict deferral parks
  ops until their causal past is lifted.
- **Fold** (`RoomStore::view()` + `hhhs_core`): a deterministic, order-independent
  reduction over the DAG snapshot:
  - **Pitches**: content-keyed **add-wins set** — an add is live iff *no* same-key remove
    has it as a strict causal ancestor (`ReachIndex::is_ancestor`).
  - **Pieces**: owner-gated per-author **seq registers** — greatest-seq lifecycle op of the
    owner decides liveness, greatest-seq move decides position. No reachability needed;
    within one author's log, seq order is total.
  - **Tuning / config**: cross-author registers resolved by **causal maxima**, tie-broken
    by max raw-bytes entry hash (`hhhs_core::register::resolve`).
- **Convergence digest**: `sync_root()` = blake3 over the **sorted flat list** of entry
  hashes. Two peers agree iff they hold the same entry set.

### 1.2 The validity statement

A snapshot receiver wants a succinct proof π for:

> **Given** public values (commitment **H** to an entry set E, topic **T**, snapshot
> **S**): (1) every entry in E carries a validly **Ed25519-signed** p2panda op, bound to
> topic T, schema-valid, whose blake3 ids and prev-links are consistent (E is causally
> closed); (2) **S = materialize(E)** under the deterministic fold rules above.

Zero-knowledge (hiding the ops) is **not** the requirement here — every room member sees
every op anyway. What is needed is **succinctness + soundness**: a small proof, cheap to
verify on a phone, that an untrusted peer cannot forge. That is the same posture as
zk-rollup "validity proofs," which are typically run *without* the ZK privacy property.
ZK-privacy becomes relevant only in a future E2EE-room / untrusted-relay setting (§4.2,
KiloNova row).

### 1.3 What is cheap and what is expensive in that statement

| Sub-claim | In-circuit character | Cost driver |
|---|---|---|
| Ed25519 signature per op | non-native curve + SHA-512 | **dominant**; see §5 |
| blake3 ids, entry hashes, CBOR structure | bit-twiddly hashing/parsing | moderate; blake3/CBOR are not SNARK-friendly |
| Causal closure, prev-link consistency | positive set-membership facts | cheap with a good commitment |
| Pieces + seq registers | per-author max-seq scan | cheap (no DAG queries) |
| **Add-wins liveness / register maxima** | **NON-reachability over the DAG** | the structurally interesting cost |

The last row deserves emphasis. Add-wins ("no same-key remove reaches this add") and
register resolution ("candidate is not a strict ancestor of any other candidate") are
*negative* transitive-closure statements. A circuit cannot check "no path exists" by
exhibiting a path; the standard techniques are (a) prover supplies per-entry **ancestor
bitmaps** (transitive closure via OR of parents' bitmaps — O(n²) bits, fine at n ≤ ~10⁴,
painful past that), (b) topological-order counters plus per-key state machines, or (c) run
the *actual Rust fold* (`ReachIndex` and all) inside a zkVM and pay in cycles rather than
circuit design (§6). Note the native fold has the same shape: `ReachIndex` materializes
per-entry ancestor `BTreeSet`s, which is itself O(n²)-ish and will bite before proving
does at large n.

### 1.4 Honest scale check: what does replay cost today?

An op on the wire is a few hundred bytes; Ed25519 verify is ~50 µs native, roughly 2–4×
slower in wasm. So:

| Room size | Transfer | wasm verify time | Verdict |
|---|---|---|---|
| 10³ ops | ~0.3 MB | ~0.2 s | replay is fine |
| 10⁴ ops | ~3 MB | ~2 s | replay is fine-ish; snapshot = UX win |
| 10⁵ ops | ~30 MB | ~20 s + fold cost | snapshot clearly wins |
| 10⁶ ops | ~300 MB | minutes; `ReachIndex` blows up first | snapshot (and fold redesign) required |

At walkie's realistic room sizes (10²–10⁴ ops) the *engineering-value* of any proof
machinery is modest today; the value curve steepens with room longevity, and the snapshot
mechanism is also what would eventually allow **history truncation** (dropping cold
events), which changes storage economics entirely. The recommendation in §8 is staged
accordingly.

### 1.5 Gap zero: the state commitment itself

`sync_root` is a **flat hash over the sorted entry-hash set**. It admits no membership
proofs, no subset proofs, no incremental update, and there is no commitment to the
*materialized view* at all. Every option below — even the cheapest — starts by
introducing real commitments:

- **`ops_root`**: a Merkle tree (or MST/prolly tree, §7) over the sorted entry-hash set
  → O(log n) membership/exclusion proofs, incremental insert, canonical across peers
  (history-independent because the key set is sorted).
- **`state_root`**: a canonical serialization of `RoomView` (BTree order is already
  deterministic) hashed as a Merkle tree over (section, key) leaves → joiners can fetch
  partial state, challengers can point at a single wrong leaf.

These two roots are prerequisites shared by all three trust levels, and they are cheap.

---

## 2. Framing: the trust/effort spectrum

Three families, plus a hybrid that the CRDT structure makes unusually attractive:

**(a) Full validity proofs (SNARK/STARK/IVC).** π proves the statement in §1.2. Joiner
trusts only the cryptographic assumptions and the circuit's fidelity to the fold rules.
No honesty assumption about any peer. Heavy prover; who runs it, where, is the whole
question (§5–§6).

**(b) Authenticated data structures (ADS) / commitments only.** Merkle/MST roots,
accumulators, vector commitments, multiset hashes over the op set and the view. These
give *canonical, cross-checkable identity* ("we hold the same set", "this key maps to
this value under root R") and O(log n) reads — but **no fold-correctness**: a root can
commit beautifully to a wrong state. Their real power here: because walkie's fold is
deterministic over a content-addressed DAG, any two honest peers **independently
recompute byte-identical roots** — so a wrong snapshot is *detectable* by anyone with the
history, and *pinpointable* to a leaf. That is what makes (b) + fraud proofs (below)
meaningful.

**(c) Quorum-signed snapshots.** k of n known room members sign `(frontier F, ops_root,
state_root)`. Joiner trusts that ≥1 signer honestly recomputed the fold (an honest signer
refuses to sign a wrong root). This is the deployed industry pattern for exactly this
problem: Ethereum sync committees, Tendermint light clients + Cosmos state-sync,
CT checkpoints + witness cosigning (§3.3). Cheap in every dimension; the price is a
collusion assumption and a **freshness window** (signatures age out as membership rotates).

**(b′) Optimistic snapshots + fraud proofs — the hybrid.** Accept a signed `(F, ops_root,
state_root)` optimistically; any peer holding the history can later publish a compact
**counterexample** (a Merkle-localized wrong leaf plus the ops that decide that key).
Because the fold is deterministic and per-key decisions depend on a small causal
neighborhood, disputes re-execute only the disputed key. Optimistic-rollup security
without any ZK. Notably, **no local-first system ships this today** — Ink & Switch's
sedimentree compaction explicitly trusts strata from remotes, Matrix trusts joined room
state, Braid's antimatter prunes by acknowledgment — so this is both a literature gap and
a tractable design (§3.4).

### Comparison table

| | (c) Quorum-signed | (b) ADS/Merkle only | (b′) Optimistic + fraud proofs | (a) Validity proof (zkVM) | (a′) Validity proof (IVC/PCD) |
|---|---|---|---|---|---|
| Joiner trusts | ≥1 honest signer in quorum, within freshness window | the snapshot author entirely (for correctness) | signer short-term; any honest watcher long-term | crypto + guest program correctness | crypto + circuit correctness |
| Catches wrong fold | yes, if an honest signer recomputes | no | yes, after challenge window | yes, unconditionally | yes, unconditionally |
| Catches invalid sigs in history | yes (signers verified at ingest) | no | yes (challenge exhibits the bad op) | yes (in-guest verify) | depends on design (§5) |
| Wire cost | snapshot + k·(64 B sig) | snapshot + 32 B roots | + challenge msgs (rare) | snapshot + ~0.3–2 KB (Groth16-wrapped) | snapshot + tens of KB |
| Prover cost | native re-fold (ms) | native (ms) | native (ms) | **minutes, server/desktop-class** | per-op folding, potentially client |
| Verify cost (phone) | k ed25519 (µs) | hash paths (µs) | same + optional watch | **ms** | ms–100 ms |
| Maturity | deployed everywhere | deployed everywhere | designed but unshipped in local-first | production for EVM; straightforward for custom folds | research/prototype (§4) |
| Effort for walkie | ~1–2 wk | ~1 wk (prereq for all) | ~3–6 wk | ~1–2 mo prototype | 6 mo+ research |
| ZK-privacy option | n/a | n/a | n/a | yes (zkVMs support it) | yes (KiloNova-style) |

---

## 3. Prior art: the most-analogous systems

### 3.1 Mina — the existence proof for "constant-size proof of an entire history"

Mina maintains a recursive SNARK (Kimchi PLONK-variant + the Pickles induction layer,
over the Pallas/Vesta "Pasta" cycle) in which each block proof verifies the previous
proof plus one transition — so one ~7 KB proof transitively attests the whole chain
([Pickles overview](https://o1-labs.github.io/proof-systems/pickles/overview.html),
[Kimchi](https://minaprotocol.com/blog/kimchi-the-latest-update-to-minas-proof-system)).
The famous "22 KB blockchain" breaks down to ~11 KB: proof ~7,063 B + protocol state
~822 B + verification key ~2,039 B + one account + Merkle path
([technical reference](https://minaprotocol.com/blog/22kb-sized-blockchain-a-technical-reference)).

Two caveats transfer directly to walkie:

1. **The proof lags the tip.** Mina's recursive proof covers the *snarked ledger*, several
   blocks behind; the fresh suffix is covered by ordinary validation. Walkie's analogue:
   prove epochs/checkpoints, sync the suffix as events — which is the framing this report
   already assumes.
2. **Proof ≠ data.** The proof commits to a Merkle root; you still download the ledger
   from someone ([docs](https://docs.minaprotocol.com/glossary)). A verifiable snapshot
   never solves data availability; it authenticates what a peer serves.

Also instructive: proving is heavy and **outsourced to an incentivized SNARK-worker
market**; verification is browser-cheap (OpenMina's
[in-browser Web Node](https://medium.com/openmina/introducing-the-web-node-an-in-browser-mina-node-that-verifies-blocks-and-transfers-funds-ebc59a57e79a)
verifies blocks in a tab). Constant-size proof of unbounded history required a bespoke
proof system, a curve cycle, and a prover economy.

### 3.2 zk-rollups — "prove the fold" as an industrial commodity (for one VM)

Every zk-rollup proves walkie's exact statement shape — "S_new is the correct application
of batch B to S_old" — and aggregates many proofs through **recursion/aggregation trees**
(Polygon zkEVM
[composition/recursion/aggregation](https://docs.polygon.technology/zkEVM/architecture/zkprover/stark-recursion/composition-recursion-aggregation/),
zkSync Boojum's base→leaf→node→scheduler tree
([circuits](https://docs.zksync.io/zksync-protocol/era-vm/circuits)), Scroll
chunk→batch→bundle). 2025–26 milestones: SP1 Hypercube proved 93% of Ethereum blocks in
real time (avg 10.3 s) on ~160 RTX 4090s
([May 2025](https://blog.succinct.xyz/sp1-hypercube/)), then 99.7% under 12 s on **16 RTX
5090s** ([late 2025](https://blog.succinct.xyz/real-time-proving-16-gpus/)); the EF runs
public prover benchmarks at [ethproofs.org](https://ethproofs.org); native-rollup
enshrinement is proposed as [EIP-8079](https://eips.ethereum.org/EIPS/eip-8079). ZK
overhead is commonly quoted under $0.01/tx
([benchmark survey](https://eprint.iacr.org/2024/889.pdf)).

Takeaways: (i) the *pattern* — datacenter provers, universally cheap verifiers — is
proven at planetary scale; (ii) the aggregation trees are **DAG-shaped proof composition
in production** (relevant to §4); (iii) none of it proves on clients.

### 3.3 Quorum/checkpoint patterns without ZK (the deployed cheap tier)

- **Ethereum weak-subjectivity checkpoints**: TOFU a recent finalized root from providers
  you choose; nothing in-protocol detects a lying provider
  ([spec](https://github.com/ethereum/consensus-specs/blob/master/specs/phase0/weak-subjectivity.md)).
- **Ethereum sync committees**: 512 sampled validators BLS-sign each header; light clients
  track committees with Merkle handoffs — a true signed-quorum snapshot at ~25 kB/day
  ([annotated spec](https://github.com/ethereum/annotated-spec/blob/master/altair/sync-protocol.md));
  known weakness: not slashable
  ([issue](https://github.com/ethereum/consensus-specs/issues/3321)).
- **Tendermint light clients + Cosmos state-sync**: accept a distant header if >1/3 of a
  *trusted* validator set signed, within a **trusting period**; then download state-chunks
  and check the restored hash against the header's `AppHash` — the deployed
  "snapshot-instead-of-replay," days → minutes
  ([verification spec](https://github.com/tendermint/tendermint/blob/v0.34.x/spec/light-client/verification/verification_001_published.md),
  [state sync](https://blog.cosmos.network/cosmos-sdk-state-sync-guide-99e4cf43be2f)).
- **Certificate Transparency checkpoints + witnesses**: log signs Merkle heads; independent
  witnesses countersign **only after verifying an append-only consistency proof** against
  the last head they signed — equivocation becomes self-incriminating signed evidence
  ([tlog-checkpoint](https://github.com/C2SP/C2SP/blob/main/tlog-checkpoint.md),
  [tlog-witness](https://github.com/C2SP/C2SP/blob/main/tlog-witness.md)).
- **Bitcoin assumeutxo**: load a UTXO snapshot whose hash is hardcoded via social
  consensus, start at the tip, and **revalidate from genesis in the background**, erasing
  the trust ([doc](https://github.com/bitcoin/bitcoin/blob/master/doc/assumeutxo.md)).
  The "optimistic accept + lazy self-verification" move is directly stealable: a
  walkie joiner can accept a signed snapshot instantly and let ordinary anti-entropy
  backfill the DAG behind it, re-folding and cross-checking `state_root` when done.

### 3.4 Verifiable/Byzantine CRDT literature

- Kleppmann, *Making CRDTs Byzantine Fault Tolerant*
  ([PaPoC 2022](https://martin.kleppmann.com/papers/bft-crdt-papoc22.pdf)): hash-DAG
  causality + decide-from-causal-past = Sybil-immune eventual consistency. Walkie already
  implements essentially this. The paper **does not address joining from a snapshot** —
  it assumes full-history exchange.
- **Proof-Carrying CRDTs allow Succinct Non-Interactive Byzantine Update Validation**
  (Marx, Jacob, Hartenstein; [PaPoC 2025](https://dl.acm.org/doi/10.1145/3721473.3722142);
  [code](https://github.com/kit-dsn/proof-carrying-crdts)) — **the direct prior art**:
  PCD over a hash-DAG CRDT so a peer validates state without holding the causal history,
  built on Mina's o1js/Pickles with **proof merging at DAG convergence points** (counter
  and hash-DAG CRDTs, arity-2 recursion). Workshop-prototype maturity, seconds-per-merge
  costs — but it validates the exact architecture (§4) and is the closest running code.
- **Ink & Switch Keyhive / sedimentree**
  ([notebook](https://www.inkandswitch.com/keyhive/notebook/)): capability chains say *who
  may write*; sedimentree compacts commit ranges into strata with hash-determined
  boundaries — and its docs are **silent on verifying that a stratum faithfully
  summarizes what it replaced**. The state of the art in local-first compaction currently
  punts on walkie's exact question.
- Matrix state resolution (deterministic fold over a signed auth-DAG, servers trusting
  joined state — the "state reset" soft spot), Braid antimatter (prune-by-acknowledgment),
  Hypercore signed tree roots and **atproto's signed commit over a Merkle Search Tree**
  ([repo spec](https://atproto.com/specs/repository)) — the single-signer snapshot
  pattern in production.
- Academic blueprint for "one party executes, everyone verifies": Setty et al.,
  *Verifiable state machines* / Spice ([2020/758](https://eprint.iacr.org/2020/758)),
  Piperine ([2020/195](https://eprint.iacr.org/2020/195)).

**No shipped system does verifiable CRDT snapshots.** The intersection is open as of
mid-2026; the PaPoC 2025 paper is the only running artifact, and it is not
snapshot-oriented (it proves update-validity, not fold-to-state).

---

## 4. Incremental and recursive proving — can proofs merge the way states merge?

This is the crux: the DAG grows continuously and merges concurrent branches, so
whole-history re-proving is a non-starter; the proof system must support **incremental
extension** (IVC) *and* **merge-shaped composition** (PCD).

### 4.1 The theory answer: yes — merge-shaped proving is exactly PCD

Proof-carrying data (Chiesa–Tromer 2010; accumulation-scheme constructions
[BCMS 2020/499](https://eprint.iacr.org/2020/499),
[BCLMS 2020/1618](https://eprint.iacr.org/2020/1618)) generalizes IVC from chains to
**arbitrary DAGs**: a step may consume *several* incoming (state, proof) edges — a merge
node is not an extension of the theory, it *is* the theory. The question is which
concrete folding instantiations support **accumulator ⊗ accumulator** folding (merging
two running instances, not just absorbing a fresh step) and at what cost.

### 4.2 Folding schemes 2025–26, and which can merge

| Scheme | Merge (acc⊗acc) support | Notes |
|---|---|---|
| [Nova](https://eprint.iacr.org/2021/370) (+[CycleFold](https://eprint.iacr.org/2023/1192)) | algebraically native (a running accumulator *is* a relaxed R1CS instance) but the IVC bookkeeping/soundness for trees is not in the paper | ~2 MSMs/step prover; recursion overhead ~10⁴ constraints, CycleFold shrinks the second-curve circuit to a few scalar-mults |
| Paranova ([zkresear.ch](https://zkresear.ch/t/parallelizing-nova-visualizations-and-mental-models-behind-paranova/198)) | explicit binary-tree nodes, 4-to-1 folding — merges two running instances | experimental PSE PR, never productionized |
| [HyperNova](https://eprint.iacr.org/2023/573) | multi-folding (μ running, ν fresh) defined; headline is 1+1 | 1 MSM/step |
| [PCD from multi-folding](https://eprint.iacr.org/2023/1282) | **r-ary DAG merge spelled out**: 1 MSM prover per node, recursion overhead 1 MSM of size 2r−1 | paper + reference numbers (~49 s/node at their params) |
| [ProtoStar](https://eprint.iacr.org/2023/620) / [ProtoGalaxy](https://eprint.iacr.org/2023/1106) | **ProtoGalaxy folds k instances AND k accumulators** | Aztec chose this family (linear call chains, no DAG merges yet) |
| [Mangrove](https://eprint.iacr.org/2024/416) (CRYPTO 24) | **k-ary tree PCD**, with the extraction-soundness analysis that *favors* trees over deep chains | est. 2 min / 2²⁴ gates at ~390 MB on a laptop; no production code |
| [KiloNova](https://eprint.iacr.org/2023/1579) | non-uniform PCD **with ZK** between parties | relevant only if E2EE-privacy is ever wanted |
| [NeutronNova](https://eprint.iacr.org/2024/1606) | folds n instances in log n sum-check rounds | engine for wide merges; PCD layer not built |
| [LatticeFold+](https://eprint.iacr.org/2025/247) / [Neo](https://eprint.iacr.org/2025/294) / [Arc](https://eprint.iacr.org/2024/1731) | PQ / hash-based accumulation lines; Arc explicitly targets unbounded-depth PCD | the post-quantum route, earlier-stage |

Implementations: [microsoft/Nova](https://github.com/microsoft/Nova),
[arecibo](https://github.com/argumentcomputer/arecibo) (SuperNova+CycleFold),
[Sonobe](https://github.com/privacy-scaling-explorations/sonobe) (Nova/HyperNova/
ProtoGalaxy, experimental, unaudited) — **none exposes accumulator-merge as an API**.
Production DAG-shaped composition exists only as *full recursion* (rollup aggregation
trees; Mina Pickles at arity 2): each merge verifies whole proofs in-circuit
(~10⁵–10⁶ constraints, seconds), roughly 1–2 orders of magnitude costlier per merge than
a folding merge (~10⁴ constraints, a few scalar-mults) — but battle-tested.

Contrast: *distributed provers of one statement* (DIZK, Pianist, deVirgo,
[2025/1653](https://eprint.iacr.org/2025/1653.pdf)) need coordinated challenges among
provers — the wrong model for asynchronous CRDT peers. PCD's "pass (state, proof) along
DAG edges, no coordination" is precisely the CRDT-shaped model.

### 4.3 Is walkie's fold actually PCD-shaped? (walkie-specific analysis)

Mostly yes — with one structural caveat and one classic pitfall.

**What the folded "state" must be.** Not `RoomView` itself: the mergeable object is a
**per-key causal summary**, because merge decisions need causal metadata:

- **Pieces / seq registers**: per (owner, piece): greatest-seq lifecycle + greatest-seq
  move. Merges by `max` — a true anonymous semilattice join, trivially PCD-friendly.
- **Add-wins pitches**: per degree-key: the set of **live add entry-hashes**. Merge rule
  for branches A, B over shared prefix P:
  `live(A∪B) = (liveA ∩ liveB restricted to P) ∪ (liveA \ P) ∪ (liveB \ P)` — a remove in
  A can kill a prefix add (A observed it) but never a B-exclusive add (concurrent ⇒
  survives, that's add-wins). So the merge is computable *from the two summaries plus
  membership-in-prefix*, without re-walking history.
- **Registers**: candidates = causal maxima. `maxima(A∪B) ⊆ maximaA ∪ maximaB`, and
  cross-branch supersession can only strike prefix elements — same shape: mergeable given
  summaries + prefix membership.

So the **join really does compose over merges**, provided each branch's proof carries
(commitment to its entry set, per-key summaries). The circuit's merge work is proportional
to *summary size* (≤ pitches × live adds + register candidates), not history size. This is
the encouraging, novel-ish result of this analysis: **walkie's alphabet was already shaped
(content-keyed sets, seq registers, causal-maxima registers) so that its fold summaries
merge with local rules** — the CRDT is PCD-shaped.

**The pitfall: shared-prefix double counting.** Two branch accumulators share a causal
prefix; naive folding proves prefix ops twice (sound but wasteful) and would double-count
any additive state. Two clean fixes from the literature, matching CRDT practice:
(i) **delta proofs** — each merge consumes each branch's accumulator *since the fork*,
so every op folds exactly once (this is delta-state CRDT sync, reborn); (ii) represent
"ops applied" as a **subtractable homomorphic multiset hash** (ECMH
[arXiv 1601.06502](https://arxiv.org/abs/1601.06502), LtHash
[2019/227](https://eprint.iacr.org/2019/227.pdf), MSet-Mu-Hash as used by Spice
[2018/907](https://eprint.iacr.org/2018/907)) so the merge subtracts the prefix:
`h(A∪B) = h(A) + h(B) − h(P)`. Because the fold is order-independent, the per-op circuit
needs no ordering constraints at all — "op valid given predecessors; add to multiset
accumulator" — which is *cheaper* than generic PCD, per the offline-memory-checking line
(Spice; Nebula [2024/1605](https://eprint.iacr.org/2024/1605); Twist & Shout
[2025/105](https://eprint.iacr.org/2025/105)).

**The caveat: "prefix membership" needs an authenticated set.** The merge rules above
query "is this add/candidate in the other branch's history?" — so branch commitments must
support membership proofs (Merkle/MST over entry hashes — the same `ops_root` from §1.5).

**Maturity verdict for §4:** theory solved (PCD + multi-accumulator folding), costs
attractive on paper (merge ≈ one extra folding step), running code limited to one
workshop prototype on Mina's stack (PaPoC 2025) and experimental branches (Paranova).
Nobody ships an audited accumulator-merge API in 2026. Building this for walkie is
research-grade work — publishable, not schedulable.

---

## 5. The Ed25519 problem

Every op is Ed25519-signed over CBOR header bytes (SHA-512 inside Ed25519) and
blake3-addressed. All three primitives are SNARK-hostile; this is usually the dominant
cost of any "prove the fold" design, and it decides which architectures are sane.

### 5.1 Options and costs

| Option | Cost per signature | Migration | Verdict |
|---|---|---|---|
| **In-circuit, non-native** (circom/halo2/Noir) | **~2.56 M R1CS constraints** ([ed25519-circom](https://github.com/Electron-Labs/ed25519-circom), incl. SHA-512; ~99 sigs max per Groth16 proof at the largest trusted setups); **~1.74 M halo2 cells** ([Axiom bounty](https://shuklaayu.sh/blog/axiom-ed25519/)); **~228 k Barretenberg gates** in Noir, whose own README says *"If you can use another signature algorithm you should"* ([noir-ed25519](https://github.com/willemolding/noir-ed25519)) | none | **avoid** — every implementation in this family is unaudited, with published soundness bugs ([ed25519-circom exploit](https://gist.github.com/uvicorn/6758e70dbcd01bbc2ef1ffa959c7196d), [circom-pairing bug](https://medium.com/veridise/circom-pairing-a-million-dollar-zk-bug-caught-early-c5624b278f25)); nobody voluntarily does non-native Ed25519 in hand-written circuits anymore |
| **zkVM precompile** (SP1 / RISC Zero patched `ed25519-dalek`) | **~134 k RISC-V cycles per verification, SHA-512 included** (SP1 CI: 13.35 M cycles / 100 verifies, [sp1#2929](https://github.com/succinctlabs/sp1/pull/2929#issuecomment-5200854579)); RISC Zero ships the same accelerator ([precompiles](https://dev.risczero.com/api/zkvm/precompiles)); unpatched dalek costs single-digit *millions* of cycles — the precompile is a ~25–40× buy | **zero** — the guest verifies the actual p2panda bytes with the actual crate | **the pragmatic default.** 10⁴ ops ≈ 1.3 G cycles + parsing: minutes of desktop/GPU proving, out of reach of browsers, trivial for a delegated prover |
| **SNARK-friendly signatures** (EdDSA-Poseidon/Baby Jubjub: **4,218 constraints** ([Heimdall, Table 1](https://arxiv.org/pdf/2301.00823)); Mina-style Schnorr over the native curve: hundreds of rows) | ~600× cheaper in-circuit than Ed25519 | **forks p2panda**: new keys, new wire format, nonstandard crypto (no RFC), and blake3 ids would push toward Poseidon too (a deeper fork) | only rational if ZK-provability becomes the product core; Mina/Aleo could do this because they control key issuance end-to-end |
| **Signatures outside the proof** (prove fold over hash-committed ops; verifier batch-verifies sigs natively) | verifier pays O(N): 64·N wire bytes + ~25 µs·N with `verify_batch` (~2×, mind [ZIP-215/cofactor semantics](https://docs.rs/ed25519-dalek/latest/ed25519_dalek/fn.verify_batch.html)); [half-aggregation](https://eprint.iacr.org/2021/350.pdf) halves bytes to (N+1)·32 but not verify time | none | **breaks the point of the snapshot** as a full replacement (verifier work is O(history) again) — but see §5.2 for where it is still useful |
| **BLS re-signing** (aggregate: one 96 B sig + one pairing for N signers) | native ms-per-pairing; in-circuit **19.2 M constraints** ([circom-pairing](https://github.com/yi-sun/circom-pairing)); zkVMs ship blst precompiles | new keys | wrong tool here; noted for completeness |

Hashing context: SHA-256 costs ~410 constraints/byte, BLAKE2s ~509/byte (closest proxy
for blake3's ARX core), vs Poseidon2 at ~8/byte
([hash-circuits](https://github.com/bkomuves/hash-circuits),
[Poseidon](https://eprint.iacr.org/2019/458.pdf)) — the ~60–200× gap is the entire
"SNARK-friendly" story, and it applies to walkie's blake3 op ids and entry hashes just as
much as to the signatures. In a zkVM these are merely thousands of cycles per chunk.

Production precedent all points the same way: zkLogin keeps the legacy signature
in-circuit but delegates its ~1 M-constraint Groth16 proof to a proving service (~3 s,
[arXiv 2401.11735](https://arxiv.org/html/2401.11735v2)); zkEmail precomputes partial
SHA outside the circuit ([docs](https://docs.zk.email/architecture/dkim-verification));
Electron Labs batched ~99 Ed25519 sigs per proof and fought for years; Mina avoided the
problem by co-designing the signature scheme with the proof system
([Mina signature spec](https://github.com/MinaProtocol/mina/blob/master/docs/specs/signatures/description.md)).

### 5.2 What this means for walkie

1. **The validity-proof route is zkVM-with-precompiles, full stop.** ~134 k cycles/sig ×
   10⁴ ops is a delegated-prover workload measured in minutes, with zero migration —
   the guest runs `verify_signed_op` as-is. Hand-rolled circuits are dominated 20× on
   cost and infinitely on audit risk; scheme migration is a disproportionate ecosystem
   break.
2. **"Signatures outside" has a legitimate niche**: in Stage 1 (§8), the quorum signers
   *are* the signature verifiers — they verified every op at ingest, which is exactly the
   trust the quorum signature conveys. A validity proof of *fold-correctness only* over a
   quorum-authenticated `ops_root` would already remove the "honest signer computed the
   fold correctly" assumption while leaving authenticity on the quorum — a meaningful
   intermediate rung that avoids all in-proof signature cost.
3. **Authorship is load-bearing in the fold** (owner-gating of pieces, `pitch_authors`,
   per-author seq registers), so any proof that omits signature validity lets a malicious
   prover misattribute ops; misattribution is bounded by (2)'s quorum or caught by
   backfill, but a *self-contained* proof for fully-untrusted parties must include the
   signatures — hence (1).

---

## 6. Browser / wasm prover feasibility (the gating question)

Summary of 2025–26 client-side proving reality, anchored on PSE's
[zkID client-side proving comparison](https://pse.dev/blog/efficient-client-side-proving-for-zkid)
(Jun 2025), the [FibRace field study](https://arxiv.org/abs/2510.14693) (2.19 M real
phone proofs, Oct 2025), the
[Hyli in-browser p256 benchmark](https://hyli.ghost.io/benchmarking-in-browser-p256-ecdsa-proving-systems/)
(Mar 2025), and [ethproofs.org/csp-benchmarks](https://ethproofs.org/csp-benchmarks).

| Stack | In-browser prover? | Prover numbers | Proof size / verify | Verdict for walkie |
|---|---|---|---|---|
| **Noir + Barretenberg** (UltraHonk / ClientIVC) | **yes — the only production-grade one** | Aztec tx: ~2.5 s native / **~6.8 s browser, 1.3 GB peak** ([Jun 2026](https://aztec.network/blog/client-side-proof-generation), [memory work](https://aztec.network/blog/testnet-retro---2-0-3-network-upgrade)); p256 ECDSA ~2 s multithreaded ([Hyli](https://hyli.ghost.io/benchmarking-in-browser-p256-ecdsa-proving-systems/)); needs SharedArrayBuffer (COOP/COEP), ~4× slower single-threaded; iOS Safari tab-RAM caps cause OOMs | ~20 KiB proof; ms off-chain verify (heavy only on-chain) | the realistic browser-prover if a custom circuit path is ever taken |
| **Halo2 (PSE)** | yes, but capped ~K=15 by wasm32 4 GB ([guide](https://zcash.github.io/halo2/user/wasm-port.html)); browser path bit-rotting (2025 build failures) | zk-email ~15 s in-browser (~1 M constraints) | KB-scale proofs, ms verify | legacy; not recommended for new work |
| **Plonky2 / Plonky3** | no credible browser prover; speed needs AVX/NEON | 20 s / 2.4 GB laptop; **OOM on phones** ([PSE zkID](https://pse.dev/blog/efficient-client-side-proving-for-zkid)) | 43–175 KB, ~ms verify | no |
| **RISC Zero** (zkVM) | **verify-only** in wasm ([example](https://github.com/risc0/risc0/tree/main/examples/browser-verify)); prover needs ≥10 GB + GPU | Ethereum block: 44 s on R0VM 2.0 datacenter ([blog](https://risczero.com/blog/introducing-R0VM-2.0)) | STARK ~200 kB → **Groth16 wrap ~200–256 B, ms verify** | delegated proving + tiny proof: strong fit for Stage 2 |
| **SP1** (zkVM) | no (16 cores/16 GB+; GPU); **wasm verifier shipped** ([example](https://github.com/succinctlabs/example-sp1-wasm-verifier)) | real-time Ethereum blocks on 16×5090 ([Hypercube](https://blog.succinct.xyz/sp1-hypercube/)) | Groth16 ~260 B / PLONK ~868 B, ms verify | same profile as RISC Zero |
| **Jolt** (a16z) | not yet (alpha, no wasm target) — but **streaming prover < 2 GB RAM**, ZK at +3 KB, explicitly aiming at phones ([64-bit](https://a16zcrypto.com/posts/article/64-bit-proving-jolt/), [ZK](https://a16zcrypto.com/posts/article/zkvm-jolt-zero-knowledge/)) | >500 k RISC-V cycles/s on a MacBook | ~50 KB, no on-chain wrap yet | the credible *future* client-side zkVM — watch list |
| **Stwo / Cairo M** (circle STARKs) | wasm-SIMD backend; WebGPU ~2× e2e ([zkSecurity](https://blog.zksecurity.xyz/posts/webgpu/)); **field-proven on phones**: 2.19 M proofs, most modern phones < 5 s, ≥3 GB RAM ([FibRace](https://arxiv.org/abs/2510.14693)) | Starknet-mainnet prover since Nov 2025 | ~92 KiB, 2.5 ms verify | proven mobile CSP — but Cairo toolchain, not Rust |
| **Binius / Binius64** | no official wasm; **ultra-low memory**: 1.85 s / 27 MB laptop, ~5 s / ~45 MB on phones ([PSE zkID](https://pse.dev/blog/efficient-client-side-proving-for-zkid)) | Keccak 142 ms native | 300–475 KB proofs, non-succinct verify | interesting memory profile; ecosystem pivoted Sep 2025 |
| **Ligero/Ligetron** | **wasm-native by design**, billions of gates in a tab at ~hundreds of MB ([S&P 24](https://ieeexplore.ieee.org/document/10646776/)); WebGPU-required OSS ([repo](https://github.com/ligeroinc/ligero-prover)) | 12 s laptop / 30–94 s phones (SHA-256/2 kB) | **sqrt(n), up to ~3.5 MB**, slow verify | wrong proof-size profile for tiny-snapshot wire |
| **Mopro** (mobile wrapper) | native mobile beats browser wasm 3–14× (Keccak: iPhone 630 ms native vs 1.7–5.2 s browser) ([perf](https://zkmopro.org/docs/performance/)) | — | — | if walkie's Tauri/native builds ever prove, go native-mobile, not wasm |

**Reading for walkie:**

1. **The recipient side is a solved problem.** Groth16-wrapped zkVM proofs are ~260 bytes
   and verify in milliseconds in wasm — cheaper than verifying a handful of Ed25519
   signatures. Snapshot + proof would be *smaller and faster to check* than any event
   stream. This is the part of the dream that is real today.
2. **The prover side is not browser-shaped for zkVMs.** Nothing that can run walkie's
   actual Rust fold (SP1/RISC Zero) proves in a tab or on a phone; the model is
   delegation (a desktop peer, a room host, or Boundless/Succinct-style networks).
3. A **custom circuit** (Noir) could prove in-browser at ~seconds for small statements —
   but per §1.3/§5 the statement (many Ed25519 + blake3 + CBOR + reachability) is exactly
   the expensive kind, and hand-maintaining that circuit against evolving CRDT logic is
   the biggest schedule risk of all (§8).
4. WebGPU proving is still research-grade (best e2e ~2×; WGSL 32-bit limits); wasm
   multithreading (SharedArrayBuffer → COOP/COEP headers) matters for Trunk/hosting
   config if browser proving is ever attempted.

---

## 7. What to borrow, per layer

1. **Commitments (do regardless):** MST/Merkle `ops_root` (canonical, history-independent
   — Merkle Search Trees per [Auvolat–Taïani](https://inria.hal.science/hal-02303490),
   as deployed in atproto) + canonical `state_root` over `RoomView`. Optionally an
   **ECMH/LtHash multiset digest** of entry hashes as an O(1)-incremental convergence
   check alongside RBSR.
2. **Snapshot messages:** `(frontier F, ops_root, state_root, author sig)` — Hypercore/
   atproto single-signer pattern; store strict-deferral learns to accept F as satisfied
   prevs for post-snapshot ops.
3. **Trust hardening:** assumeutxo-style **optimistic accept + background backfill/
   re-fold**; then **k-of-n co-signing** with CT-style witness rules (sign only if
   consistent with the last snapshot you signed) and a freshness window (Tendermint
   trusting-period analogue); then **fraud proofs** localized by `state_root` leaves.
4. **Validity proofs:** zkVM guest = the existing `verify_signed_op` + `RoomStore` fold
   compiled to RISC-V; delegated proving; Groth16-wrapped proof in the snapshot message.
   Mina for the recursive-checkpoint pattern; PaPoC 2025 Proof-Carrying CRDTs for the
   merge-shaped design; Mangrove/ProtoGalaxy when/if folding-PCD matures into libraries.

---

## 8. Recommendation for walkie — staged verdict

**Feasibility verdict, stated plainly:** full client-side ZK snapshot *proving* is **not
practical in 2026** for this system — the only browser-capable prover stack (Noir) would
require a hand-built circuit dominated by Ed25519/blake3/CBOR costs and locked in
lockstep with living CRDT semantics, and the stacks that could prove the *actual Rust
fold* (SP1/RISC Zero) are datacenter/desktop provers. What **is** practical: cheap
verifiable-snapshot machinery now (commitments + signatures + optimistic verification),
and a **delegated** validity-proof path whose *verifier* runs beautifully in wasm. ZK
privacy is not needed for the current threat model.

### Stage 0 — near-term cheap win (~1–2 weeks): commitments + signed snapshots + lazy self-verification

- Add `ops_root` (Merkle/MST over sorted entry hashes) beside `sync_root`; add canonical
  `state_root` over `RoomView`.
- Snapshot message `(F, ops_root, state_root, sig_author)`; joiner boots from it
  instantly, then **backfills the DAG via existing anti-entropy in the background,
  re-folds, and cross-checks** — assumeutxo-style trust erasure using machinery that
  already exists and was just hardened.
- Guarantee: instant cold start; wrong snapshots detected as soon as backfill completes;
  zero new cryptography.
- Risk: near zero. This also builds the substrate every later stage needs.

### Stage 1 — medium term (~3–6 weeks): quorum co-signing, witnesses, fraud proofs

- k-of-n co-signatures from room members (the membership set already exists as author
  keys); witnesses countersign only consistently with their previous snapshot; freshness
  window on signature validity.
- Challenge protocol: any peer with history publishes a `state_root`-leaf counterexample
  + the deciding ops; peers drop/flag the snapshot and its signers.
- Guarantee: correctness unless threshold collusion within the window, with signed
  evidence of cheating. This is the deployed-industry sweet spot (Cosmos/CT-grade) and —
  notably — **beyond what any local-first system ships today**.
- Realistic "prover" time: milliseconds (it is just the native fold). Verify: microseconds.

### Stage 2 — research-grade (~2 months to prototype, longer to productionize): delegated zkVM validity proofs

- SP1 or RISC Zero guest program that ingests the framed signed ops, runs
  **the existing `verify_signed_op` + `RoomStore::view()` code** (ed25519-dalek and blake3
  patched precompiles), and commits `(ops_root, state_root)` — no bespoke circuit, no
  circuit/CRDT drift: **the guest is the Rust fold**, pinned by the same golden-vector
  tests.
- Proving is delegated: a desktop peer, a room-host box, or a proving network; expect
  minutes per checkpoint at 10⁴–10⁵ ops on serious hardware, re-proved per epoch (proofs
  lag the tip, Mina-style; the suffix rides ordinary event sync).
- Recipient verifies a ~260 B Groth16-wrapped proof in milliseconds in wasm.
- Guarantee: unconditional fold-correctness + signature validity — trust removed from
  signers entirely, at the cost of prover logistics.

### Stage 3 — the research frontier (publishable, not schedulable): merge-shaped PCD

- Folding-based PCD whose accumulators merge like the CRDT's join (§4.3): per-key
  summaries + multiset-hash op accumulation + delta-proofs over merges. Every ingredient
  is published (ProtoGalaxy k-accumulator folding, Mangrove tree-PCD, Nebula-style memory
  in folding, PaPoC 25 Proof-Carrying CRDTs), none is shipped as an audited library.
- Watch list that could change the calculus: **Jolt's <2 GB streaming prover** reaching a
  wasm/mobile target (client-side zkVM proving), Sonobe or successor exposing
  accumulator-merge, Aztec ClientIVC growing DAG merges.

### The three biggest risks

1. **Prover locality.** Every validity-proof path makes proving a server/desktop concern;
   a pure-p2p room of phones has nobody to prove. Mitigation: Stage 1 is the floor, and
   proofs are an *upgrade* whenever a capable peer is present — never a requirement.
2. **Ed25519/hash-in-circuit cost** (§5) forecloses the bespoke-circuit shortcut and makes
   zkVM-with-precompiles the only sane validity route; a signature-scheme migration for
   SNARK-friendliness would be a disproportionate ecosystem break (p2panda compatibility).
3. **Semantics lockstep.** The fold rules are alive (v3 alphabet, evolution discipline in
   `ops.rs`). Any hand-written circuit becomes a second implementation of CRDT semantics
   that must never diverge — the strongest argument for the zkVM route (one
   implementation) and for gating every stage on the existing parity/golden-vector tests.

---

## 9. Sources

Grouped, deduplicated key sources (all linked inline above):

- **Folding/IVC/PCD:** Nova [2021/370](https://eprint.iacr.org/2021/370) · CycleFold
  [2023/1192](https://eprint.iacr.org/2023/1192) · HyperNova
  [2023/573](https://eprint.iacr.org/2023/573) · ProtoStar
  [2023/620](https://eprint.iacr.org/2023/620) · ProtoGalaxy
  [2023/1106](https://eprint.iacr.org/2023/1106) · PCD-from-multi-folding
  [2023/1282](https://eprint.iacr.org/2023/1282) · KiloNova
  [2023/1579](https://eprint.iacr.org/2023/1579) · Mangrove
  [2024/416](https://eprint.iacr.org/2024/416) · NeutronNova
  [2024/1606](https://eprint.iacr.org/2024/1606) · Nebula
  [2024/1605](https://eprint.iacr.org/2024/1605) · Arc
  [2024/1731](https://eprint.iacr.org/2024/1731) · LatticeFold+
  [2025/247](https://eprint.iacr.org/2025/247) · Neo
  [2025/294](https://eprint.iacr.org/2025/294) · PCD foundations
  [2020/499](https://eprint.iacr.org/2020/499),
  [2020/1618](https://eprint.iacr.org/2020/1618) · Paranova
  [zkresear.ch](https://zkresear.ch/t/parallelizing-nova-visualizations-and-mental-models-behind-paranova/198)
  · [Sonobe](https://github.com/privacy-scaling-explorations/sonobe) ·
  [arecibo](https://github.com/argumentcomputer/arecibo) ·
  [awesome-folding](https://github.com/lurk-lab/awesome-folding)
- **CRDT/BFT prior art:** Kleppmann BFT-CRDTs
  [PaPoC 22](https://martin.kleppmann.com/papers/bft-crdt-papoc22.pdf) ·
  **Proof-Carrying CRDTs** [PaPoC 25](https://dl.acm.org/doi/10.1145/3721473.3722142) +
  [code](https://github.com/kit-dsn/proof-carrying-crdts) · BEC
  [arXiv 2012.00472](https://arxiv.org/abs/2012.00472) · Keyhive/sedimentree
  [Ink & Switch](https://www.inkandswitch.com/keyhive/notebook/) · Matrix
  [state res v2](https://matrix.org/docs/older/stateres-v2/) · antimatter
  [braid.org](https://braid.org/antimatter) · MST
  [SRDS 19](https://inria.hal.science/hal-02303490) · atproto
  [repo spec](https://atproto.com/specs/repository) · Hypercore
  [DEP-0002](https://www.datprotocol.com/deps/0002-hypercore/) · Spice
  [2018/907](https://eprint.iacr.org/2018/907) /
  [verifiable state machines](https://eprint.iacr.org/2020/758) · Piperine
  [2020/195](https://eprint.iacr.org/2020/195)
- **Snapshot/checkpoint patterns:** Mina
  [22KB reference](https://minaprotocol.com/blog/22kb-sized-blockchain-a-technical-reference)
  / [Pickles](https://o1-labs.github.io/proof-systems/pickles/overview.html) · Ethereum
  [weak subjectivity](https://github.com/ethereum/consensus-specs/blob/master/specs/phase0/weak-subjectivity.md)
  / [sync protocol](https://github.com/ethereum/annotated-spec/blob/master/altair/sync-protocol.md)
  · Tendermint
  [light client](https://github.com/tendermint/tendermint/blob/v0.34.x/spec/light-client/verification/verification_001_published.md)
  / [Cosmos state sync](https://blog.cosmos.network/cosmos-sdk-state-sync-guide-99e4cf43be2f)
  · CT [tlog-checkpoint](https://github.com/C2SP/C2SP/blob/main/tlog-checkpoint.md) /
  [tlog-witness](https://github.com/C2SP/C2SP/blob/main/tlog-witness.md) ·
  [assumeutxo](https://github.com/bitcoin/bitcoin/blob/master/doc/assumeutxo.md) ·
  BBF accumulators [2018/1188](https://eprint.iacr.org/2018/1188) · Utreexo
  [2019/611](https://eprint.iacr.org/2019/611) · LtHash
  [2019/227](https://eprint.iacr.org/2019/227.pdf) · ECMH
  [1601.06502](https://arxiv.org/abs/1601.06502)
- **Rollups / real-time proving:** SP1 Hypercube
  [announcement](https://blog.succinct.xyz/sp1-hypercube/) /
  [16 GPUs](https://blog.succinct.xyz/real-time-proving-16-gpus/) ·
  [ethproofs.org](https://ethproofs.org) · Polygon zkEVM
  [recursion docs](https://docs.polygon.technology/zkEVM/architecture/zkprover/stark-recursion/composition-recursion-aggregation/)
  · zkSync [Boojum circuits](https://docs.zksync.io/zksync-protocol/era-vm/circuits) ·
  [EIP-8079](https://eips.ethereum.org/EIPS/eip-8079) · rollup benchmark
  [2024/889](https://eprint.iacr.org/2024/889.pdf)
- **Ed25519 / signatures in ZK:** ed25519-circom
  [repo](https://github.com/Electron-Labs/ed25519-circom) · halo2 Ed25519
  [write-up](https://shuklaayu.sh/blog/axiom-ed25519/) · noir-ed25519
  [repo](https://github.com/willemolding/noir-ed25519) · SP1 precompiles
  [docs](https://docs.succinct.xyz/docs/sp1/optimizing-programs/precompiles) / cycle data
  [sp1#2929](https://github.com/succinctlabs/sp1/pull/2929#issuecomment-5200854579) ·
  RISC Zero [precompiles](https://dev.risczero.com/api/zkvm/precompiles) · Heimdall
  constraint table [arXiv 2301.00823](https://arxiv.org/pdf/2301.00823) · Poseidon
  [2019/458](https://eprint.iacr.org/2019/458.pdf) · hash constraint counts
  [hash-circuits](https://github.com/bkomuves/hash-circuits) · half-aggregation
  [2021/350](https://eprint.iacr.org/2021/350.pdf) · batch verify
  [verify_batch](https://docs.rs/ed25519-dalek/latest/ed25519_dalek/fn.verify_batch.html)
  · circom-pairing [repo](https://github.com/yi-sun/circom-pairing) /
  [Veridise bug](https://medium.com/veridise/circom-pairing-a-million-dollar-zk-bug-caught-early-c5624b278f25)
  · zkLogin [arXiv 2401.11735](https://arxiv.org/html/2401.11735v2) · zkEmail
  [DKIM docs](https://docs.zk.email/architecture/dkim-verification) · Mina signatures
  [spec](https://github.com/MinaProtocol/mina/blob/master/docs/specs/signatures/description.md)
- **Client-side proving:** PSE zkID comparison
  [pse.dev](https://pse.dev/blog/efficient-client-side-proving-for-zkid) · FibRace
  [arXiv 2510.14693](https://arxiv.org/abs/2510.14693) · Hyli p256
  [benchmark](https://hyli.ghost.io/benchmarking-in-browser-p256-ecdsa-proving-systems/) ·
  Aztec [client-side proving](https://aztec.network/blog/client-side-proof-generation) /
  [memory](https://aztec.network/blog/testnet-retro---2-0-3-network-upgrade) · halo2
  [wasm guide](https://zcash.github.io/halo2/user/wasm-port.html) · RISC Zero
  [browser-verify](https://github.com/risc0/risc0/tree/main/examples/browser-verify) /
  [R0VM 2.0](https://risczero.com/blog/introducing-R0VM-2.0) · SP1
  [wasm verifier](https://github.com/succinctlabs/example-sp1-wasm-verifier) · Jolt
  [64-bit](https://a16zcrypto.com/posts/article/64-bit-proving-jolt/) /
  [ZK](https://a16zcrypto.com/posts/article/zkvm-jolt-zero-knowledge/) · Stwo
  [mainnet](https://www.starknet.io/blog/s-two-is-live-on-starknet-mainnet-the-fastest-prover-for-a-more-private-future/)
  / [WebGPU](https://blog.zksecurity.xyz/posts/webgpu/) · Ligetron
  [S&P 24](https://ieeexplore.ieee.org/document/10646776/) /
  [repo](https://github.com/ligeroinc/ligero-prover) · Mopro
  [perf](https://zkmopro.org/docs/performance/) ·
  [csp-benchmarks](https://ethproofs.org/csp-benchmarks)
