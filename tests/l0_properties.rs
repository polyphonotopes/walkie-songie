//! L0 randomized property: W15.
//!
//! For each seed in `0..64`, build 4–6 peers, apply 6–30 random ops interleaved
//! with random partitions/heals under a per-seed policy, then heal + reconcile and
//! assert convergence. Each seed runs under `catch_unwind` so a failure prints the
//! reproducing seed, and each scenario is run twice to guard determinism (the
//! trace must be bit-identical).

mod support;

use support::{random_op, Policy, Rng, SimNet, TraceEvent};

/// Run one fully-seeded scenario and return its trace.
fn run_scenario(seed: u64) -> Vec<TraceEvent> {
    let mut sel = Rng::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xDEAD_BEEF);

    let n_peers = 4 + sel.gen_range(3); // 4..=6
    let all = ["P0", "P1", "P2", "P3", "P4", "P5"];
    let names: Vec<&str> = all[..n_peers].to_vec();
    let policy = match sel.gen_range(3) {
        0 => Policy::Fifo,
        1 => Policy::RandomSeeded,
        _ => Policy::Adversarial,
    };

    let mut net = SimNet::new(seed, &names, policy);
    let n_ops = 6 + sel.gen_range(25); // 6..=30
    for _ in 0..n_ops {
        match sel.gen_range(6) {
            0 => {
                let a = names[sel.gen_range(n_peers)];
                let b = names[sel.gen_range(n_peers)];
                if a != b {
                    net.partition(a, b);
                }
            }
            1 => net.heal(),
            2 => {
                net.step();
            }
            _ => {}
        }
        let author = names[sel.gen_range(n_peers)];
        net.act(author, random_op(&mut sel));
    }

    // Heal everything, drain the queue, then repair dropped ops and drain again.
    net.heal();
    net.step_until_quiescent();
    net.reconcile_all();
    net.step_until_quiescent();
    net.assert_converged();
    net.trace().to_vec()
}

/// W15 — N-peer randomized convergence property with a determinism guard.
///
/// Every seed in `0..64` gets a convergence check (`run_scenario` ends in
/// `assert_converged`). A representative subset additionally runs twice and
/// asserts the two traces are bit-identical — the trace-determinism guard. Each
/// seed runs under `catch_unwind` so a failure prints the reproducing seed.
#[test]
fn w15_n_peer_randomized_property() {
    /// Seeds that also get the (2×) trace-determinism guard.
    const DETERMINISM_GUARD_SEEDS: u64 = 8;

    for seed in 0..64u64 {
        let outcome = std::panic::catch_unwind(|| {
            let first = run_scenario(seed);
            if seed < DETERMINISM_GUARD_SEEDS {
                let second = run_scenario(seed);
                assert_eq!(first, second, "scenario is not deterministic at seed {seed}");
            }
        });
        if let Err(payload) = outcome {
            eprintln!("\n=== W15 property FAILED — reproduce with seed = {seed} ===\n");
            std::panic::resume_unwind(payload);
        }
    }
}
