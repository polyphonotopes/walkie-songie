//! Desktop-AMY tutti leaf — the audible render (experiment 1 of
//! `docs/research/tutti-amy-esp32-leaf.md`).
//!
//! Run: `cargo run` inside tutti-amy/ (AMY is compiled by build.rs).
//!
//!   Checkpoint 1: prove Rust ↔ AMY renders non-silent audio (raw oscillator).
//!   Checkpoint 2: run the REAL two-peer partition→rejoin scenario over
//!                 `Store<MusicLang>` and drive AMY from its converging fold —
//!                 the same path the acceptance test asserts.
//!   Checkpoint 3: dump the partition→rejoin render to partition-rejoin.wav so a
//!                 human can hear the union click into place.

use tutti_amy::music::{self, EDO};
use tutti_amy::{block_frames, nchans, rms, sample_rate, write_wav, Amy};

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

fn main() {
    println!("=== tutti-amy: desktop AMY tutti leaf ===");
    println!(
        "AMY geometry: {} frames/block × {} chans @ {} Hz  (one block = {:.2} ms)",
        block_frames(),
        nchans(),
        sample_rate(),
        block_frames() as f64 * 1000.0 / sample_rate() as f64,
    );

    let amy = Amy::start();
    println!("amy_start() OK. sysclock = {} ms", amy.sysclock());

    // ------------------------------------------------------------------
    // Checkpoint 1 — raw render proof: note-on → non-silent; note-off → decay.
    // ------------------------------------------------------------------
    println!("\n--- Checkpoint 1: Rust ↔ AMY render proof ---");
    let mut baseline = Vec::new();
    for _ in 0..2 {
        baseline.push(rms(&amy.render_block()));
    }
    amy.send("v0n60l1"); // osc 0, middle C, velocity 1
    let mut on = Vec::new();
    for _ in 0..10 {
        let block = amy.render_block();
        on.push(rms(&block));
    }
    amy.send("v0l0"); // note off
    let mut off = Vec::new();
    for _ in 0..10 {
        off.push(rms(&amy.render_block()));
    }
    let (b, o, tail) = (mean(&baseline), mean(&on), mean(&off[off.len() - 3..]));
    let on_peak = on.iter().cloned().fold(0.0_f64, f64::max);
    assert!(on_peak > 0.0, "silent after note-on");
    assert!(o > b + 0.01, "rms did not rise after note-on");
    assert!(tail < o, "rms did not decay after note-off");
    println!(
        "PASS: rms {b:.4} → {o:.4} on note-on (peak {on_peak:.4}), decayed to {tail:.4} on note-off."
    );
    // Let the tail ring out before the scenario so it doesn't smear step 0.
    for _ in 0..12 {
        let _ = amy.render_block();
    }

    // ------------------------------------------------------------------
    // Checkpoint 2 — the REAL partition→rejoin scenario drives AMY.
    // ------------------------------------------------------------------
    println!("\n--- Checkpoint 2: two Store<MusicLang> peers, partition → rejoin ---");
    let s = music::run_scenario();
    println!("  partition:  A.view={:?}   B.view={:?}", s.a_partition, s.b_partition);
    assert_ne!(s.a_partition, s.b_partition, "partition views must differ");
    println!(
        "  rejoin:     A.view={:?}  ==  B.view={:?}",
        s.a_converged, s.b_converged
    );
    assert_eq!(s.a_converged, s.b_converged, "peers must converge");
    assert_eq!(s.a_converged, s.expected_union, "must equal the union");
    println!(
        "  converged to the union {:?} (degree {} removed — the remove wins).",
        s.expected_union, s.removed_degree
    );

    println!("\n  driving AMY from the converging fold (EDO={EDO}):");
    let report = music::drive_amy(&amy, &s.timeline, EDO, 24, 40);
    for (i, degrees) in s.timeline.iter().enumerate() {
        println!(
            "    step {i}: view={:<20} rms={:.4}",
            format!("{degrees:?}"),
            report.step_rms[i]
        );
    }
    println!("    teardown rms = {:.4}", report.teardown_rms);
    println!("    wire events: {:?}", report.events);

    let union_rms = report.step_rms[s.timeline.len() - 1];
    assert!(union_rms > report.step_rms[2], "union must be louder than the 2-note partition chord");
    assert_eq!(report.note_ons, report.note_offs, "balanced note on/off");
    assert!(report.stuck_oscs.is_empty(), "no stuck notes");
    assert!(report.teardown_rms < union_rms * 0.2, "silent after release");
    println!(
        "  PASS: union ({union_rms:.4}) louder than partition; {} on / {} off balanced; silent tail ({:.4}) — NO STUCK NOTES.",
        report.note_ons, report.note_offs, report.teardown_rms
    );

    // A peek at the microtonal payoff: fractional MIDI notes for a 31-EDO room.
    let fractional: Vec<&String> = report
        .events
        .iter()
        .filter(|e| e.contains('n') && e.contains('.'))
        .collect();
    println!("  microtonal (31-EDO) note-ons carry fractional MIDI notes: {fractional:?}");

    // ------------------------------------------------------------------
    // Checkpoint 3 — write the partition→rejoin WAV.
    // ------------------------------------------------------------------
    let wav = concat!(env!("CARGO_MANIFEST_DIR"), "/partition-rejoin.wav");
    match write_wav(wav, &report.pcm, sample_rate() as u32, nchans() as u16) {
        Ok(()) => println!(
            "\n--- Checkpoint 3: wrote {wav} ({} frames, {:.2} s) ---",
            report.pcm.len() / nchans(),
            (report.pcm.len() / nchans()) as f64 / sample_rate() as f64
        ),
        Err(err) => eprintln!("could not write WAV: {err}"),
    }

    println!("\nfinal sysclock = {} ms. all checkpoints passed.", amy.sysclock());
    drop(amy); // amy_stop()
}
