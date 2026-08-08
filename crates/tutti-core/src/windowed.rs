//! M3.0 — the bounded-window store, **no compaction**
//! (`docs/vision/windowed-store-design.md` §7 "M3.0", §6.1, §3.3, §1.3).
//!
//! Two types, mirroring the kernel's own [`MemDagStore`](hhhs_core::MemDagStore)
//! vs [`Store<L>`](crate::Store) split (design §6.1):
//!
//! - [`WindowedDag`] — the L-free kernel piece: a bounded suffix window of a causal
//!   DAG with dense window indices and an incremental **index-compressed bitset
//!   reach** (§3.3). It implements [`DagRead`], and provides the bounded-window
//!   [`WindowedDag::appended_since`] (the `DagDelta` contract: `Some` inside the
//!   window, **`None` past it**, dag.rs:228-235). While `N ≤ W` it retains the
//!   *entire* DAG and is therefore exactly a [`MemDagStore`].
//! - [`WindowedStore<L>`] — the leaf-profile sibling of [`Store<L>`](crate::Store):
//!   the same lift / strict-deferral / drain machinery over a [`WindowedDag`], a
//!   cut-scoped sync surface (§1.2, §4.2), and a fenced [`WindowedStore::view`] that
//!   folds the window through the boundary-aware [`WindowedReach`] backend (§3.5)
//!   via the public [`FoldCtx::over`](crate::FoldCtx::over) constructor.
//!
//! **What M3.0 is, precisely.** With `retain` left at its retain-everything default
//! (§7: "nothing is ever discarded and the theorem is trivially true"), the window
//! holds every op it has lifted. While a room's life fits the window (`N ≤ W`) this
//! is *exactly* correct (the windowed fold is byte-identical to the full-history
//! fold, §2.6) and *exactly* bounded (`≤ W` ops, ~64 KB @ W≤256, §5). This is what
//! the AMY verifying leaf needs first (§7: "a jam session is a few hundred ops, the
//! window is the world") and what makes every leaf-column *(model)* number in the
//! RAM budget measurable by M4.
//!
//! **The `view()` fence (§1.3, §6.2 delta 6) — the load-bearing safety property.**
//! A truncated DAG is a *legal* [`DagRead`] value (present-only is the kernel
//! doctrine), so a plain [`Reach`](crate::Reach)/`ReachIndex` built over a truncated
//! window silently computes `is_ancestor = false` for every cross-cut fact — a
//! **wrong view, not an error**. M3.0 refuses to ship that trap: the window is
//! window-complete *by construction* while `N ≤ W` (it holds the whole causal past,
//! so the bitset reach is exact), and the moment truncation occurs
//! ([`WindowedDag::is_complete`] flips to `false`) [`WindowedStore::view`] **hard-
//! refuses** (panics) rather than fold a window it can no longer answer ancestry
//! over. This is the design's option (a): "M3.0 is only claimed correct for `N ≤ W`."
//!
//! **The honest gap to M3.1.** The instant `N > W` the window truncates and the
//! fence trips: there is no correct fold, because a discarded op's contribution to a
//! *future* fold (an old add a future remove kills, a killed piece a future unremove
//! resurrects, a register write read at a narrow horizon) depends on the op still
//! being present. Making `N > W` fold correctly is M3.1: the **monotone-shadowing
//! retention** of §2.4-2.5 — discard an op only when every fold predicate consuming
//! it is a monotone consequence of causal facts fixed at lift, keeping the rest as a
//! residue of *candidates* — plus the cut masks / residue reach matrix of §3.2-3.4.
//! M3.0 deliberately builds none of that; it builds the scaffolding it slots into.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use hhhs_core::{DagRead, Entry, EntryHash, GrowthEpoch, Position};

#[cfg(any(test, feature = "test-support"))]
use hhhs_core::cover::ReachIndex;

use crate::ops::{AuthorId, LogHead, OpId, OpLanguage, SignedOp, SigningKey, VerifiedOpG};
use crate::store::{
    CausalPast, DecodedOp, FoldCtx, frame_signed, sync_root_of, unframe_signed,
};

// ===========================================================================
// §3.3 — the index-compressed bitset reach primitive.
// ===========================================================================

/// A dense strict-ancestor row: bit `j` is set iff the entry at window index `j` is
/// a strict causal ancestor of this row's entry.
///
/// This is the shape §3.3 prescribes to kill the perf suite's "reach RAM ≈ Θ(W²) ≈
/// ~2 MB @ W=256" scare — that number priced the closure as 32-byte hashes in
/// `BTreeSet`s (the [`Reach`](crate::Reach) memo shape). Θ(W²) **bits** is 8 KB @
/// W=256, affordable where Θ(W²) hashes was not; the ancestor closure of the whole
/// window is one W×W bit matrix, `is_ancestor` is one bit test, and each lift is one
/// row-OR (`row(op) = OR of row(prev) | bit(prev)`).
#[derive(Clone, Debug)]
struct BitRow {
    /// `ceil(width_bits / 64)` little-endian words. Fixed at the window cap so every
    /// row is OR-compatible.
    words: Box<[u64]>,
}

impl BitRow {
    /// A zeroed row wide enough for `width_bits` dense indices.
    fn zeroed(width_bits: usize) -> Self {
        Self {
            words: vec![0u64; width_bits.div_ceil(64)].into_boxed_slice(),
        }
    }

    /// Set bit `i` (dense window index `i` is an ancestor).
    fn set(&mut self, i: usize) {
        self.words[i / 64] |= 1u64 << (i % 64);
    }

    /// Test bit `i`.
    fn get(&self, i: usize) -> bool {
        (self.words[i / 64] >> (i % 64)) & 1 == 1
    }

    /// Union `other` into `self` (both rows share the cap-derived width).
    fn or_in(&mut self, other: &BitRow) {
        for (dst, src) in self.words.iter_mut().zip(other.words.iter()) {
            *dst |= *src;
        }
    }
}

// ===========================================================================
// §3.5 — the boundary oracle as a `CausalPast` backend.
// ===========================================================================

/// The window's causal-ancestry oracle: strict, present-only `is_ancestor` answered
/// by one bit test over the §3.3 closure, exposed as a [`CausalPast`] backend so the
/// *same* `L::fold` runs unchanged (design §3.5 — a third [`CausalPast`] backend
/// beside the cheap [`Reach`](crate::Reach) and the kernel `ReachIndex`).
///
/// For M3.0 (`N ≤ W`, no cut) the window holds the entire causal history, so this is
/// the whole `is_ancestor` oracle — there is no residue and no cut mask yet (those
/// are §3.2/§3.4, built in M3.1). `resolve` inherits kernel-identical register
/// resolution from [`CausalPast`]'s default (drop strict ancestors, max raw-bytes
/// tiebreak) — a pure function of this `is_ancestor` (§1.1).
///
/// Constructed only from a *complete* window ([`WindowedDag::windowed_reach`]); a
/// truncated window never reaches this type — [`WindowedStore::view`] fences first.
pub struct WindowedReach {
    /// entry → its dense window index. Present-only: an entry absent here is not in
    /// the window, and every query touching it answers `false`.
    index_of: BTreeMap<EntryHash, usize>,
    /// `rows[i]` = the strict, present-only causal ancestor set of the window entry
    /// with dense index `i`, as a bitset over dense indices (§3.3).
    rows: Vec<BitRow>,
}

impl CausalPast for WindowedReach {
    /// `true` iff `a` is a strict transitive `prevs`-ancestor of `b`, answered by a
    /// single bit test `rows[idx(b)][idx(a)]`. Strict by construction (a row never
    /// carries its own bit) and present-only (an out-of-window endpoint → `false`),
    /// so it agrees with `ReachIndex::is_ancestor` for every in-window pair — the
    /// property the §6.3 gate asserts directly.
    fn is_ancestor(&self, a: &EntryHash, b: &EntryHash) -> bool {
        match (self.index_of.get(a), self.index_of.get(b)) {
            (Some(&ai), Some(&bi)) => self.rows[bi].get(ai),
            _ => false,
        }
    }
}

// ===========================================================================
// §6.1 — `WindowedDag`: the L-free bounded-window kernel piece.
// ===========================================================================

/// A bounded suffix window of a causal DAG (cap `W`) with dense window indices and an
/// incremental §3.3 bitset reach.
///
/// Entries older than the window are simply not present (§1.3: present-only is
/// already the kernel doctrine, so a truncated DAG is a legal [`DagRead`] value).
/// While `N ≤ W` no entry has ever been evicted, so the window holds the entire DAG
/// and is exactly a [`MemDagStore`](hhhs_core::MemDagStore); the bitset reach is then
/// the whole causal history, exact for every pair.
///
/// **Completeness invariant.** [`WindowedDag::is_complete`] is `true` iff the window
/// still holds the entire causal history it has ever lifted (no eviction). This is
/// the fence predicate: the §3.3 closure is exact **iff** the window is complete,
/// because an evicted entry that is still referenced leaves a dangling `prev` whose
/// ancestry the bitset can no longer see. The reach structures ([`BitRow`] rows and
/// the dense index) are maintained only while complete and dropped on the first
/// truncation — a truncated window's reach is never read (the fence refuses), and
/// dropping it keeps memory bounded.
///
/// **Bounded, always.** On overflow the window evicts oldest-by-admission to stay at
/// `≤ W` entries. Correctness of any *produced* view is unaffected because
/// [`WindowedStore::view`] refuses to fold once truncated; eviction serves only the
/// leaf's RAM bound (§5), which M4 measures.
pub struct WindowedDag {
    /// The window cap `W`. Configurable per the design (leaf default W=128, bench
    /// axis 64/128/256, §5.3).
    cap: usize,
    /// Retained window entries by hash. `len ≤ cap`.
    entries: BTreeMap<EntryHash, Entry>,
    /// Admission order of retained entries (front = oldest). Drives eviction; while
    /// complete, `admission[i]` has dense window index `i`.
    admission: VecDeque<EntryHash>,
    /// entry → dense window index `0..cap`. Maintained only while complete; dropped
    /// on truncation.
    index_of: BTreeMap<EntryHash, usize>,
    /// `rows[i]` = §3.3 strict-ancestor closure of `admission[i]`. Maintained only
    /// while complete; dropped on truncation.
    rows: Vec<BitRow>,
    /// Per-entry admission epoch (1-based; `GrowthEpoch::INITIAL` = 0 is "before
    /// anything"), for the bounded-window [`WindowedDag::appended_since`].
    epochs: BTreeMap<EntryHash, u64>,
    /// Next admission epoch to hand out.
    next_epoch: u64,
    /// The greatest epoch ever evicted — the window's lower boundary. A delta query
    /// whose `since` predates this cannot be answered (§1.3, dag.rs:228-235).
    evicted_through_epoch: u64,
    /// `true` while the window holds its entire lifted causal history (no eviction).
    complete: bool,
}

impl WindowedDag {
    /// A bounded window with cap `W` (`cap ≥ 1`).
    pub fn with_cap(cap: usize) -> Self {
        assert!(cap >= 1, "windowed dag cap must be >= 1");
        Self {
            cap,
            entries: BTreeMap::new(),
            admission: VecDeque::new(),
            index_of: BTreeMap::new(),
            rows: Vec::new(),
            epochs: BTreeMap::new(),
            next_epoch: 1,
            evicted_through_epoch: 0,
            complete: true,
        }
    }

    /// The window cap `W`.
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// Number of retained window entries (`≤ W`).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether the window still holds its entire lifted causal history — the fence
    /// predicate. `true` while `N ≤ W` (no eviction); `false` the instant truncation
    /// occurs, after which the §3.3 closure is no longer exact and
    /// [`WindowedStore::view`] refuses to fold (§1.3).
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// The retained entry hashes (the cut-scoped identity set the window's
    /// `sync_root`/RBSR is over, §4.2).
    pub fn retained_hashes(&self) -> BTreeSet<EntryHash> {
        self.entries.keys().copied().collect()
    }

    /// Admit `entry` into the window, returning the entries this admission **evicted**
    /// (empty while `N ≤ W`).
    ///
    /// While complete and within cap, the entry is assigned the next dense index and
    /// its §3.3 closure row is built incrementally: `row = OR over present prevs p of
    /// (rows[idx(p)] | bit(idx(p)))`. Because the store lifts an op only once every
    /// `prev` is present (strict deferral) and every `prev` was admitted earlier
    /// (smaller index), the row is exact — the standard memoized-topo closure, in
    /// bits.
    ///
    /// On overflow the window drops to truncated mode: `complete` flips to `false`,
    /// the reach structures are freed (a truncated window's reach is never read), and
    /// oldest-by-admission entries are evicted until `len ≤ cap`.
    pub fn append_capped(&mut self, entry: &Entry) -> Vec<EntryHash> {
        let hash = entry.hash();
        if self.entries.contains_key(&hash) {
            return Vec::new(); // duplicate — no growth, no eviction
        }

        let epoch = self.next_epoch;
        self.next_epoch += 1;
        self.entries.insert(hash, entry.clone());
        self.epochs.insert(hash, epoch);
        self.admission.push_back(hash);

        if self.complete {
            // Dense index = admission position while complete. `< cap` ⇒ the window
            // still fits its full history in the W×W closure.
            let idx = self.admission.len() - 1;
            if idx < self.cap {
                let mut row = BitRow::zeroed(self.cap);
                for prev in &entry.header.prevs.0 {
                    if let Some(&pidx) = self.index_of.get(prev) {
                        row.set(pidx);
                        row.or_in(&self.rows[pidx]);
                    }
                    // present-only: an absent (already-evicted) prev contributes
                    // nothing — but while complete no prev is ever absent.
                }
                self.rows.push(row);
                self.index_of.insert(hash, idx);
            } else {
                // The (cap+1)-th distinct entry overflows the window: truncate.
                self.complete = false;
                self.rows = Vec::new();
                self.index_of = BTreeMap::new();
            }
        }

        // Enforce the cap (bounded memory, §5). Never evicts the just-admitted entry
        // (it is at the back of `admission`); with cap ≥ 1 the newest always stays.
        let mut evicted = Vec::new();
        while self.entries.len() > self.cap {
            let oldest = self
                .admission
                .pop_front()
                .expect("over-cap window has an oldest entry");
            self.entries.remove(&oldest);
            if let Some(gone) = self.epochs.remove(&oldest) {
                self.evicted_through_epoch = self.evicted_through_epoch.max(gone);
            }
            evicted.push(oldest);
        }
        evicted
    }

    /// The bounded-window [`DagDelta::appended_since`](hhhs_core::DagDelta) contract
    /// (dag.rs:228-235), provided inherently.
    ///
    /// `Some(window suffix)` — entries admitted after `since`, in admission order —
    /// while `since` is at or after the window's lower boundary; **`None` past it**
    /// (`since` predates an evicted epoch, so the delta would be incomplete). `None`
    /// rather than an empty vec keeps "nothing changed" distinguishable from "I don't
    /// know", which is the escape hatch designed for exactly this store (§1.3) — the
    /// delta `hhhs-reactive`-class engines fall back from (§7).
    ///
    /// (Provided as an inherent method rather than the `DagDelta` trait impl, whose
    /// `Growth: Send + Sync` supertrait would force interior mutability this
    /// single-owner leaf store does not otherwise need; the trait impl is an M3.1
    /// follow-up under the reorg's n=2 promotion gate, §6.1. The contract is
    /// identical.)
    pub fn appended_since(&self, since: GrowthEpoch) -> Option<Vec<Entry>> {
        if since.get() < self.evicted_through_epoch {
            return None; // history after `since` has been evicted — cannot answer
        }
        let mut out: Vec<(u64, Entry)> = self
            .epochs
            .iter()
            .filter(|&(_, &epoch)| epoch > since.get())
            .filter_map(|(hash, &epoch)| self.entries.get(hash).map(|e| (epoch, e.clone())))
            .collect();
        out.sort_by_key(|(epoch, _)| *epoch); // admission order
        Some(out.into_iter().map(|(_, e)| e).collect())
    }

    /// Build the §3.5 boundary oracle from the *complete* window. Panics if the
    /// window has truncated — callers ([`WindowedStore::view`]) fence first.
    pub fn windowed_reach(&self) -> WindowedReach {
        assert!(
            self.complete,
            "windowed_reach over a truncated window: the §3.3 closure is no longer \
             exact (windowed-store-design.md §1.3)",
        );
        WindowedReach {
            index_of: self.index_of.clone(),
            rows: self.rows.clone(),
        }
    }
}

impl DagRead for WindowedDag {
    fn entry(&self, h: &EntryHash) -> Option<Entry> {
        self.entries.get(h).cloned()
    }

    fn contains(&self, h: &EntryHash) -> bool {
        self.entries.contains_key(h)
    }

    fn frontier(&self) -> Position {
        frontier_of(&self.entries)
    }

    fn entries_topo(&self) -> Vec<Entry> {
        topo_of(&self.entries)
    }

    fn all_hashes(&self) -> Vec<EntryHash> {
        self.entries.keys().copied().collect()
    }
}

/// Heads of the retained window: entries not referenced as a `prev` by any *retained*
/// entry. Present-only, so a window edge whose successor references an evicted entry
/// is unaffected. Mirrors `hhhs_core::dag::frontier_of`.
fn frontier_of(entries: &BTreeMap<EntryHash, Entry>) -> Position {
    let referenced: BTreeSet<EntryHash> = entries
        .values()
        .flat_map(|entry| entry.header.prevs.0.iter().copied())
        .collect();
    Position(
        entries
            .keys()
            .filter(|hash| !referenced.contains(hash))
            .copied()
            .collect(),
    )
}

/// Deterministic topological order over the retained entries (predecessors before
/// successors, ties by entry hash), counting only *present* `prevs` — so cut-dangling
/// `prevs` are legal (§1.3, mirroring `hhhs_core::dag::topo_of`).
fn topo_of(entries: &BTreeMap<EntryHash, Entry>) -> Vec<Entry> {
    let present: BTreeSet<EntryHash> = entries.keys().copied().collect();
    let mut indeg: BTreeMap<EntryHash, usize> = BTreeMap::new();
    let mut children: BTreeMap<EntryHash, Vec<EntryHash>> = BTreeMap::new();
    for (hash, entry) in entries {
        let degree = entry
            .header
            .prevs
            .0
            .iter()
            .filter(|prev| present.contains(*prev))
            .count();
        indeg.insert(*hash, degree);
        for prev in &entry.header.prevs.0 {
            if present.contains(prev) {
                children.entry(*prev).or_default().push(*hash);
            }
        }
    }
    let mut ready: BTreeSet<EntryHash> = indeg
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(hash, _)| *hash)
        .collect();
    let mut out = Vec::with_capacity(entries.len());
    while let Some(hash) = ready.iter().next().copied() {
        ready.remove(&hash);
        out.push(entries[&hash].clone());
        if let Some(next) = children.get(&hash) {
            let mut next = next.clone();
            next.sort();
            for child in next {
                let degree = indeg.get_mut(&child).expect("known child");
                *degree -= 1;
                if *degree == 0 {
                    ready.insert(child);
                }
            }
        }
    }
    out
}

// ===========================================================================
// §6.1 — `WindowedStore<L>`: the leaf-profile domain sibling of `Store<L>`.
// ===========================================================================

/// The leaf-profile sibling of [`Store<L>`](crate::Store): the same lift / strict-
/// deferral / drain machinery over a bounded [`WindowedDag`], a fenced
/// [`WindowedStore::view`], and a cut-scoped sync surface (§1.2, §4.2).
///
/// It is **byte-compatible by construction** with [`Store<L>`](crate::Store): it lifts
/// through the identical [`frame_signed`] framing, so the same op yields the same
/// [`EntryHash`] on a windowed leaf and a full peer — the precondition for
/// convergence (§4.1). While `N ≤ W` its retained set equals a full store's, so
/// `entry_hashes`, `sync_root`, `ops_root` and, above all, `view()` all match a
/// [`Store<L>`](crate::Store) fed the same ops (§2.6, the §6.3 gate).
pub struct WindowedStore<L: OpLanguage> {
    /// The bounded-window causal DAG. Identity ([`EntryHash`]) is fixed here.
    dag: WindowedDag,
    /// op id → the retained entry that lifts it.
    source_to_entry: BTreeMap<OpId, EntryHash>,
    /// retained entry → op id (inverse).
    entry_to_source: BTreeMap<EntryHash, OpId>,
    /// retained entry → decoded op — the map the fold iterates (§2.2).
    decoded: BTreeMap<EntryHash, DecodedOp<L>>,
    /// Per-author log head, so the local author can chain new commits. (The own-author
    /// head is checkpoint state that must survive compaction, §1.2; for M3.0 it is
    /// just the full head map.)
    heads: BTreeMap<AuthorId, LogHead>,
    /// Ops whose causal past is not all lifted yet — parked (strict deferral), drained
    /// after every successful lift.
    pending: Vec<VerifiedOpG<L>>,
}

impl<L: OpLanguage> WindowedStore<L> {
    /// A windowed store with cap `W` (`cap ≥ 1`).
    pub fn with_cap(cap: usize) -> Self {
        Self {
            dag: WindowedDag::with_cap(cap),
            source_to_entry: BTreeMap::new(),
            entry_to_source: BTreeMap::new(),
            decoded: BTreeMap::new(),
            heads: BTreeMap::new(),
            pending: Vec::new(),
        }
    }

    /// The window cap `W`.
    pub fn cap(&self) -> usize {
        self.dag.cap()
    }

    /// Whether the window still holds its entire lifted causal history — the fence
    /// predicate. `true` while `N ≤ W`; folding is exact iff this is `true`.
    pub fn is_complete(&self) -> bool {
        self.dag.is_complete()
    }

    /// Number of retained (materialized) ops (`≤ W`).
    pub fn len(&self) -> usize {
        self.source_to_entry.len()
    }

    pub fn is_empty(&self) -> bool {
        self.source_to_entry.is_empty()
    }

    /// The retained entry-hash identity set (cut-scoped, §4.2). While `N ≤ W` this
    /// equals a full [`Store<L>`](crate::Store)'s set for the same ops.
    pub fn entry_hashes(&self) -> BTreeSet<EntryHash> {
        self.entry_to_source.keys().copied().collect()
    }

    /// Ops parked awaiting their causal past (strict deferral). Zero after quiescence
    /// is the liveness invariant.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Whether an op is already lifted (retained) or parked.
    pub fn knows_op(&self, id: OpId) -> bool {
        self.source_to_entry.contains_key(&id) || self.pending.iter().any(|p| p.id() == id)
    }

    /// The retained entry lifting op `id`, if materialized (and not since evicted).
    pub fn lifted_entry(&self, id: OpId) -> Option<EntryHash> {
        self.source_to_entry.get(&id).copied()
    }

    /// The cut-scoped convergence digest over the retained entry-hash set (§4.2).
    ///
    /// Uses the identical [`sync_root_of`] definition a full peer uses, so for a
    /// session anchored at the window's boundary both sides digest the same set. While
    /// `N ≤ W` the retained set is the whole set, so this equals a full store's
    /// `sync_root` outright.
    pub fn sync_root(&self) -> [u8; 32] {
        sync_root_of(self.entry_to_source.keys())
    }

    /// The verbatim signed bytes of every retained op, keyed by lifting entry hash —
    /// the cut-scoped RBSR `Fetch` surface (§1.2). Recovered losslessly from the DAG
    /// payloads, byte-identical to what the author signed.
    pub fn signed_ops(&self) -> BTreeMap<EntryHash, SignedOp> {
        self.dag
            .entries_topo()
            .into_iter()
            .map(|entry| (entry.hash(), unframe_signed::<L>(&entry.payload)))
            .collect()
    }

    /// Signed bytes plus causal-entry predecessors for ONE retained entry (§1.2).
    pub fn repair_record(&self, hash: &EntryHash) -> Option<(SignedOp, Vec<EntryHash>)> {
        let entry = self.dag.entry(hash)?;
        Some((
            unframe_signed::<L>(&entry.payload),
            entry.header.prevs.0.iter().copied().collect(),
        ))
    }

    /// The op ids of the retained frontier — the causal horizon a new local op stamps
    /// into its `observed`. Narrow by construction (§1.2), deterministic (ascending
    /// entry-hash order).
    pub fn observed_frontier(&self) -> Vec<[u8; 32]> {
        self.dag
            .frontier()
            .0
            .iter()
            .filter_map(|entry| self.entry_to_source.get(entry).map(|id| id.0))
            .collect()
    }

    /// Lift a verified op into the window. Deduplicates, advances the author's head,
    /// parks on incomplete causal past (strict deferral), and drains the pending set
    /// after every successful lift. Identical control flow to
    /// [`Store::ingest_verified`](crate::Store::ingest_verified); the only difference
    /// is the bounded backing DAG.
    ///
    /// Returns the entries this call newly lifted. An empty return means the op parked
    /// (or is a duplicate). A parked op is not in [`WindowedStore::entry_hashes`].
    pub fn ingest_verified(&mut self, op: VerifiedOpG<L>) -> Vec<EntryHash> {
        let id = op.id();
        if self.source_to_entry.contains_key(&id) {
            return Vec::new();
        }
        if self.pending.iter().any(|p| p.id() == id) {
            return Vec::new();
        }
        self.advance_head(&op);
        self.pending.push(op);
        self.drain_pending()
    }

    /// Advance (never regress) the author's tracked head to the greatest seq seen.
    fn advance_head(&mut self, op: &VerifiedOpG<L>) {
        let advanced = op.advanced_head();
        let slot = self.heads.entry(op.author()).or_insert_with(LogHead::genesis);
        if advanced.next_seq > slot.next_seq {
            *slot = advanced;
        }
    }

    /// Resolve an op's `prevs` = `{ lift(backlink) } ∪ { lift(o) : o in observed }`
    /// against the *retained* window. `None` (defer) if any referenced op is not
    /// retained — including a deep-laggard reference below the window boundary, which
    /// parks (defer-never-reject). While `N ≤ W` no reference is ever below the
    /// boundary.
    fn resolve_prevs(&self, op: &VerifiedOpG<L>) -> Option<BTreeSet<EntryHash>> {
        let mut prevs = BTreeSet::new();
        if let Some(backlink) = op.backlink() {
            prevs.insert(*self.source_to_entry.get(&OpId(backlink))?);
        }
        for observed in op.observed() {
            prevs.insert(*self.source_to_entry.get(&OpId(*observed))?);
        }
        Some(prevs)
    }

    /// Try to lift one op. Returns its entry hash iff appended; `None` (no mutation) if
    /// its causal past is incomplete. On a successful lift that overflows the window,
    /// evicted entries are pruned from every retained-op map in lockstep with the
    /// [`WindowedDag`], keeping the store and its DAG's retained sets identical.
    fn try_lift(&mut self, op: &VerifiedOpG<L>) -> Option<EntryHash> {
        let prevs = self.resolve_prevs(op)?;
        let entry = Entry::new(frame_signed::<L>(&op.signed()), Position(prevs));
        let entry_hash = entry.hash();
        let evicted = self.dag.append_capped(&entry);

        let id = op.id();
        self.source_to_entry.insert(id, entry_hash);
        self.entry_to_source.insert(entry_hash, id);
        self.decoded.insert(
            entry_hash,
            DecodedOp::new(op.author(), op.payload().clone(), op.timestamp_ms(), op.seq_num()),
        );

        for gone in evicted {
            if let Some(gone_id) = self.entry_to_source.remove(&gone) {
                self.source_to_entry.remove(&gone_id);
            }
            self.decoded.remove(&gone);
        }
        Some(entry_hash)
    }

    /// Repeatedly attempt to lift parked ops until a full pass makes no progress.
    fn drain_pending(&mut self) -> Vec<EntryHash> {
        let mut lifted = Vec::new();
        loop {
            let parked = std::mem::take(&mut self.pending);
            let mut still_pending = Vec::with_capacity(parked.len());
            let mut progressed = false;
            for op in parked {
                if let Some(hash) = self.try_lift(&op) {
                    lifted.push(hash);
                    progressed = true;
                } else {
                    still_pending.push(op);
                }
            }
            self.pending = still_pending;
            if !progressed {
                break;
            }
        }
        lifted
    }

    /// Author and sign a local op without mutating the projection (two-phase commit,
    /// for durable runtimes). Stamps the retained frontier as `observed`.
    pub fn prepare_commit(
        &self,
        key: &SigningKey,
        topic: &str,
        ts_micros: u64,
        op: L::Op,
    ) -> SignedOp {
        use crate::ops::{VersionedOpG, sign_versioned_op};
        let author = AuthorId(*key.verifying_key().as_bytes());
        let head = self.heads.get(&author).copied().unwrap_or_else(LogHead::genesis);
        let observed = self.observed_frontier();
        let versioned =
            VersionedOpG::<L>::current_for_topic(op, ts_micros, topic).observing(observed);
        let (signed, _advanced) = sign_versioned_op(key, &head, versioned);
        signed
    }

    /// Author, sign, verify, and ingest a new local op, returning the signed bytes.
    /// In-memory/test convenience; durable runtimes call [`WindowedStore::prepare_commit`]
    /// then persist then ingest.
    pub fn commit(
        &mut self,
        key: &SigningKey,
        topic: &str,
        ts_micros: u64,
        op: L::Op,
    ) -> SignedOp {
        use crate::ops::verify_signed_op_in;
        let signed = self.prepare_commit(key, topic, ts_micros, op);
        let verified = verify_signed_op_in::<L>(&signed).expect("a just-signed op verifies");
        self.ingest_verified(verified);
        signed
    }

    /// The bounded-window [`DagDelta::appended_since`](hhhs_core::DagDelta) contract,
    /// forwarded from the backing [`WindowedDag`]: `Some` inside the window, `None`
    /// past its boundary (§1.3, §7).
    pub fn appended_since(&self, since: GrowthEpoch) -> Option<Vec<Entry>> {
        self.dag.appended_since(since)
    }

    /// Materialize the read model from the window — **fenced** (§1.3, §6.2 delta 6).
    ///
    /// The fold runs the byte-identical `L::fold` over the retained-op map through the
    /// window-complete [`WindowedReach`] backend, assembled via the public
    /// [`FoldCtx::over`](crate::FoldCtx::over) constructor — so windowed-vs-full
    /// equivalence is *structural* (same fold, only the [`CausalPast`] backend
    /// differs, §3.5).
    ///
    /// # The fence
    ///
    /// It **hard-refuses (panics)** if the window has truncated
    /// ([`WindowedStore::is_complete`] is `false`). A truncated window cannot answer
    /// `is_ancestor` across its boundary, and a plain fold would silently mis-answer
    /// it — a *wrong view, not an error* (§1.3). M3.0 is claimed correct only for
    /// `N ≤ W`, and this is the guard that makes the claim safe rather than a silent
    /// trap. Use [`WindowedStore::try_view`] for the non-panicking form, or
    /// [`WindowedStore::is_complete`] to check first.
    pub fn view(&self) -> L::View {
        assert!(
            self.dag.is_complete(),
            "windowed view fence: the window has truncated (N > W). A fold over a \
             truncated window would silently mis-answer is_ancestor across the cut \
             (a wrong view, not an error — windowed-store-design.md §1.3, §6.2 delta \
             6). M3.0 is exact only for N <= W; folding past W needs M3.1 compaction \
             with the monotone-shadowing retention of §2.4-2.5.",
        );
        let reach = self.dag.windowed_reach();
        let ctx = FoldCtx::over(&self.decoded, &self.entry_to_source, Box::new(reach));
        L::fold(&ctx)
    }

    /// The non-panicking form of [`WindowedStore::view`]: `Some(view)` while the
    /// window is complete (`N ≤ W`), `None` once truncated. The fence, as a value.
    pub fn try_view(&self) -> Option<L::View> {
        if self.dag.is_complete() {
            Some(self.view())
        } else {
            None
        }
    }

    /// The §3.5 boundary oracle over the current (complete) window, exposed so the
    /// §6.3 gate can assert `WindowedReach::is_ancestor ≡ ReachIndex::is_ancestor` for
    /// every in-window pair. Panics if truncated.
    pub fn windowed_reach(&self) -> WindowedReach {
        self.dag.windowed_reach()
    }

    /// The backing bounded-window DAG, read-only. Exposed (feature `test-support`) so
    /// the equivalence gate can build a kernel `ReachIndex` over the window and
    /// cross-check the bitset reach.
    #[cfg(any(test, feature = "test-support"))]
    pub fn dag(&self) -> &WindowedDag {
        &self.dag
    }

    /// The reference projection: the identical `L::fold` driven by the kernel
    /// `ReachIndex` over the window instead of the [`WindowedReach`] bitset — the
    /// windowed analogue of [`Store::view_reference`](crate::Store::view_reference).
    /// While complete this equals both [`WindowedStore::view`] and a full store's
    /// `view()`, which is the root-of-trust cross-check the §6.3 gate makes. Fenced.
    #[cfg(any(test, feature = "test-support"))]
    pub fn view_reference(&self) -> L::View {
        assert!(
            self.dag.is_complete(),
            "windowed view_reference fence: truncated window (windowed-store-design.md §1.3)",
        );
        let snapshot = self.dag.snapshot();
        let reach = ReachIndex::new(&snapshot);
        let ctx = FoldCtx::over(&self.decoded, &self.entry_to_source, Box::new(reach));
        L::fold(&ctx)
    }
}

/// Generic Merkle commitments over the retained entry-hash set (feature `merkle`),
/// mirroring [`Store<L>`](crate::Store)'s. `ops_root` over the window is the **window
/// `ops_root`** of §4.3 (comparable cut-scoped); while `N ≤ W` it equals a full
/// store's outright.
#[cfg(feature = "merkle")]
impl<L: OpLanguage> WindowedStore<L> {
    /// The window `ops_root`: a canonical blake3-256 Merkle commitment to the retained
    /// entry-hash set (§4.3), over the same `entry_to_source.keys()` iterator
    /// [`WindowedStore::sync_root`] digests.
    pub fn ops_root(&self) -> [u8; 32] {
        crate::merkle::ops_root_of(self.entry_to_source.keys())
    }

    /// An inclusion / non-inclusion proof for `entry` against
    /// [`WindowedStore::ops_root`] — producible only for retained entries (§4.3).
    pub fn prove_op(&self, entry: &EntryHash) -> radix_immutable::Proof {
        crate::merkle::prove_op(self.entry_to_source.keys(), entry)
    }
}
