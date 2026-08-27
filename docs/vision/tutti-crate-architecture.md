# tutti as crates: factoring walkie-songie's substrate into a reusable stack

**Status:** design + extraction plan, 2026-08-07. Companion to
`docs/vision/eventually-consistent-pitchsets.md` (the paradigm; its §6 contract
is what these crates realize) and the five research designs it leans on
(`docs/research/{ui-state-coupling-design, reactive-rollback-api-design,
reactive-effectful-ui-adapter-design, reconciliation-tree-fit,
radix-immutable-merkle-audit, zk-provable-dag-snapshots,
peer-discovery-design}.md`). Grounded in walkie-songie at HEAD, `hhhs-rs @
bd23d4e` (the pinned rev, `Cargo.toml:103-104`), and
`/laboratory/radix_immutable` v0.2.0 (Merkle layer implemented:
`src/{merkle,proof}.rs`). No code changed.

The question under design: walkie-songie's state / reconciliation /
reactive-UI layer has become, in substance, a general-purpose substrate — a
reconciling signed-op store with a deterministic causal fold, an
anti-entropy driver, an intent/rollback discipline, and a
projection-exclusive reactive UI adapter — wearing a music costume. Factor
the substrate out as **tutti** crates; re-home walkie as a thin music app (a
*domain instantiation*) on top; and fold in the design work that is
specified but not yet landed: the Merkle commitment layer, the historical
`view(at, from)` read, the intent lifecycle, the bound-effects adapter.

---

## 1. The division of labor: hhhs-core / tutti / walkie

The first structural fact, stated before any crate is named: **tutti-core
must sit ATOP hhhs-core, not beside it.** hhhs-core is already the
strategy-agnostic causal kernel — opaque-payload DAG with fixed entry
identity, `ReachIndex` causal cover, causal-maxima registers, the void
engine, staged/rollback primitives, sans-io RBSR reconciliation, and the
reactive `Growth` seam (`hhhs-core/src/lib.rs:10-27`) — and its own status
file draws the boundary tutti needs, verbatim: **"Deliberately not
implemented: … Network transport, signatures, authorship, or an authority
root. Downstream p2panda applications retain those responsibilities"**
(`hhhs-rs/STATUS.md`, "Deliberately not implemented"). Durable storage is on
the same list.

That excluded band — signed authorship, topic binding, wire framing, durable
journaling, domain-fold assembly, commitments, presence leases, intents —
is exactly what `src/room/**` and `src/net/**` built. Tutti is that band,
made domain-agnostic. Three layers, each owning what the layer below
refuses:

| layer | owns | explicitly does NOT own |
|---|---|---|
| **hhhs-core / hhhs-reactive** | causal DAG + entry identity (`dag.rs`), reachability (`cover.rs`), registers (`register.rs`), void/verdict engine (`void.rs`), staged/rollback taxonomy (`staged.rs`, `rollback.rs`), sans-io RBSR (`reconciliation.rs`, `sync_session.rs`), keyed reactive views (`hhhs-reactive/src/lib.rs:1-32`) | signatures, authors, topics, transport, durability (STATUS.md) |
| **tutti-*** | signed per-author op envelopes + verification-at-ingress, the lift (verbatim-bytes → entries, strict deferral), the fold-combinator kit + `view(at, from)`, effect attribution, intents/cotransactions, Merkle `ops_root`/`state_root`, presence leases, journals, the transport-neutral sync driver + rendezvous + iroh endpoints, the reactive projection/effect discipline | any domain alphabet, any domain fold rule, any UI |
| **walkie-songie** | `WalkieOp` + its fold semantics, tuning/Scala/KBM (`src/tuning/`), pitch-class UI + MIDI + voice, the app hosts | replication, verification, sync, rollback machinery |

The non-reinvention checklist, because the temptation is real:

- tutti does **not** implement reconciliation — it drives
  `hhhs_core::sync_session::SyncSession` (`src/net/sync.rs:1-8`), whose nine
  hardening passes (frame caps, ack ledger, O(|union|) transfer,
  `Divergent` status) live in the kernel and were paid for there.
- tutti does **not** implement observed-remove — the content-keyed add-wins
  combinator is built on `ReachIndex::is_ancestor` (`store.rs:439-441`),
  and the register combinator is `register::resolve` (`store.rs:579-604`).
  hhhs-core's own `lens::Op` observed-remove set stays distinct: it is
  *position-keyed* by design (`hhhs-core/src/lens.rs:1-7`); tutti's
  content-keyed set is the complementary shape walkie proved.
- tutti does **not** define verdict semantics beyond the fold — the void
  engine and its no-verdict-cache doctrine remain kernel property
  (`eventually-consistent-pitchsets.md` §6.5), consumed when policy-shaped
  features (ownership evaluators, capability channels) eventually need them.

---

## 2. Crate decomposition

```
            hhhs-core     hhhs-reactive     p2panda-core     radix_immutable(+merkle)
                ▲               ▲                 ▲                  ▲
                │               │                 │                  │
        ┌───────┴───────────────┼─────────────────┴──────────────────┘
        │                       │
   tutti-core ◀─────────────────┤
        ▲   ▲                   │
        │   └── tutti-net ──────┼──────▶ tutti-net-iroh   (iroh, iroh-gossip,
        │            ▲          │              ▲           tickets, rendezvous)
        │            │          │              │
   tutti-reactive ───┘◀─────────┘              │
        ▲                                      │
        └──────────────┬───────────────────────┘
                       │
                 walkie-songie   (domain + UI + hosts + plugin + src-tauri)
```

### 2.1 `tutti-core` — the reconciling signed-op substrate (domain-agnostic)

The realization of the vision's substrate contract (`TuttiSubstrate`,
`eventually-consistent-pitchsets.md:653-688`): properties (a)–(d) —
self-certifying authorship, deterministic `view(at, from)`, commuting merge
semantics, append-only causally-frontiered log (ibid. 622-650) — as one
concrete generic store rather than a dynamic-dispatch abstraction (§3.1
below). Contents, each with its walkie source:

| module | extracted from | what generalizes |
|---|---|---|
| signed envelope | `src/room/ops.rs` | `VersionedOp` (version, `ts_micros` display-only, topic binding, `observed` horizon — `ops.rs:194-211`), wire framing + size ladder (`ops.rs:49-78`, `269-350`), sign/verify (`ops.rs:426-484`, `537-608`), `VerifiedOp` with private fields so unverified data cannot reach a store write (`ops.rs:352-367`), `AuthorId`/`OpId`/`LogHead` (`ops.rs:82-109`, `248-261`). Only `WalkieOp` itself and `validate_wire` (`ops.rs:112-190`) are domain. |
| the lift | `src/room/store.rs` | verbatim-bytes framing (`store.rs:36-73`), `prevs = lift(backlink) ∪ lift(observed)` (`store.rs:275-284`), strict deferral + drain (`store.rs:319-339`), dual `OpId ↔ EntryHash` maps + decoded index + per-author heads (`store.rs:112-127`), `prepare_commit`/`commit` two-phase (`store.rs:357-390`), sync-layer surface (`entry_hashes`, `signed_ops`, `repair_record(s)`, `lifted_op_ids`, `sync_root` — `store.rs:143-236`). None of this names music. |
| fold combinators | `store.rs:411-605` | content-keyed add-wins with author attribution (`with_pitches`, 411-452), owner-gated per-owner seq objects (`with_pieces`, 454-551), causal-maxima registers (`with_registers`, 553-605) — as a `FoldCtx` combinator kit (§3.2); the *composition* stays domain |
| `view(at, from)` | new (gap 5 of the vision, `eventually-consistent-pitchsets.md:573-576`) | fold over a causal prefix; the DAG already has everything (`store.rs:392-408` folds only the full snapshot today) |
| `EffectMap` | designed, `reactive-rollback-api-design.md` §5.2 | per-op effect attribution computed in the same fold pass — `Effective / Superseded{by, authors, kind} / Inert` |
| intents | designed, `reactive-rollback-api-design.md` §5.3-5.4, §6 | `IntentLog` (local journal ⋈ `EffectMap`), `SubmitMode::{Publish, Draft{ttl}}` over `hhhs_core::StagedDag`, `revert()` via the kernel's `rollback_for` discriminator; `Batch` as one signed op |
| Merkle commitments | designed, `reconciliation-tree-fit.md` §6 + `radix-immutable-merkle-audit.md` | `ops_root` (canonical radix trie over entry hashes, `radix_immutable` `merkle` feature), `state_root` (flat canonical Merkle over the view's canonical bytes), inclusion/exclusion proofs, snapshot message `(F, ops_root, state_root, sig)` |
| presence leases | `src/room/presence.rs` | the signed/sequenced/leased envelope (topic, session, sequence, `lease_ms` bounds — `presence.rs:21-79`, `81-163`) generic over the body payload; the pitch body stays walkie |
| journal | `src/room/journal.rs` | append-only signed-op journal with torn-tail recovery; payload-agnostic already (it journals wire bytes) |
| testkit | `src/room/test_support.rs` + `tests/support/` | `Peer`, the op-graph oracle, `SimNet` + `Policy` + `assert_converged` (`tests/l0_*.rs`) — behind a `test-support` feature, exactly as today (`Cargo.toml:63-66`) |

Dependency posture (load-bearing for the leaf profile, §6.4): `p2panda-core`
+ `hhhs-core` + `blake3` + `serde` + `radix_immutable` only — all wasm-safe,
no tokio, no iroh, no web-sys. This is already true of `src/room/**`
(`Cargo.toml:99-107`).

### 2.2 `tutti-reactive` — the reactive-effectful adapter (domain-agnostic)

The UI discipline the three UI design docs specify, packaged so a downstream
app cannot rebuild the bug family walkie just diagnosed:

- **The projection facade**: the generalized `ProjectedRoom`
  (`reactive-effectful-ui-adapter-design.md` §2.2) — keyed
  `MutableBTreeMap` facets + scalar `Mutable`s with exactly one private
  writer, `ReadOnlyMap`/`ReadOnlyMutable` handles for everything else (§3.1
  there), the same capability move as `VerifiedOp`'s private fields.
- **Bound effects**: the `effect(signal, sink)` discipline and its
  enforcement kit — the module-split pattern (view/gesture/hit, §3.2 there)
  and the `clippy.toml` disallowed-methods backstop (§3.3) shipped as
  documented convention + a lint-config template, since Rust privacy is the
  real mechanism and it lives in the consumer.
- **The gesture layer**: the pure
  `step(state, event, facts) → (state, verdict)` transition-function shape
  with `Settling`-style typed, deadline-bounded optimism (§4.1-4.2 there) —
  as a small trait + helpers, natively unit-testable (no `web_sys`).
- **Tier 0 rollback**: nothing — structurally. Rollback-as-reprojection is
  the *absence* of code (`reactive-rollback-api-design.md` §3), and the
  crate's job is to make that absence stable: signals are read-only
  projections of store state, "there is no method that changes a signal
  without changing data" (§5.4 there).
- **Tier 1 lifecycle**: the signal surface over `tutti-core`'s
  `IntentLog ⋈ EffectMap` join — `intents() → SignalVec<TrackedIntent>`,
  `lifecycle() → Stream<LifecycleEvent>`, per-handle `phase()`
  (`reactive-rollback-api-design.md` §5.4, adopted essentially verbatim).
- **The hhhs-reactive bridge**: `Revision{added, retracted, at}` /
  `signal_vec_view` (`hhhs-reactive/src/lib.rs:55-67`, 245) bound to
  `Store<L>` once the store exposes a `Growth` handle (its `MemDagStore` is
  private today, `store.rs:115` — the exposure is part of this crate's
  extraction, coupling design §5). `retracted` carrying the previous row
  value is what feeds exit animations and MIDI note-offs (`lib.rs:60-67`).

Depends on: `tutti-core`, `hhhs-reactive`, `futures-signals`. **Not**
`dominator` — dominator is the consumer's renderer; the facade speaks
futures-signals, which is dominator's own engine
(`openspec/changes/archive/2026-08-27-rewrite-p2panda-hhhs-stack/proposal.md:31-35`), so the
binding needs no adapter. dominator-specific helpers (if any prove worth
sharing) go in a `dominator` feature, off by default.

### 2.3 `tutti-net` + `tutti-net-iroh` — sync and transport

Split along the seam `src/net/mod.rs` already draws (`net/mod.rs:1-45`):

**`tutti-net`** (transport-neutral, wasm-safe, iroh-free):

- The RBSR driver: `drive_initiator`/`drive_responder`, `RoomSyncSource`
  (the source/index consistency invariant, `src/net/sync.rs:10-18`),
  `SyncLimits`/`SessionBudget` scaling rules (`sync.rs:106-120`),
  `SyncTimer` (runtime-neutral sleep, `sync.rs:96-104`), the authoritative
  driver contract (resume-admitted after every `Entries`; close on
  `status() != Exchanging`, never `is_complete()` — `sync.rs:20-33`).
- `SyncStoreAccess` — already the store-generic seam (`net/mod.rs:79-82`);
  generalizing the driver's two walkie imports (`SignedOp`,
  `verify_signed_op_for_topic` — `sync.rs:50-53`) onto `tutti-core`'s
  generic envelope is the whole port.
- `LoopbackTransport` (`net/loopback.rs`) and the portable identity layer
  (`net/identity.rs`: `SeedStore`, `WalkieIdentity` → `TuttiIdentity` — one
  32-byte seed deriving both the author signing key and the transport key).
- Frame caps as *derived* constants: the size ladder from the op layer
  (`MAX_SYNC_FRAME_BYTES` compile-time-asserted against
  `MAX_SIGNED_OP_WIRE_BYTES`, `sync.rs:66-83`) becomes an assertion the
  generic crate states over `L`'s declared caps.

**`tutti-net-iroh`** (the concrete transports):

- `iroh_common` (topics, tickets, relay policy, framed QUIC sync streams),
  `native.rs` (QUIC + mDNS + gossip), `browser.rs` (relay-only wasm
  endpoint), `repair.rs`, and `rendezvous.rs` (the y-webrtc topic
  rendezvous, `net/rendezvous.rs:1-23`, per
  `peer-discovery-design.md` §3 Option 1), plus the pkarr address-lookup
  posture from that design's §2.
- **Protocol identity becomes a parameter.** Today the ALPNs, wire magics,
  rendezvous channel prefix (`walkie-rdv-v1-`, `rendezvous.rs:36`), topic
  derivation string, and relay/signaling URLs
  (`relay.wondering.xyz` / `signal.wondering.xyz`) are hardcoded walkie
  constants. The generic crate takes a `ProtocolIds { alpn_gossip,
  alpn_sync, topic_domain, rdv_prefix, relays, … }` struct; walkie supplies
  its current strings so extraction is wire-invisible. This matters
  doubly because ALPN bumps are how wire generations are coordinated with
  potluck (the co-consumer pinning the same hhhs rev).

### 2.4 What stays in walkie-songie (the music app)

- **The op alphabet + fold**: `WalkieOp` (`ops.rs:112-142`), its domain
  validation (`ops.rs:145-190`), and the fold composition — including the
  genuinely domain-shaped parts: tuning-scoped validity filters and the
  eclipse semantics (degrees from non-active tunings hidden, not
  reinterpreted — `store.rs:426-431`, pinned at `store.rs:926-952`), and
  the pieces owner-gating rule (`store.rs:454-456`) pending the §5
  shared-vs-owner decision (`reactive-effectful-ui-adapter-design.md` §5).
- **Tuning**: all of `src/tuning/` — `TuningId` = blake3 of canonical Scala
  bytes (`tuning/mod.rs:119-123`), `TunedDegree`/`TunedPeriodicPitch`
  (229, 256), `QuantizeResult.cents_deviation` (181-186), SCL/KBM parsing.
  This is the paradigm's music-theory floor, not substrate.
- **Presence body**: the voice-pitch lease payload
  (`Option<TunedPeriodicPitch>`, `presence.rs:29`) as a domain
  instantiation of the generic lease envelope.
- **UI + I/O**: keyboard/components/graph/solfege (`src/web/`), the
  all-around-keyboard component, Web MIDI + `src/midi/`, voice
  pipeline, `src/words.rs` room names, the plugin, `src-tauri`.
- **The hosts**: `browser_host.rs` and the Tauri runtime, including the
  `ClientCommand`/`AppEvent` seam (`src/client.rs:38, 179`,
  `CLIENT_PROTOCOL_VERSION` at `client.rs:11`). Deliberately NOT extracted
  yet — §6.2.
- **Legacy to delete, not extract**: the yrs render adapter
  (`yrs_state.rs`, "a render adapter on death row" —
  `eventually-consistent-pitchsets.md:346-348`), and the pre-rewrite types
  still in `room/mod.rs` (`CombinationMethod`, `PeerPitchSet`,
  `RoomPitchResult` — `room/mod.rs:52-140`) whose union/intersection model
  the add-wins fold superseded.

---

## 3. Public API

### 3.1 `OpLanguage`: the domain seam

The sharpest design decision. The vision's `TuttiSubstrate` trait
(`eventually-consistent-pitchsets.md:653-688`) is a *conformance contract* —
the four properties an implementation must honor. It is not a good *runtime
abstraction* for this codebase: walkie needs exactly one substrate, generics
over payload beat trait objects for a fold that returns a domain view type,
and the contract's real force is in tests (golden vectors, permutation
convergence, oracle parity), not vtables. So:

- **The runtime seam is `OpLanguage`** — a trait the app implements once,
  describing its alphabet and its fold.
- **`TuttiSubstrate` ships as documentation + a conformance test suite** in
  the testkit (assert: equal verified op-sets ⇒ identical views across
  ingest permutations, `root()` equality ⇔ set equality, deferral liveness)
  — the properties `store.rs`'s tests already pin for walkie
  (out-of-order convergence, `store.rs:1107-1138`-style), generalized. A
  trait-object façade can be added later for the leaf profile without
  disturbing anything (§6.4).

```rust
// tutti-core
pub trait OpLanguage: Sized + 'static {
    /// The domain alphabet. CBOR via serde; evolution discipline is the
    /// walkie rule, now stated generically: append variants, never reorder,
    /// new fields only as #[serde(default)], bump SCHEMA_VERSION on shape
    /// change (ops.rs:27-29).
    type Op: Serialize + DeserializeOwned + Clone + PartialEq;
    /// The materialized read model. `Canonical` supplies the byte encoding
    /// state_root commits to (§3.4).
    type View: Default + Clone + PartialEq + Canonical;

    const SCHEMA_VERSION: u16;                 // ops.rs:48
    /// Domain-separating framing tags. Walkie keeps its current values so
    /// entry hashes and wire frames are byte-identical across the
    /// extraction (store.rs:36, ops.rs:70).
    const ENTRY_FRAME_MAGIC: &'static [u8];
    const WIRE_MAGIC: &'static [u8];
    /// Size ladder root (ops.rs:49-64). Everything downstream derives.
    const MAX_PAYLOAD_BYTES: usize;

    /// Domain wire validation — bounds and well-formedness, run once at
    /// ingress inside verify (ops.rs:145-190, called at ops.rs:575-578).
    fn validate_wire(op: &Self::Op) -> Result<(), String>;

    /// THE deterministic fold: a pure function of the decoded op-set and
    /// its causal indexes. Two peers with equal op-sets MUST return equal
    /// views (contract property (b)).
    fn fold(ctx: &FoldCtx<'_, Self>) -> Self::View;
}
```

### 3.2 `Store<L>`, the fold kit, and `view(at, from)`

```rust
pub struct Store<L: OpLanguage> { /* dag, dual maps, decoded, heads, pending */ }

impl<L: OpLanguage> Store<L> {
    // ── ingest / commit (store.rs:247-390, unchanged in shape) ──────────
    pub fn ingest_verified(&mut self, op: VerifiedOp<L>) -> Vec<EntryHash>;
    pub fn prepare_commit(&self, key: &SigningKey, topic: &str,
                          ts_micros: u64, op: L::Op) -> SignedOp;
    pub fn commit(&mut self, ...) -> SignedOp;

    // ── reads: state as a query ─────────────────────────────────────────
    /// Today's whole-snapshot fold (store.rs:392-408): view_at(f, f).
    pub fn view(&self) -> L::View;
    /// NEW — the vision's gap 5 (eventually-consistent-pitchsets.md:573-576):
    /// fold the causal prefix at `at`, judged from horizon `from`. The scrub
    /// bar, forks, and "what was Ada holding at the bridge" are all reads.
    pub fn view_at(&self, at: &Frontier, from: &Frontier) -> L::View;
    /// Effect attribution, computed in the SAME pass as the view — never a
    /// second source of truth (reactive-rollback-api-design.md §5.2).
    pub fn view_with_effects(&self) -> (L::View, EffectMap);

    pub fn frontier(&self) -> Frontier;              // store.rs:343-350
    pub fn entry_hashes(&self) -> BTreeSet<EntryHash>;

    // ── convergence + commitments (§3.4) ────────────────────────────────
    pub fn sync_root(&self) -> [u8; 32];             // legacy digest, store.rs:194-196
    pub fn ops_root(&self) -> [u8; 32];              // Merkle; supersedes sync_root
    pub fn state_root(&self) -> [u8; 32];
    pub fn prove_op(&self, id: OpId) -> OpProof;     // inclusion/exclusion
}
```

`FoldCtx` packages what `RoomView`'s builders consume today — the decoded op
index, `ReachIndex`, `register::resolve` — as combinators:

```rust
pub struct FoldCtx<'a, L: OpLanguage> { /* decoded ops, reach, entry↔op maps */ }

impl<'a, L: OpLanguage> FoldCtx<'a, L> {
    /// Content-keyed causal ADD-WINS set with authorship: an add is live iff
    /// no same-key remove causally observed it (store.rs:435-450). The
    /// result carries per-key live add-entries AND holders — walkie's
    /// pitch_authors (store.rs:623) generalized, so authorship-as-channel
    /// (vision §3.1 axis 2) is a substrate affordance, not app code.
    pub fn add_wins_set<K: Ord + Clone>(
        &self, classify: impl Fn(&L::Op) -> Option<SetEvent<K>>,
    ) -> AddWins<K>;   // AddWins::holders(&K) -> &BTreeSet<AuthorId>

    /// Cross-author register: causal maxima, max-raw-bytes entry-hash
    /// tiebreak (store.rs:553-605; hhhs_core::register::resolve).
    pub fn causal_register<T>(
        &self, classify: impl Fn(&L::Op) -> Option<T>,
    ) -> Option<Resolved<T>>;

    /// Owner-gated object table: identity = creating op's OpId; only the
    /// owner's greatest-seq lifecycle/move ops take effect
    /// (store.rs:454-551). Kept BESIDE a shared-object variant
    /// (causal register + observed-remove lifecycle) so the pieces
    /// decision (adapter design §5) is a one-line swap, not a fork.
    pub fn owner_seq_objects<T>(&self, classify: ...) -> BTreeMap<OpId, Owned<T>>;
    pub fn shared_objects<T>(&self, classify: ...)   -> BTreeMap<OpId, Shared<T>>;
}
```

**The staging leak, faced now rather than discovered later**: walkie's fold
is *staged* — registers resolve first, and the set/object folds then filter
by the resolved tuning (the validity gates at `store.rs:415-420`, `426-431`,
`458-463`; the eclipse test at 926-952). Facets are not independent. The
combinator API therefore makes staging explicit: `fold()` is one ordinary
function that may call `causal_register` first and close over its result in
later `classify` closures. No framework, no facet DSL — the staging is just
Rust control flow, and the eclipse (`Superseded{kind: TuningEclipse}` in
walkie's `EffectMap`) is emitted by the domain's classify, with the generic
`SupersessionKind` enum carrying an `Domain(&'static str)` arm rather than
hardcoding walkie's names.

### 3.3 Intents and cotransactions

The write path adopts the rollback design's surface
(`reactive-rollback-api-design.md` §5.4) with one generalization: the intent
body is `L::Op` or a batch of them.

```rust
pub enum SubmitMode { Publish, Draft { ttl: Option<Duration> } }

impl<L: OpLanguage> RoomHandle<L> {
    /// Tier 0. Absolute, idempotent intent — derived from the projection,
    /// never an involution (ui-state-coupling-design.md §3). Dropping the
    /// handle opts out of Tier 1.
    pub fn submit(&self, body: IntentBody<L>, mode: SubmitMode) -> IntentHandle;
    pub fn revert(&self, id: IntentId) -> Result<IntentHandle, RevertError>;
}

pub enum IntentBody<L: OpLanguage> {
    One(L::Op),
    /// Atomic multi-op intent: ONE signed op, one entry hash, one verdict
    /// (rollback design §6 option 1; HHS3's "a multi-table write is one
    /// atomic operation"). Sub-ops key as (entry_hash, index).
    Batch(Vec<L::Op>),
}
```

The cotransaction story costs the substrate nothing new, by the coupling
design's own analysis (§4.2 there): `commit` stamps
`observed = store frontier` (`store.rs:370`), so every intent already
carries its implicit precondition, re-evaluated deterministically by every
replica's fold. Two sharpenings are *reserved in the generic alphabet* and
schema-gated:

- **`Retract { targets: Vec<OpId> }`** — the explicit-target retract
  (walkie's `RetractDegreeAdds`, coupling §4.3), which is the multi-target
  generalization of hhhs-core's own blessed shape
  (`lens::Op::Remove(EntryHash)`, `lens.rs:1-25`). Provided as an envelope-
  level optional wrapper so any `L` gets intent-as-data + per-room
  ownership-as-evaluator-policy without redesign. **Gated** on the next
  `SCHEMA_VERSION` bump and on alignment with Santi's `hhs3-ts` — the
  cotransaction semantics' source of truth
  (`eventually-consistent-pitchsets.md:3-11`, §6.5) — per the coupling
  design's "adopt at the next planned schema move, not as part of this fix."
- **Draft staging** over `hhhs_core::StagedDag` with the published-wins
  re-check (`hhhs-core/tests/rollback.rs:45-57` doctrine).

### 3.4 Merkle commitments — wiring the unused layer

`radix_immutable` is the one piece of "the good stuff" that is fully built
and wired to nothing: v0.2.0 carries the audited canonical trie
(canonicity property-tested across permutations and interleaved deletes,
`radix-immutable-merkle-audit.md` §1.3) plus the implemented `merkle`
feature — blake3-256 `merkle_root`/`prove`/`verify`, per-node depth-8 binary
child commitment (~0.8 KB proofs), golden vectors
(`/laboratory/radix_immutable/src/{merkle,proof}.rs`). Walkie has no
dependency on it (`Cargo.toml` — absent). tutti-core wires it as designed:

- **`ops_root`**: a `Trie<[u8;32], (), …>` maintained incrementally on
  every lift (insert on `try_lift`, `store.rs:289-315`; delete support
  covers future history truncation — the property the MST crate lacked,
  `reconciliation-tree-fit.md` §3.1). Root equality ⇔ entry-set equality
  *plus* O(log n) inclusion/exclusion proofs — a strictly stronger digest
  than `sync_root` (`store.rs:89-96`), which it deprecates after a window
  (`reconciliation-tree-fit.md` §5, end).
- **`state_root`**: flat canonical Merkle over the view's `Canonical` bytes
  (sorted `(section, key, value)` leaves) — "no search-tree machinery is
  warranted there at all" (`reconciliation-tree-fit.md` §2).
- **The depth guard** (`MAX_PROOF_DEPTH`, grinding defense) lives in
  tutti-core's wrapper, not the trie — per the audit's placement call
  (`radix-immutable-merkle-audit.md` §2.6) — and is written into the spec
  before the first frozen root.
- **Posture: complement, never replace.** RBSR stays the wire protocol; the
  tree is the commitment/proof layer (`reconciliation-tree-fit.md` §5 — the
  tree-guided-sync trap and the salted-fingerprint hole are both documented
  there and inherited here as a standing prohibition). The snapshot message
  `(F, ops_root, state_root, sig_author)` + optimistic-accept +
  background-backfill is `zk-provable-dag-snapshots.md` Stage 0; quorum
  co-signing (Stage 1) and delegated zkVM proofs (Stage 2) hang off the
  same two roots without further substrate change.

### 3.5 Reactive projection and effects (tutti-reactive surface)

```rust
/// The generalized ProjectedRoom (adapter design §2.2): keyed facets +
/// scalars, ONE private writer, read-only handles out.
pub struct Projected<F: Facets> { … }
impl<F: Facets> Projected<F> {
    fn apply(&self, view: &F::View, local: AuthorId);   // private: THE writer
    pub fn facets(&self) -> F::ReadOnly;                // ReadOnlyMap / ReadOnlyMutable
}

/// Tier 1, opt-in (rollback design §5.4, verbatim shape):
pub fn intents(&self) -> impl SignalVec<Item = TrackedIntent>;
pub fn lifecycle(&self) -> impl Stream<Item = LifecycleEvent>;
// per-handle: IntentHandle::phase() -> impl Signal<Item = IntentPhase>

/// Gesture layer (adapter design §4.1): pure, native-testable.
pub trait Gesture {
    type Event; type Facts; type Verdict;
    fn step(self, ev: Self::Event, facts: &Self::Facts) -> (Self, Self::Verdict);
}
```

Two structural properties carried over as crate-level invariants, with their
epigraphs: *"a render adapter that only the projection can construct cannot
show unbacked state"* (coupling §2.1), and *"there is no method that changes
a signal without changing data"* (rollback §5.4).

### 3.6 Walkie as an instantiation

```rust
// walkie-songie/src/domain.rs (illustrative)
pub struct WalkieLang;
impl OpLanguage for WalkieLang {
    type Op = WalkieOp;                       // ops.rs:112-142, unchanged
    type View = RoomView;                     // store.rs:617-632, unchanged
    const SCHEMA_VERSION: u16 = 3;            // ops.rs:48
    const ENTRY_FRAME_MAGIC: &'static [u8] = b"walkie.hhhs.signed-op/1";  // store.rs:36
    const WIRE_MAGIC: &'static [u8] = b"walkie.signed-op/3\0";            // ops.rs:70
    const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;                          // ops.rs:64

    fn validate_wire(op: &WalkieOp) -> Result<(), String> { /* ops.rs:145-190 */ }

    fn fold(ctx: &FoldCtx<'_, Self>) -> RoomView {
        // Stage 1: registers (store.rs:553-605)
        let tuning = ctx.causal_register(as_tuning)
                        .map(|r| r.value).unwrap_or_else(TuningDefinition::twelve_tet);
        let active = tuning.validate("active room tuning").ok();
        // Stage 2: the hot set, tuning-scoped — the eclipse is the classify
        // closure returning None for out-of-tuning degrees (store.rs:426-431)
        let degrees = ctx.add_wins_set(|op| match op {
            WalkieOp::AddDegree { pitch }    if valid(pitch, &active) => Some(Add(*pitch)),
            WalkieOp::RemoveDegree { pitch } if valid(pitch, &active) => Some(Remove(*pitch)),
            _ => None,
        });
        // Stage 3: pieces — owner-gated today; shared_objects if §5 flips
        let pieces = ctx.owner_seq_objects(...);
        RoomView { pitches: degrees.keys(), pitch_authors: degrees.holders_map(),
                   pieces, tuning: Some(tuning), … }
    }
}

pub type RoomStore = tutti_core::Store<WalkieLang>;   // src/room re-exports it
```

The extraction is **byte-compatible by construction**: the magics, the
framing, the CBOR envelope, and the fold semantics are unchanged, so the
golden entry-hash vector (the schema pin the frame magic documents,
`store.rs:33-36`) and the L0 convergence suite (`tests/l0_*.rs`) are the
acceptance gates — they must pass unmodified against `Store<WalkieLang>`.

---

## 4. "All the good stuff", placed

| design finding | source | lands in |
|---|---|---|
| substrate contract (a)-(d) + `root()` | vision §6.1-6.3 | `tutti-core` (concrete `Store<L>`; contract as testkit conformance suite) |
| intent over involution; absolute idempotent gestures | coupling §3 (shipped for taps: `keyboard.rs:1416-1426`, `ToggleDegree` deleted) | discipline: `tutti-reactive` (submit-from-projection); substrate: `tutti-core` (commuting alphabet requirement on `L`) |
| implicit cotransaction (observed-stamped commits) | coupling §4.2; `store.rs:370` | `tutti-core::Store::commit`, unchanged |
| explicit cotx: `Retract{targets}` / `RetractDegreeAds` | coupling §4.3 | `tutti-core` envelope, **schema-gated + hhs3-ts-gated** (§3.3) |
| `Batch` atomic multi-op intents | rollback §6 | `tutti-core::IntentBody::Batch` |
| rollback taxonomy (Abandon/Retract/Compensate), published-wins | rollback §1.2; `hhhs-core/src/rollback.rs` | consumed from hhhs-core; surfaced via `revert()`/Draft in `tutti-core` |
| Tier 0 rollback = re-projection | rollback §3 | `tutti-reactive` (as an invariant, i.e. no code) |
| `EffectMap` + supersession attribution | rollback §5.2 | `tutti-core` fold pass |
| IntentLog / phases / `lifecycle()` | rollback §5.3-5.4 | data: `tutti-core`; signals: `tutti-reactive` |
| projection facade + read-only handles + bound effects + gesture machine + `Settling` | adapter §2-4 | `tutti-reactive` |
| Merkle `ops_root`/`state_root` + proofs (radix_immutable, **currently unused**) | tree-fit §6; audit; zk §1.5 | `tutti-core` commitments module (§3.4) — this design wires it |
| snapshot message + optimistic accept + backfill; quorum/fraud/zkVM ladder | zk §8 Stages 0-2 | Stage 0 split `tutti-core` (roots, message) / `tutti-net` (serving); Stages 1-2 future, no new substrate needed |
| RBSR driver + hardened session contract | `net/sync.rs`; hhhs `sync_session` | `tutti-net` |
| topic rendezvous + pkarr addressing + live tickets | peer-discovery §2-3; `net/rendezvous.rs` | `tutti-net-iroh` |
| presence leases (the anti-stuck-note tier) | `presence.rs:1-17` | envelope: `tutti-core`; body: walkie |
| time-travel / scrub (`getView(at, from)`) | vision §1.2, §5.2 gap 5, §5.3 exp 1 | `tutti-core::view_at`; scrub UI in walkie (experiment 1 — read-side only) |
| channels reimagined: topics / author-as-channel / typed / slices / capability | vision §3.1 | topics: `tutti-net(-iroh)` + topic-bound envelopes (`ops.rs:594-608`); author-as-channel: `AddWins::holders`; typed channels: one `L` per topic (the type param IS the channel schema); slices: view functions + `Revision` streams; capability: **parked** (needs HHS3 capability machinery hhhs-core doesn't carry — vision §3.1 "importing it is a real project") |
| leaf/ESP-32 profile | vision §6.6 | a dependency-posture constraint on `tutti-core` today; a windowed-store `DagRead` impl later (§6.4) |

---

## 5. Extraction plan

Two tracks. The data track (D) and the UI track (U) are independent until
the final re-home; every step leaves walkie building and its test matrix
green (`cargo build --lib --features native-net` + `cargo test` + the wasm
`web-ui` build — the gates the adapter design already runs, §7 there).

**Track D — substrate:**

1. **Workspace + in-place genericization of the envelope.** Promote
   `crates/` in the walkie workspace (`Cargo.toml:68-71` already hosts
   members). Introduce `OpLanguage` + `SignedEnvelope<L>`/`VerifiedOp<L>`
   *inside* `src/room/ops.rs`, make `WalkieOp` the first `L`, keep every
   public path re-exported so no call site moves. Gate: `ops.rs` round-trip
   + wire tests unchanged; signed bytes byte-identical (magics are
   `L` consts).
2. **In-place genericization of the store.** `Store<L>` + `FoldCtx`
   combinators in `src/room/store.rs`; `RoomStore = Store<WalkieLang>`
   with `WalkieLang::fold` composed from combinators. Gate: the golden
   entry-hash vector, the oracle parity + mutation tests, the permutation
   convergence tests, and `tests/l0_*` — all unmodified.
3. **Extract `tutti-core` + `tutti-testkit`.** Mechanical move of the now-
   generic halves; walkie's `src/room/` shrinks to `WalkieLang`, tuning-
   facing types, presence body. `test_support`'s `Peer`/oracle/`SimNet`
   generalize into the testkit behind the same `test-support` pattern.
4. **`view_at(at, from)` + `EffectMap`** in `tutti-core` (new capability;
   pure read-side). Ship walkie's scrub bar against it — the vision's
   cheapest decisive experiment (§5.3.1), zero wire change.
5. **Extract `tutti-net` / `tutti-net-iroh`** with `ProtocolIds`; walkie
   supplies its current ALPNs/magics/URLs (wire-invisible). **Sequencing
   caution**: `src/net/**` and `start_room` have had concurrent in-flight
   edits (coupling §6 callout; the browser-transport cutover is still being
   verified end-to-end) — this step waits for that surface to quiesce.
6. **Merkle wiring** (§3.4): `radix_immutable` dep (needs its v0.2.0
   Merkle layer committed/pinned — currently uncommitted work), `ops_root`
   maintained on lift, `state_root` over `Canonical` views, snapshot
   message, `sync_root` deprecation window. Independent of steps 4-5.

**Track U — reactive:**

7. **Finish the UI discipline in walkie first.** Coupling steps 3-5
   (single-writer sweep — the remaining direct writes at
   `keyboard.rs:993, 1046, 1215, 1338` and friends; offline host +
   IndexedDB signed-op journal; `state.room` privacy) and adapter steps 1-5
   (`ProjectedRoom`, gesture machine, bound effects, enforcement). These
   are already fully specified with their own test plans (coupling §6,
   adapter §7). **Extracting tutti-reactive before this lands would
   extract the tangle** — the facade and gesture layer must exist and be
   proven in walkie before their generic forms are frozen.
8. **Extract `tutti-reactive`**: `Projected<F>`, `ReadOnlyMap`, gesture
   trait, Tier-1 signal surface over `IntentLog ⋈ EffectMap`, the
   hhhs-reactive bridge (exposing the `Growth` handle from `Store<L>` —
   the deferred item from coupling §5). Walkie's facade becomes an
   instantiation; adapter step 6b (direct `signal_vec_view` binding,
   retiring the snapshot→facade hop) lands here.
9. **Re-home walkie**: delete `yrs_state.rs` + `room/mod.rs` legacy types +
   the AppSnapshot→facade laundering (rollback §4's four-hop pipeline
   collapses to `Store → tutti-reactive views → dominator`); hosts consume
   `RoomHandle<WalkieLang>`.

**Parked, with owners and gates:**

- `Retract{targets}` + `Batch`: next `SCHEMA_VERSION` bump, after hhs3-ts
  alignment (coupling §4.3 verdict; vision §6.5).
- Shared-vs-owner-gated pieces: product decision (adapter §5, explicitly
  the user's call). The combinator pair (§3.2) makes either a one-line
  swap; deciding *before* freezing tutti-core's combinator API is
  preferred, but not required.
- Host/`ClientCommand` seam extraction: only after a second host shape
  exists (§6.2).
- Capability channels, leaf-profile crate, temporal coordinate over sets:
  research (§6.4; vision §8).

**Standing blockers/coordination:**

- The hhhs pin `bd23d4e` is a branch head (`harden-sync-session`), not
  master; crate extraction hardens the dependency story (git-pinned is
  fine) but publishing tutti anywhere implies getting the kernel branch
  merged + potluck coordinated on the wire-generation bump — the same
  coordination the ALPN bump already requires.
- Potluck is the second consumer and the genericity test (§6.1); co-review
  the `OpLanguage`/combinator surface against its shapes (rounds,
  tri-state delivery — rollback §2 R3′) before freezing.

---

## 6. Honesty

### 6.1 Genuinely reusable vs. walkie-masquerading-as-generic

**Genuinely generic today** (the code contains no music): the envelope +
verification (`ops.rs` minus `WalkieOp`), the lift/deferral/heads machinery
(`store.rs:112-390`), the three fold combinators *as mechanisms*, the sync
driver (`net/sync.rs` — its imports of walkie types are two shallow seams),
loopback, identity, rendezvous (modulo the hardcoded protocol strings), the
journal, the presence envelope, the rollback taxonomy and intent lifecycle
(designed generic from day one), the Merkle layer (lives in a separate crate
already).

**Masquerading, or n=1 abstractions to hold loosely:**

- **The fold-combinator kit is generalized from one domain.** Walkie
  exercises add-wins + owner-seq + causal-maxima; potluck adds rounds/
  boundaries; nothing yet exercises, say, sequences or counters. The kit
  should ship as the *minimal* three walkie proved plus `shared_objects`,
  and resist growing speculative combinators. The `fold()`-is-plain-Rust
  design (§3.2) is the hedge: a domain that outgrows the kit writes
  against `FoldCtx`'s raw indexes without forking the crate.
- **The tuning eclipse is domain logic that *looks* structural.** It lives
  inside `with_pitches`' classify path (`store.rs:426-431`), and the
  staged-fold design keeps it in walkie's `fold()`. If a second domain
  reproduces the "register scopes a set" shape, promote it; not before.
- **`AppSnapshot`/`ClientCommand`/`AppEvent`** (`client.rs`) enumerate
  walkie's domain verbs (`ToggleDegree` is already gone; `AddDegree`,
  `PutPiece`… remain). This seam is walkie's, full stop.
- **Occupancy, clefs, drag deltas, emoji palettes**: UI-domain facts;
  the adapter design already marks occupancy as UI-only with no store rule
  (adapter §4.3) — none of it enters tutti.

### 6.2 What NOT to extract yet

- **The host runtime** (`browser_host.rs`, `src-tauri`): the
  identity+store+network+commit-loop+projection assembly is a strong
  *pattern* (`HostState`/`dispatch`/`apply_room_view`/`commit_room_op`,
  `browser_host.rs:72, 193, 909, 1283`) but has exactly one UI consumer and
  two nearly-identical instantiations that are still being actively
  reshaped (offline host pending, coupling §5; browser transport cutover
  in flight). Extract a `RoomHost<L>` only when the offline host lands and
  the three modes provably share one skeleton.
- **The word-list room naming, QR/ticket UX, relay/signaling deployments**:
  product/ops surface, not substrate.
- **`hhhs-reactive` re-wrapping**: tutti-reactive *bridges* it; it must not
  absorb it. The `Revision`/`signal_vec_view` shapes are kernel-adjacent
  and shared with potluck.

### 6.3 Where the abstraction leaks

- **Staged folds** (§3.2): facet independence is false; the API admits it
  rather than hiding it.
- **`SupersessionKind`**: the generic enum needs a domain arm
  (`TuningEclipse` is not a substrate concept) — attribution UIs must
  handle domain kinds they cannot enumerate.
- **Wire identity is app identity.** Magics, ALPNs, topic-derivation
  strings, schema versions are all `L`/`ProtocolIds` parameters — which
  means tutti cannot promise cross-app interop, only intra-app
  convergence. That is correct (a walkie peer and a potluck peer *should*
  refuse each other) but it makes "tutti the protocol" (vision §6) a
  *family* of protocols sharing a contract, not one wire format.
- **The no-verdict-cache doctrine caps fold cost.** `view()` (and now
  `view_at`) recompute per growth; `ReachIndex` is O(n²)-ish
  (`zk-provable-dag-snapshots.md` §1.3). Room-sized logs are fine;
  score-sized logs are unpriced (vision §8). The permitted escape is the
  advisory-accelerator discipline — an accelerator that must equal the
  from-scratch oracle — and it must be built inside tutti-core *before*
  any temporal-coordinate feature multiplies op counts.

### 6.4 The leaf profile, honestly

The ESP-32 story (vision §6.6) is a *constraint* on tutti-core today and a
*deliverable* later. Constraint honored now: tutti-core's dependency set
(p2panda-core, hhhs-core, blake3, serde, radix_immutable) is wasm-safe and
tokio-free — but **not** `no_std`; hhhs-core and p2panda-core are std
crates, `MemDagStore` holds full history, and Ed25519 signing on-MCU is
milliseconds. A real leaf needs the windowed store the vision sketches
(bounded suffix + compacted view + delegated archive) — that is a new
`DagRead` implementation plus a pruning contract, and it is exactly where
the `TuttiSubstrate` trait-object façade (§3.1) earns its keep: a leaf
implements the contract *scoped to a window* without carrying `Store<L>`.
Not schedulable now; the crate boundaries just have to not preclude it, and
with the §2.1 posture they don't.

### 6.5 Naming

`tutti` (vision §7) for the family; crate names `tutti-core`,
`tutti-reactive`, `tutti-net`, `tutti-net-iroh`, `tutti-testkit`. Git
dependencies for now (the hhhs pattern, `Cargo.toml:103-104`); crates.io is
a later decision with no design force.

---

## 7. Risks

1. **Premature generalization (the big one).** Every generic surface here
   is extrapolated from one shipped domain plus one sibling's patterns.
   Mitigations: generalize-in-place before extracting (steps D1-D2), keep
   `fold()` as plain Rust over combinators rather than a facet framework,
   and treat potluck co-review as a freeze gate for `OpLanguage`.
2. **Wire drift during extraction.** The whole extraction is only safe
   because identity is pinned by bytes: the golden entry-hash vector, the
   wire-frame round-trips, and the L0 suite are non-negotiable gates at
   every step. Any step that changes a magic, a CBOR layout, or a fold
   verdict is not an extraction step — it is a schema move and must say so
   (`OP_SCHEMA_VERSION` / ALPN discipline).
3. **Extracting the reactive layer before the discipline lands.** Coupling
   steps 3-5 and adapter steps 1-5 are specified but unshipped (the
   optimistic writes at `keyboard.rs:993, 1046, 1215, 1338` are still
   live). Track U's ordering exists precisely to avoid freezing a generic
   facade around a known-broken shape.
4. **Three artifacts, one entry set.** After Merkle wiring, `ops_root`,
   the RBSR index, and legacy `sync_root` all describe the same set; they
   must derive from one capture or skew becomes phantom divergence
   (`reconciliation-tree-fit.md` risk 3). The deprecation window for
   `sync_root` should be short.
5. **Coordination surface.** The kernel branch pin, the potluck wire-
   generation bump, the radix_immutable commit, and the hhs3-ts-gated
   schema move are four external synchronization points; each is cheap
   alone and easy to deadlock on jointly. The plan sequences them so only
   step D6 (Merkle) and the parked schema items actually wait on anyone.
6. **The substrate outrunning the app.** Walkie is a jam app; tutti is a
   paradigm. The discipline that keeps this honest is the vision's own:
   every extracted affordance must have a walkie consumer (the scrub bar
   for `view_at`, authorship color for holders, the tuning toast for
   Tier 1) — extraction steps that add generic capability with no walkie
   surface behind them should be treated as scope creep and parked.
