//! L0 late-join / offline catch-up cases: W7, W12.
//!
//! A peer that missed a long run of ops rejoins and reconciles to an identical
//! view + entry-hash set. Both end in `assert_converged`.

mod support;

use support::{Policy, Rng, SimNet, random_op};

/// W7 — late joiner catch-up. C is isolated while A and B commit ~50 ops; C joins
/// and reconciles to an identical view / entry-hash set in a small round count.
#[test]
fn w7_late_joiner_catch_up() {
    let mut net = SimNet::new(11, &["A", "B", "C"], Policy::RandomSeeded);
    // Isolate the late joiner for the whole warm-up.
    net.partition("A", "C");
    net.partition("B", "C");

    let mut rng = Rng::new(0x00C0_FFEE);
    for i in 0..50 {
        let author = if i % 2 == 0 { "A" } else { "B" };
        net.act(author, random_op(&mut rng));
    }
    net.step_until_quiescent(); // A and B converge (fully connected)

    // C joins and catches up.
    net.heal();
    let rounds = net.reconcile("A", "C");
    net.reconcile("B", "C");
    net.step_until_quiescent();

    net.assert_converged();
    assert_eq!(
        net.store("C").view(),
        net.view("A"),
        "C's view matches A after catch-up"
    );
    assert_eq!(
        net.store("C").entry_hashes(),
        net.store("A").entry_hashes(),
        "C's entry-hash set matches A",
    );
    assert!(
        rounds < 60,
        "RBSR catch-up round count stays small (got {rounds})"
    );
}

/// W12 — offline peer full catch-up. C is offline for 100 ops (A/B commit and
/// gossip, with some in-run delivery); after heal + reconcile C holds the full
/// set.
#[test]
fn w12_offline_peer_full_catch_up() {
    let mut net = SimNet::new(12, &["A", "B", "C"], Policy::Adversarial);
    net.partition("A", "C");
    net.partition("B", "C");

    let mut rng = Rng::new(0x0000_5EED);
    for i in 0..100 {
        let author = if i % 2 == 0 { "A" } else { "B" };
        net.act(author, random_op(&mut rng));
        if i % 7 == 0 {
            net.step(); // interleave some delivery between A and B
        }
    }
    net.step_until_quiescent();

    net.heal();
    net.reconcile_all();
    net.step_until_quiescent();

    net.assert_converged();
    assert_eq!(
        net.store("C").entry_hashes(),
        net.store("A").entry_hashes(),
        "the offline peer holds the full set after reconcile",
    );
}
