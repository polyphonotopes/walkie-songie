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

use crate::{Amy, Envelope, Pitch, degrees_to_amy_events, pitchset_to_amy_events, rms};

// ===========================================================================
// The music OpLanguage: an add-wins pitch-set + per-degree envelope registers.
// Zero imperative wire in the log.
// ===========================================================================

/// The music op alphabet. `AddDegree` asserts a scale degree (pitch class) into the
/// room's hot set; `RemoveDegree` retracts it (both COMMUTE — the fold resolves them
/// causally, add-wins, never by wall-clock). `SetEnvelope` writes a **continuous
/// envelope facet** onto a degree — a register write, resolved by causal maxima like
/// any timbre/config register (§3.3, §4). All three merge cleanly where a stream of
/// AMY wire deltas could not: the log stores the *description* (a function), never
/// samples.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MusicOp {
    /// Assert scale degree `pc` (a step in the room's EDO) into the pitch-set.
    AddDegree { pc: u16 },
    /// Retract scale degree `pc`. Only cancels the adds it causally observed.
    RemoveDegree { pc: u16 },
    /// Write the amplitude-envelope facet for degree `pc` (a causal register).
    /// Appended after the original two variants (serde discipline: append, never
    /// reorder), which is why [`MusicLang::SCHEMA_VERSION`] steps to 2.
    SetEnvelope { pc: u16, env: Envelope },
}

/// Domain well-formedness bound — `MusicLang`'s OWN cap. Generous enough for a
/// 4096-degree microtonal scale (the vision's `.scl` ceiling), unrelated to any
/// walkie value.
pub const MAX_DEGREE: u16 = 4096;

/// The materialized read model: the live pitch-set AND the per-degree envelope
/// registers. Degrees fold **add-wins**; envelopes fold as **causal-maxima
/// registers** — two orthogonal CRDT semantics over the SAME signed op-DAG, both
/// the substrate's, resolved in one deterministic fold.
///
/// **Persistence semantics (stated honestly):** an envelope facet is a *register*
/// keyed by degree and is independent of that degree's add-wins liveness. Removing
/// a degree drops its sounding note (it leaves `live`) but its envelope register
/// PERSISTS — the facet is durable timbre configuration ("the description is
/// shared; the performance is local", §3.3), so a re-add of the degree resumes
/// under the same converged curve. The edge only *applies* an envelope when the
/// degree is live (a note is sounding); the register itself never depends on it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MusicView {
    /// The live pitch-set (degrees), folded add-wins (observed-remove OR-Set).
    pub live: BTreeSet<u16>,
    /// Per-degree amplitude-envelope facets, each a causal-maxima register.
    pub envelopes: BTreeMap<u16, Envelope>,
}

/// The music `OpLanguage` instantiation. Its consts are all its OWN, distinct from
/// walkie's and the KV domain's, proving nothing generic is hardcoded to a literal.
pub struct MusicLang;

impl OpLanguage for MusicLang {
    type Op = MusicOp;
    type View = MusicView;

    const SCHEMA_VERSION: u16 = 2;
    const ENTRY_FRAME_MAGIC: &'static [u8] = b"tutti.music.entry/1";
    const WIRE_MAGIC: &'static [u8] = b"tutti.music.wire/1\0";
    const MAX_PAYLOAD_BYTES: usize = 64 * 1024;

    fn validate_wire(op: &MusicOp) -> Result<(), String> {
        match op {
            MusicOp::AddDegree { pc } | MusicOp::RemoveDegree { pc } => {
                if *pc >= MAX_DEGREE {
                    return Err(format!("degree {pc} exceeds MAX_DEGREE={MAX_DEGREE}"));
                }
            }
            MusicOp::SetEnvelope { pc, env } => {
                if *pc >= MAX_DEGREE {
                    return Err(format!("degree {pc} exceeds MAX_DEGREE={MAX_DEGREE}"));
                }
                if env.points.is_empty() || env.points.len() > crate::MAX_ENV_POINTS {
                    return Err(format!(
                        "envelope must carry 1..={} breakpoints (got {})",
                        crate::MAX_ENV_POINTS,
                        env.points.len()
                    ));
                }
                if let Some(&(_, level)) = env.points.iter().find(|(_, l)| *l > crate::MAX_ENV_LEVEL)
                {
                    return Err(format!(
                        "envelope level {level} exceeds MAX_ENV_LEVEL={}",
                        crate::MAX_ENV_LEVEL
                    ));
                }
            }
        }
        Ok(())
    }

    /// One deterministic fold, two resolutions:
    ///
    /// * **Degrees (add-wins observed-remove)** — EXACTLY the `store.rs` smoke fold
    ///   and walkie's degree semantics: a degree is live iff SOME `Add` for it is
    ///   not causally observed (`is_ancestor`) by ANY `Remove` for it. A `Remove`
    ///   cancels only the adds in its causal past; a concurrent add survives.
    /// * **Envelopes (causal-maxima register per degree)** — EXACTLY the KV/tuning
    ///   register machinery: every `SetEnvelope` on a degree is a candidate, and
    ///   `ctx.resolve` drops any candidate strictly in another's causal past
    ///   (superseded) then breaks the surviving concurrent maxima by max raw-bytes
    ///   `EntryHash` — so the latest-in-causal-order envelope wins, order-
    ///   independently. Reuses the substrate's `resolve`; no ad-hoc tiebreak here.
    ///
    /// Reads ancestry ONLY through the erased `FoldCtx` (`is_ancestor`/`resolve`).
    fn fold(ctx: &FoldCtx<'_, Self>) -> MusicView {
        let mut adds: BTreeMap<u16, Vec<EntryHash>> = BTreeMap::new();
        let mut removes: BTreeMap<u16, Vec<EntryHash>> = BTreeMap::new();
        let mut env_writes: BTreeMap<u16, BTreeSet<EntryHash>> = BTreeMap::new();
        for (entry, decoded) in ctx.decoded() {
            match decoded.op() {
                MusicOp::AddDegree { pc } => adds.entry(*pc).or_default().push(*entry),
                MusicOp::RemoveDegree { pc } => removes.entry(*pc).or_default().push(*entry),
                MusicOp::SetEnvelope { pc, .. } => {
                    env_writes.entry(*pc).or_default().insert(*entry);
                }
            }
        }

        // Degrees: add-wins.
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

        // Envelopes: one causal-maxima register per degree (persists regardless of
        // whether the degree is currently live — see MusicView's doc).
        let mut envelopes = BTreeMap::new();
        for (pc, candidates) in &env_writes {
            if let Some(winner) = ctx.resolve(candidates) {
                if let MusicOp::SetEnvelope { env, .. } = ctx.decoded()[&winner].op() {
                    envelopes.insert(*pc, env.clone());
                }
            }
        }

        MusicView { live, envelopes }
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

/// Construct a fresh `Store<MusicLang>` — so a bin/test can drive convergence
/// without naming the tutti-core `Store`/`MusicLang` types itself.
pub fn new_store() -> Store<MusicLang> {
    Store::new()
}

/// Verify a signed music op into the ingest-ready `VerifiedOpG<MusicLang>`.
pub fn verify_signed(signed: &SignedOp) -> VerifiedOpG<MusicLang> {
    verify(signed)
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
        timeline.push(b.view().live);
    }

    let a_partition = a.view().live;
    let b_partition = b.view().live;

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
    timeline.push(b.view().live);

    Scenario {
        a_partition,
        b_partition,
        a_converged: a.view().live,
        b_converged: b.view().live,
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
// Continuous-envelope facet: demo curves, the two-peer convergence scenario, and
// the render/analysis helpers shared by the bin, the tests, and the wav.
// ===========================================================================

use crate::Interp;

/// A slow SWELL: jump to a whisper, LINEAR-ramp to full over 350 ms, then release.
/// While held, RMS RISES — a pad-like onset. (Last pair is the note-off release.)
pub fn swell() -> Envelope {
    Envelope {
        points: vec![(0, 8), (350, 127), (60, 0)],
        interp: Interp::Linear,
    }
}

/// A fast PLUCK: jump to full, TRUE-EXPONENTIAL-decay to a whisper over 120 ms,
/// then release. While held, RMS FALLS — a plucked/percussive onset.
pub fn pluck() -> Envelope {
    Envelope {
        points: vec![(0, 127), (120, 12), (40, 0)],
        interp: Interp::Exp,
    }
}

/// Outcome of the two-peer **envelope-facet** partition→rejoin scenario. Pure
/// tutti-core; no AMY. Both peers concurrently `SetEnvelope` on the SAME degree
/// (`pc_contested`, different curves) and each on a disjoint degree, then exchange
/// signed ops and converge.
pub struct EnvelopeScenario {
    /// Peer A's converged `MusicView` (live set + envelope registers).
    pub a_converged: MusicView,
    /// Peer B's converged `MusicView`. MUST equal `a_converged`.
    pub b_converged: MusicView,
    /// The degree both peers wrote an envelope on, concurrently (the contested register).
    pub pc_contested: u16,
    /// A degree only A wrote an envelope on.
    pub pc_a_only: u16,
    /// A degree only B wrote an envelope on.
    pub pc_b_only: u16,
    /// A's envelope for the contested degree (a swell).
    pub env_a: Envelope,
    /// B's envelope for the contested degree (a pluck).
    pub env_b: Envelope,
    /// The causal-maxima winner on the contested degree (== `env_a` or `env_b`).
    pub winner: Envelope,
    /// The other one — what the audio must NOT sound like after convergence.
    pub loser: Envelope,
    pub a_pending: usize,
    pub b_pending: usize,
}

/// The degrees used by the envelope scenario / op-set.
const ENV_PC_CONTESTED: u16 = 0;
const ENV_PC_A_ONLY: u16 = 10;
const ENV_PC_B_ONLY: u16 = 18;

fn ka_env() -> tutti_core::SigningKey {
    signing_key_from_seed(&[11u8; 32])
}
fn kb_env() -> tutti_core::SigningKey {
    signing_key_from_seed(&[22u8; 32])
}

/// Run the two-peer envelope-facet scenario: A and B each build an independent
/// causal chain (so their contested-degree writes are genuinely CONCURRENT), then
/// each `ingest_verified`s the other's signed ops and refolds. Returns both
/// converged views + the resolved winner.
pub fn run_envelope_scenario() -> EnvelopeScenario {
    let (ka, kb) = (ka_env(), kb_env());
    let mut a: Store<MusicLang> = Store::new();
    let mut b: Store<MusicLang> = Store::new();
    let (env_a, env_b) = (swell(), pluck());

    // Partition: A adds+envelopes the contested degree and its own disjoint degree.
    let a_ops = vec![
        a.commit(&ka, TOPIC, 1, MusicOp::AddDegree { pc: ENV_PC_CONTESTED }),
        a.commit(&ka, TOPIC, 2, MusicOp::SetEnvelope { pc: ENV_PC_CONTESTED, env: env_a.clone() }),
        a.commit(&ka, TOPIC, 3, MusicOp::AddDegree { pc: ENV_PC_A_ONLY }),
        a.commit(&ka, TOPIC, 4, MusicOp::SetEnvelope { pc: ENV_PC_A_ONLY, env: env_a.clone() }),
    ];
    // Partition: B does the same with its OWN curve on the SAME contested degree.
    let b_ops = vec![
        b.commit(&kb, TOPIC, 1, MusicOp::AddDegree { pc: ENV_PC_CONTESTED }),
        b.commit(&kb, TOPIC, 2, MusicOp::SetEnvelope { pc: ENV_PC_CONTESTED, env: env_b.clone() }),
        b.commit(&kb, TOPIC, 3, MusicOp::AddDegree { pc: ENV_PC_B_ONLY }),
        b.commit(&kb, TOPIC, 4, MusicOp::SetEnvelope { pc: ENV_PC_B_ONLY, env: env_b.clone() }),
    ];

    // Rejoin: exchange signed ops.
    for signed in &b_ops {
        a.ingest_verified(verify(signed));
    }
    for signed in &a_ops {
        b.ingest_verified(verify(signed));
    }

    let a_converged = a.view();
    let b_converged = b.view();
    let winner = a_converged.envelopes[&ENV_PC_CONTESTED].clone();
    let loser = if winner == env_a { env_b.clone() } else { env_a.clone() };

    EnvelopeScenario {
        a_converged,
        b_converged,
        pc_contested: ENV_PC_CONTESTED,
        pc_a_only: ENV_PC_A_ONLY,
        pc_b_only: ENV_PC_B_ONLY,
        env_a,
        env_b,
        winner,
        loser,
        a_pending: a.pending_len(),
        b_pending: b.pending_len(),
    }
}

/// The raw signed op-set behind [`run_envelope_scenario`] (A's chain then B's) —
/// two mutually-concurrent chains — for order-independence / determinism tests
/// that ingest it in many arrival orders into fresh stores.
pub fn envelope_op_set() -> Vec<SignedOp> {
    let (ka, kb) = (ka_env(), kb_env());
    let mut a: Store<MusicLang> = Store::new();
    let mut b: Store<MusicLang> = Store::new();
    let (env_a, env_b) = (swell(), pluck());
    vec![
        a.commit(&ka, TOPIC, 1, MusicOp::AddDegree { pc: ENV_PC_CONTESTED }),
        a.commit(&ka, TOPIC, 2, MusicOp::SetEnvelope { pc: ENV_PC_CONTESTED, env: env_a.clone() }),
        a.commit(&ka, TOPIC, 3, MusicOp::AddDegree { pc: ENV_PC_A_ONLY }),
        a.commit(&ka, TOPIC, 4, MusicOp::SetEnvelope { pc: ENV_PC_A_ONLY, env: env_a.clone() }),
        b.commit(&kb, TOPIC, 1, MusicOp::AddDegree { pc: ENV_PC_CONTESTED }),
        b.commit(&kb, TOPIC, 2, MusicOp::SetEnvelope { pc: ENV_PC_CONTESTED, env: env_b.clone() }),
        b.commit(&kb, TOPIC, 3, MusicOp::AddDegree { pc: ENV_PC_B_ONLY }),
        b.commit(&kb, TOPIC, 4, MusicOp::SetEnvelope { pc: ENV_PC_B_ONLY, env: env_b.clone() }),
    ]
}

/// Play a single held degree `pc` under envelope facet `env` and capture it:
/// project the envelope-carrying note-on via [`degrees_to_amy_events`], render
/// `held_blocks` (returning per-block RMS — the audible trajectory of the curve),
/// then note-off and render `tail_blocks` of release so the oscillator returns to
/// silence before the next call. Returns the full PCM (held + tail) and the
/// held-phase per-block RMS.
pub fn render_held_with_envelope(
    amy: &Amy,
    pc: u16,
    env: &Envelope,
    edo: u16,
    held_blocks: usize,
    tail_blocks: usize,
) -> (Vec<i16>, Vec<f64>) {
    let mut envs = BTreeMap::new();
    envs.insert(pc, env.clone());
    let before = BTreeSet::new();
    let after = BTreeSet::from([pc]);

    let mut pcm = Vec::new();
    for ev in degrees_to_amy_events(&before, &after, &envs, edo, MAX_OSCS) {
        amy.send(&ev);
    }
    let mut held_rms = Vec::with_capacity(held_blocks);
    for _ in 0..held_blocks {
        let block = amy.render_block();
        held_rms.push(rms(&block));
        pcm.extend_from_slice(&block);
    }
    for ev in degrees_to_amy_events(&after, &before, &envs, edo, MAX_OSCS) {
        amy.send(&ev);
    }
    for _ in 0..tail_blocks {
        pcm.extend_from_slice(&amy.render_block());
    }
    (pcm, held_rms)
}

/// The wire events a held-degree note-on emits under `env` — the byte stream the
/// determinism gate compares across peers. (Just the note-on; the note-off is a
/// facet-independent `vNl0`.)
pub fn envelope_note_on_wire(pc: u16, env: &Envelope, edo: u16) -> Vec<String> {
    let mut envs = BTreeMap::new();
    envs.insert(pc, env.clone());
    degrees_to_amy_events(&BTreeSet::new(), &BTreeSet::from([pc]), &envs, edo, MAX_OSCS)
}

/// Least-squares slope of a per-block RMS series vs. block index (RMS units per
/// block). Negative ⇒ the amplitude curve is decaying; positive ⇒ swelling. This
/// is the "measure per-block RMS slope" the gate calls for.
pub fn rms_slope(rmss: &[f64]) -> f64 {
    let n = rmss.len();
    if n < 2 {
        return 0.0;
    }
    let mean_x = (n as f64 - 1.0) / 2.0;
    let mean_y = rmss.iter().sum::<f64>() / n as f64;
    let (mut num, mut den) = (0.0, 0.0);
    for (i, &y) in rmss.iter().enumerate() {
        let dx = i as f64 - mean_x;
        num += dx * (y - mean_y);
        den += dx * dx;
    }
    if den == 0.0 { 0.0 } else { num / den }
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
                store.view().live,
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
        assert!(!b.view().live.contains(&7));

        // Rejoin: A learns B's add+remove; B learns A's concurrent add.
        a.ingest_verified(verify_op(&b_add));
        a.ingest_verified(verify_op(&b_rem));
        b.ingest_verified(verify_op(&a_add));

        // Add-wins: A's concurrent add was NOT observed by B's remove → 7 survives.
        assert!(
            a.view().live.contains(&7),
            "concurrent add must win over the remove"
        );
        assert_eq!(a.view(), b.view(), "peers converge");
        assert_eq!(a.view().live, BTreeSet::from([7]));
    }

    /// A degree added then removed WITHIN one causal chain (the remove observes the
    /// add) is gone — observed-remove actually removes.
    #[test]
    fn observed_remove_actually_removes() {
        let ka = signing_key_from_seed(&[9u8; 32]);
        let mut a: Store<MusicLang> = Store::new();
        a.commit(&ka, TOPIC, 1, MusicOp::AddDegree { pc: 3 });
        a.commit(&ka, TOPIC, 2, MusicOp::RemoveDegree { pc: 3 });
        assert!(a.view().live.is_empty(), "observed remove clears the degree");
    }

    // -----------------------------------------------------------------------
    // Envelope FACET convergence — the causal-maxima register, PURE tutti-core.
    // -----------------------------------------------------------------------

    /// Two partitioned peers concurrently `SetEnvelope` on the SAME degree (with
    /// DIFFERENT envelopes) and on DIFFERENT degrees; after exchanging signed ops
    /// they converge to the identical `BTreeMap<pc, Envelope>` — the causal-maxima
    /// winner on the contested degree, both peers' writes on the disjoint ones.
    #[test]
    fn envelope_registers_converge_across_peers() {
        let s = run_envelope_scenario();

        // Both peers land on the identical envelope map (and identical live set).
        assert_eq!(
            s.a_converged, s.b_converged,
            "peers must converge on the same envelope registers"
        );

        // The contested degree resolved to ONE of the two concurrent writes...
        let won = &s.a_converged.envelopes[&s.pc_contested];
        assert!(
            *won == s.env_a || *won == s.env_b,
            "the winner must be one of the two concurrent envelopes"
        );
        assert_eq!(*won, s.winner, "scenario winner matches the folded winner");
        // ...and it is NOT a merge/blend of the two — a register picks a maximum.
        assert!(s.env_a != s.env_b, "the two envelopes must differ for a real tie");

        // The disjoint degrees keep each peer's own write (different registers merge).
        assert_eq!(s.a_converged.envelopes[&s.pc_a_only], s.env_a);
        assert_eq!(s.a_converged.envelopes[&s.pc_b_only], s.env_b);

        // Liveness: nothing parked.
        assert_eq!(s.a_pending, 0);
        assert_eq!(s.b_pending, 0);
    }

    /// The envelope register is order-INDEPENDENT and equals the kernel oracle: a
    /// fresh peer ingesting the full op-set in several shuffled orders lands on the
    /// same `MusicView` every time, and the cheap lazy `Reach` view equals the
    /// `ReachIndex` `view_reference()`.
    #[test]
    fn envelope_convergence_is_order_independent() {
        let ops = envelope_op_set();

        let mut reference: Option<MusicView> = None;
        for rot in [0usize, 1, 2, 4, 6] {
            let mut store: Store<MusicLang> = Store::new();
            let n = ops.len();
            for i in 0..n {
                store.ingest_verified(verify(&ops[(i + rot) % n]));
            }
            assert_eq!(store.pending_len(), 0, "rot {rot} left ops parked");
            // Lazy Reach view == kernel ReachIndex oracle: same fold, no drift.
            assert_eq!(store.view(), store.view_reference());
            match &reference {
                None => reference = Some(store.view()),
                Some(r) => assert_eq!(&store.view(), r, "rot {rot} diverged"),
            }
        }
    }

    /// A later `SetEnvelope` that causally OBSERVES an earlier one supersedes it —
    /// the register is last-writer-wins by causal order (not wall-clock).
    #[test]
    fn envelope_register_lww_by_causal_order() {
        let ka = signing_key_from_seed(&[42u8; 32]);
        let mut a: Store<MusicLang> = Store::new();
        let first = swell();
        let second = pluck();
        a.commit(&ka, TOPIC, 1, MusicOp::SetEnvelope { pc: 0, env: first.clone() });
        a.commit(&ka, TOPIC, 2, MusicOp::SetEnvelope { pc: 0, env: second.clone() });
        // The second write is in the same causal chain (observes the first) → wins.
        assert_eq!(a.view().envelopes[&0], second);
        assert_ne!(a.view().envelopes[&0], first);
    }

    /// Removing a degree drops its sounding note but PRESERVES its envelope register
    /// (the honest persistence semantics stated on `MusicView`).
    #[test]
    fn removing_a_degree_keeps_its_envelope_facet() {
        let ka = signing_key_from_seed(&[7u8; 32]);
        let mut a: Store<MusicLang> = Store::new();
        a.commit(&ka, TOPIC, 1, MusicOp::AddDegree { pc: 4 });
        a.commit(&ka, TOPIC, 2, MusicOp::SetEnvelope { pc: 4, env: pluck() });
        a.commit(&ka, TOPIC, 3, MusicOp::RemoveDegree { pc: 4 });
        let v = a.view();
        assert!(!v.live.contains(&4), "degree 4 is retracted → not sounding");
        assert_eq!(
            v.envelopes.get(&4),
            Some(&pluck()),
            "its envelope register persists past the remove"
        );
    }
}
