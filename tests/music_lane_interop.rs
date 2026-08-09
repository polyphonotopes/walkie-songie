//! THE decisive wire-embedding vector (room v4): one fixed `MusicLang`
//! `SignedOp`, authored exactly as a bare tutti-music peer (an ESP32) would,
//! must verify and lift through BOTH a standalone `Store<MusicLang>` AND
//! walkie's music lane to the SAME `OpId` and the SAME `EntryHash`.
//!
//! This is the gate that proves the two-lane model: if walkie's embedding
//! changed one signed byte, one framing byte, or one predecessor, the pinned
//! hashes here would move and the vector would fail. The bare-peer half is
//! deliberately spelled ONLY in `tutti_core` + `tutti_music` — no walkie type
//! appears until the bytes cross to the room. Even the room topic is derived
//! INDEPENDENTLY here (`blake3::derive_key` over the pinned context), never by
//! calling walkie's derivation.
//!
//! **The signed-topic contract is production's, not a test convenience.**
//! Production never signs the human room name: it signs the DERIVED topic's
//! lowercase-hex string — `RoomTopic::from_room_name(name).to_string()` in
//! `src/net/iroh_common.rs`, i.e. `hex(blake3::derive_key("walkie-songie room
//! topic v1", name))` — and enforces it at ingress
//! (`verify_signed_op_for_topic` on v3, `verify_music_op`'s expected-topic
//! argument on the v4 lanes). An implementation that signed the human name
//! would pass a weaker vector and then fail every production peer's topic
//! gate; [`SIGNED_TOPIC`] pins the exact string so that mistake is caught
//! here.
//!
//! Beyond the hashes, the vector pins the exact WIRE BYTES: the CBOR payload,
//! the signed header, and the full `to_wire_bytes_in::<MusicLang>` frame.
//! The frame pin is load-bearing on its own — lifting hashes over
//! `ENTRY_FRAME_MAGIC` framing, so a `MusicLang::WIRE_MAGIC` change would
//! move NEITHER pinned hash; only the frame constant catches it.
//!
//! These fixtures are self-generated-then-frozen: they catch any future drift
//! in walkie or tutti, but they are not yet an independently produced
//! artifact. A truly independent fixture — bytes captured from a real
//! second-implementation peer (the ESP32 firmware) — is a future step.
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

/// The human room name. Never signed — only the derivation input.
const ROOM_NAME: &str = "sunny-garden-melody";
/// The production topic-derivation context (`ROOM_TOPIC_CONTEXT` in
/// `src/net/iroh_common.rs`). Byte-pinned: changing it severs every deployed
/// room.
const ROOM_TOPIC_CONTEXT: &str = "walkie-songie room topic v1";

/// v4 FIXTURE — the exact string every op in this room signs and every
/// conforming peer enforces at ingress: the derived topic's lowercase hex.
const SIGNED_TOPIC: &str = "072aaa8bdb9bea93fe8b3af1a3214533027e9973fb007440b55606e2fe452a7a";

/// v4 FIXTURES — the pinned identity of the vector op below. Ed25519 is
/// deterministic, so a fixed seed + fixed bytes pin these forever; they move
/// only if the MusicLang wire itself changes (which is tutti-music's schema
/// bump to make, never walkie's) or the signed-topic contract changes.
const VECTOR_OP_ID: &str = "fc5ae35d9a75ecc81c6244250b1225d73711a7a4fb640ec29f633e8b488460c6";
const VECTOR_ENTRY: &str = "59fd1ad628d0f279b579a4e7c06db23f9d950cd9918e9f77bb4494be2c4871ac";

/// v4 FIXTURES — the vector op's exact bytes, hex-encoded: the CBOR payload
/// (the signed `VersionedOpG` body), the signed p2panda header, and the full
/// `MusicLang` wire frame (`WIRE_MAGIC` + length-delimited header/payload).
const VECTOR_PAYLOAD: &str = "a46776657273696f6e036974735f6d6963726f731b00060a24181e400065746f706963784030373261616138626462396265613933666538623361663161333231343533333032376539393733666230303734343062353536303665326665343532613761626f70a169416464446567726565a166646567726565a26974756e696e675f696498201823185b18a2183c1846188b182d18fb187518fe189818f3021839060118490b1862184718ea18d518221832188a18b2186818a1183d181918b9188b6664656772656504";
const VECTOR_HEADER: &str = "86015820ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c5840b0e4c740ce9ddb60a38740c999dbeb5a32ca2e38612937514ab5f67f38fa21e80cad5a2937276b224d8b9e20f0828a8b9983506f576e2988bb2dcffe60aa1d0118cc5820b2e5eed0a07e93db73f9761f93ff93f0270c021a09826305daf2c514fae282f600";
const VECTOR_WIRE_FRAME: &str = "74757474692e6d757369632e776972652f32008b000000cc00000086015820ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c5840b0e4c740ce9ddb60a38740c999dbeb5a32ca2e38612937514ab5f67f38fa21e80cad5a2937276b224d8b9e20f0828a8b9983506f576e2988bb2dcffe60aa1d0118cc5820b2e5eed0a07e93db73f9761f93ff93f0270c021a09826305daf2c514fae282f600a46776657273696f6e036974735f6d6963726f731b00060a24181e400065746f706963784030373261616138626462396265613933666538623361663161333231343533333032376539393733666230303734343062353536303665326665343532613761626f70a169416464446567726565a166646567726565a26974756e696e675f696498201823185b18a2183c1846188b182d18fb187518fe189818f3021839060118490b1862184718ea18d518221832188a18b2186818a1183d181918b9188b6664656772656504";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The production topic derivation, recomputed independently of walkie: an
/// ESP32 that knows only the context string and the room name derives the
/// identical signed topic.
fn production_topic() -> String {
    hex(&blake3::derive_key(
        ROOM_TOPIC_CONTEXT,
        ROOM_NAME.as_bytes(),
    ))
}

/// Author the fixed vector op the way a bare tutti-music peer does: genesis
/// head, empty causal horizon, the canonical envelope, MusicLang's schema,
/// the PRODUCTION signed topic.
fn bare_peer_vector_op() -> SignedOp {
    let key = signing_key_from_seed(&SEED_ESP32);
    let degree = TunedDegree::new(&Tuning::twelve_tet(), 4).expect("valid degree");
    let versioned = VersionedOpG::<MusicLang>::current_for_topic(
        MusicOp::AddDegree { degree },
        TS,
        SIGNED_TOPIC,
    );
    let (signed, _head) = sign_versioned_op(&key, &LogHead::genesis(), versioned);
    signed
}

/// The byte-level pins: signed topic, payload, header, wire frame, and the
/// genesis predecessor set. Everything an independent implementation must
/// reproduce EXACTLY, checked byte-for-byte.
#[test]
fn vector_wire_bytes_and_signed_topic_are_pinned() {
    // The derivation itself is part of the contract.
    assert_eq!(
        production_topic(),
        SIGNED_TOPIC,
        "the signed topic is the derived topic's lowercase hex"
    );

    let signed = bare_peer_vector_op();
    assert_eq!(hex(&signed.payload), VECTOR_PAYLOAD, "pinned CBOR payload");
    assert_eq!(hex(&signed.header), VECTOR_HEADER, "pinned signed header");

    // The DOMAIN frame — what actually crosses a v4 music-lane wire. Pinned
    // separately from the hashes because lifting frames with
    // `ENTRY_FRAME_MAGIC`, so a `WIRE_MAGIC` change moves only this constant.
    let frame = signed
        .to_wire_bytes_in::<MusicLang>()
        .expect("vector op frames");
    assert_eq!(
        hex(&frame),
        VECTOR_WIRE_FRAME,
        "pinned MusicLang wire frame"
    );
    assert_eq!(
        SignedOp::from_wire_bytes_in::<MusicLang>(&frame).expect("frame deframes"),
        signed,
        "the frame round-trips to the identical signed bytes"
    );

    // Walkie's ingress enforces the production topic...
    let verified = verify_music_op(&signed, SIGNED_TOPIC).expect("vector verifies");
    assert_eq!(verified.topic(), Some(SIGNED_TOPIC));
    // ...and refuses the human room name — the trap a weaker vector would
    // have blessed: an implementation signing the name verifies fine WITHOUT
    // a topic gate and then fails every production peer.
    assert!(
        matches!(
            verify_music_op(&signed, ROOM_NAME),
            Err(tutti_core::OpVerifyError::TopicMismatch { .. })
        ),
        "the human room name is not the signed topic"
    );

    // The pinned predecessor set: a genesis op — no backlink, empty horizon.
    assert!(verified.observed().is_empty(), "empty causal horizon");
    assert_eq!(verified.backlink(), None, "no backlink");
    assert_eq!(verified.seq_num(), 0, "first op of its log");
}

/// Walkie's own derivation must agree with the independent recomputation the
/// bare peer runs (compiled only when the net layer is present).
#[cfg(feature = "native-net")]
#[test]
fn walkie_room_topic_matches_the_pinned_derivation() {
    use walkie_songie::net::iroh_common::RoomTopic;
    let topic = RoomTopic::from_room_name(ROOM_NAME);
    assert_eq!(topic.to_hex(), SIGNED_TOPIC, "RoomTopic::to_hex");
    assert_eq!(topic.to_string(), SIGNED_TOPIC, "the exact signed string");
}

/// The vector itself: bare peer -> both stores -> identical identity.
#[test]
fn music_op_lifts_identically_through_bare_store_and_walkie_lane() {
    let signed = bare_peer_vector_op();

    // The signed identity is pinned before any store sees the bytes. (The
    // language check alone fixes the identity; a conforming peer ALSO gates
    // on the room topic, as walkie does below.)
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

    // Walkie's music lane, fed the SAME bytes through walkie's own ingress —
    // which enforces the production topic the op signed.
    let mut room = Room::new();
    let lifted =
        room.ingest_music(verify_music_op(&signed, SIGNED_TOPIC).expect("walkie verifies it too"));
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
    let topic = production_topic();

    // Walkie's room, with extension traffic that must NEVER leak into the
    // music lane's causal horizon.
    let mut room = Room::new();
    room.commit_extension(
        &walkie_key,
        &topic,
        TS,
        ExtensionOp::SetConfig {
            pieces_locked: Some(true),
            available_emojis: None,
        },
    );
    let w0 = room.commit_music(
        &walkie_key,
        &topic,
        TS + 1,
        MusicOp::AddDegree { degree: degree0 },
    );
    let w1 = room.commit_music(
        &walkie_key,
        &topic,
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
        assert_eq!(
            verified.topic(),
            Some(topic.as_str()),
            "walkie signs the derived topic, so the bare peer's own topic \
             gate would admit it"
        );
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
        &topic,
        TS + 3,
        MusicOp::RemoveDegree { degree: degree0 },
    );
    let verified_reply =
        verify_music_op(&esp_reply, &topic).expect("walkie verifies the ESP32 reply");
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
            verify_music_op(&oversized, SIGNED_TOPIC),
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
            verify_music_op(&v3_signed, SIGNED_TOPIC),
            Err(OpVerifyError::PayloadDecode(_))
        ),
        "a v3 walkie payload is not a MusicOp"
    );
    // The music lane's OWN ops verify as extension ops even less: framing and
    // schema both differ (see room::v4's unit tests for the schema gate).
    assert!(verify_extension_op(&v3_signed, SIGNED_TOPIC).is_err());
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
        SIGNED_TOPIC,
        TS + 1,
        MusicOp::AddDegree {
            degree: TunedDegree::new(&Tuning::twelve_tet(), 0).unwrap(),
        },
    );

    let mut forward = Room::new();
    forward.ingest_music(verify_music_op(&esp_signed, SIGNED_TOPIC).unwrap());
    forward.ingest_music(verify_music_op(&w_signed, SIGNED_TOPIC).unwrap());

    let mut reverse = Room::new();
    reverse.ingest_music(verify_music_op(&w_signed, SIGNED_TOPIC).unwrap());
    reverse.ingest_music(verify_music_op(&esp_signed, SIGNED_TOPIC).unwrap());

    assert_eq!(
        forward.music().entry_hashes(),
        reverse.music().entry_hashes()
    );
    assert_eq!(forward.view(), reverse.view());

    let vector_id = verify_music_op(&esp_signed, SIGNED_TOPIC).unwrap().id();
    assert_eq!(vector_id.to_hex(), VECTOR_OP_ID);
    assert_eq!(
        forward.music().lifted_entry(vector_id).unwrap().to_hex(),
        VECTOR_ENTRY,
        "the pinned vector holds inside a busier room too"
    );
}
