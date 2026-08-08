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

use std::collections::BTreeSet;
use std::ffi::CString;
use std::os::raw::{c_char, c_int};
use std::sync::atomic::{AtomicBool, Ordering};

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

#[cfg(test)]
mod tests {
    use super::*;

    fn set(pitches: &[Pitch]) -> BTreeSet<Pitch> {
        pitches.iter().copied().collect()
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
