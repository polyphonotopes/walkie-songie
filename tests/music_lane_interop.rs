//! THE decisive wire-embedding vector (room v4): one fixed `MusicLang`
//! `SignedOp`, authored exactly as a bare tutti-music peer (an ESP32) would,
//! must verify and lift through BOTH a standalone `Store<MusicLang>` AND
//! walkie's music lane to the SAME `OpId` and the SAME `EntryHash`.
//!
//! This is the gate that proves the two-lane model: if walkie's embedding
//! changed one signed byte, one framing byte, or one predecessor, the pinned
//! hashes here would move and the vector would fail. The bare-peer half is
//! deliberately spelled ONLY in `tutti_core` + `tutti_music` — no walkie type
//! appears until the bytes cross to the room.
//!
//! The v3 fixtures (`9e2179…3568` and friends) pin the OLD single-lane wire in
//! `src/room/store.rs` and `tests/l0_convergence.rs`; the vectors here are NEW
//! v4 pins, never updates of those.

use std::collections::BTreeSet;

// The bare peer's whole vocabulary: the substrate and the music protocol.
use tutti_core::{
    LogHead, SignedOp, Store, VersionedOpG, sign_versioned_op, signing_key_from_seed,
    verify_signed_op_in,
};
use tutti_music::{MusicLang, MusicOp, TunedDegree, Tuning};

// Walkie appears only on the receiving side.
use walkie_songie::room::v4::{ExtensionOp, Room, verify_extension_op, verify_music_op};

const SEED_ESP32: [u8; 32] = [7u8; 32];
const SEED_WALKIE: [u8; 32] = [1u8; 32];
const TS: u64 = 1_700_000_000_000_000; // µs
const TOPIC: &str = "sunny-garden-melody";

/// v4 FIXTURE — the pinned identity of the vector op below. Ed25519 is
/// deterministic, so a fixed seed + fixed bytes pin these forever; they move
/// only if the MusicLang wire itself changes (which is tutti-music's schema
/// bump to make, never walkie's).
const VECTOR_OP_ID: &str = "63922bf7f6bcd80097fa065ca922da27741f4c87d0a6bc9717ec3b16187de762";
const VECTOR_ENTRY: &str = "f51c8931dd5cb8cff58fb2a17e21429802ef53ac8c7ae7fe4ca30fd3f4ba6887";

/// Author the fixed vector op the way a bare tutti-music peer does: genesis
/// head, empty causal horizon, the canonical envelope, MusicLang's schema.
fn bare_peer_vector_op() -> SignedOp {
    let key = signing_key_from_seed(&SEED_ESP32);
    let degree = TunedDegree::new(&Tuning::twelve_tet(), 4).expect("valid degree");
    let versioned =
        VersionedOpG::<MusicLang>::current_for_topic(MusicOp::AddDegree { degree }, TS, TOPIC);
    let (signed, _head) = sign_versioned_op(&key, &LogHead::genesis(), versioned);
    signed
}

/// The vector itself: bare peer -> both stores -> identical identity.
#[test]
fn music_op_lifts_identically_through_bare_store_and_walkie_lane() {
    let signed = bare_peer_vector_op();

    // The signed identity is pinned before any store sees the bytes.
    let verified = verify_signed_op_in::<MusicLang>(&signed).expect("bare music op verifies");
    assert_eq!(
        verified.id().to_hex(),
        VECTOR_OP_ID,
        "pinned MusicLang op id"
    );

    // Standalone tutti-music store — what the ESP32 itself runs.
    let mut bare = Store::<MusicLang>::new();
    bare.ingest_verified(verified.clone());
    let bare_entry = bare.lifted_entry(verified.id()).expect("bare store lifts");
    assert_eq!(
        bare_entry.to_hex(),
        VECTOR_ENTRY,
        "pinned MusicLang entry hash"
    );

    // Walkie's music lane, fed the SAME bytes through walkie's own ingress.
    let mut room = Room::new();
    let lifted = room.ingest_music(verify_music_op(&signed).expect("walkie verifies it too"));
    assert_eq!(lifted.len(), 1, "walkie lifts the op immediately");
    let lane_entry = room
        .music()
        .lifted_entry(verified.id())
        .expect("music lane lifts");

    // The make-or-break assertions: same OpId, same EntryHash, both pinned.
    assert_eq!(lane_entry, bare_entry, "one op, one identity, both stores");
    assert_eq!(lane_entry.to_hex(), VECTOR_ENTRY);

    // The op means the same thing in the composed room view.
    let degree = TunedDegree::new(&Tuning::twelve_tet(), 4).unwrap();
    assert!(room.view().pitches.contains(&degree));

    // And walkie holds the VERBATIM signed bytes — what it would relay is
    // byte-for-byte what the ESP32 signed, never a reserialization.
    assert_eq!(room.music().signed_ops()[&lane_entry], signed);
}

/// The reverse direction, plus a full cross-author session: walkie-authored
/// music ops are valid bare-peer ops (even with a busy extension lane), and an
/// ESP32 op observing them lifts identically on both sides.
#[test]
fn walkie_and_bare_peer_share_one_music_history() {
    let walkie_key = signing_key_from_seed(&SEED_WALKIE);
    let esp_key = signing_key_from_seed(&SEED_ESP32);
    let degree0 = TunedDegree::new(&Tuning::twelve_tet(), 0).unwrap();
    let degree4 = TunedDegree::new(&Tuning::twelve_tet(), 4).unwrap();

    // Walkie's room, with extension traffic that must NEVER leak into the
    // music lane's causal horizon.
    let mut room = Room::new();
    room.commit_extension(
        &walkie_key,
        TOPIC,
        TS,
        ExtensionOp::SetConfig {
            pieces_locked: Some(true),
            available_emojis: None,
        },
    );
    let w0 = room.commit_music(
        &walkie_key,
        TOPIC,
        TS + 1,
        MusicOp::AddDegree { degree: degree0 },
    );
    let w1 = room.commit_music(
        &walkie_key,
        TOPIC,
        TS + 2,
        MusicOp::AddDegree { degree: degree4 },
    );

    // The bare peer ingests walkie's music bytes through PLAIN MusicLang
    // verification — if a walkie-only predecessor were stamped into either op,
    // strict deferral would park it here forever.
    let mut bare = Store::<MusicLang>::new();
    for signed in [&w0, &w1] {
        let verified = verify_signed_op_in::<MusicLang>(signed)
            .expect("a walkie music op is a valid bare music op");
        bare.ingest_verified(verified);
    }
    assert_eq!(bare.pending_len(), 0, "nothing parks: no undecodable prevs");
    assert_eq!(
        bare.entry_hashes(),
        room.music().entry_hashes(),
        "both sides lift the identical entry set"
    );

    // The ESP32 answers with an op OBSERVING walkie's — cross-author causality
    // through the shared lane. `bare.commit` stamps the bare store's frontier.
    let esp_reply = bare.commit(
        &esp_key,
        TOPIC,
        TS + 3,
        MusicOp::RemoveDegree { degree: degree0 },
    );
    let verified_reply = verify_music_op(&esp_reply).expect("walkie verifies the ESP32 reply");
    assert!(
        !verified_reply.observed().is_empty(),
        "the reply's horizon references walkie's music ops"
    );
    room.ingest_music(verified_reply);

    assert_eq!(room.music().pending_len(), 0);
    assert_eq!(
        room.music().entry_hashes(),
        bare.entry_hashes(),
        "after the round trip the lanes are the same music history"
    );
    // And the fold agrees on meaning: degree 0 removed (the remove observed the
    // add), degree 4 live.
    assert_eq!(room.view().pitches, BTreeSet::from([degree4]));
    assert_eq!(bare.view().live, BTreeSet::from([degree4]));
}

/// The music lane enforces MusicLang's OWN 64 KiB cap, not walkie's larger
/// allowance — an oversized payload fails verification and the wire frame.
#[test]
fn music_lane_enforces_the_64k_cap() {
    use tutti_core::{OpLanguage, OpVerifyError, SignedOpWireError};

    let cap = MusicLang::MAX_PAYLOAD_BYTES;
    assert_eq!(cap, 64 * 1024, "the music cap is 64 KiB");

    let oversized = SignedOp {
        header: vec![0u8; 32],
        payload: vec![0u8; cap + 1],
    };
    assert!(
        matches!(
            verify_music_op(&oversized),
            Err(OpVerifyError::PayloadTooLarge { actual, max })
                if actual == cap + 1 && max == cap
        ),
        "verification rejects an over-cap payload before any decode"
    );
    assert!(
        matches!(
            oversized.to_wire_bytes_in::<MusicLang>(),
            Err(SignedOpWireError::PayloadTooLarge { .. })
        ),
        "the music wire frame refuses to carry it at all"
    );
}

/// A v3 single-lane walkie op can never enter the music lane: same schema
/// number by coincidence, but the payload does not decode as a `MusicOp`.
#[test]
fn v3_walkie_op_cannot_enter_the_music_lane() {
    use tutti_core::OpVerifyError;
    use walkie_songie::room::ops::{WalkieOp, sign_op};
    use walkie_songie::tuning::TunedDegree as WalkieDegree;

    let key = signing_key_from_seed(&SEED_WALKIE);
    let (v3_signed, _) = sign_op(
        &key,
        &LogHead::genesis(),
        TS,
        WalkieOp::AddDegree {
            pitch: WalkieDegree::new(&Tuning::twelve_tet(), 4).unwrap(),
        },
    );
    assert!(
        matches!(
            verify_music_op(&v3_signed),
            Err(OpVerifyError::PayloadDecode(_))
        ),
        "a v3 walkie payload is not a MusicOp"
    );
    // The music lane's OWN ops verify as extension ops even less: framing and
    // schema both differ (see room::v4's unit tests for the schema gate).
    assert!(verify_extension_op(&v3_signed).is_err());
}

/// The lifted entry identity is order-independent across peers: the ESP32 op
/// and a walkie op ingested in OPPOSITE orders on two rooms produce the same
/// entry set — the lane inherits the store's convergence whole.
#[test]
fn music_lane_identity_is_order_independent() {
    let esp_signed = bare_peer_vector_op();
    let walkie_key = signing_key_from_seed(&SEED_WALKIE);

    // Author walkie's op against an EMPTY frontier (a fresh room) so it is
    // concurrent with the ESP32 op, then feed both to two rooms in opposite
    // orders.
    let mut authoring = Room::new();
    let w_signed = authoring.commit_music(
        &walkie_key,
        TOPIC,
        TS + 1,
        MusicOp::AddDegree {
            degree: TunedDegree::new(&Tuning::twelve_tet(), 0).unwrap(),
        },
    );

    let mut forward = Room::new();
    forward.ingest_music(verify_music_op(&esp_signed).unwrap());
    forward.ingest_music(verify_music_op(&w_signed).unwrap());

    let mut reverse = Room::new();
    reverse.ingest_music(verify_music_op(&w_signed).unwrap());
    reverse.ingest_music(verify_music_op(&esp_signed).unwrap());

    assert_eq!(
        forward.music().entry_hashes(),
        reverse.music().entry_hashes()
    );
    assert_eq!(forward.view(), reverse.view());

    let vector_id = verify_music_op(&esp_signed).unwrap().id();
    assert_eq!(vector_id.to_hex(), VECTOR_OP_ID);
    assert_eq!(
        forward.music().lifted_entry(vector_id).unwrap().to_hex(),
        VECTOR_ENTRY,
        "the pinned vector holds inside a busier room too"
    );
}
