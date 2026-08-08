//! tutti-amy — Stage 1 of the tutti/AMY ESP32 grand challenge.
//!
//! Two things live here:
//!
//! 1. A thin, safe Rust wrapper over the AMY C synthesizer (`Amy`), proving
//!    Rust ↔ AMY works end-to-end on the desktop: start the engine, feed it
//!    compact ASCII wire events, render audio blocks, read the sysclock.
//!
//! 2. The **fold → AMY edge seam** (`pitchset_to_amy_events`): a pure function
//!    that diffs two tutti pitch-sets into AMY note-on/off wire strings. This is
//!    exactly the shape the real fold `Revision{added, retracted}` diff will
//!    feed later (docs/research/tutti-amy-esp32-leaf.md §3.4). Framing (a): AMY
//!    is a render target; the shared object stays the pitch-set; AMY control
//!    state is a *lens over the fold*.
//!
//! AMY is a global singleton (one `amy_global`), so `Amy::start()` hands out a
//! single guard; construct at most one at a time.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::os::raw::{c_char, c_int};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

/// The REAL music domain over tutti-core + the partition→rejoin scenario and AMY
/// driver (docs/research/tutti-amy-esp32-leaf.md, experiment 1).
pub mod music;

mod ffi {
    use super::{c_char, c_int};
    extern "C" {
        // Our config bridge (csrc/amy_shim.c).
        pub fn ws_amy_start_headless();
        pub fn ws_amy_render_block() -> *mut i16;
        pub fn ws_amy_block_frames() -> c_int;
        pub fn ws_amy_nchans() -> c_int;
        pub fn ws_amy_block_samples() -> c_int;
        pub fn ws_amy_sample_rate() -> c_int;

        // AMY's own clean C-ABI surface, bound directly.
        pub fn amy_add_message(message: *const c_char);
        pub fn amy_sysclock() -> u32;
        pub fn amy_stop();
    }
}

/// Ensures only one live `Amy` guard exists (AMY has one global engine).
static AMY_LIVE: AtomicBool = AtomicBool::new(false);

/// A running AMY engine (headless). Dropping it calls `amy_stop()`.
pub struct Amy {
    _priv: (),
}

impl Amy {
    /// Start AMY headless (no audio device, no MIDI). Panics if one is already
    /// live in this process.
    pub fn start() -> Amy {
        if AMY_LIVE.swap(true, Ordering::SeqCst) {
            panic!("AMY is already running (it is a global singleton)");
        }
        // SAFETY: single-threaded startup, guarded by AMY_LIVE.
        unsafe { ffi::ws_amy_start_headless() };
        Amy { _priv: () }
    }

    /// Feed one compact ASCII wire event (e.g. `"v0n60l1"`). Plays immediately
    /// (no `t` prefix → scheduled "now").
    pub fn send(&self, message: &str) {
        let c = CString::new(message).expect("wire message contained a NUL byte");
        // SAFETY: AMY copies out what it needs during parse; pointer valid for the call.
        unsafe { ffi::amy_add_message(c.as_ptr()) };
    }

    /// Render one block: returns a fresh interleaved-stereo i16 buffer of
    /// [`Self::block_samples`] samples (256 frames × 2 chans = 512 by default).
    pub fn render_block(&self) -> Vec<i16> {
        let n = block_samples();
        // SAFETY: ws_amy_render_block returns AMY's output block, valid until the
        // next render call; we copy it out immediately.
        unsafe {
            let p = ffi::ws_amy_render_block();
            assert!(!p.is_null(), "AMY returned a null output block");
            std::slice::from_raw_parts(p, n).to_vec()
        }
    }

    /// AMY's millisecond clock, derived from total samples rendered.
    pub fn sysclock(&self) -> u32 {
        // SAFETY: trivial read.
        unsafe { ffi::amy_sysclock() }
    }
}

impl Drop for Amy {
    fn drop(&mut self) {
        // SAFETY: matched with start(); engine is live.
        unsafe { ffi::amy_stop() };
        AMY_LIVE.store(false, Ordering::SeqCst);
    }
}

/// Frames per render block (AMY_BLOCK_SIZE, 256 on desktop).
pub fn block_frames() -> usize {
    // SAFETY: pure constant getter.
    unsafe { ffi::ws_amy_block_frames() as usize }
}
/// Interleaved channels (AMY_NCHANS, 2).
pub fn nchans() -> usize {
    unsafe { ffi::ws_amy_nchans() as usize }
}
/// Samples per block = frames × channels (512 by default).
pub fn block_samples() -> usize {
    unsafe { ffi::ws_amy_block_samples() as usize }
}
/// Output sample rate in Hz (AMY_SAMPLE_RATE, 44100 on desktop).
pub fn sample_rate() -> usize {
    unsafe { ffi::ws_amy_sample_rate() as usize }
}

/// Root-mean-square level of a block, normalized to [0,1] against full scale.
pub fn rms(block: &[i16]) -> f64 {
    if block.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = block.iter().map(|&s| (s as f64).powi(2)).sum();
    (sum_sq / block.len() as f64).sqrt() / i16::MAX as f64
}

/// Peak absolute sample of a block.
pub fn peak(block: &[i16]) -> i16 {
    block.iter().map(|&s| s.saturating_abs()).max().unwrap_or(0)
}

/// Write interleaved i16 samples as a 16-bit PCM WAV (no external crate). Shared by
/// the render-proof bin and the partition→rejoin acceptance test.
pub fn write_wav(
    path: &str,
    samples: &[i16],
    sample_rate: u32,
    channels: u16,
) -> std::io::Result<()> {
    use std::io::Write;

    let bits_per_sample: u16 = 16;
    let byte_rate = sample_rate * channels as u32 * (bits_per_sample as u32 / 8);
    let block_align = channels * (bits_per_sample / 8);
    let data_bytes = (samples.len() * 2) as u32;

    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data_bytes).to_le_bytes())?;
    f.write_all(b"WAVE")?;
    f.write_all(b"fmt ")?;
    f.write_all(&16u32.to_le_bytes())?; // PCM fmt chunk size
    f.write_all(&1u16.to_le_bytes())?; // audio format = PCM
    f.write_all(&channels.to_le_bytes())?;
    f.write_all(&sample_rate.to_le_bytes())?;
    f.write_all(&byte_rate.to_le_bytes())?;
    f.write_all(&block_align.to_le_bytes())?;
    f.write_all(&bits_per_sample.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&data_bytes.to_le_bytes())?;
    for &s in samples {
        f.write_all(&s.to_le_bytes())?;
    }
    f.flush()
}

// ---------------------------------------------------------------------------
// The tutti → AMY projection (fold → AMY edge seam).
// ---------------------------------------------------------------------------

/// A tuned pitch degree — a stand-in for a tutti/walkie hot-set member.
///
/// A pitch is an integer `degree` (a step in an equal division of the octave)
/// plus the `edo` (divisions per octave). This keeps `Pitch` `Ord`/`Hash` for
/// `BTreeSet` membership while still resolving to a possibly-**fractional** MIDI
/// note — the microtonal payoff AMY gives for free (`n` parses via `atoff`, so
/// AMY takes float notes natively; a non-12 `.scl` renders exactly, with no MPE
/// channel rotation or bend-range negotiation).
///
/// `degree 0, edo 12` is defined to be MIDI note 60 (middle C).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct Pitch {
    /// Steps from the reference (may be negative).
    pub degree: i32,
    /// Divisions of the octave. 12 = standard semitones; 31 = 31-EDO; etc.
    pub edo: u16,
}

/// Reference: `degree 0` maps to this MIDI note.
const REFERENCE_MIDI_NOTE: f32 = 60.0;

impl Pitch {
    /// A standard 12-EDO pitch, `degree` semitones from middle C.
    pub const fn semitone(degree: i32) -> Pitch {
        Pitch { degree, edo: 12 }
    }

    /// A pitch in an arbitrary EDO.
    pub const fn new(degree: i32, edo: u16) -> Pitch {
        Pitch { degree, edo }
    }

    /// The (possibly fractional) MIDI note this degree resolves to.
    pub fn midi_note(&self) -> f32 {
        REFERENCE_MIDI_NOTE + (self.degree as f32) * 12.0 / (self.edo as f32)
    }

    /// The AMY oscillator index this pitch is assigned to.
    ///
    /// A *pure, stable* function of the pitch, so a note-off targets exactly the
    /// oscillator its note-on lit — the anti-stuck-note discipline by
    /// construction. (The real leaf will hand voice allocation to AMY's synth
    /// layer per §3.4; addressing raw oscs here keeps the projection a pure
    /// function with no allocation state, which is what the task asks for.)
    ///
    /// Distinct degrees within one EDO map to distinct oscillators until the
    /// count wraps; a small chord never collides.
    pub fn osc(&self, max_oscs: u16) -> u16 {
        (self.degree.rem_euclid(max_oscs as i32)) as u16
    }
}

/// Format a MIDI note for the wire: integers stay integral (`60`), fractional
/// notes get up to 3 decimals (`60.387`), matching AMY's own 3-dp convention.
fn fmt_midi_note(note: f32) -> String {
    if (note.fract()).abs() < 1e-4 {
        format!("{}", note.round() as i64)
    } else {
        let s = format!("{:.3}", note);
        // trim trailing zeros ("60.500" -> "60.5") for tidy wire strings.
        let trimmed = s.trim_end_matches('0').trim_end_matches('.');
        trimmed.to_string()
    }
}

/// Diff two pitch-sets into AMY wire events.
///
/// * a pitch in `after` but not `before` (added)   → `vN nMIDI l1` (note-on)
/// * a pitch in `before` but not `after` (removed)  → `vN l0`      (note-off)
///
/// Note-offs are emitted before note-ons so that, if two pitches happen to share
/// an oscillator across the transition, the freed voice is reused rather than
/// stomped. `max_oscs` bounds the oscillator address space.
///
/// This is the seam the real fold `Revision{added, retracted, at}` diff will
/// feed (docs/research/tutti-amy-esp32-leaf.md §3.1, §3.4): "reconciliation
/// upstream, events downstream."
pub fn pitchset_to_amy_events(
    before: &BTreeSet<Pitch>,
    after: &BTreeSet<Pitch>,
    max_oscs: u16,
) -> Vec<String> {
    let mut events = Vec::new();

    // Removed → note-off first.
    for p in before.difference(after) {
        events.push(format!("v{}l0", p.osc(max_oscs)));
    }
    // Added → note-on.
    for p in after.difference(before) {
        events.push(format!(
            "v{}n{}l1",
            p.osc(max_oscs),
            fmt_midi_note(p.midi_note())
        ));
    }

    events
}

// ---------------------------------------------------------------------------
// Continuous facets — per-degree amplitude ENVELOPE, projected onto AMY's EG0.
// ---------------------------------------------------------------------------
//
// This is the north-star INTERPOLATION axis made concrete: tutti ships the
// *function* — a sparse breakpoint list plus the rule to fill between the points
// — not audio-rate samples, and AMY evaluates that function at 44.1 kHz locally
// (docs/research/tutti-amy-esp32-leaf.md §4). AMY's own control model is already
// "generators, not streams": each oscillator has two breakpoint envelope
// generators (EG0/EG1), `(time_ms, value)` pairs with a per-EG interpolation
// `eg_type` (amy.h:238-242, 330-333; docs/api.md `A`/`B`/`T`/`X`). The default
// oscillator gates its amplitude by EG0 (`amp_coefs[COEF_EG0]=1`, amy.c:868), so
// an EG0 breakpoint string shapes the note's loudness contour directly.

/// Interpolation kind carried by a continuous envelope facet — the vision's
/// INTERPOLATION axis (docs §8.2): a control point ships *with* the rule the
/// renderer uses to fill between points. Bounded + serde so it is a legal op
/// payload; maps onto AMY's `eg_type` vocabulary at the edge (docs/api.md `T`:
/// 0 Normal/RC, 1 Linear, 2 DX7, 3 True-exponential).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum Interp {
    /// Straight-line between breakpoints → AMY `eg_type = 1` (ENVELOPE_LINEAR).
    #[default]
    Linear,
    /// True-exponential between breakpoints → AMY `eg_type = 3`.
    Exp,
    /// Piecewise-constant (sample-and-hold). AMY has no native step `eg_type`, so
    /// the projection realizes it *honestly* as a staircase over the LINEAR engine
    /// (hold each level flat, then a short jump at each breakpoint), not by
    /// mislabeling it as some other curve family.
    Step,
}

/// Max breakpoints in one envelope facet. Comfortably under AMY's `MAX_BREAKPOINTS`
/// (24, amy.h:240) even after `Interp::Step`'s staircase expansion (≤ 2N-1).
pub const MAX_ENV_POINTS: usize = 8;

/// Max linear-amplitude level a breakpoint may carry (7-bit, MIDI-ish).
pub const MAX_ENV_LEVEL: u8 = 127;

/// A continuous **envelope facet**: a sparse breakpoint list + an interpolation
/// kind. Each `(ms, level)` is a *segment*: reach `level` (0..=127, a linear
/// amplitude) over `ms` milliseconds from the previous point — AMY's own native
/// breakpoint semantics (per-segment deltas, cumulated; envelope.c:81). Per AMY,
/// the LAST point is the *release* segment: it only fires on note-off, so while a
/// note is held the curve runs the earlier points and sustains at the
/// second-to-last level (docs/synth.md, envelope.c:116-131).
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct Envelope {
    /// `(segment_ms, level_0_127)` breakpoints, in order. Domain bound 1..=MAX_ENV_POINTS.
    pub points: Vec<(u16, u8)>,
    /// How the renderer fills between breakpoints.
    pub interp: Interp,
}

/// AMY's `eg_type` code (docs/api.md `T`) for an interpolation kind. `Step` rides
/// the LINEAR engine (its staircase makes the curve piecewise-constant anyway).
fn eg_type_code(interp: Interp) -> u8 {
    match interp {
        Interp::Linear | Interp::Step => 1, // ENVELOPE_LINEAR
        Interp::Exp => 3,                   // ENVELOPE_TRUE_EXPONENTIAL
    }
}

/// One breakpoint level (0..=127) as AMY's linear-amplitude float (0..1), formatted
/// like AMY's own patch strings: fixed 4-dp then trimmed, so it is compact AND a
/// deterministic pure function of the level (determinism gate).
fn fmt_level(level: u8) -> String {
    let v = (level.min(MAX_ENV_LEVEL) as f32) / (MAX_ENV_LEVEL as f32);
    let s = format!("{:.4}", v);
    let trimmed = s.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

/// `Interp::Step` expansion: over AMY's LINEAR engine, hold each level flat for its
/// whole segment then jump to the next in `STEP_JUMP_MS`, yielding a
/// piecewise-constant curve. The last emitted pair stays the release segment.
const STEP_JUMP_MS: u16 = 1;
fn staircase(points: &[(u16, u8)]) -> Vec<(u16, u8)> {
    if points.len() <= 1 {
        return points.to_vec();
    }
    let mut out = Vec::with_capacity(points.len() * 2 - 1);
    out.push(points[0]); // reach level0 over its ms (usually 0 = instant)
    for w in points.windows(2) {
        let (_prev_ms, prev_level) = w[0];
        let (seg_ms, level) = w[1];
        let hold = seg_ms.saturating_sub(STEP_JUMP_MS);
        out.push((hold, prev_level)); // hold flat at the previous level
        out.push((STEP_JUMP_MS, level)); // then jump to the new level
    }
    out
}

/// Project an [`Envelope`] facet into AMY's amplitude-EG wire fragment: the `A`
/// breakpoint-set string (docs/api.md `A` → `eg0_times`/`eg0_values`; comma-
/// separated `time_ms,value` pairs, last pair = release) plus the `T` eg_type
/// code (docs/api.md `T` → `eg_type[0]`). AMY's tokenizer copies the `A` argument
/// over the charset `" 0123456789-,."` and stops at the next letter (parse.c:188),
/// so appending `T…` (and later `l1`) to the same message is unambiguous.
///
/// Example — `Envelope{ points:[(0,127),(120,12),(40,0)], interp: Exp }`
/// projects to `"A0,1,120,0.0945,40,0T3"`: jump to full, true-exp decay to ~0.09
/// over 120 ms, then a 40 ms release to silence on note-off.
pub fn envelope_to_amy(env: &Envelope) -> String {
    let pairs: Vec<(u16, u8)> = match env.interp {
        Interp::Step => staircase(&env.points),
        _ => env.points.clone(),
    };
    let mut s = String::from("A");
    for (i, (ms, level)) in pairs.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&ms.to_string());
        s.push(',');
        s.push_str(&fmt_level(*level));
    }
    s.push('T');
    s.push_str(&eg_type_code(env.interp).to_string());
    s
}

/// Diff two degree-sets into AMY wire events, where each *added* degree's note-on
/// carries its converged **envelope facet** (if the room holds a register for that
/// degree) as an AMY `A`/`T` amplitude-EG fragment; a degree with no facet uses
/// AMY's default envelope. Degrees are the room's scale steps (== `Pitch::degree`);
/// `edo` tunes them to (possibly fractional) MIDI notes, so microtonality is
/// preserved through the envelope-carrying note-on too.
///
/// Note-offs precede note-ons (voice reuse), exactly as [`pitchset_to_amy_events`].
/// With an empty `envelopes` map this emits byte-identical events to that function.
pub fn degrees_to_amy_events(
    before: &BTreeSet<u16>,
    after: &BTreeSet<u16>,
    envelopes: &BTreeMap<u16, Envelope>,
    edo: u16,
    max_oscs: u16,
) -> Vec<String> {
    let mut events = Vec::new();
    for &pc in before.difference(after) {
        let p = Pitch::new(pc as i32, edo);
        events.push(format!("v{}l0", p.osc(max_oscs)));
    }
    for &pc in after.difference(before) {
        let p = Pitch::new(pc as i32, edo);
        let mut ev = format!("v{}n{}", p.osc(max_oscs), fmt_midi_note(p.midi_note()));
        if let Some(env) = envelopes.get(&pc) {
            ev.push_str(&envelope_to_amy(env));
        }
        ev.push_str("l1");
        events.push(ev);
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(pitches: &[Pitch]) -> BTreeSet<Pitch> {
        pitches.iter().copied().collect()
    }

    #[test]
    fn envelope_to_amy_linear_and_exp_grammar() {
        // Linear pluck-ish: instant to full, ramp to half over 200 ms, release.
        let lin = Envelope {
            points: vec![(0, 127), (200, 64), (100, 0)],
            interp: Interp::Linear,
        };
        // 127/127=1, 64/127=0.5039..→"0.5039", 0→"0". Last pair is release.
        assert_eq!(envelope_to_amy(&lin), "A0,1,200,0.5039,100,0T1");

        // True-exp decay (the docstring's example), eg_type 3.
        let exp = Envelope {
            points: vec![(0, 127), (120, 12), (40, 0)],
            interp: Interp::Exp,
        };
        assert_eq!(envelope_to_amy(&exp), "A0,1,120,0.0945,40,0T3");
    }

    #[test]
    fn envelope_to_amy_step_is_a_staircase_on_linear() {
        // Step over 3 points expands to hold-then-jump on the LINEAR engine (T1).
        let step = Envelope {
            points: vec![(0, 0), (200, 64), (100, 127)],
            interp: Interp::Step,
        };
        // points[0]=(0,0); window0 -> (200-1, 0),(1, 0.5039); window1 -> (100-1, 0.5039),(1,1)
        assert_eq!(
            envelope_to_amy(&step),
            "A0,0,199,0,1,0.5039,99,0.5039,1,1T1"
        );
    }

    #[test]
    fn envelope_projection_is_a_deterministic_pure_function() {
        // Equal envelopes ⇒ byte-identical strings, every time (determinism gate).
        let e = Envelope {
            points: vec![(0, 8), (350, 127), (60, 0)],
            interp: Interp::Linear,
        };
        assert_eq!(envelope_to_amy(&e), envelope_to_amy(&e.clone()));
    }

    #[test]
    fn degree_note_on_carries_the_facet_or_the_default() {
        let mut envs: BTreeMap<u16, Envelope> = BTreeMap::new();
        envs.insert(
            0,
            Envelope {
                points: vec![(0, 127), (120, 12), (40, 0)],
                interp: Interp::Exp,
            },
        );
        let before = BTreeSet::new();
        let after = BTreeSet::from([0u16, 7u16]);
        let ev = degrees_to_amy_events(&before, &after, &envs, 12, 250);
        // Degree 0 (osc 0) carries its envelope; degree 7 (osc 7) has no facet → default.
        assert!(ev.contains(&"v0n60A0,1,120,0.0945,40,0T3l1".to_string()));
        assert!(ev.contains(&"v7n67l1".to_string()));

        // With NO facets the envelope path is byte-identical to the plain projection.
        let plain = pitchset_to_amy_events(
            &BTreeSet::new(),
            &BTreeSet::from([Pitch::semitone(0), Pitch::semitone(7)]),
            250,
        );
        let via_env = degrees_to_amy_events(&before, &after, &BTreeMap::new(), 12, 250);
        assert_eq!(via_env, plain);
    }

    #[test]
    fn midi_note_reference_and_semitones() {
        assert_eq!(Pitch::semitone(0).midi_note(), 60.0);
        assert_eq!(Pitch::semitone(12).midi_note(), 72.0);
        assert_eq!(Pitch::semitone(-12).midi_note(), 48.0);
    }

    #[test]
    fn fractional_midi_note_for_31_edo() {
        // One step of 31-EDO above middle C = 60 + 12/31 ≈ 60.387.
        let p = Pitch::new(1, 31);
        assert!((p.midi_note() - 60.3871).abs() < 1e-3);
        assert_eq!(fmt_midi_note(p.midi_note()), "60.387");
    }

    #[test]
    fn empty_to_chord_is_all_note_ons() {
        let before = set(&[]);
        let after = set(&[Pitch::semitone(0), Pitch::semitone(4), Pitch::semitone(7)]);
        let ev = pitchset_to_amy_events(&before, &after, 250);
        // C major triad on distinct oscs 0, 4, 7.
        assert_eq!(ev.len(), 3);
        assert!(ev.contains(&"v0n60l1".to_string()));
        assert!(ev.contains(&"v4n64l1".to_string()));
        assert!(ev.contains(&"v7n67l1".to_string()));
    }

    #[test]
    fn chord_to_empty_is_all_note_offs() {
        let before = set(&[Pitch::semitone(0), Pitch::semitone(7)]);
        let after = set(&[]);
        let ev = pitchset_to_amy_events(&before, &after, 250);
        assert_eq!(ev, vec!["v0l0".to_string(), "v7l0".to_string()]);
    }

    #[test]
    fn only_the_delta_is_emitted() {
        // Hold 0 and 7, add 4, drop 7.
        let before = set(&[Pitch::semitone(0), Pitch::semitone(7)]);
        let after = set(&[Pitch::semitone(0), Pitch::semitone(4)]);
        let ev = pitchset_to_amy_events(&before, &after, 250);
        // 0 is held (no event), 7 off, 4 on. Off precedes on.
        assert_eq!(ev, vec!["v7l0".to_string(), "v4n64l1".to_string()]);
    }

    #[test]
    fn note_off_targets_the_same_osc_as_note_on() {
        // Any pitch's note-on and note-off address the same oscillator — the
        // no-stuck-note invariant, as a pure property of Pitch::osc.
        for degree in -40..40 {
            let p = Pitch::semitone(degree);
            let on = pitchset_to_amy_events(&set(&[]), &set(&[p]), 250);
            let off = pitchset_to_amy_events(&set(&[p]), &set(&[]), 250);
            let on_osc = on[0].split('n').next().unwrap();
            let off_osc = off[0].trim_end_matches("l0");
            assert_eq!(on_osc, off_osc, "on/off osc mismatch for degree {degree}");
        }
    }
}
