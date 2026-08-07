//! L0 fault cases: W2, W8–W11, W13 (bus), W14.
//!
//! Loss, deferral, dedup, forgery rejection, link flap, and cross-key causal
//! coupling — each ending in `assert_converged`.

mod support;

use support::{
    Peer, Policy, SEED_C, SimNet, tet_definition, tet_degree, tet_pitch, tuning_with_step,
};
use walkie_songie::room::ops::WalkieOp::*;
use walkie_songie::room::ops::{LogHead, sign_op_for_topic_observing};

/// W2 — a remove lost in transit. A adds then removes (5,12); the remove is
/// dropped, so A and B diverge; `reconcile()` repairs -> the key is dead.
#[test]
fn w2_remove_lost_in_transit_repaired_by_reconcile() {
    let mut net = SimNet::new(1, &["A", "B"], Policy::Fifo);
    net.act(
        "A",
        AddDegree {
            pitch: tet_degree(7),
        },
    );
    net.step_until_quiescent(); // B receives the add
    assert!(
        net.view("B").pitches.contains(&tet_degree(7)),
        "key is live on B"
    );

    net.act_dropped(
        "A",
        RemoveDegree {
            pitch: tet_degree(7),
        },
    ); // A removes; lost in transit
    assert!(
        !net.view("A").pitches.contains(&tet_degree(7)),
        "A sees the key dead"
    );
    assert!(
        net.view("B").pitches.contains(&tet_degree(7)),
        "B still sees it live -> divergence"
    );

    net.reconcile("A", "B");
    net.assert_converged();
    assert!(
        !net.view("A").pitches.contains(&tet_degree(7)),
        "reconcile repairs -> key dead everywhere"
    );
}

/// W8 — loss stalls a deterministic prefix. Drop v3 of a 5-op config chain: the
/// view stalls at v2 (never gap-jumps to v5) with v4/v5 parked; repair -> v5.
/// Pins the strict-deferral liveness invariant.
#[test]
fn w8_loss_stalls_a_deterministic_prefix() {
    let mut net = SimNet::new(1, &["A", "B"], Policy::Fifo);
    let config = |locked| SetConfig {
        pieces_locked: Some(locked),
        available_emojis: None,
    };
    net.act("A", config(true)); // v1
    net.act("A", config(false)); // v2
    net.step_until_quiescent(); // B: v1, v2

    net.act_dropped("A", config(true)); // v3 lost
    net.act("A", config(false)); // v4
    net.act("A", config(true)); // v5
    net.step_until_quiescent(); // v4, v5 arrive but park behind missing v3

    assert_eq!(
        net.store("B").pending_len(),
        2,
        "v4, v5 parked behind missing v3"
    );
    assert!(
        !net.view("B").pieces_locked,
        "view is v2, not the gap-jumped v5"
    );

    net.reconcile("A", "B");
    net.assert_converged();
    assert!(
        net.view("B").pieces_locked,
        "repair drains the prefix -> v5"
    );
}

/// W9 — dedup / idempotency on reconnect. Re-delivering all of A's history and a
/// reconcile leave every peer's entry-hash set and view unchanged, and a second
/// reconcile transfers zero.
#[test]
fn w9_dedup_idempotency_on_reconnect() {
    let mut net = SimNet::new(3, &["A", "B", "C"], Policy::RandomSeeded);
    net.act(
        "A",
        AddDegree {
            pitch: tet_degree(1),
        },
    );
    net.act(
        "B",
        AddDegree {
            pitch: tet_degree(0),
        },
    );
    net.act(
        "C",
        SetTuning {
            definition: tet_definition(),
        },
    );
    net.act(
        "A",
        PutPiece {
            emoji: "🎵".into(),
            pitch: tet_pitch(60),
        },
    );
    net.step_until_quiescent();
    net.assert_converged();

    let names: Vec<String> = net.names().to_vec();
    let before: Vec<_> = names.iter().map(|n| net.store(n).entry_hashes()).collect();

    // Reconnect: A re-dumps its whole history to everyone.
    net.regossip("A");
    net.step_until_quiescent();
    net.reconcile("A", "B");
    for (i, n) in names.iter().enumerate() {
        assert_eq!(
            net.store(n).entry_hashes(),
            before[i],
            "re-delivery changed {n}"
        );
    }

    // A second reconcile is opening + agreement: one round, zero transfer.
    let hashes_a = net.store("A").entry_hashes();
    let rounds2 = net.reconcile("A", "B");
    assert_eq!(
        net.store("A").entry_hashes(),
        hashes_a,
        "second reconcile changed nothing"
    );
    assert_eq!(rounds2, 1, "second reconcile transfers zero");
    net.assert_converged();
}

/// W10 — a duplicate delivered while pending. Op X (parent missing) arrives twice
/// then the parent arrives; X is lifted exactly once.
#[test]
fn w10_dup_while_pending_single_lift() {
    let mut net = SimNet::new(1, &["A", "B"], Policy::Fifo);
    let parent = net.act_dropped(
        "A",
        AddDegree {
            pitch: tet_degree(3),
        },
    ); // parent, not sent
    let child = net.act_dropped(
        "A",
        AddDegree {
            pitch: tet_degree(4),
        },
    ); // observes parent, not sent

    net.inject("B", &child);
    net.inject("B", &child); // duplicate while pending
    assert_eq!(
        net.store("B").pending_len(),
        1,
        "duplicate deferral is idempotent"
    );

    net.inject("B", &parent);
    assert_eq!(net.store("B").pending_len(), 0, "parent releases the child");
    assert_eq!(
        net.store("B").entry_hashes().len(),
        2,
        "parent + one child (no double-lift)"
    );
    net.assert_converged();
}

/// W11 — forged / tampered / wrong-topic bytes are rejected, and the converged
/// state is byte-identical to a clean run that never saw them.
#[test]
fn w11_forged_and_tampered_rejected() {
    // Clean baseline.
    let mut clean = SimNet::new(2, &["A", "B"], Policy::Fifo);
    clean.act(
        "A",
        AddDegree {
            pitch: tet_degree(5),
        },
    );
    clean.act(
        "B",
        AddDegree {
            pitch: tet_degree(0),
        },
    );
    clean.step_until_quiescent();
    clean.assert_converged();
    let clean_hashes = clean.store("A").entry_hashes();
    let clean_view = clean.view("A");

    // Same scenario, but B is also fed forged bytes.
    let mut net = SimNet::new(2, &["A", "B"], Policy::Fifo);
    let good = net.act(
        "A",
        AddDegree {
            pitch: tet_degree(5),
        },
    );
    net.act(
        "B",
        AddDegree {
            pitch: tet_degree(0),
        },
    );

    // (1) tampered payload -> PayloadMismatch.
    let mut tampered = good.clone();
    let last = tampered.payload.len() - 1;
    tampered.payload[last] ^= 0xff;
    let out = net.inject("B", &tampered);
    assert!(
        out.starts_with("rejected"),
        "tampered payload rejected: {out}"
    );

    // (2) wrong topic -> TopicMismatch.
    let forger = Peer::new(&SEED_C);
    let (wrong_topic, _) = sign_op_for_topic_observing(
        &forger.key,
        &LogHead::genesis(),
        1,
        "other-room",
        [],
        AddDegree {
            pitch: tet_degree(9),
        },
    );
    let out = net.inject("B", &wrong_topic);
    assert!(
        out.starts_with("rejected"),
        "wrong-topic op rejected: {out}"
    );

    net.step_until_quiescent();
    net.assert_converged();
    assert_eq!(
        net.store("A").entry_hashes(),
        clean_hashes,
        "forged bytes leave state identical"
    );
    assert_eq!(
        net.view("A"),
        clean_view,
        "forged bytes do not perturb the view"
    );
}

/// W13 — intermittent flap (bus): the A–B link flaps 10× (seeded) while all peers
/// commit; entry-hash sets grow monotonically, and a final heal + reconcile
/// converges.
#[test]
fn w13_intermittent_flap_bus() {
    let mut net = SimNet::new(7, &["A", "B", "C"], Policy::RandomSeeded);
    let mut prev = net.entry_hash_sizes();
    for i in 0..10 {
        net.partition("A", "B");
        net.act(
            "A",
            AddDegree {
                pitch: tet_degree((i % 12) as u16),
            },
        );
        net.act(
            "B",
            SetConfig {
                pieces_locked: Some(i % 2 == 0),
                available_emojis: None,
            },
        );
        net.act(
            "C",
            SetTuning {
                definition: tuning_with_step(100 + i as u16 * 50),
            },
        );
        net.step_until_quiescent();
        net.heal();
        net.step_until_quiescent();

        let sizes = net.entry_hash_sizes();
        for (p, &s) in sizes.iter().enumerate() {
            assert!(s >= prev[p], "entry-hash set shrank on peer {p}");
        }
        prev = sizes;
    }

    net.heal();
    net.reconcile_all();
    net.step_until_quiescent();
    net.assert_converged();
}

/// W14 — cross-key causal coupling. An op on key Y observes an op on key X in its
/// horizon; where X is missing, Y defers (neither key visible). Repair releases
/// both.
#[test]
fn w14_cross_key_causal_coupling() {
    let mut net = SimNet::new(1, &["A", "B"], Policy::Fifo);
    net.act_dropped(
        "A",
        AddDegree {
            pitch: tet_degree(1),
        },
    ); // key X, lost
    net.act(
        "A",
        AddDegree {
            pitch: tet_degree(2),
        },
    ); // key Y, observes X
    net.step_until_quiescent(); // B gets Y, defers (X missing)

    assert_eq!(
        net.store("B").pending_len(),
        1,
        "Y defers on the missing X (frontier coupling)"
    );
    assert!(
        net.view("B").pitches.is_empty(),
        "neither key visible while Y is parked"
    );

    net.reconcile("A", "B"); // repair releases both
    net.assert_converged();
    let view = net.view("B");
    assert!(
        view.pitches.contains(&tet_degree(1)) && view.pitches.contains(&tet_degree(2)),
        "both keys released after repair",
    );
}
