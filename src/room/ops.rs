//! Walkie-songie's op alphabet ([`WalkieOp`]) and its [`OpLanguage`]
//! instantiation ([`WalkieLang`]) — the domain half of the signed-op layer.
//!
//! The generic signed envelope (versioned/verified op types, sign/verify,
//! wire framing + size ladder, `AuthorId`/`OpId`/`LogHead`) now lives in
//! [`tutti_core`] (tutti extraction Track-D step 3). This module keeps only what
//! is music: the [`WalkieOp`] alphabet, its wire validation, the schema/framing
//! consts, and the thin walkie-facing sign/verify wrappers that fix
//! `L = WalkieLang`. Everything else is **re-exported** from `tutti_core` so
//! every external call site keeps its `crate::room::ops::…` spelling unchanged.
//!
//! ## v3 alphabet
//! - **Degrees** are a content-keyed add-wins set keyed by [`TunedDegree`]. A
//!   `RemoveDegree` supersedes only the adds in its causal past; a concurrent add
//!   survives.
//! - **Pieces** are graph-shaped and SHARED: identity is the *op id* of the
//!   `PutPiece` that created them; `MovePiece`/`RemovePiece`/`UnremovePiece`
//!   reference that [`OpId`]. ANY author's ops take effect, resolved by cross-
//!   author observed-remove and a causal-maxima position register. The
//!   `PutPiece` author is attribution only; `pieces_locked` is the consent gate.
//! - **Tuning/config** are room-wide registers resolved by causal maxima.
//! - **Voice preview** is deliberately absent: it is signed, sequenced, leased
//!   presence (`room::presence`) and never enters durable history.
//!
//! **Evolution discipline:** append variants to [`WalkieOp`], never reorder them,
//! add fields only as `#[serde(default)]`, and bump [`OP_SCHEMA_VERSION`] when the
//! payload shape changes.
//!
//! **wasm timestamps:** `ts_micros` must be author-supplied. On wasm pass
//! `js_sys::Date::now() as u64 * 1000`; never call p2panda's `Timestamp::now()`.

use serde::{Deserialize, Serialize};
use tutti_core::FoldCtx;

use crate::tuning::{MAX_SCALE_DEGREES, TunedDegree, TunedPeriodicPitch, TuningDefinition};

/// The substrate envelope, verification, and identities — re-exported so the rest
/// of walkie names them through `room::ops` and never takes a direct
/// `tutti_core`/`p2panda-core` dependency. Fixed `L = WalkieLang` spellings
/// ([`VersionedOp`], [`VerifiedOp`]) and the walkie wrappers are below.
pub use tutti_core::{
    AuthorId, LogHead, MAX_OBSERVED_OPS, MAX_SIGNED_HEADER_BYTES, MAX_SIGNED_OP_WIRE_BYTES,
    MAX_SIGNED_PAYLOAD_BYTES, MAX_TOPIC_BYTES, OpId, OpLanguage, OpVerifyError, SignedOp,
    SignedOpWireError, SigningKey, VerifiedOpG, VerifyingKey, VersionedOpG, WindowIngest,
    sign_versioned_op, signing_key_from_seed, verify_signed_op_in,
};

/// The current op-payload schema version (walkie domain).
pub const OP_SCHEMA_VERSION: u16 = 3;
/// Absolute period bound for a tuned pitch — a domain well-formedness cap.
pub const MAX_ABS_PERIOD: i32 = 1_000_000;
/// Largest emoji string on a piece.
pub const MAX_EMOJI_BYTES: usize = 256;
/// Largest room-wide emoji palette.
pub const MAX_EMOJI_PALETTE_BYTES: usize = 16 * 1024;

/// The domain operation (v3). Materialization semantics live in `room::store`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WalkieOp {
    /// Add one tuning-scoped degree to the shared content-keyed set.
    AddDegree { pitch: TunedDegree },
    /// Retract this author's observed adds of one tuning-scoped degree.
    RemoveDegree { pitch: TunedDegree },
    /// Create an emoji piece. Its identity is THIS op's [`OpId`]; `MovePiece`/
    /// `RemovePiece` reference that id.
    PutPiece {
        emoji: String,
        pitch: TunedPeriodicPitch,
    },
    /// Move the piece created by `piece` to a new periodic pitch (shared: any
    /// author, resolved by the cross-author position register).
    MovePiece {
        piece: OpId,
        pitch: TunedPeriodicPitch,
    },
    /// Remove the piece created by `piece` (shared: any author; observed-remove).
    RemovePiece { piece: OpId },
    /// Undo a `RemovePiece` (a remove-of-remove); resurrects the piece.
    UnremovePiece { remove: OpId },
    /// Canonical room-wide tuning definition (register; causal-maxima resolved).
    SetTuning { definition: TuningDefinition },
    /// Room-wide configuration (register). Fields optional so one op carries one change.
    SetConfig {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pieces_locked: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        available_emojis: Option<String>,
    },
}

/// Walkie-songie's instantiation of [`OpLanguage`] — the first (and today only)
/// `L`. Every associated const is walkie's literal value, so the substrate is
/// wire-invisible: signed bytes and entry hashes are byte-for-byte unchanged.
pub struct WalkieLang;

impl OpLanguage for WalkieLang {
    type Op = WalkieOp;
    /// The room read model. Materialization is walkie-facing and lives beside it
    /// in [`crate::room::store`]; `fold` delegates there.
    type View = crate::room::store::RoomView;

    const SCHEMA_VERSION: u16 = OP_SCHEMA_VERSION;
    const ENTRY_FRAME_MAGIC: &'static [u8] = b"walkie.hhhs.signed-op/1";
    const WIRE_MAGIC: &'static [u8] = tutti_core::SIGNED_OP_WIRE_MAGIC;
    const MAX_PAYLOAD_BYTES: usize = MAX_SIGNED_PAYLOAD_BYTES;

    fn validate_wire(op: &WalkieOp) -> Result<(), String> {
        let validate_degree = |pitch: TunedDegree| {
            if usize::from(pitch.degree.index()) >= MAX_SCALE_DEGREES {
                Err(format!(
                    "degree {} exceeds the supported bound",
                    pitch.degree.index()
                ))
            } else {
                Ok(())
            }
        };
        let validate_periodic = |pitch: TunedPeriodicPitch| {
            validate_degree(pitch.degree())?;
            if pitch.pitch.period().unsigned_abs() > MAX_ABS_PERIOD as u32 {
                return Err(format!(
                    "period {} exceeds the supported bound",
                    pitch.pitch.period()
                ));
            }
            Ok(())
        };

        match op {
            WalkieOp::AddDegree { pitch } | WalkieOp::RemoveDegree { pitch } => {
                validate_degree(*pitch)
            }
            WalkieOp::PutPiece { emoji, pitch } => {
                if emoji.is_empty() || emoji.len() > MAX_EMOJI_BYTES {
                    return Err(format!(
                        "piece emoji must contain 1..={MAX_EMOJI_BYTES} UTF-8 bytes"
                    ));
                }
                validate_periodic(*pitch)
            }
            WalkieOp::MovePiece { pitch, .. } => validate_periodic(*pitch),
            WalkieOp::SetTuning { definition } => definition
                .validate("signed room tuning")
                .map(|_| ())
                .map_err(|error| error.to_string()),
            WalkieOp::SetConfig {
                available_emojis: Some(emojis),
                ..
            } if emojis.len() > MAX_EMOJI_PALETTE_BYTES => Err(format!(
                "emoji palette exceeds {MAX_EMOJI_PALETTE_BYTES} UTF-8 bytes"
            )),
            _ => Ok(()),
        }
    }

    /// Walkie's fold: the register → add-wins → object composition that
    /// materializes a [`RoomView`](crate::room::store::RoomView). Kept beside the
    /// view and its `with_*` builders in [`crate::room::store`]; this is a one-
    /// line delegation so the domain semantics stay in one place.
    fn fold(ctx: &FoldCtx<'_, Self>) -> Self::View {
        crate::room::store::walkie_fold(ctx)
    }
}

/// Walkie's signed-op envelope — [`VersionedOpG`] fixed at [`WalkieLang`]. Every
/// call site keeps the pre-extraction spelling `VersionedOp`.
pub type VersionedOp = VersionedOpG<WalkieLang>;

/// Walkie's verified op — [`VerifiedOpG`] fixed at [`WalkieLang`]. Every call site
/// keeps the pre-extraction spelling `VerifiedOp`.
pub type VerifiedOp = VerifiedOpG<WalkieLang>;

/// Sign one op bound to a room topic, stamping its observed causal horizon.
///
/// `observed` is the set of op ids this author had accepted at signing time (the
/// store frontier). It is load-bearing: the HHHS mirror lifts it into the entry's
/// predecessors so cross-author causality is expressible.
pub fn sign_op_for_topic_observing(
    signing_key: &SigningKey,
    head: &LogHead,
    ts_micros: u64,
    topic: &str,
    observed: impl IntoIterator<Item = [u8; 32]>,
    op: WalkieOp,
) -> (SignedOp, LogHead) {
    sign_versioned_op(
        signing_key,
        head,
        VersionedOp::current_for_topic(op, ts_micros, topic).observing(observed),
    )
}

/// Sign a topic-agnostic op with no observed horizon (tests / non-room-scoped uses).
pub fn sign_op(
    signing_key: &SigningKey,
    head: &LogHead,
    ts_micros: u64,
    op: WalkieOp,
) -> (SignedOp, LogHead) {
    sign_versioned_op(signing_key, head, VersionedOp::current(op, ts_micros))
}

/// Verify a signed op. Walkie's concrete entry point — the `L = WalkieLang`
/// instantiation of the generic [`verify_signed_op_in`]. The concrete return type
/// keeps every external call site's spelling and inference unchanged.
pub fn verify_signed_op(signed: &SignedOp) -> Result<VerifiedOp, OpVerifyError> {
    verify_signed_op_in::<WalkieLang>(signed)
}

/// Verify a signed op and require it to be bound to `expected_topic`.
pub fn verify_signed_op_for_topic(
    signed: &SignedOp,
    expected_topic: &str,
) -> Result<VerifiedOp, OpVerifyError> {
    let verified = verify_signed_op(signed)?;
    match verified.topic() {
        None => Err(OpVerifyError::MissingTopic),
        Some(actual) if actual != expected_topic => Err(OpVerifyError::TopicMismatch {
            expected: expected_topic.to_string(),
            actual: actual.to_string(),
        }),
        Some(_) => Ok(verified),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tuning::{TunedDegree, TunedPeriodicPitch, Tuning};
    use p2panda_core::{Header, OperationError, cbor::decode_cbor};

    const SEED_A: [u8; 32] = [7u8; 32];
    const SEED_B: [u8; 32] = [9u8; 32];
    const TS: u64 = 1_700_000_000_000_000; // µs

    fn degree(index: u16) -> TunedDegree {
        TunedDegree::new(&Tuning::twelve_tet(), index).unwrap()
    }

    fn pitch(absolute: i32) -> TunedPeriodicPitch {
        let relative = absolute - 60;
        TunedPeriodicPitch::new(
            &Tuning::twelve_tet(),
            relative.rem_euclid(12) as u16,
            relative.div_euclid(12),
        )
        .unwrap()
    }

    #[test]
    fn sign_then_verify_round_trips() {
        let key = signing_key_from_seed(&SEED_A);
        let op = WalkieOp::AddDegree { pitch: degree(4) };
        let (signed, advanced) = sign_op(&key, &LogHead::genesis(), TS, op.clone());

        let verified = verify_signed_op(&signed).expect("valid op verifies");
        assert_eq!(verified.author(), AuthorId(*key.verifying_key().as_bytes()));
        assert_eq!(verified.payload(), &op);
        assert_eq!(verified.seq_num(), 0);
        assert_eq!(verified.backlink(), None);
        assert_eq!(verified.timestamp_ms(), TS / 1_000);
        assert_eq!(verified.id(), OpId(verified.hash()));
        assert_eq!(advanced.next_seq, 1);
        assert_eq!(advanced.backlink, Some(verified.hash()));
        assert_eq!(advanced, verified.advanced_head());
    }

    #[test]
    fn second_op_chains_onto_the_first() {
        let key = signing_key_from_seed(&SEED_A);
        let (_s0, head1) = sign_op(
            &key,
            &LogHead::genesis(),
            TS,
            WalkieOp::AddDegree { pitch: degree(0) },
        );
        let (signed1, _head2) = sign_op(
            &key,
            &head1,
            TS + 1_000,
            WalkieOp::AddDegree { pitch: degree(7) },
        );
        let v1 = verify_signed_op(&signed1).expect("chained op verifies");
        assert_eq!(v1.seq_num(), 1);
        assert_eq!(v1.backlink(), head1.backlink);
    }

    #[test]
    fn observed_horizon_round_trips() {
        let key = signing_key_from_seed(&SEED_A);
        let obs = [[1u8; 32], [2u8; 32]];
        let (signed, _) = sign_op_for_topic_observing(
            &key,
            &LogHead::genesis(),
            TS,
            "sunny-garden-melody",
            obs,
            WalkieOp::RemoveDegree { pitch: degree(4) },
        );
        let v = verify_signed_op(&signed).expect("verifies");
        assert_eq!(v.observed(), &obs);
        assert_eq!(v.topic(), Some("sunny-garden-melody"));
    }

    #[test]
    fn piece_ops_reference_an_op_id() {
        let key = signing_key_from_seed(&SEED_A);
        let (put, head1) = sign_op(
            &key,
            &LogHead::genesis(),
            TS,
            WalkieOp::PutPiece {
                emoji: "🌵".into(),
                pitch: pitch(60),
            },
        );
        let put = verify_signed_op(&put).unwrap();
        let move_op = WalkieOp::MovePiece {
            piece: put.id(),
            pitch: pitch(72),
        };
        let (mv, _) = sign_op(&key, &head1, TS + 1_000, move_op.clone());
        let mv = verify_signed_op(&mv).unwrap();
        assert_eq!(mv.payload(), &move_op);
    }

    #[test]
    fn topic_scoped_verification_rejects_missing_and_wrong_topics() {
        let key = signing_key_from_seed(&SEED_A);
        let topic = "sunny-garden-melody";
        let (scoped, _) = sign_op_for_topic_observing(
            &key,
            &LogHead::genesis(),
            TS,
            topic,
            [],
            WalkieOp::AddDegree { pitch: degree(7) },
        );
        let verified = verify_signed_op_for_topic(&scoped, topic).expect("matching topic verifies");
        assert_eq!(verified.topic(), Some(topic));
        assert!(matches!(
            verify_signed_op_for_topic(&scoped, "other-room"),
            Err(OpVerifyError::TopicMismatch { .. })
        ));

        let (topicless, _) = sign_op(
            &key,
            &LogHead::genesis(),
            TS,
            WalkieOp::AddDegree { pitch: degree(7) },
        );
        assert!(matches!(
            verify_signed_op_for_topic(&topicless, topic),
            Err(OpVerifyError::MissingTopic)
        ));
    }

    #[test]
    fn tampered_payload_fails_verification() {
        let key = signing_key_from_seed(&SEED_A);
        let (mut signed, _) = sign_op(
            &key,
            &LogHead::genesis(),
            TS,
            WalkieOp::AddDegree { pitch: degree(4) },
        );
        let last = signed.payload.len() - 1;
        signed.payload[last] ^= 0xff;
        let err = verify_signed_op(&signed).unwrap_err();
        assert!(
            matches!(err, OpVerifyError::Invalid(OperationError::PayloadMismatch)),
            "expected PayloadMismatch, got {err:?}"
        );
    }

    #[test]
    fn signature_from_the_wrong_key_fails_verification() {
        let key_a = signing_key_from_seed(&SEED_A);
        let key_b = signing_key_from_seed(&SEED_B);
        let (signed, _) = sign_op(
            &key_a,
            &LogHead::genesis(),
            TS,
            WalkieOp::SetConfig {
                pieces_locked: Some(true),
                available_emojis: None,
            },
        );

        let mut header: Header<()> = decode_cbor(signed.header.as_slice()).unwrap();
        header.verifying_key = key_b.verifying_key();
        let forged = SignedOp {
            header: header.to_bytes(),
            payload: signed.payload.clone(),
        };

        let err = verify_signed_op(&forged).unwrap_err();
        assert!(
            matches!(
                err,
                OpVerifyError::Invalid(OperationError::SignatureMismatch)
            ),
            "expected SignatureMismatch, got {err:?}"
        );
    }

    #[test]
    fn distinct_authors_have_distinct_ids() {
        let a = signing_key_from_seed(&SEED_A);
        let b = signing_key_from_seed(&SEED_B);
        let ida = AuthorId(*a.verifying_key().as_bytes());
        let idb = AuthorId(*b.verifying_key().as_bytes());
        assert_ne!(ida, idb);
        assert_eq!(ida.to_hex().len(), 64);
    }

    #[test]
    fn signed_wire_frame_round_trips_and_rejects_trailing_bytes() {
        let key = signing_key_from_seed(&SEED_A);
        let (signed, _) = sign_op(
            &key,
            &LogHead::genesis(),
            TS,
            WalkieOp::AddDegree { pitch: degree(4) },
        );
        let bytes = signed.to_wire_bytes().unwrap();
        assert_eq!(SignedOp::from_wire_bytes(&bytes).unwrap(), signed);

        let mut trailing = bytes;
        trailing.push(0);
        assert_eq!(
            SignedOp::from_wire_bytes(&trailing),
            Err(SignedOpWireError::LengthMismatch)
        );
    }
}
