//! `windowed_equiv` — the M3.0 hard correctness gate
//! (`docs/vision/windowed-store-design.md` §6.3).
//!
//! The whole M3.0 claim in one suite: while `N ≤ W`, the bounded
//! [`WindowedStore`] folds **byte-identically** to a full [`Store`], both agreeing
//! with the kernel `ReachIndex` reference oracle — under shuffled arrival,
//! multi-author concurrent add/remove races, narrow-horizon laggards, equivocating
//! (forked) author logs, and object resurrection straddling the window. Plus the
//! §1.3 fence: the instant `N > W`, the window truncates and `view()` **refuses**
//! rather than silently mis-answer `is_ancestor` across the cut.
//!
//! Style matches `tests/second_domain.rs` and walkie's `reach_equiv`: seeded
//! SplitMix64, shuffled ingest, `view() == view_reference()` (kernel `ReachIndex`)
//! as the root of trust. The test alphabet [`WinLang`] exercises every combinator
//! the design's retention lemmas cover (§6.3): content-keyed add-wins (A), a full-
//! horizon register (R), a sub-horizon-gated read (R′, the lock), and an object
//! graph with resurrection (P/M) — so every lemma has teeth in one alphabet, even
//! though M3.0 discards nothing (retain-everything, §7).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use hhhs_core::cover::ReachIndex;
use hhhs_core::{DagRead, GrowthEpoch};

use tutti_core::{
    AuthorId, CausalPast, EntryHash, FoldCtx, LogHead, OpId, OpLanguage, SignedOp, SigningKey,
    Store, VerifiedOpG, VersionedOpG, WindowedStore, sign_versioned_op, signing_key_from_seed,
    verify_signed_op_in,
};

// ===========================================================================
// The fourth OpLanguage: every retention-lemma combinator in one alphabet (§6.3).
// ===========================================================================

/// A(dd)/R(em): content-keyed add-wins over a small keyspace (degrees-shaped).
/// SetReg: a full-horizon register (tuning/config-shaped, R). SetLock: a register
/// whose value gates object ops per their causal past (pieces_locked-shaped, R′).
/// Put/Move/Del/Undel: an object graph with resurrection (pieces-shaped, P/M) — an
/// object id is its `Put` op id; `Del` kills observed asserts; `Undel` overrides the
/// `Del` it observed, resurrecting the object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum WinOp {
    Add { key: u16 },
    Rem { key: u16 },
    SetReg { slot: u8, val: u32 },
    SetLock { locked: bool },
    Put { emoji: u8, pos: u32 },
    Move { obj: OpId, pos: u32 },
    Del { obj: OpId },
    Undel { del: OpId },
}

/// The materialized read model — small, primitive-typed fields so a canonical
/// `state_root` (below) is unambiguous.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct WinView {
    /// Add-wins live content keys.
    live_keys: BTreeSet<u16>,
    /// Per live key, the authors holding a live add of it.
    key_authors: BTreeMap<u16, BTreeSet<AuthorId>>,
    /// Full-horizon register winners per slot (R).
    reg: BTreeMap<u8, u32>,
    /// Full-horizon lock-register read (the view field).
    locked: bool,
    /// Live objects: id → (emoji, resolved position) (P/M).
    objects: BTreeMap<OpId, (u8, u32)>,
}

struct WinLang;

impl OpLanguage for WinLang {
    type Op = WinOp;
    type View = WinView;

    const SCHEMA_VERSION: u16 = 1;
    const ENTRY_FRAME_MAGIC: &'static [u8] = b"tutti.winlang.entry/1";
    const WIRE_MAGIC: &'static [u8] = b"tutti.winlang.wire/1\0";
    const MAX_PAYLOAD_BYTES: usize = 64 * 1024;

    fn validate_wire(_op: &WinOp) -> Result<(), String> {
        Ok(())
    }

    /// Registers → add-wins keys → objects, mirroring walkie's staged
    /// `with_registers`/`with_pitches`/`with_pieces` composition (src/room/store.rs).
    /// Reads ancestry ONLY through the erased [`FoldCtx`] combinators, so the exact
    /// same code runs on the cheap `Reach`, the kernel `ReachIndex`, and the
    /// `WindowedReach` bitset — the equivalence the gate asserts.
    fn fold(ctx: &FoldCtx<'_, Self>) -> WinView {
        let mut reg_writes: BTreeMap<u8, BTreeSet<EntryHash>> = BTreeMap::new();
        let mut lock_writes: BTreeSet<EntryHash> = BTreeSet::new();
        let mut adds: BTreeMap<u16, Vec<EntryHash>> = BTreeMap::new();
        let mut rems: BTreeMap<u16, Vec<EntryHash>> = BTreeMap::new();
        let mut puts: Vec<(EntryHash, OpId, u8, u32)> = Vec::new();
        let mut moves: Vec<(EntryHash, OpId, u32)> = Vec::new();
        let mut dels: Vec<(EntryHash, OpId, OpId)> = Vec::new();
        let mut undels: Vec<(EntryHash, OpId)> = Vec::new();

        for (entry, decoded) in ctx.decoded() {
            let op_id = ctx.op_id(entry);
            match decoded.op() {
                WinOp::Add { key } => adds.entry(*key).or_default().push(*entry),
                WinOp::Rem { key } => rems.entry(*key).or_default().push(*entry),
                WinOp::SetReg { slot, .. } => {
                    reg_writes.entry(*slot).or_default().insert(*entry);
                }
                WinOp::SetLock { .. } => {
                    lock_writes.insert(*entry);
                }
                WinOp::Put { emoji, pos } => puts.push((*entry, op_id, *emoji, *pos)),
                WinOp::Move { obj, pos } => moves.push((*entry, *obj, *pos)),
                WinOp::Del { obj } => dels.push((*entry, op_id, *obj)),
                WinOp::Undel { del } => undels.push((*entry, *del)),
            }
        }

        // R — full-horizon registers.
        let mut reg = BTreeMap::new();
        for (slot, candidates) in &reg_writes {
            if let Some(winner) = ctx.resolve(candidates)
                && let WinOp::SetReg { val, .. } = ctx.decoded()[&winner].op()
            {
                reg.insert(*slot, *val);
            }
        }
        // Full-horizon lock read (the view field).
        let locked = ctx
            .resolve(&lock_writes)
            .map(|w| matches!(ctx.decoded()[&w].op(), WinOp::SetLock { locked: true }))
            .unwrap_or(false);

        // R′ — the sub-horizon lock gate: resolve the lock register over ONLY an op's
        // causal ancestors. A move/del/undel is suppressed iff an active lock sits in
        // its causal past (a lock CONCURRENT with the op does not suppress it).
        let locked_as_of = |op: &EntryHash| -> bool {
            let observed: BTreeSet<EntryHash> = lock_writes
                .iter()
                .copied()
                .filter(|w| ctx.is_ancestor(w, op))
                .collect();
            ctx.resolve(&observed)
                .is_some_and(|w| matches!(ctx.decoded()[&w].op(), WinOp::SetLock { locked: true }))
        };

        // A — add-wins content keys.
        let mut live_keys = BTreeSet::new();
        let mut key_authors = BTreeMap::new();
        for (key, add_entries) in &adds {
            let key_rems = rems.get(key).map(Vec::as_slice).unwrap_or(&[]);
            let mut authors: BTreeSet<AuthorId> = BTreeSet::new();
            for add in add_entries {
                let killed = key_rems.iter().any(|r| ctx.is_ancestor(add, r));
                if !killed {
                    authors.insert(ctx.decoded()[add].author());
                }
            }
            if !authors.is_empty() {
                live_keys.insert(*key);
                key_authors.insert(*key, authors);
            }
        }

        // P/M — objects with resurrection + lock-gated moves/dels/undels + a position
        // register over the surviving adds.
        let mut objects = BTreeMap::new();
        for (put_entry, obj_id, emoji, put_pos) in &puts {
            let effective_removes: Vec<EntryHash> = dels
                .iter()
                .filter(|(_, _, target)| target == obj_id)
                .filter(|(del_entry, del_id, _)| {
                    if locked_as_of(del_entry) {
                        return false;
                    }
                    let overridden = undels.iter().any(|(un_entry, target_del)| {
                        target_del == del_id
                            && ctx.is_ancestor(del_entry, un_entry)
                            && !locked_as_of(un_entry)
                    });
                    !overridden
                })
                .map(|(del_entry, _, _)| *del_entry)
                .collect();

            let survives = |add: &EntryHash| {
                !effective_removes.iter().any(|r| ctx.is_ancestor(add, r))
            };
            let mut surviving: BTreeSet<EntryHash> = BTreeSet::new();
            if survives(put_entry) {
                surviving.insert(*put_entry);
            }
            for (move_entry, _, _) in moves.iter().filter(|(_, target, _)| target == obj_id) {
                if !locked_as_of(move_entry) && survives(move_entry) {
                    surviving.insert(*move_entry);
                }
            }
            if surviving.is_empty() {
                continue;
            }
            let pos = ctx
                .resolve(&surviving)
                .map(|w| match ctx.decoded()[&w].op() {
                    WinOp::Put { pos, .. } | WinOp::Move { pos, .. } => *pos,
                    _ => unreachable!("a surviving add is a Put or Move"),
                })
                .unwrap_or(*put_pos);
            objects.insert(*obj_id, (*emoji, pos));
        }

        WinView {
            live_keys,
            key_authors,
            reg,
            locked,
            objects,
        }
    }
}

/// A canonical blake3-256 digest of a [`WinView`] — a stand-in `state_root` (§4.3:
/// "`state_root` survives windowing completely intact"; the generic `L::View:
/// Canonical` bound is not wired in tutti-core, so the gate defines its own canonical
/// encoding). A pure, deterministic function of the sorted view fields.
fn win_state_root(v: &WinView) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"tutti.winlang.state/1");
    h.update(&[v.locked as u8]);
    h.update(b"|keys|");
    for k in &v.live_keys {
        h.update(&k.to_le_bytes());
    }
    h.update(b"|key_authors|");
    for (k, authors) in &v.key_authors {
        h.update(&k.to_le_bytes());
        for a in authors {
            h.update(&a.0);
        }
        h.update(b";");
    }
    h.update(b"|reg|");
    for (slot, val) in &v.reg {
        h.update(&[*slot]);
        h.update(&val.to_le_bytes());
    }
    h.update(b"|obj|");
    for (id, (emoji, pos)) in &v.objects {
        h.update(&id.0);
        h.update(&[*emoji]);
        h.update(&pos.to_le_bytes());
    }
    *h.finalize().as_bytes()
}

// ===========================================================================
// Deterministic harness — seeded SplitMix64, per-author log heads, laggard +
// equivocation injection. No Date::now / no rand crate.
// ===========================================================================

const TOPIC: &str = "winlang-windowed-equiv";
const TS_BASE: u64 = 1_700_000_000_000_000;

struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03)
    }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn upto(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
    fn pct(&mut self, p: u64) -> bool {
        self.next() % 100 < p
    }
}

/// Deterministic Fisher-Yates over a seeded PRNG.
fn shuffled(ops: &[SignedOp], seed: u64) -> Vec<SignedOp> {
    let mut items = ops.to_vec();
    let mut rng = Rng::new(seed ^ 0x5EED_5EED_5EED_5EED);
    for i in (1..items.len()).rev() {
        let j = rng.upto(i + 1);
        items.swap(i, j);
    }
    items
}

/// A per-author signing log head, so the harness controls each op's `observed`
/// horizon (laggards) and can fork a log (equivocation) — capabilities `commit`'s
/// auto-frontier does not give.
struct Author {
    key: SigningKey,
    head: LogHead,
}
impl Author {
    fn new(seed: u8) -> Self {
        Self {
            key: signing_key_from_seed(&[seed; 32]),
            head: LogHead::genesis(),
        }
    }
    fn author_id(&self) -> AuthorId {
        AuthorId(*self.key.verifying_key().as_bytes())
    }
    /// Sign, advancing this author's head. Returns the wire bytes + the op's id.
    fn sign(&mut self, ts: u64, observed: Vec<[u8; 32]>, op: WinOp) -> (SignedOp, [u8; 32]) {
        let versioned =
            VersionedOpG::<WinLang>::current_for_topic(op, ts, TOPIC).observing(observed);
        let (signed, advanced) = sign_versioned_op(&self.key, &self.head, versioned);
        self.head = advanced;
        let id = verify_signed_op_in::<WinLang>(&signed).expect("verifies").hash();
        (signed, id)
    }
    /// Sign from an explicit (stale) head WITHOUT advancing — a forked-log op sharing
    /// a seq/backlink with the op that really advanced the head (equivocation, which
    /// verification admits: ops.rs:626-630). Distinct content ⇒ distinct hash.
    fn fork(&self, head: LogHead, ts: u64, observed: Vec<[u8; 32]>, op: WinOp) -> (SignedOp, [u8; 32]) {
        let versioned =
            VersionedOpG::<WinLang>::current_for_topic(op, ts, TOPIC).observing(observed);
        let (signed, _adv) = sign_versioned_op(&self.key, &head, versioned);
        let id = verify_signed_op_in::<WinLang>(&signed).expect("verifies").hash();
        (signed, id)
    }
}

/// A seeded-random multi-author causal history in valid causal order, with laggard
/// horizons, equivocation forks, and object resurrection races (§6.3 "Drive").
///
/// Every op's `observed` references only PRIOR op hashes, so ingest in generation
/// order never parks and any shuffle drains — while the horizons range from the full
/// recent frontier to deliberately stale early prefixes (laggards), so removes /
/// unremoves / register writes straddle every point of history both causally and
/// temporally.
fn build_history(seed: u64, authors: usize, steps: usize) -> Vec<SignedOp> {
    let mut rng = Rng::new(seed);
    let mut peers: Vec<Author> = (0..authors).map(|i| Author::new((i + 1) as u8)).collect();

    let mut out: Vec<SignedOp> = Vec::new();
    let mut op_hashes: Vec<[u8; 32]> = Vec::new();
    let mut put_ids: Vec<OpId> = Vec::new();
    let mut del_ids: Vec<OpId> = Vec::new();

    for step in 0..steps {
        let author = rng.upto(authors);
        let ts = TS_BASE + step as u64;

        // Horizon: usually recent (last few ops), sometimes a STALE early prefix
        // (laggard) — the narrow-horizon source of add-wins concurrency and the R′
        // sub-horizon-read hazard.
        let laggard = rng.pct(22);
        let mut observed: Vec<[u8; 32]> = Vec::new();
        if !op_hashes.is_empty() {
            let (lo, hi) = if laggard {
                (0, (op_hashes.len() / 3).max(1))
            } else {
                (op_hashes.len().saturating_sub(6), op_hashes.len())
            };
            for h in &op_hashes[lo..hi] {
                if observed.len() >= 4 {
                    break;
                }
                if rng.pct(45) {
                    observed.push(*h);
                }
            }
        }

        let op = match rng.upto(12) {
            0..=2 => WinOp::Add { key: rng.upto(4) as u16 },
            3 => WinOp::Rem { key: rng.upto(4) as u16 },
            4 => WinOp::SetReg { slot: rng.upto(3) as u8, val: rng.upto(1000) as u32 },
            5 => WinOp::SetLock { locked: rng.pct(50) },
            6 => WinOp::Put { emoji: rng.upto(4) as u8, pos: rng.upto(88) as u32 },
            7 => match pick(&put_ids, &mut rng) {
                Some(obj) => WinOp::Move { obj, pos: rng.upto(88) as u32 },
                None => WinOp::Add { key: 0 },
            },
            8 | 9 => match pick(&put_ids, &mut rng) {
                Some(obj) => WinOp::Del { obj },
                None => WinOp::Add { key: 1 },
            },
            _ => match pick(&del_ids, &mut rng) {
                Some(del) => WinOp::Undel { del },
                None => WinOp::Add { key: 2 },
            },
        };

        let head_before = peers[author].head;
        let (signed, id) = peers[author].sign(ts, observed.clone(), op.clone());
        match &op {
            WinOp::Put { .. } => put_ids.push(OpId(id)),
            WinOp::Del { .. } => del_ids.push(OpId(id)),
            _ => {}
        }
        op_hashes.push(id);
        out.push(signed);

        // Equivocation: ~1-in-13 steps, the same author ALSO signs a forked op from
        // the pre-advance head — a genuine antichain of per-author maxima (§2.5-A3).
        if rng.pct(8) {
            let fork_op = WinOp::Add { key: rng.upto(4) as u16 };
            let (fsigned, fid) =
                peers[author].fork(head_before, ts + 500, observed, fork_op);
            op_hashes.push(fid);
            out.push(fsigned);
        }
    }
    out
}

fn pick(ids: &[OpId], rng: &mut Rng) -> Option<OpId> {
    if ids.is_empty() {
        None
    } else {
        Some(ids[rng.upto(ids.len())])
    }
}

fn verified(signed: &SignedOp) -> VerifiedOpG<WinLang> {
    verify_signed_op_in::<WinLang>(signed).expect("op verifies")
}

fn ingest_full(ops: &[SignedOp]) -> Store<WinLang> {
    let mut store = Store::new();
    for signed in ops {
        store.ingest_verified(verified(signed));
    }
    store
}

fn ingest_windowed(cap: usize, ops: &[SignedOp]) -> WindowedStore<WinLang> {
    let mut store = WindowedStore::with_cap(cap);
    for signed in ops {
        store.ingest_verified(verified(signed));
    }
    store
}

// ===========================================================================
// The gate.
// ===========================================================================

/// The cheap half of the bundle: view triple-equality against the kernel oracle, and
/// identity of `state_root` / entry-set / `sync_root` between a windowed store and a
/// full store fed the same ops with `N ≤ W` (window complete).
fn assert_view_equiv(windowed: &WindowedStore<WinLang>, full: &Store<WinLang>, label: &str) {
    assert!(windowed.is_complete(), "{label}: window must be complete (N <= W)");
    assert_eq!(windowed.pending_len(), 0, "{label}: windowed liveness (pending == 0)");
    assert_eq!(full.pending_len(), 0, "{label}: full liveness (pending == 0)");

    // The retained set equals the full set while N <= W.
    assert_eq!(windowed.entry_hashes(), full.entry_hashes(), "{label}: retained set == full set");

    // View triple-equality: windowed bitset == full cheap Reach == kernel ReachIndex.
    let wv = windowed.view();
    let fv = full.view();
    assert_eq!(wv, fv, "{label}: windowed.view() != full.view()");
    assert_eq!(fv, full.view_reference(), "{label}: full.view() != full.view_reference() (kernel oracle)");
    assert_eq!(wv, windowed.view_reference(), "{label}: windowed.view() != windowed.view_reference()");

    // state_root survives windowing intact (§4.3).
    assert_eq!(
        win_state_root(&wv),
        win_state_root(&fv),
        "{label}: windowed state_root != full state_root"
    );

    // Cut-scoped sync_root over the retained set == full sync_root (identical set).
    assert_eq!(windowed.sync_root(), full.sync_root(), "{label}: sync_root mismatch");
}

/// The expensive half: the bitset `is_ancestor` ≡ kernel `ReachIndex::is_ancestor`
/// for EVERY in-window pair (§6.3) — the O(N²) reach cross-check, run on a focused
/// subset of the random cases (and on every small targeted vector).
fn assert_reach_equiv(windowed: &WindowedStore<WinLang>, full: &Store<WinLang>, label: &str) {
    let wreach = windowed.windowed_reach();
    let kernel = ReachIndex::new(&full.dag().snapshot());
    let hashes: Vec<EntryHash> = full.entry_hashes().into_iter().collect();
    for a in &hashes {
        for b in &hashes {
            assert_eq!(
                CausalPast::is_ancestor(&wreach, a, b),
                ReachIndex::is_ancestor(&kernel, a, b),
                "{label}: WindowedReach vs ReachIndex is_ancestor disagreement",
            );
        }
    }
}

/// The FULL bundle (cheap + reach) — used by the small targeted vectors.
fn assert_windowed_equiv(windowed: &WindowedStore<WinLang>, full: &Store<WinLang>, label: &str) {
    assert_view_equiv(windowed, full, label);
    assert_reach_equiv(windowed, full, label);
}

/// THE gate: over many seeded histories, two replicas with DIFFERENT W (both `N ≤ W`)
/// ingesting SHUFFLED arrivals equal each other, the full store, and the kernel
/// oracle — including `ops_root` equality over the retained set (feature `merkle`).
#[test]
fn windowed_equiv_matches_full_history() {
    let mut view_checks = 0usize;
    let mut reach_checks = 0usize;
    for seed in 0..8u64 {
        let authors = 3 + (seed as usize % 2);
        let steps = 90 + (seed as usize % 5) * 20;
        let ops = build_history(seed, authors, steps);
        let n = ops.len();
        // Two DIFFERENT windows, both comfortably >= N so nothing truncates.
        let w1 = n + 1;
        let w2 = n + 137;

        for (order_idx, order_seed) in [1u64, 999_983].into_iter().enumerate() {
            let arrival = shuffled(&ops, seed ^ order_seed);
            let full = ingest_full(&arrival);
            let windowed_a = ingest_windowed(w1, &arrival);
            let windowed_b = ingest_windowed(w2, &shuffled(&ops, seed ^ order_seed ^ 0xABCD));

            assert_view_equiv(&windowed_a, &full, &format!("seed {seed} W={w1} order {order_seed}"));
            assert_view_equiv(&windowed_b, &full, &format!("seed {seed} W={w2} order {order_seed}"));

            // §4.1 — different W, different shuffles, SAME op-set ⇒ equal views.
            assert_eq!(
                windowed_a.view(),
                windowed_b.view(),
                "seed {seed}: replicas with different W diverged (§4.1)"
            );

            // ops_root over the retained set == full ops_root (identical set, §4.3).
            #[cfg(feature = "merkle")]
            {
                assert_eq!(windowed_a.ops_root(), full.ops_root(), "seed {seed}: windowed ops_root != full");
                assert_eq!(windowed_b.ops_root(), full.ops_root(), "seed {seed}: windowed ops_root != full");
            }
            view_checks += 1;

            // The O(N²) bitset-vs-ReachIndex pairwise cross-check, once per seed (on
            // the first arrival order) — every adversarial DAG gets its full pairwise
            // audit; the small targeted vectors below add more, cheaply.
            if order_idx == 0 {
                assert_reach_equiv(
                    &windowed_a,
                    &full,
                    &format!("seed {seed} W={w1} pairwise"),
                );
                reach_checks += 1;
            }
        }
        assert!(!ops.is_empty());
    }
    println!(
        "PASS windowed_equiv: {view_checks} (seed x order) view/state/root/set checks + \
         {reach_checks} full O(N^2) is_ancestor pairwise audits; windowed.view() == full.view() == \
         kernel ReachIndex oracle; bitset is_ancestor == ReachIndex on all in-window pairs; \
         state_root/sync_root/ops_root/entry-set identical; different-W replicas converge"
    );
}

/// §4.1 convergence, isolated: two windowed replicas with different W and randomized
/// arrival orders reach equal views at every quiescent point across MANY seeds.
#[test]
fn different_windows_converge() {
    let mut cases = 0usize;
    for seed in 0..12u64 {
        let ops = build_history(seed ^ 0xC0FFEE, 4, 120 + (seed as usize % 7) * 10);
        let n = ops.len();
        let a = ingest_windowed(n + 3, &shuffled(&ops, seed));
        let b = ingest_windowed(n + 200, &shuffled(&ops, seed ^ 0xF00D));
        assert_eq!(a.pending_len(), 0);
        assert_eq!(b.pending_len(), 0);
        assert!(a.is_complete() && b.is_complete());
        assert_eq!(a.view(), b.view(), "seed {seed}: different-W windows diverged");
        assert_eq!(a.entry_hashes(), b.entry_hashes());
        cases += 1;
    }
    println!("PASS different_windows_converge: {cases} seeds, different-W replicas converge (§4.1)");
}

// ===========================================================================
// Targeted vectors the design names (§6.3 "Targeted vectors").
// ===========================================================================

/// old-add-killed-by-a-later-remove, both retained (`N ≤ W`): a remove observing an
/// early add kills it across a long span of intervening ops — the exact "old add
/// killed by a future remove" the design opens with (§2.3), answered identically by
/// the window's bitset and the kernel.
#[test]
fn old_add_killed_by_later_remove_in_window() {
    let mut a = Author::new(1);
    let mut b = Author::new(2);
    let mut ops: Vec<SignedOp> = Vec::new();

    let (add, add_id) = a.sign(TS_BASE, vec![], WinOp::Add { key: 9 });
    ops.push(add);
    // A concurrent add of the same key by B (add-wins keeps the key alive via B).
    let (badd, _bid) = b.sign(TS_BASE + 1, vec![], WinOp::Add { key: 9 });
    ops.push(badd);
    // 40 unrelated ops widen the span between the add and its remove.
    let mut hashes = vec![add_id];
    for i in 0..40u64 {
        let (o, h) = a.sign(TS_BASE + 10 + i, vec![], WinOp::Add { key: (i % 3) as u16 });
        hashes.push(h);
        ops.push(o);
    }
    // Remove observing ONLY the first add ⇒ kills a's add but not b's concurrent one.
    let (rem, _r) = a.sign(TS_BASE + 100, vec![add_id], WinOp::Rem { key: 9 });
    ops.push(rem);

    let full = ingest_full(&ops);
    let windowed = ingest_windowed(ops.len() + 5, &ops);
    assert_windowed_equiv(&windowed, &full, "old_add_killed_by_later_remove");
    // The add-wins survivor keeps key 9 live (B's concurrent add was never observed).
    assert!(windowed.view().live_keys.contains(&9), "add-wins survivor keeps key 9 live");
    assert_eq!(windowed.view().key_authors[&9], BTreeSet::from([b.author_id()]));
    println!("PASS targeted: old add killed by a later remove, both in-window; windowed == full");
}

/// killed-object-resurrected-by-a-later-undel whose target del is EARLY: the window
/// must still answer `is_ancestor(del, undel)` correctly across the span, or
/// resurrection arithmetic diverges (§2.3, §2.5-P).
#[test]
fn killed_object_resurrected_by_later_undel_in_window() {
    let mut a = Author::new(1);
    let mut ops: Vec<SignedOp> = Vec::new();

    let (put, put_id) = a.sign(TS_BASE, vec![], WinOp::Put { emoji: 7, pos: 60 });
    ops.push(put);
    let (del, del_id) = a.sign(TS_BASE + 1, vec![put_id], WinOp::Del { obj: OpId(put_id) });
    ops.push(del);
    // A long span.
    for i in 0..30u64 {
        let (o, _h) = a.sign(TS_BASE + 10 + i, vec![], WinOp::Add { key: (i % 3) as u16 });
        ops.push(o);
    }
    // Undel observing the early del ⇒ overrides it, resurrecting the object.
    let (undel, _u) = a.sign(TS_BASE + 100, vec![del_id], WinOp::Undel { del: OpId(del_id) });
    ops.push(undel);

    let full = ingest_full(&ops);
    let windowed = ingest_windowed(ops.len() + 5, &ops);
    assert_windowed_equiv(&windowed, &full, "resurrection");
    assert!(windowed.view().objects.contains_key(&OpId(put_id)), "undel resurrects the object");
    println!("PASS targeted: killed object resurrected by a later undel, both in-window; windowed == full");
}

/// register write read by a narrow-horizon laggard — the R′ hazard (§2.5, §8.7): a
/// move CONCURRENT with a lock still applies, while a move observing the lock is
/// suppressed. The window's sub-horizon `locked_as_of` (a `resolve` over an op's
/// causal ancestors) must match the kernel exactly.
#[test]
fn narrow_horizon_lock_gate_r_prime() {
    let mut a = Author::new(1);
    let mut b = Author::new(2);
    let mut ops: Vec<SignedOp> = Vec::new();

    let (put, put_id) = a.sign(TS_BASE, vec![], WinOp::Put { emoji: 3, pos: 60 });
    ops.push(put);
    // Lock observing the put.
    let (lock, lock_id) = a.sign(TS_BASE + 1, vec![put_id], WinOp::SetLock { locked: true });
    ops.push(lock);
    // Move CONCURRENT with the lock (observes only the put) ⇒ still applies.
    let (mov_concurrent, _m1) = b.sign(TS_BASE + 2, vec![put_id], WinOp::Move { obj: OpId(put_id), pos: 64 });
    ops.push(mov_concurrent);
    // Move observing the lock ⇒ suppressed.
    let (mov_locked, _m2) = b.sign(TS_BASE + 3, vec![lock_id], WinOp::Move { obj: OpId(put_id), pos: 70 });
    ops.push(mov_locked);

    let full = ingest_full(&ops);
    let windowed = ingest_windowed(ops.len() + 5, &ops);
    assert_windowed_equiv(&windowed, &full, "r_prime_lock_gate");
    assert!(windowed.view().locked, "the room ends up locked (full-horizon)");
    // The surviving position is the concurrent move (64), never the locked move (70).
    assert_eq!(windowed.view().objects[&OpId(put_id)].1, 64, "concurrent move applied; locked move suppressed");
    println!("PASS targeted: R' narrow-horizon lock gate (concurrent move applies, observed move suppressed); windowed == full");
}

// ===========================================================================
// The §1.3 fence: N > W trips it (a refusal, never a silent wrong answer).
// ===========================================================================

/// N > W truncates the window: it stays bounded at `W`, reports itself INCOMPLETE,
/// yields NO view, and reports `appended_since` past its boundary as `None` — the
/// design's option (a) refusal, not a silent mis-answer (§1.3, §6.2 delta 6).
#[test]
fn fence_n_gt_w_is_incomplete_bounded_and_yields_no_view() {
    let ops = build_history(0x5CA1AB1E, 3, 40);
    let cap = 8;
    let windowed = ingest_windowed(cap, &ops);

    assert!(ops.len() > cap, "the history must exceed the window to trip the fence");
    assert!(!windowed.is_complete(), "window past W must report incomplete");
    assert!(windowed.len() <= cap, "window stays bounded at W (len={}, cap={cap})", windowed.len());
    assert!(windowed.try_view().is_none(), "fence: no view over a truncated window");
    assert!(
        windowed.appended_since(GrowthEpoch::INITIAL).is_none(),
        "appended_since past the window boundary is None (dag.rs:228-235)"
    );
    println!(
        "PASS fence: N={} > W={cap} -> incomplete, bounded (len={}), try_view()==None, appended_since==None",
        ops.len(),
        windowed.len()
    );
}

/// The fence is a HARD refusal: `view()` panics rather than fold a truncated window
/// (a silent wrong answer is the one thing M3.0 must never ship, §1.3).
#[test]
#[should_panic(expected = "windowed view fence")]
fn fence_view_panics_on_truncated_window() {
    let ops = build_history(0x5CA1AB1E, 3, 40);
    let windowed = ingest_windowed(8, &ops);
    // Precondition: actually truncated.
    assert!(!windowed.is_complete());
    let _ = windowed.view(); // MUST panic — never a silent mis-answer.
}
