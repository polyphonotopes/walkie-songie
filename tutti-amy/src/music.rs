//! `MusicLang` — a minimal music domain over the REAL tutti-core substrate, plus
//! the scenario + AMY driver that prove the thesis: **verifiable eventually-
//! consistent convergence produces audio**.
//!
//! This is the desktop-AMY tutti leaf (experiment 1 of
//! `docs/research/tutti-amy-esp32-leaf.md`). It is modelled EXACTLY on the second
//! `OpLanguage` in `crates/tutti-core/tests/second_domain.rs` (the KV register
//! store) and on the add-wins smoke fold in `crates/tutti-core/src/store.rs`, but
//! with a set-valued, add-wins pitch-set as the view:
//!
//! * `MusicOp = { AddDegree{pc}, RemoveDegree{pc} }` — the op alphabet.
//! * `View = BTreeSet<u16>` — the live pitch-set, folded **add-wins**: a degree is
//!   live iff SOME `Add` for it is not causally observed by ANY `Remove` for it
//!   (observed-remove OR-Set; walkie's degree semantics, set-valued LWW).
//! * `MusicLang: OpLanguage` with its OWN schema/magic consts.
//!
//! Two real `Store<MusicLang>` peers partition, diverge, then `ingest_verified`
//! each other's SIGNED ops and converge to the identical union pitch-set. The
//! successive `view()`s are diffed through [`crate::pitchset_to_amy_events`] to
//! drive AMY: framing (a), "AMY as a render target; the shared object stays the
//! pitch-set" (§3.1). Because a non-12 EDO is used, degrees resolve to FRACTIONAL
//! MIDI notes — the microtonal path AMY takes for free.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use tutti_core::{
    EntryHash, FoldCtx, OpLanguage, SignedOp, Store, VerifiedOpG, signing_key_from_seed,
    verify_signed_op_in,
};

use crate::{Amy, Pitch, pitchset_to_amy_events, rms};

// ===========================================================================
// The music OpLanguage: an add-wins pitch-set. Zero imperative wire in the log.
// ===========================================================================

/// The music op alphabet. `AddDegree` asserts a scale degree (pitch class) into the
/// room's hot set; `RemoveDegree` retracts it. Both COMMUTE — the fold resolves
/// them causally (add-wins), never by wall-clock — which is exactly why an op-set
/// of these merges cleanly where a stream of AMY wire deltas could not (§3.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MusicOp {
    /// Assert scale degree `pc` (a step in the room's EDO) into the pitch-set.
    AddDegree { pc: u16 },
    /// Retract scale degree `pc`. Only cancels the adds it causally observed.
    RemoveDegree { pc: u16 },
}

/// Domain well-formedness bound — `MusicLang`'s OWN cap. Generous enough for a
/// 4096-degree microtonal scale (the vision's `.scl` ceiling), unrelated to any
/// walkie value.
pub const MAX_DEGREE: u16 = 4096;

/// The music `OpLanguage` instantiation. Its consts are all its OWN, distinct from
/// walkie's and the KV domain's, proving nothing generic is hardcoded to a literal.
pub struct MusicLang;

impl OpLanguage for MusicLang {
    type Op = MusicOp;
    type View = BTreeSet<u16>;

    const SCHEMA_VERSION: u16 = 1;
    const ENTRY_FRAME_MAGIC: &'static [u8] = b"tutti.music.entry/1";
    const WIRE_MAGIC: &'static [u8] = b"tutti.music.wire/1\0";
    const MAX_PAYLOAD_BYTES: usize = 64 * 1024;

    fn validate_wire(op: &MusicOp) -> Result<(), String> {
        let pc = match op {
            MusicOp::AddDegree { pc } | MusicOp::RemoveDegree { pc } => *pc,
        };
        if pc >= MAX_DEGREE {
            return Err(format!("degree {pc} exceeds MAX_DEGREE={MAX_DEGREE}"));
        }
        Ok(())
    }

    /// Add-wins observed-remove set — EXACTLY the pattern of the `store.rs` smoke
    /// fold and walkie's degree semantics, but keyed on pitch class. Every `Add`
    /// and `Remove` on a degree is a candidate; a degree is live iff SOME `Add` for
    /// it is not causally observed (`is_ancestor`) by ANY `Remove` for it. A
    /// `Remove` therefore cancels only the adds in its causal past; a concurrent
    /// add survives (add-wins). Reads ancestry ONLY through the erased `FoldCtx`.
    fn fold(ctx: &FoldCtx<'_, Self>) -> BTreeSet<u16> {
        let mut adds: BTreeMap<u16, Vec<EntryHash>> = BTreeMap::new();
        let mut removes: BTreeMap<u16, Vec<EntryHash>> = BTreeMap::new();
        for (entry, decoded) in ctx.decoded() {
            match decoded.op() {
                MusicOp::AddDegree { pc } => adds.entry(*pc).or_default().push(*entry),
                MusicOp::RemoveDegree { pc } => removes.entry(*pc).or_default().push(*entry),
            }
        }

        let mut live = BTreeSet::new();
        for (pc, add_ops) in &adds {
            let rem_ops = removes.get(pc).map(Vec::as_slice).unwrap_or(&[]);
            let survives = add_ops
                .iter()
                .any(|a| !rem_ops.iter().any(|r| ctx.is_ancestor(a, r)));
            if survives {
                live.insert(*pc);
            }
        }
        live
    }
}

// ===========================================================================
// Room constants + the degree → pitch bridge.
// ===========================================================================

/// The room's topic — every op is bound to it (contract: topic-scoped authorship).
pub const TOPIC: &str = "tutti-amy-music";

/// The room's tuning: 31 divisions of the octave. Non-12, so degrees resolve to
/// FRACTIONAL MIDI notes and the microtonal path is exercised end to end.
pub const EDO: u16 = 31;

/// AMY oscillator address space bound (well under the desktop default of 250).
pub const MAX_OSCS: u16 = 250;

/// Lift a fold output (a set of scale degrees) into the pitch-set AMY renders, at
/// the room's `edo`. `degree * 12.0 / edo` above middle C, per [`Pitch::midi_note`].
pub fn pitchset(degrees: &BTreeSet<u16>, edo: u16) -> BTreeSet<Pitch> {
    degrees
        .iter()
        .map(|&pc| Pitch::new(pc as i32, edo))
        .collect()
}

fn verify(signed: &SignedOp) -> VerifiedOpG<MusicLang> {
    verify_signed_op_in::<MusicLang>(signed).expect("a signed music op verifies")
}

// ===========================================================================
// The partition → rejoin scenario, driven by TWO real Store<MusicLang> peers.
// ===========================================================================

/// The outcome of running the two-peer partition→rejoin scenario. Pure tutti-core:
/// no AMY, fully testable on its own.
pub struct Scenario {
    /// Peer A's view WHILE PARTITIONED (before it saw any of B's ops).
    pub a_partition: BTreeSet<u16>,
    /// Peer B's view WHILE PARTITIONED (before it saw any of A's ops).
    pub b_partition: BTreeSet<u16>,
    /// Peer A's view AFTER rejoin (ingested B's signed ops).
    pub a_converged: BTreeSet<u16>,
    /// Peer B's view AFTER rejoin (ingested A's signed ops).
    pub b_converged: BTreeSet<u16>,
    /// The hand-computed add-wins union oracle — what both peers MUST converge to.
    pub expected_union: BTreeSet<u16>,
    /// The degree B added then retracted; the remove observed the add, so it must
    /// be ABSENT from the union (remove wins ⇒ never sounds after teardown).
    pub removed_degree: u16,
    /// The leaf's REAL fold outputs over the whole partition→rejoin timeline — each
    /// a `store.view()` after a commit or the rejoin. Consecutive diffs drive AMY.
    pub timeline: Vec<BTreeSet<u16>>,
    /// The room tuning used for the audio projection.
    pub edo: u16,
    /// Ops parked in A / B after rejoin (must be 0 — liveness, nothing stuck).
    pub a_pending: usize,
    pub b_pending: usize,
}

/// Run the two-peer scenario end to end:
///
/// 1. **Partition.** A commits `AddDegree`s for a near-just 31-EDO triad
///    (`{0,10,18}`); B commits its own degrees AND retracts one
///    (`Add 8, Add 25, Add 5, Remove 5`). They do NOT exchange ops — their views
///    diverge. B's `view()` is snapshotted after each commit → the real fold
///    timeline the leaf's speaker plays while partitioned.
/// 2. **Rejoin.** Each peer `ingest_verified`s the OTHER's signed ops (the
///    tutti-core convergence path). Both refold to the identical union pitch-set.
///
/// Degree 5 is added then removed inside B's own causal chain, so the remove
/// observes the add and 5 dies; A never touches 5. Under add-wins the union is
/// `{0, 8, 10, 18, 25}` — 5 is gone (remove wins).
pub fn run_scenario() -> Scenario {
    let ka = signing_key_from_seed(&[1u8; 32]);
    let kb = signing_key_from_seed(&[2u8; 32]);
    let mut ts: u64 = 1_700_000_000_000_000; // µs, monotone; NOT used for ordering
    let mut tick = || {
        let t = ts;
        ts += 1;
        t
    };

    let mut a: Store<MusicLang> = Store::new();
    let mut b: Store<MusicLang> = Store::new();

    // --- Partition: A plays a 31-EDO near-just major triad (steps 0, 10, 18). ---
    let a_ops = vec![
        a.commit(&ka, TOPIC, tick(), MusicOp::AddDegree { pc: 0 }),
        a.commit(&ka, TOPIC, tick(), MusicOp::AddDegree { pc: 10 }),
        a.commit(&ka, TOPIC, tick(), MusicOp::AddDegree { pc: 18 }),
    ];

    // --- Partition: B, the leaf with a speaker, plays 8 & 25 and momentarily 5,
    //     then RETRACTS 5. Snapshot B's fold after each commit → the audio timeline.
    let mut timeline: Vec<BTreeSet<u16>> = vec![BTreeSet::new()]; // start silent
    let mut b_ops = Vec::new();
    for op in [
        MusicOp::AddDegree { pc: 8 },
        MusicOp::AddDegree { pc: 25 },
        MusicOp::AddDegree { pc: 5 },
        MusicOp::RemoveDegree { pc: 5 },
    ] {
        b_ops.push(b.commit(&kb, TOPIC, tick(), op));
        timeline.push(b.view());
    }

    let a_partition = a.view();
    let b_partition = b.view();

    // --- Rejoin: each peer verifies + ingests the other's SIGNED ops in causal
    //     (commit) order. This is the convergence path from the KV test. ---
    for signed in &b_ops {
        a.ingest_verified(verify(signed));
    }
    for signed in &a_ops {
        b.ingest_verified(verify(signed));
    }

    // The leaf's post-rejoin fold: the union clicks into place. This last timeline
    // entry is B's converged view — the audio diff from `{8,25}` adds A's degrees.
    timeline.push(b.view());

    Scenario {
        a_partition,
        b_partition,
        a_converged: a.view(),
        b_converged: b.view(),
        expected_union: BTreeSet::from([0, 8, 10, 18, 25]),
        removed_degree: 5,
        timeline,
        edo: EDO,
        a_pending: a.pending_len(),
        b_pending: b.pending_len(),
    }
}

// ===========================================================================
// The AMY driver + the end-to-end no-stuck-notes ledger.
// ===========================================================================

/// The audio produced by driving AMY along a fold timeline, plus the ledger that
/// proves NO STUCK NOTES: every emitted note-on had a matching note-off.
pub struct AudioReport {
    /// Interleaved-stereo i16 PCM of the whole render (timeline + teardown).
    pub pcm: Vec<i16>,
    /// Mean RMS while each timeline step rang (same length as the driven timeline).
    pub step_rms: Vec<f64>,
    /// Mean RMS during the teardown tail (after release-all). Near-zero ⇒ silence.
    pub teardown_rms: f64,
    /// Every wire event emitted, in order (for inspection / the ledger).
    pub events: Vec<String>,
    /// Total note-on (`l1`) and note-off (`l0`) events.
    pub note_ons: usize,
    pub note_offs: usize,
    /// Oscillators still holding a note-on after teardown — MUST be empty. A
    /// non-empty set is a stuck note.
    pub stuck_oscs: BTreeSet<u16>,
    /// Note-offs that hit an oscillator that was not sounding (should be 0).
    pub unmatched_offs: usize,
}

/// `(osc, is_note_on)` for a wire event: `v8n63.097l1` (on) or `v8l0` (off).
fn parse_event(ev: &str) -> (u16, bool) {
    let rest = ev.strip_prefix('v').expect("event starts with v<osc>");
    let osc_end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let osc: u16 = rest[..osc_end].parse().expect("osc index parses");
    (osc, ev.ends_with("l1"))
}

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

/// Drive AMY along `fold_timeline` (a sequence of real `view()`s), diffing each
/// consecutive pair through [`pitchset_to_amy_events`], then release everything at
/// teardown. Renders `blocks_per_step` blocks per timeline entry and
/// `teardown_blocks` for the release tail, accumulating PCM. Builds the
/// no-stuck-notes ledger straight from the emitted events.
pub fn drive_amy(
    amy: &Amy,
    fold_timeline: &[BTreeSet<u16>],
    edo: u16,
    blocks_per_step: usize,
    teardown_blocks: usize,
) -> AudioReport {
    let mut pcm: Vec<i16> = Vec::new();
    let mut step_rms: Vec<f64> = Vec::new();
    let mut events: Vec<String> = Vec::new();
    let mut prev: BTreeSet<Pitch> = BTreeSet::new();

    let render = |n: usize, pcm: &mut Vec<i16>| -> f64 {
        let mut rmss = Vec::with_capacity(n);
        for _ in 0..n {
            let block = amy.render_block();
            rmss.push(rms(&block));
            pcm.extend_from_slice(&block);
        }
        mean(&rmss)
    };

    // Drive each real fold output.
    for degrees in fold_timeline {
        let cur = pitchset(degrees, edo);
        let evs = pitchset_to_amy_events(&prev, &cur, MAX_OSCS);
        for ev in &evs {
            amy.send(ev);
        }
        events.extend(evs);
        step_rms.push(render(blocks_per_step, &mut pcm));
        prev = cur;
    }

    // Teardown: release everything still sounding → silence (the no-stuck-note
    // discipline applied end to end — fail to silence).
    let empty = BTreeSet::new();
    let evs = pitchset_to_amy_events(&prev, &empty, MAX_OSCS);
    for ev in &evs {
        amy.send(ev);
    }
    events.extend(evs);
    let teardown_rms = render(teardown_blocks, &mut pcm);

    // The ledger: fold the events into a sounding set. Every note-on adds an osc,
    // every note-off removes one; after teardown the set MUST be empty.
    let mut sounding: BTreeSet<u16> = BTreeSet::new();
    let (mut note_ons, mut note_offs, mut unmatched_offs) = (0usize, 0usize, 0usize);
    for ev in &events {
        let (osc, on) = parse_event(ev);
        if on {
            note_ons += 1;
            sounding.insert(osc);
        } else {
            note_offs += 1;
            if !sounding.remove(&osc) {
                unmatched_offs += 1;
            }
        }
    }

    AudioReport {
        pcm,
        step_rms,
        teardown_rms,
        events,
        note_ons,
        note_offs,
        stuck_oscs: sounding,
        unmatched_offs,
    }
}

// ===========================================================================
// Unit tests — the convergence thesis, PURE tutti-core (no AMY, no audio).
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::signing_key_from_seed;

    fn verify_op(signed: &SignedOp) -> VerifiedOpG<MusicLang> {
        verify_signed_op_in::<MusicLang>(signed).expect("verifies")
    }

    /// The headline acceptance property, WITHOUT audio: two partitioned peers
    /// diverge, then converge to the identical union under add-wins on rejoin.
    #[test]
    fn partition_then_converge_to_union() {
        let s = run_scenario();

        // 1. Partitioned views DIFFER (they never exchanged ops).
        assert_ne!(
            s.a_partition, s.b_partition,
            "partitioned peers must hold different views"
        );
        assert_eq!(s.a_partition, BTreeSet::from([0, 10, 18]));
        assert_eq!(s.b_partition, BTreeSet::from([8, 25]));

        // 2. After rejoin BOTH converge to the identical union pitch-set.
        assert_eq!(s.a_converged, s.b_converged, "peers must converge");
        assert_eq!(s.a_converged, s.expected_union);
        assert_eq!(s.a_converged, BTreeSet::from([0, 8, 10, 18, 25]));

        // 3. The retracted degree (remove wins) is ABSENT from the union.
        assert!(
            !s.a_converged.contains(&s.removed_degree),
            "degree {} was removed and the remove must win",
            s.removed_degree
        );

        // 4. Liveness: nothing parked after rejoin.
        assert_eq!(s.a_pending, 0);
        assert_eq!(s.b_pending, 0);
    }

    /// Convergence is order-INDEPENDENT: a fresh peer ingesting the full op-set in
    /// several shuffled orders lands on the same union every time (== the oracle).
    #[test]
    fn convergence_is_order_independent() {
        let s = run_scenario();

        // Re-derive the full signed op-set by replaying the scenario's commits into
        // one producer, then ingest it in different orders into fresh stores.
        let ka = signing_key_from_seed(&[1u8; 32]);
        let kb = signing_key_from_seed(&[2u8; 32]);
        let mut producer: Store<MusicLang> = Store::new();
        let mut ts = 1_700_000_000_000_000u64;
        let mut ops: Vec<SignedOp> = Vec::new();
        // Interleave A and B so causality really crosses authors in the producer.
        for op in [
            (&ka, MusicOp::AddDegree { pc: 0 }),
            (&kb, MusicOp::AddDegree { pc: 8 }),
            (&ka, MusicOp::AddDegree { pc: 10 }),
            (&kb, MusicOp::AddDegree { pc: 25 }),
            (&kb, MusicOp::AddDegree { pc: 5 }),
            (&ka, MusicOp::AddDegree { pc: 18 }),
            (&kb, MusicOp::RemoveDegree { pc: 5 }),
        ] {
            ops.push(producer.commit(op.0, TOPIC, ts, op.1));
            ts += 1;
        }

        // Deterministic shuffles (simple index rotations — enough to reorder deps).
        for rot in [0usize, 1, 3, 5] {
            let mut store: Store<MusicLang> = Store::new();
            let n = ops.len();
            for i in 0..n {
                store.ingest_verified(verify_op(&ops[(i + rot) % n]));
            }
            assert_eq!(store.pending_len(), 0, "rot {rot} left ops parked");
            assert_eq!(
                store.view(),
                s.expected_union,
                "shuffle rot {rot} diverged from the union"
            );
            // The cheap lazy Reach view equals the kernel ReachIndex oracle.
            assert_eq!(store.view(), store.view_reference());
        }
    }

    /// Add-wins under genuine CONCURRENCY: if two peers add the same degree while
    /// partitioned and only one removes it, the degree SURVIVES — the concurrent
    /// add was never observed by the remove.
    #[test]
    fn add_wins_over_concurrent_remove() {
        let ka = signing_key_from_seed(&[10u8; 32]);
        let kb = signing_key_from_seed(&[20u8; 32]);
        let mut a: Store<MusicLang> = Store::new();
        let mut b: Store<MusicLang> = Store::new();

        // Partitioned: both add degree 7; B then removes it (observing only B's add).
        let a_add = a.commit(&ka, TOPIC, 1, MusicOp::AddDegree { pc: 7 });
        let b_add = b.commit(&kb, TOPIC, 1, MusicOp::AddDegree { pc: 7 });
        let b_rem = b.commit(&kb, TOPIC, 2, MusicOp::RemoveDegree { pc: 7 });

        // B alone: its own add is removed → 7 absent locally.
        assert!(!b.view().contains(&7));

        // Rejoin: A learns B's add+remove; B learns A's concurrent add.
        a.ingest_verified(verify_op(&b_add));
        a.ingest_verified(verify_op(&b_rem));
        b.ingest_verified(verify_op(&a_add));

        // Add-wins: A's concurrent add was NOT observed by B's remove → 7 survives.
        assert!(a.view().contains(&7), "concurrent add must win over the remove");
        assert_eq!(a.view(), b.view(), "peers converge");
        assert_eq!(a.view(), BTreeSet::from([7]));
    }

    /// A degree added then removed WITHIN one causal chain (the remove observes the
    /// add) is gone — observed-remove actually removes.
    #[test]
    fn observed_remove_actually_removes() {
        let ka = signing_key_from_seed(&[9u8; 32]);
        let mut a: Store<MusicLang> = Store::new();
        a.commit(&ka, TOPIC, 1, MusicOp::AddDegree { pc: 3 });
        a.commit(&ka, TOPIC, 2, MusicOp::RemoveDegree { pc: 3 });
        assert!(a.view().is_empty(), "observed remove clears the degree");
    }
}
