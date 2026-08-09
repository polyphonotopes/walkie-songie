//! L0 convergence cases: W1, W3–W6, W16.
//!
//! Each test drives real `SignedOp` wire bytes between real `RoomStore`s and ends
//! in `assert_converged` (all views equal + all entry-hash sets equal + all
//! `pending_len() == 0` + equality with the independent oracle).

mod support;

use std::collections::BTreeSet;

use support::{Peer, Policy, SEED_A, SimNet, op_id_of, tet_degree, tet_pitch, tuning_with_step};
use walkie_songie::room::ops::WalkieOp::*;

/// W16 — wire-bytes cross-store identity: commit on one store, ingest the bytes on
/// another, and both lift the SAME `EntryHash` (the pinned golden vector).
#[test]
fn w16_wire_bytes_cross_store_identity() {
    // Author the golden op with the exact key/inputs as the store golden vector:
    // seed A, first op, empty horizon, the pinned timestamp.
    let mut author = Peer::new(&SEED_A);
    let v0 = author.sign(
        1_700_000_000_000_000,
        vec![],
        AddDegree {
            pitch: tet_degree(0),
        },
    );
    let signed0 = v0.signed();

    let mut net = SimNet::new(1, &["A", "B"], Policy::Fifo);
    net.inject("A", &signed0);
    net.inject("B", &signed0);

    // v3 FIXTURE — the schema-3 single-lane wire's pinned entry hash, retained
    // while that wire remains deployed (the v4 lanes pin their own vectors in
    // `room::v4` and `tests/music_lane_interop.rs`).
    const GOLDEN_V3: &str = "9e217937915d7f0969a214c904ab6adb00da97c873d89407d82b7e5bf0bf3568";
    for name in ["A", "B"] {
        let hashes: Vec<String> = net
            .store(name)
            .entry_hashes()
            .iter()
            .map(|h| h.to_hex())
            .collect();
        assert_eq!(
            hashes,
            vec![GOLDEN_V3.to_string()],
            "{name} lifts the golden entry hash"
        );
    }
    net.assert_converged();
}

/// W1 — add-wins survives partition + heal. {A} | {B,C}; A and B both add (5,12),
/// A removes (observing only its own add); after heal + reconcile the key is live
/// with authors {B}.
#[test]
fn w1_add_wins_survives_partition_heal() {
    let mut net = SimNet::new(1, &["A", "B", "C"], Policy::Fifo);
    net.partition("A", "B");
    net.partition("A", "C");

    net.act(
        "A",
        AddDegree {
            pitch: tet_degree(5),
        },
    ); // dropped to B, C
    net.act(
        "A",
        RemoveDegree {
            pitch: tet_degree(5),
        },
    ); // observes only A's own add
    net.act(
        "B",
        AddDegree {
            pitch: tet_degree(5),
        },
    ); // reaches C, dropped to A
    net.step_until_quiescent(); // B <-> C exchange

    net.heal();
    net.reconcile_all();
    net.step_until_quiescent();

    let b_author = net.author("B");
    let view = net.view("A");
    assert!(
        view.pitches.contains(&tet_degree(5)),
        "add-wins keeps the key live"
    );
    assert_eq!(
        view.pitch_authors[&tet_degree(5)],
        BTreeSet::from([b_author]),
        "only B's add survives"
    );
    net.assert_converged();
}

/// W3 — register recency after reorder. A's SetTuning t1 -> t2; t2 (which chains
/// after t1) is delivered first and deferred, then t1 drains it. Final tuning = t2.
#[test]
fn w3_register_recency_after_reorder() {
    // Adversarial policy delivers the newest queued record first, so t2 lands
    // before its own predecessor t1.
    let mut net = SimNet::new(2, &["A", "B"], Policy::Adversarial);
    let first = tuning_with_step(600);
    let second = tuning_with_step(700);
    net.act("A", SetTuning { definition: first });
    net.act(
        "A",
        SetTuning {
            definition: second.clone(),
        },
    );
    net.step_until_quiescent();

    assert_eq!(
        net.view("B").tuning,
        Some(second),
        "latest register value wins"
    );
    assert_eq!(
        net.store("B").pending_len(),
        0,
        "deferred t2 drained once t1 arrived"
    );
    net.assert_converged();
}

/// W4 — concurrent tuning tiebreak across a full partition. Three writers set
/// different tunings while fully partitioned; after heal + reconcile every peer
/// resolves the SAME winner (register rule), matching the independent oracle.
#[test]
fn w4_concurrent_tuning_tiebreak_across_partition() {
    let mut net = SimNet::new(3, &["A", "B", "C"], Policy::RandomSeeded);
    net.partition("A", "B");
    net.partition("A", "C");
    net.partition("B", "C");

    net.act(
        "A",
        SetTuning {
            definition: tuning_with_step(500),
        },
    );
    net.act(
        "B",
        SetTuning {
            definition: tuning_with_step(600),
        },
    );
    net.act(
        "C",
        SetTuning {
            definition: tuning_with_step(700),
        },
    );
    net.step_until_quiescent();

    net.heal();
    net.reconcile_all();
    net.step_until_quiescent();

    assert!(
        net.view("A").tuning.is_some(),
        "some concurrent writer wins"
    );
    net.assert_converged();
}

/// W5 (shared pieces) — cross-node non-owner writes take effect, including the
/// before-put (defer) order. A creates a piece; non-owner B moves then removes
/// it; a third peer C that receives B's ops before the put defers them until the
/// put arrives, then converges. Under shared semantics B's remove (causally after
/// its own move, so it observed both put and move) wins: the piece is gone on
/// every peer. `owner` never gated anything.
#[test]
fn w5_shared_non_owner_writes_cross_node_defer_before_put() {
    let mut net = SimNet::new(4, &["A", "B", "C"], Policy::Fifo);
    net.partition("A", "C");
    net.partition("B", "C"); // isolate the late peer C

    let put = net.act(
        "A",
        PutPiece {
            emoji: "🌵".into(),
            pitch: tet_pitch(60),
        },
    );
    net.step_until_quiescent(); // B receives the put
    let piece_id = op_id_of(&put);

    let mv = net.act(
        "B",
        MovePiece {
            piece: piece_id,
            pitch: tet_pitch(61),
        },
    ); // non-owner, observes put
    let rm = net.act("B", RemovePiece { piece: piece_id }); // non-owner, observes the move
    net.step_until_quiescent(); // A receives mv, rm

    net.heal();
    // Deliver to C in the adversarial 'before-put' order: mv, rm defer, put drains.
    net.inject("C", &mv);
    net.inject("C", &rm);
    assert!(
        net.store("C").pending_len() >= 1,
        "C defers ops that observe the missing put"
    );
    net.inject("C", &put);
    assert_eq!(
        net.store("C").pending_len(),
        0,
        "the put releases the deferred ops"
    );

    net.assert_converged();
    // Shared: B's remove observed both the put and its own move -> the piece is
    // observed-removed on every peer, non-owner writes and all.
    assert!(
        net.view("A").pieces.is_empty(),
        "a non-owner move+remove takes effect across nodes"
    );
}

/// W6 — remove/unremove race across a partition. Observed-remove lifecycle: an
/// unremove that observed the remove overrides it (alive); a bare remove that
/// observed the put kills it (dead). Both terminal states are exercised.
#[test]
fn w6_remove_unremove_race_across_partition() {
    // Variant 1: ...put, remove, UNremove -> alive.
    {
        let mut net = SimNet::new(5, &["A", "B"], Policy::Adversarial);
        net.partition("A", "B");
        let put = net.act(
            "A",
            PutPiece {
                emoji: "🎵".into(),
                pitch: tet_pitch(60),
            },
        );
        let piece = op_id_of(&put);
        let rem = net.act("A", RemovePiece { piece });
        let rem_id = op_id_of(&rem);
        net.act("A", UnremovePiece { remove: rem_id });

        net.heal();
        net.reconcile("A", "B");
        net.assert_converged();
        assert_eq!(
            net.view("B").pieces.len(),
            1,
            "the unremove observed the remove -> override -> alive"
        );
    }
    // Variant 2: ...put, remove -> dead.
    {
        let mut net = SimNet::new(6, &["A", "B"], Policy::Adversarial);
        net.partition("A", "B");
        let put = net.act(
            "A",
            PutPiece {
                emoji: "🎵".into(),
                pitch: tet_pitch(60),
            },
        );
        let piece = op_id_of(&put);
        net.act("A", RemovePiece { piece });

        net.heal();
        net.reconcile("A", "B");
        net.assert_converged();
        assert!(
            net.view("B").pieces.is_empty(),
            "a bare remove that observed the put -> dead"
        );
    }
}
