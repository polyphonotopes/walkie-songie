//! Desktop-AMY render proof + tutti→AMY projection demo.
//!
//! Run: `cargo run` inside tutti-amy/ (AMY is compiled by build.rs).
//!
//!   Checkpoint 1: prove Rust ↔ AMY renders non-silent audio.
//!   Checkpoint 2: drive AMY from a diff of hand-built pitch-sets (the fold→AMY
//!                 edge seam), including a microtonal (31-EDO) chord.
//!   Checkpoint 3: dump the whole render to render-proof.wav so a human can listen.

use std::collections::BTreeSet;
use std::io::Write;
use tutti_amy::{
    block_frames, block_samples, nchans, peak, pitchset_to_amy_events, rms, sample_rate, Amy, Pitch,
};

/// Render `n` blocks, appending interleaved samples to `sink`; return per-block RMS.
fn render_n(amy: &Amy, n: usize, sink: &mut Vec<i16>) -> Vec<f64> {
    let mut rmss = Vec::with_capacity(n);
    for _ in 0..n {
        let block = amy.render_block();
        rmss.push(rms(&block));
        sink.extend_from_slice(&block);
    }
    rmss
}

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

fn main() {
    println!("=== tutti-amy: desktop AMY render proof ===");
    println!(
        "AMY geometry: {} frames/block × {} chans = {} samples/block @ {} Hz  (one block = {:.2} ms)",
        block_frames(),
        nchans(),
        block_samples(),
        sample_rate(),
        block_frames() as f64 * 1000.0 / sample_rate() as f64,
    );

    let amy = Amy::start();
    println!("amy_start() OK. sysclock = {} ms", amy.sysclock());

    // Accumulate everything rendered for the WAV dump (checkpoint 3).
    let mut pcm: Vec<i16> = Vec::new();

    // ------------------------------------------------------------------
    // Checkpoint 1 — render proof: note-on must produce non-silent audio;
    // note-off must let it decay.
    // ------------------------------------------------------------------
    println!("\n--- Checkpoint 1: render proof ---");

    // Baseline: a couple of blocks of silence before any note.
    let baseline = render_n(&amy, 2, &mut pcm);
    println!("baseline (pre-note) RMS: {:?}", fmt_rmss(&baseline));

    // Note-on: osc 0, MIDI note 60 (middle C), velocity 1.0 — a bare sine osc.
    println!("send  \"v0n60l1\"   (osc 0, note 60, vel 1.0)");
    amy.send("v0n60l1");

    let mut on_rmss = Vec::new();
    println!("rendering 10 blocks after note-on:");
    for i in 0..10 {
        let block = amy.render_block();
        let r = rms(&block);
        let pk = peak(&block);
        on_rmss.push(r);
        pcm.extend_from_slice(&block);
        println!(
            "  block {:2}: sysclock={:5} ms  rms={:.4}  peak={:6}",
            i,
            amy.sysclock(),
            r,
            pk
        );
    }

    // Note-off: velocity 0 on the same osc.
    println!("send  \"v0l0\"      (note off)");
    amy.send("v0l0");
    let off_rmss = render_n(&amy, 10, &mut pcm);
    println!("post-note-off RMS: {:?}", fmt_rmss(&off_rmss));

    // Assertions: the heart of the proof.
    let baseline_mean = mean(&baseline);
    let on_mean = mean(&on_rmss);
    let off_tail = mean(&off_rmss[off_rmss.len().saturating_sub(3)..]);
    let on_peak = on_rmss.iter().cloned().fold(0.0_f64, f64::max);

    assert!(
        on_peak > 0.0,
        "RENDER PROOF FAILED: output was silent after note-on"
    );
    assert!(
        on_mean > baseline_mean + 0.01,
        "RENDER PROOF FAILED: RMS did not rise after note-on (baseline={baseline_mean:.4}, on={on_mean:.4})"
    );
    assert!(
        off_tail < on_mean,
        "RENDER PROOF FAILED: RMS did not decay after note-off (on={on_mean:.4}, tail={off_tail:.4})"
    );
    println!(
        "PASS: non-silent after note-on (peak RMS {on_peak:.4}), RMS rose {baseline_mean:.4} -> {on_mean:.4} \
         and decayed to {off_tail:.4} after note-off."
    );

    // ------------------------------------------------------------------
    // Checkpoint 2 — tutti → AMY projection: drive AMY from a diff of
    // hand-built pitch-sets (a tiny fake "room"), asserting audible changes
    // and, at the end, a return to silence (no stuck notes).
    // ------------------------------------------------------------------
    println!("\n--- Checkpoint 2: tutti→AMY projection (fold→AMY edge seam) ---");

    const MAX_OSCS: u16 = 250;

    // A fake room's evolving hot set. Each step is what the fold would resolve to.
    let empty: BTreeSet<Pitch> = BTreeSet::new();
    let c = Pitch::semitone(0); // C4
    let e = Pitch::semitone(4); // E4
    let g = Pitch::semitone(7); // G4
    let b = Pitch::semitone(11); // B4
    let c5 = Pitch::semitone(12); // C5

    let timeline: Vec<BTreeSet<Pitch>> = vec![
        [c].into_iter().collect(),           // single note
        [c, e, g].into_iter().collect(),     // C major triad
        [c, e, g, b].into_iter().collect(),  // Cmaj7
        [e, g, b, c5].into_iter().collect(), // drop the root, add the octave
        empty.clone(),                       // release everything
    ];

    let mut prev = empty.clone();
    let mut per_step_rms = Vec::new();
    for (i, next) in timeline.iter().enumerate() {
        let events = pitchset_to_amy_events(&prev, next, MAX_OSCS);
        println!(
            "step {}: set={:<22} events={:?}",
            i,
            fmt_set(next),
            events
        );
        for ev in &events {
            amy.send(ev);
        }
        // Let each chord ring for ~120 ms (~21 blocks).
        let rmss = render_n(&amy, 21, &mut pcm);
        let m = mean(&rmss);
        per_step_rms.push(m);
        println!("         mean RMS while ringing: {m:.4}");
        prev = next.clone();
    }

    // Audible-change assertions.
    let single = per_step_rms[0];
    let triad = per_step_rms[1];
    let silence = per_step_rms[per_step_rms.len() - 1];
    assert!(
        triad > single,
        "PROJECTION FAILED: a 3-note chord was not louder than a single note ({triad:.4} vs {single:.4})"
    );
    assert!(
        silence < single * 0.2,
        "PROJECTION FAILED: room did not fall silent after releasing all pitches (tail RMS {silence:.4}) — stuck note?"
    );
    println!(
        "PASS: chord ({triad:.4}) louder than single note ({single:.4}); silent after full release ({silence:.4}, no stuck notes)."
    );

    // ------------------------------------------------------------------
    // The microtonal payoff (rides checkpoint 2): render a 31-EDO chord
    // exactly, via fractional MIDI notes — no MPE, no bend-range negotiation.
    // ------------------------------------------------------------------
    println!("\n--- Microtonal payoff: a 31-EDO chord via fractional MIDI notes ---");
    // A near-just major triad in 31-EDO: steps 0, 10, 18 (≈ 5:4 and 3:2).
    let micro: BTreeSet<Pitch> = [Pitch::new(0, 31), Pitch::new(10, 31), Pitch::new(18, 31)]
        .into_iter()
        .collect();
    let micro_events = pitchset_to_amy_events(&empty, &micro, MAX_OSCS);
    println!("31-EDO triad events (note the fractional notes): {micro_events:?}");
    for ev in &micro_events {
        amy.send(ev);
    }
    let micro_rms = render_n(&amy, 21, &mut pcm);
    println!("         mean RMS: {:.4}", mean(&micro_rms));
    // Release it.
    for ev in pitchset_to_amy_events(&micro, &empty, MAX_OSCS) {
        amy.send(&ev);
    }
    render_n(&amy, 10, &mut pcm);
    assert!(
        mean(&micro_rms) > 0.01,
        "MICROTONAL DEMO FAILED: 31-EDO chord was silent"
    );
    println!("PASS: 31-EDO chord rendered non-silent through fractional MIDI notes.");

    // ------------------------------------------------------------------
    // Checkpoint 3 — dump the whole render to a WAV.
    // ------------------------------------------------------------------
    let wav_path = concat!(env!("CARGO_MANIFEST_DIR"), "/render-proof.wav");
    match write_wav(wav_path, &pcm, sample_rate() as u32, nchans() as u16) {
        Ok(()) => println!(
            "\n--- Checkpoint 3: wrote {} ({} frames, {:.2} s) ---",
            wav_path,
            pcm.len() / nchans(),
            (pcm.len() / nchans()) as f64 / sample_rate() as f64
        ),
        Err(err) => eprintln!("could not write WAV: {err}"),
    }

    println!("\nfinal sysclock = {} ms. all checkpoints passed.", amy.sysclock());
    drop(amy); // amy_stop()
}

fn fmt_rmss(rmss: &[f64]) -> Vec<String> {
    rmss.iter().map(|r| format!("{r:.4}")).collect()
}

fn fmt_set(set: &BTreeSet<Pitch>) -> String {
    let notes: Vec<String> = set.iter().map(|p| format!("{}", p.midi_note())).collect();
    format!("[{}]", notes.join(","))
}

/// Minimal 16-bit PCM WAV writer (no external crate).
fn write_wav(path: &str, samples: &[i16], sample_rate: u32, channels: u16) -> std::io::Result<()> {
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
