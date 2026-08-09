//! Room v4 — the two-lane room a bare tutti-music peer can join.
//!
//! The v3 room is one signed op log speaking [`WalkieOp`](crate::room::ops::WalkieOp):
//! degrees, pieces, tuning, and config in a single causal DAG. That shape can
//! never admit a peer that only speaks the music protocol — wrapping `MusicOp`
//! in a walkie variant makes a *walkie* op (different entry/wire magics, an
//! extra CBOR discriminator, different signed bytes, so a different
//! `OpId`/`EntryHash`), and in one combined DAG a music op would stamp walkie-
//! only ops into its causal horizon, which a music-only peer can neither decode
//! nor lift past (strict deferral parks it forever).
//!
//! v4 therefore splits the room into two causal lanes with SEPARATE frontiers:
//!
//! * **Music lane** — literally a [`Store`]`<`[`MusicLang`]`>`, the canonical
//!   tutti-music spelling: MusicLang bytes, framing, schema version, and its
//!   64 KiB payload cap. A bare tutti-music peer (an ESP32) joins ONLY this
//!   lane and is a first-class peer in it. Nothing about a music op's signed
//!   header, payload, `OpId`, or lifted `EntryHash` depends on walkie —
//!   `tests/music_lane_interop.rs` pins that as a golden vector.
//! * **Extension lane** — walkie's own [`ExtensionOp`] log: emoji pieces and
//!   room config. Only walkie peers speak it.
//!
//! The separation is structural, not procedural: each lane is its own [`Store`]
//! with its own DAG, so a committed music op can only ever observe music ops —
//! there is no combined frontier to leak an undecodable predecessor into a
//! music-only peer's causal past.
//!
//! **Scope of that invariant, stated honestly:** it binds every op AUTHORED
//! through a lane's store — [`Room::commit_music`]/[`Room::commit_extension`]
//! stamp only their own lane's frontier, and walkie exposes no other v4
//! authoring path, so walkie can never accidentally construct a cross-lane
//! reference (the per-lane stores ARE the lane-typed authoring heads). It is
//! NOT an ingress invariant over arbitrary valid signers: causal refs are
//! untyped 32-byte hashes, so a Byzantine (or merely broken) author can sign a
//! music op `.observing` an extension entry hash — indistinguishable at
//! verification time from an op citing a music entry we have not yet received.
//! Such an op fails closed: strict deferral parks it forever (the dangling ref
//! never resolves inside the music lane), it never lifts, and it never touches
//! the fold — pinned by `cross_lane_ref_parks_forever_and_fails_closed` below.
//! Dangling refs from arbitrary signers are unavoidable in any content-hash
//! DAG; parking is the designed containment.
//!
//! [`Room::view`] composes both lanes into the familiar [`RoomView`]. Because
//! the extension lane cannot causally observe the music lane's tuning register,
//! tuning-scoping of pieces is a composition-time projection: each piece's
//! position register is resolved per `(piece, TuningId)` — an other-tuning
//! move can never displace the put-tuning winner (see
//! [`fold_pieces`](crate::room::store)) — and a piece whose put asserts
//! another tuning is hidden (never reinterpreted), resurrecting when the room
//! switches back. This keeps the v3 piece semantics observably unchanged. The
//! music lane's per-degree envelopes are folded in [`MusicView`] but
//! deliberately NOT part of [`RoomView`] — adding them to committed state is
//! an explicit `state_root` schema change, deferred.
//!
//! **Received bytes are sacred:** both lanes store and re-emit the exact bytes
//! an author signed ([`Store`] lifts verbatim frames); nothing here re-signs,
//! re-wraps, or reserializes a verified op.
//!
//! **Deferred to the net-layer generation pass** (see
//! `docs/vision/wire-embedding-design.md` §"Migration"): the v4 repair ALPN,
//! versioned gossip topics, the room-ticket format, journal magic, and wiring
//! the app/sync layer from the v3 single-lane store onto this room. Until that
//! lands the deployed wire is still v3; this module is the correctness core it
//! migrates onto.

use std::collections::BTreeMap;

use hhhs::EntryHash;
use serde::{Deserialize, Serialize};
use tutti_core::{
    FoldCtx, OpLanguage, OpVerifyError, SignedOp, SigningKey, Store, VerifiedOpG,
    verify_signed_op_in,
};
use tutti_music::fold::register;

/// The music lane's language, alphabet, and read model — re-exported so walkie
/// names the music protocol through `room::v4` and every spelling stays in one
/// place. These are tutti-music's own types: using them IS the interop.
pub use tutti_music::{MusicLang, MusicOp, MusicView};

use crate::room::ops::{
    MAX_ABS_PERIOD, MAX_EMOJI_BYTES, MAX_EMOJI_PALETTE_BYTES, MAX_SIGNED_PAYLOAD_BYTES, OpId,
};
use crate::room::store::{Piece, PieceEvent, RoomView, fold_pieces};
use crate::tuning::{MAX_SCALE_DEGREES, TunedPeriodicPitch};

/// The walkie-only extension operation (v4): emoji pieces and room config.
/// Everything musical — degrees, envelopes, tuning — travels the music lane as
/// [`MusicOp`] and never appears here.
///
/// Semantics are the v3 piece/config semantics unchanged (shared pieces,
/// observed-remove lifecycle, causal-maxima registers); only the lane is new.
/// Evolution discipline: append variants, never reorder, add fields only as
/// `#[serde(default)]`, bump [`ExtensionLang::SCHEMA_VERSION`] on a payload-
/// shape change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExtensionOp {
    /// Create an emoji piece. Its identity is THIS op's [`OpId`]; the other
    /// piece ops reference that id.
    PutPiece {
        emoji: String,
        pitch: TunedPeriodicPitch,
    },
    /// Move the piece created by `piece` (shared: any author, resolved by the
    /// cross-author position register).
    MovePiece {
        piece: OpId,
        pitch: TunedPeriodicPitch,
    },
    /// Remove the piece created by `piece` (shared: any author; observed-remove).
    RemovePiece { piece: OpId },
    /// Undo a `RemovePiece` (a remove-of-remove); resurrects the piece.
    UnremovePiece { remove: OpId },
    /// Room-wide configuration (register). Fields optional so one op carries one
    /// change.
    SetConfig {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pieces_locked: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        available_emojis: Option<String>,
    },
}

/// The extension lane's read model: pieces plus the config registers.
///
/// `pieces` is deliberately UNSCOPED by the ACTIVE tuning — this lane cannot
/// causally observe the music lane's tuning register, so active-tuning scoping
/// happens once, at composition ([`Room::view`]). Each piece's position
/// register IS scoped to the tuning its put asserts (the pitch carries its
/// [`TuningId`](crate::tuning::TuningId); see `fold_pieces`), so a resolved
/// position always shares its put's tuning. Nothing outside this module
/// consumes the raw extension view; the room hands out [`RoomView`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExtensionView {
    /// Live pieces keyed by their `PutPiece` op id, across all tunings.
    pub pieces: BTreeMap<OpId, Piece>,
    /// Whether pieces are locked (default false).
    pub pieces_locked: bool,
    /// Room-wide available-emoji palette, if set.
    pub available_emojis: Option<String>,
}

/// The extension lane's [`OpLanguage`]: fresh v4 wire identity, walkie's larger
/// payload allowance (the music lane keeps [`MusicLang`]'s 64 KiB cap), and a
/// fold that is the shared pieces semantics plus the config registers.
pub struct ExtensionLang;

impl OpLanguage for ExtensionLang {
    type Op = ExtensionOp;
    type View = ExtensionView;

    /// v4 — the first extension-lane generation. Deliberately NOT 3: a v3
    /// single-lane op must fail this lane's verification loudly, never lift.
    const SCHEMA_VERSION: u16 = 4;
    const ENTRY_FRAME_MAGIC: &'static [u8] = b"walkie.ext.entry/4";
    const WIRE_MAGIC: &'static [u8] = b"walkie.ext.wire/4\0";
    const MAX_PAYLOAD_BYTES: usize = MAX_SIGNED_PAYLOAD_BYTES;

    fn validate_wire(op: &ExtensionOp) -> Result<(), String> {
        let validate_pitch = |pitch: TunedPeriodicPitch| {
            if usize::from(pitch.degree().degree.index()) >= MAX_SCALE_DEGREES {
                return Err(format!(
                    "degree {} exceeds the supported bound",
                    pitch.degree().degree.index()
                ));
            }
            if pitch.pitch.period().unsigned_abs() > MAX_ABS_PERIOD as u32 {
                return Err(format!(
                    "period {} exceeds the supported bound",
                    pitch.pitch.period()
                ));
            }
            Ok(())
        };

        match op {
            ExtensionOp::PutPiece { emoji, pitch } => {
                if emoji.is_empty() || emoji.len() > MAX_EMOJI_BYTES {
                    return Err(format!(
                        "piece emoji must contain 1..={MAX_EMOJI_BYTES} UTF-8 bytes"
                    ));
                }
                validate_pitch(*pitch)
            }
            ExtensionOp::MovePiece { pitch, .. } => validate_pitch(*pitch),
            ExtensionOp::SetConfig {
                available_emojis: Some(emojis),
                ..
            } if emojis.len() > MAX_EMOJI_PALETTE_BYTES => Err(format!(
                "emoji palette exceeds {MAX_EMOJI_PALETTE_BYTES} UTF-8 bytes"
            )),
            _ => Ok(()),
        }
    }

    fn fold(ctx: &FoldCtx<'_, Self>) -> ExtensionView {
        ExtensionView {
            pieces: fold_pieces(ctx, |decoded| match decoded.op() {
                ExtensionOp::PutPiece { emoji, pitch } => Some(PieceEvent::Put {
                    emoji: emoji.clone(),
                    pitch: *pitch,
                }),
                ExtensionOp::MovePiece { piece, pitch } => Some(PieceEvent::Move {
                    piece: *piece,
                    pitch: *pitch,
                }),
                ExtensionOp::RemovePiece { piece } => Some(PieceEvent::Remove { piece: *piece }),
                ExtensionOp::UnremovePiece { remove } => {
                    Some(PieceEvent::Unremove { remove: *remove })
                }
                ExtensionOp::SetConfig {
                    pieces_locked: Some(locked),
                    ..
                } => Some(PieceEvent::Lock { locked: *locked }),
                _ => None,
            }),
            pieces_locked: register(ctx, |decoded| match decoded.op() {
                ExtensionOp::SetConfig {
                    pieces_locked: Some(locked),
                    ..
                } => Some(*locked),
                _ => None,
            })
            .unwrap_or(false),
            available_emojis: register(ctx, |decoded| match decoded.op() {
                ExtensionOp::SetConfig {
                    available_emojis: Some(emojis),
                    ..
                } => Some(emojis.clone()),
                _ => None,
            }),
        }
    }
}

/// The music lane's store — exactly what a bare tutti-music peer runs.
pub type MusicStore = Store<MusicLang>;
/// The extension lane's store.
pub type ExtensionStore = Store<ExtensionLang>;
/// A verified music-lane op.
pub type VerifiedMusicOp = VerifiedOpG<MusicLang>;
/// A verified extension-lane op.
pub type VerifiedExtensionOp = VerifiedOpG<ExtensionLang>;

/// Require the verified op to be bound to `expected_topic` — the same room-topic
/// gate v3 ingress applies (`verify_signed_op_for_topic`). Runs AFTER language
/// verification so schema/decode failures surface first.
fn require_topic<L: OpLanguage>(
    verified: VerifiedOpG<L>,
    expected_topic: &str,
) -> Result<VerifiedOpG<L>, OpVerifyError> {
    match verified.topic() {
        None => Err(OpVerifyError::MissingTopic),
        Some(actual) if actual != expected_topic => Err(OpVerifyError::TopicMismatch {
            expected: expected_topic.to_string(),
            actual: actual.to_string(),
        }),
        Some(_) => Ok(verified),
    }
}

/// Verify a music-lane [`SignedOp`] — [`MusicLang`]'s verification, byte-for-byte
/// what a bare tutti-music peer runs (schema gate, 64 KiB cap, music wire rules) —
/// and require the op to be bound to `expected_topic`.
///
/// The topic is part of the room contract, not a walkie extra: production signs
/// the DERIVED topic's hex string (`RoomTopic::from_room_name(name).to_string()`
/// in `net::iroh_common` — `blake3::derive_key("walkie-songie room topic v1",
/// name)`), never the human room name, and every conforming peer — walkie or a
/// bare ESP32 — enforces that exact string at ingress just as v3's
/// `verify_signed_op_for_topic` does. `tests/music_lane_interop.rs` pins the
/// derivation and the signed value.
pub fn verify_music_op(
    signed: &SignedOp,
    expected_topic: &str,
) -> Result<VerifiedMusicOp, OpVerifyError> {
    require_topic(verify_signed_op_in::<MusicLang>(signed)?, expected_topic)
}

/// Verify an extension-lane [`SignedOp`] against [`ExtensionLang`] and require it
/// to be bound to `expected_topic` (the same derived-topic contract as
/// [`verify_music_op`]; lane-separate topics are net-layer generation work).
pub fn verify_extension_op(
    signed: &SignedOp,
    expected_topic: &str,
) -> Result<VerifiedExtensionOp, OpVerifyError> {
    require_topic(
        verify_signed_op_in::<ExtensionLang>(signed)?,
        expected_topic,
    )
}

/// A v4 room: the music lane and the extension lane, composed on read.
///
/// Mutation goes through per-lane `commit_*`/`ingest_*` so an op can only enter
/// the lane whose language verified it; reads compose both lanes into a
/// [`RoomView`]. The lane accessors are read-only — handing out `&mut` would
/// invite cross-lane confusion the types are here to prevent.
#[derive(Default)]
pub struct Room {
    music: MusicStore,
    extension: ExtensionStore,
}

impl Room {
    pub fn new() -> Self {
        Self::default()
    }

    // --- music lane -------------------------------------------------------

    /// Author, sign, and ingest a local music op, returning the signed bytes for
    /// gossip. Its causal horizon is the MUSIC lane's frontier alone, so the op
    /// is indistinguishable from one a bare tutti-music peer authored — any
    /// music-only peer can verify and lift it.
    pub fn commit_music(
        &mut self,
        key: &SigningKey,
        topic: &str,
        ts_micros: u64,
        op: MusicOp,
    ) -> SignedOp {
        self.music.commit(key, topic, ts_micros, op)
    }

    /// Ingest a verified music-lane op (e.g. from an ESP32). The store keeps the
    /// verbatim signed bytes; nothing is reserialized.
    pub fn ingest_music(&mut self, op: VerifiedMusicOp) -> Vec<EntryHash> {
        self.music.ingest_verified(op)
    }

    /// The music lane, read-only — entry hashes, signed bytes, and roots exactly
    /// as a standalone `Store<MusicLang>` reports them.
    pub fn music(&self) -> &MusicStore {
        &self.music
    }

    // --- extension lane ---------------------------------------------------

    /// Author, sign, and ingest a local extension op (pieces/config), returning
    /// the signed bytes for gossip to other walkie peers.
    pub fn commit_extension(
        &mut self,
        key: &SigningKey,
        topic: &str,
        ts_micros: u64,
        op: ExtensionOp,
    ) -> SignedOp {
        self.extension.commit(key, topic, ts_micros, op)
    }

    /// Ingest a verified extension-lane op from another walkie peer.
    pub fn ingest_extension(&mut self, op: VerifiedExtensionOp) -> Vec<EntryHash> {
        self.extension.ingest_verified(op)
    }

    /// The extension lane, read-only.
    pub fn extension(&self) -> &ExtensionStore {
        &self.extension
    }

    // --- composed read model ---------------------------------------------

    /// The composed room read model: the music lane's degrees and tuning plus
    /// the extension lane's pieces and config, scoped to one tuning.
    ///
    /// Pieces are filtered here — not in the extension fold — because tuning is
    /// music-lane state the extension lane cannot causally observe. A piece's
    /// resolved position always belongs to its put's tuning (the fold's
    /// per-`(piece, TuningId)` register scoping guarantees it), so the filter
    /// reduces to "is this piece's tuning the active one": an other-tuning
    /// piece is hidden as-is (its ops are preserved, never reinterpreted
    /// through a bare degree index) and reappears when the room switches back.
    pub fn view(&self) -> RoomView {
        let music = self.music.view();
        let extension = self.extension.view();

        // A register winner was wire-validated at ingress, so this is purely
        // defensive (mirroring MusicLang::fold): an unusable tuning shows no
        // pieces rather than pieces with unresolvable positions.
        let pieces = match music.tuning.validate("active room tuning") {
            Ok(active) => extension
                .pieces
                .into_iter()
                .filter(|(_, piece)| piece.pitch.validate(&active).is_ok())
                .collect(),
            Err(_) => BTreeMap::new(),
        };

        RoomView {
            pitches: music.live,
            pitch_authors: music.holders,
            pieces,
            tuning: Some(music.tuning),
            pieces_locked: extension.pieces_locked,
            available_emojis: extension.available_emojis,
        }
    }

    /// The canonical commitment to the composed [`RoomView`] (feature `merkle`).
    /// Same leaf grammar as v3's — the view schema did not change, so this moves
    /// only when the canonical projected state does.
    #[cfg(feature = "merkle")]
    pub fn state_root(&self) -> [u8; 32] {
        self.view().state_root()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::room::ops::signing_key_from_seed;
    use crate::room::test_support::{
        SEED_A, SEED_B, TOPIC, tet_degree, tet_pitch, tuning_with_step,
    };
    use crate::tuning::{TunedDegree, TuningDefinition};

    const TS: u64 = 1_700_000_000_000_000; // µs

    #[test]
    fn extension_lane_golden_entry_hash() {
        // v4 FIXTURE — pins the extension lane's signed identity (schema 4, the
        // walkie.ext framing) for a fixed first op. Moves iff the extension
        // wire's signed bytes or entry framing change.
        let key = signing_key_from_seed(&SEED_A);
        let mut room = Room::new();
        let signed = room.commit_extension(
            &key,
            TOPIC,
            TS,
            ExtensionOp::PutPiece {
                emoji: "🌵".into(),
                pitch: tet_pitch(60),
            },
        );
        let verified = verify_extension_op(&signed, TOPIC).expect("a just-committed op verifies");
        assert_eq!(
            verified.id().to_hex(),
            "7f9c48b66e443436fbfd91cc0ca83f7000e25a86d31e6c228a30adb58bbdf001",
            "v4 extension op id",
        );
        assert_eq!(
            room.extension()
                .lifted_entry(verified.id())
                .expect("committed op is lifted")
                .to_hex(),
            "3f7a6691b640c3556b7dfe08cfee88c164c8a58e56c109f5d2b80d559da0dc09",
            "v4 extension entry hash",
        );
    }

    #[test]
    fn lanes_compose_into_one_room_view() {
        let key_a = signing_key_from_seed(&SEED_A);
        let key_b = signing_key_from_seed(&SEED_B);
        let mut room = Room::new();

        room.commit_music(
            &key_a,
            TOPIC,
            TS,
            MusicOp::AddDegree {
                degree: tet_degree(0),
            },
        );
        room.commit_music(
            &key_b,
            TOPIC,
            TS + 1,
            MusicOp::AddDegree {
                degree: tet_degree(4),
            },
        );
        let put = room.commit_extension(
            &key_b,
            TOPIC,
            TS + 2,
            ExtensionOp::PutPiece {
                emoji: "🌵".into(),
                pitch: tet_pitch(60),
            },
        );
        let piece = verify_extension_op(&put, TOPIC).unwrap().id();
        room.commit_extension(
            &key_a,
            TOPIC,
            TS + 3,
            ExtensionOp::SetConfig {
                pieces_locked: None,
                available_emojis: Some("🌵🎵".into()),
            },
        );

        let view = room.view();
        assert_eq!(view.pitches, BTreeSet::from([tet_degree(0), tet_degree(4)]));
        assert_eq!(view.pieces[&piece].pitch, tet_pitch(60));
        assert_eq!(view.tuning, Some(TuningDefinition::twelve_tet()));
        assert!(!view.pieces_locked);
        assert_eq!(view.available_emojis.as_deref(), Some("🌵🎵"));
    }

    #[test]
    fn each_lane_stamps_only_its_own_frontier() {
        // THE load-bearing invariant: a music op's causal horizon references
        // music ops alone, no matter how busy the extension lane is — so a
        // music-only peer never meets a predecessor it can't decode.
        let key_a = signing_key_from_seed(&SEED_A);
        let key_b = signing_key_from_seed(&SEED_B);
        let mut room = Room::new();

        room.commit_extension(
            &key_b,
            TOPIC,
            TS,
            ExtensionOp::PutPiece {
                emoji: "🌵".into(),
                pitch: tet_pitch(60),
            },
        );
        let first = room.commit_music(
            &key_a,
            TOPIC,
            TS + 1,
            MusicOp::AddDegree {
                degree: tet_degree(0),
            },
        );
        let first = verify_music_op(&first, TOPIC).unwrap();
        assert!(
            first.observed().is_empty(),
            "extension ops are invisible to the music frontier"
        );

        let second = room.commit_music(
            &key_b,
            TOPIC,
            TS + 2,
            MusicOp::AddDegree {
                degree: tet_degree(4),
            },
        );
        let second = verify_music_op(&second, TOPIC).unwrap();
        assert_eq!(
            second.observed(),
            &[first.hash()],
            "the music horizon is exactly the music frontier"
        );

        // And symmetrically: an extension op never observes a music op.
        let ext = room.commit_extension(
            &key_a,
            TOPIC,
            TS + 3,
            ExtensionOp::SetConfig {
                pieces_locked: Some(true),
                available_emojis: None,
            },
        );
        let ext = verify_extension_op(&ext, TOPIC).unwrap();
        for observed in ext.observed() {
            assert!(
                room.extension().lifted_entry(OpId(*observed)).is_some(),
                "extension horizon references extension ops only"
            );
        }
    }

    #[test]
    fn pieces_lock_gates_the_extension_lane() {
        let key_a = signing_key_from_seed(&SEED_A);
        let key_b = signing_key_from_seed(&SEED_B);
        let mut room = Room::new();
        let put = room.commit_extension(
            &key_a,
            TOPIC,
            TS,
            ExtensionOp::PutPiece {
                emoji: "🌵".into(),
                pitch: tet_pitch(60),
            },
        );
        let piece = verify_extension_op(&put, TOPIC).unwrap().id();
        room.commit_extension(
            &key_a,
            TOPIC,
            TS + 1,
            ExtensionOp::SetConfig {
                pieces_locked: Some(true),
                available_emojis: None,
            },
        );
        // B's move causally observes the lock -> suppressed.
        room.commit_extension(
            &key_b,
            TOPIC,
            TS + 2,
            ExtensionOp::MovePiece {
                piece,
                pitch: tet_pitch(64),
            },
        );

        let view = room.view();
        assert!(view.pieces_locked);
        assert_eq!(
            view.pieces[&piece].pitch,
            tet_pitch(60),
            "a move whose causal past is locked is a no-op"
        );
    }

    #[test]
    fn tuning_switch_hides_other_tuning_pieces_and_resurrects_them() {
        let key = signing_key_from_seed(&SEED_A);
        let mut room = Room::new();
        let put = room.commit_extension(
            &key,
            TOPIC,
            TS,
            ExtensionOp::PutPiece {
                emoji: "🌵".into(),
                pitch: tet_pitch(60),
            },
        );
        let piece = verify_extension_op(&put, TOPIC).unwrap().id();
        assert!(room.view().pieces.contains_key(&piece));

        // Switch the room (music lane) to another tuning: the 12-TET piece is
        // hidden — its ops preserved, never reinterpreted.
        room.commit_music(
            &key,
            TOPIC,
            TS + 1,
            MusicOp::SetTuning {
                definition: tuning_with_step(700),
            },
        );
        assert!(
            room.view().pieces.is_empty(),
            "other-tuning piece is hidden"
        );

        // Switch back: the piece resurrects untouched.
        room.commit_music(
            &key,
            TOPIC,
            TS + 2,
            MusicOp::SetTuning {
                definition: TuningDefinition::twelve_tet(),
            },
        );
        assert_eq!(room.view().pieces[&piece].pitch, tet_pitch(60));
    }

    #[test]
    fn degrees_follow_the_music_lane_tuning_scope() {
        let key = signing_key_from_seed(&SEED_A);
        let mut room = Room::new();
        room.commit_music(
            &key,
            TOPIC,
            TS,
            MusicOp::AddDegree {
                degree: tet_degree(7),
            },
        );
        let definition = tuning_with_step(700);
        let tuning = definition.validate("test").unwrap();
        room.commit_music(
            &key,
            TOPIC,
            TS + 1,
            MusicOp::SetTuning {
                definition: definition.clone(),
            },
        );
        let new_degree = TunedDegree::new(&tuning, 1).unwrap();
        room.commit_music(
            &key,
            TOPIC,
            TS + 2,
            MusicOp::AddDegree { degree: new_degree },
        );

        let view = room.view();
        assert_eq!(view.tuning, Some(definition));
        assert!(!view.pitches.contains(&tet_degree(7)));
        assert!(view.pitches.contains(&new_degree));
    }

    /// A fixed two-lane history pins the v4 roots (feature `merkle`).
    ///
    /// v4 FIXTURES — these are NEW pins, not updates of the v3 values: the
    /// signed bytes really changed (music ops travel the MusicLang wire; the
    /// extension wire is schema 4 under new magics), so each lane's `ops_root`
    /// is a fresh commitment, and the piece's identity (its `PutPiece` op id)
    /// changed with its signed bytes, so `state_root` legitimately moves for a
    /// piece-bearing history. The companion test below shows `state_root` does
    /// NOT move when the projected state doesn't.
    #[cfg(feature = "merkle")]
    #[test]
    fn merkle_golden_v4_roots() {
        let key_a = signing_key_from_seed(&SEED_A);
        let key_b = signing_key_from_seed(&SEED_B);
        let mut room = Room::new();

        // Music lane: two adds, one remove (kills degree 0), an envelope write
        // (a MusicOp no WalkieOp wrapper could carry), and an explicit tuning.
        room.commit_music(
            &key_a,
            TOPIC,
            TS,
            MusicOp::AddDegree {
                degree: tet_degree(0),
            },
        );
        room.commit_music(
            &key_b,
            TOPIC,
            TS + 1,
            MusicOp::AddDegree {
                degree: tet_degree(4),
            },
        );
        room.commit_music(
            &key_a,
            TOPIC,
            TS + 2,
            MusicOp::RemoveDegree {
                degree: tet_degree(0),
            },
        );
        room.commit_music(
            &key_a,
            TOPIC,
            TS + 3,
            MusicOp::SetEnvelope {
                degree: tet_degree(4),
                env: tutti_music::Envelope {
                    points: vec![(0, 0), (120, 96)],
                    interp: tutti_music::Interp::Linear,
                },
            },
        );
        room.commit_music(
            &key_b,
            TOPIC,
            TS + 4,
            MusicOp::SetTuning {
                definition: TuningDefinition::twelve_tet(),
            },
        );

        // Extension lane: a piece, a cross-author move, config writes.
        let put = room.commit_extension(
            &key_b,
            TOPIC,
            TS + 5,
            ExtensionOp::PutPiece {
                emoji: "🌵".into(),
                pitch: tet_pitch(60),
            },
        );
        let piece = verify_extension_op(&put, TOPIC).unwrap().id();
        room.commit_extension(
            &key_a,
            TOPIC,
            TS + 6,
            ExtensionOp::MovePiece {
                piece,
                pitch: tet_pitch(72),
            },
        );
        room.commit_extension(
            &key_a,
            TOPIC,
            TS + 7,
            ExtensionOp::SetConfig {
                pieces_locked: Some(true),
                available_emojis: None,
            },
        );
        room.commit_extension(
            &key_b,
            TOPIC,
            TS + 8,
            ExtensionOp::SetConfig {
                pieces_locked: None,
                available_emojis: Some("🌵🎵".into()),
            },
        );

        // The projected content, spelled out.
        let view = room.view();
        assert_eq!(view.pitches, BTreeSet::from([tet_degree(4)]));
        assert_eq!(view.pieces[&piece].pitch, tet_pitch(72));
        assert!(view.pieces_locked);
        assert_eq!(view.available_emojis.as_deref(), Some("🌵🎵"));

        let hex =
            |bytes: [u8; 32]| -> String { bytes.iter().map(|b| format!("{b:02x}")).collect() };
        assert_eq!(
            hex(room.music().ops_root()),
            "26ac58901110385c4ec6a08d5d8f593b1ccb212102ac6377e7bf66495eb5d347",
            "v4 music-lane ops_root",
        );
        assert_eq!(
            hex(room.extension().ops_root()),
            "e898f17274970168bfded414d6482182bb760e3642d78f3fcf40a12d6ce12971",
            "v4 extension-lane ops_root",
        );
        assert_eq!(
            hex(room.state_root()),
            "4526681a6925be2afaca73c7213486ce207cc3b2fee2add14da2e702ccc4f5e3",
            "v4 composed state_root",
        );
    }

    /// `state_root` is a commitment to the canonical projected state ALONE: the
    /// same content authored through the v3 single-lane wire and through the v4
    /// music lane projects the same [`RoomView`] and therefore the SAME root —
    /// across a whole wire generation. (Piece-bearing histories differ because
    /// a piece's identity is its creating op's id, whose signed bytes changed.)
    #[cfg(feature = "merkle")]
    #[test]
    fn state_root_moves_only_when_the_projected_state_does() {
        use crate::room::ops::WalkieOp;
        use crate::room::store::{RoomStore, RoomStoreStateRoot};

        let key = signing_key_from_seed(&SEED_A);

        let mut v3 = RoomStore::new();
        v3.commit(
            &key,
            TOPIC,
            TS,
            WalkieOp::AddDegree {
                pitch: tet_degree(0),
            },
        );
        v3.commit(
            &key,
            TOPIC,
            TS + 1,
            WalkieOp::AddDegree {
                pitch: tet_degree(4),
            },
        );

        let mut room = Room::new();
        room.commit_music(
            &key,
            TOPIC,
            TS,
            MusicOp::AddDegree {
                degree: tet_degree(0),
            },
        );
        room.commit_music(
            &key,
            TOPIC,
            TS + 1,
            MusicOp::AddDegree {
                degree: tet_degree(4),
            },
        );

        assert_eq!(
            room.view(),
            v3.view(),
            "identical canonical projected state"
        );
        assert_eq!(
            room.state_root(),
            v3.state_root(),
            "same projected state -> same state_root, even across wire generations"
        );
    }

    /// The extension lane rejects a v3 single-lane op: same crate, different
    /// generation — schema 4 gates it even before the alphabet could confuse.
    #[test]
    fn extension_lane_refuses_a_v3_op() {
        use crate::room::ops::{WalkieOp, sign_op};
        use tutti_core::LogHead;

        let key = signing_key_from_seed(&SEED_A);
        let (v3_signed, _) = sign_op(
            &key,
            &LogHead::genesis(),
            TS,
            WalkieOp::SetConfig {
                pieces_locked: Some(true),
                available_emojis: None,
            },
        );
        assert!(
            matches!(
                verify_extension_op(&v3_signed, TOPIC),
                Err(OpVerifyError::UnsupportedVersion(3))
            ),
            "a v3 op must fail the extension lane's schema gate"
        );
    }

    /// Wire frames are domain-separated: an extension frame refuses to deframe
    /// as music and vice versa, so the lanes cannot ingest each other's bytes.
    #[test]
    fn lane_wire_frames_reject_each_other() {
        use tutti_core::SignedOpWireError;

        let key = signing_key_from_seed(&SEED_A);
        let mut room = Room::new();
        let ext = room.commit_extension(
            &key,
            TOPIC,
            TS,
            ExtensionOp::SetConfig {
                pieces_locked: Some(true),
                available_emojis: None,
            },
        );
        let music = room.commit_music(
            &key,
            TOPIC,
            TS + 1,
            MusicOp::AddDegree {
                degree: tet_degree(0),
            },
        );

        let ext_frame = ext.to_wire_bytes_in::<ExtensionLang>().unwrap();
        let music_frame = music.to_wire_bytes_in::<MusicLang>().unwrap();
        assert_eq!(
            SignedOp::from_wire_bytes_in::<MusicLang>(&ext_frame),
            Err(SignedOpWireError::WrongDomain),
        );
        assert_eq!(
            SignedOp::from_wire_bytes_in::<ExtensionLang>(&music_frame),
            Err(SignedOpWireError::WrongDomain),
        );
    }

    /// `RoomView` composition drops nothing it shouldn't: the music lane's
    /// envelopes exist in [`MusicView`] but stay out of the committed room view
    /// (an explicit `state_root`-schema decision), while the degree they attach
    /// to is unaffected.
    #[test]
    fn envelopes_fold_in_the_music_lane_but_stay_out_of_room_view() {
        let key = signing_key_from_seed(&SEED_A);
        let mut room = Room::new();
        room.commit_music(
            &key,
            TOPIC,
            TS,
            MusicOp::AddDegree {
                degree: tet_degree(4),
            },
        );
        room.commit_music(
            &key,
            TOPIC,
            TS + 1,
            MusicOp::SetEnvelope {
                degree: tet_degree(4),
                env: tutti_music::Envelope {
                    points: vec![(0, 127)],
                    interp: tutti_music::Interp::Step,
                },
            },
        );

        let music_view = room.music().view();
        assert!(music_view.envelopes.contains_key(&tet_degree(4)));
        let view = room.view();
        assert!(view.pitches.contains(&tet_degree(4)));
        // RoomView has no envelope field to assert against — that absence is
        // the point; this pins that composition folds an envelope-bearing
        // history without disturbing the degrees or their attribution.
        use crate::room::ops::AuthorId;
        assert_eq!(
            view.pitch_authors[&tet_degree(4)],
            BTreeSet::from([AuthorId(*key.verifying_key().as_bytes())]),
        );
    }

    /// The extension-piece content of a view, keyed off op identity: v3 and v4
    /// spell the same history through different wires, so a piece's `OpId`
    /// legitimately differs — equivalence is over what the pieces ARE.
    fn piece_contents(
        view: &RoomView,
    ) -> Vec<(String, crate::room::ops::AuthorId, TunedPeriodicPitch)> {
        view.pieces
            .values()
            .map(|piece| (piece.emoji.clone(), piece.owner, piece.pitch))
            .collect()
    }

    fn assert_views_equivalent(v4_view: &RoomView, v3_view: &RoomView, when: &str) {
        assert_eq!(
            piece_contents(v4_view),
            piece_contents(v3_view),
            "{when}: v4 pieces must match v3's"
        );
        assert_eq!(v4_view.pitches, v3_view.pitches, "{when}: pitches");
        assert_eq!(v4_view.tuning, v3_view.tuning, "{when}: tuning");
        assert_eq!(v4_view.pieces_locked, v3_view.pieces_locked, "{when}: lock");
        assert_eq!(
            v4_view.available_emojis, v3_view.available_emojis,
            "{when}: palette"
        );
    }

    /// H1 REGRESSION GATE — the object must not disappear. Active tuning
    /// 12-TET; the piece holds a valid 12-TET move (M_A) and a LATER
    /// other-tuning move (M_B) that causally dominates it, so a GLOBAL
    /// position register would deterministically pick M_B — whose pitch then
    /// fails the active-tuning filter, hiding the whole piece (the pre-fix v4
    /// behavior). Per-`(piece, TuningId)` scoping makes M_B a non-event for
    /// the put-tuning register: the piece stays at M_A, exactly as v3 (whose
    /// classification never admits M_B). Checked as a full v3-vs-v4
    /// equivalence, through a tuning switch and back.
    #[test]
    fn other_tuning_move_never_hides_the_piece_v3_v4_equivalence() {
        use crate::room::ops::{WalkieOp, verify_signed_op};
        use crate::room::store::RoomStore;

        let key_a = signing_key_from_seed(&SEED_A);
        let key_b = signing_key_from_seed(&SEED_B);
        let other_tuning = tuning_with_step(700);
        let other_pitch =
            TunedPeriodicPitch::new(&other_tuning.validate("other tuning").unwrap(), 1, 0).unwrap();

        // v4: put -> M_A -> M_B on the extension lane; each commit observes the
        // lane frontier, so M_B causally dominates M_A.
        let mut room = Room::new();
        let put = room.commit_extension(
            &key_a,
            TOPIC,
            TS,
            ExtensionOp::PutPiece {
                emoji: "🌵".into(),
                pitch: tet_pitch(60),
            },
        );
        let piece = verify_extension_op(&put, TOPIC).unwrap().id();
        room.commit_extension(
            &key_b,
            TOPIC,
            TS + 1,
            ExtensionOp::MovePiece {
                piece,
                pitch: tet_pitch(64),
            },
        );
        room.commit_extension(
            &key_b,
            TOPIC,
            TS + 2,
            ExtensionOp::MovePiece {
                piece,
                pitch: other_pitch,
            },
        );

        // v3: the identical history through the single-lane wire.
        let mut v3 = RoomStore::new();
        let v3_put = v3.commit(
            &key_a,
            TOPIC,
            TS,
            WalkieOp::PutPiece {
                emoji: "🌵".into(),
                pitch: tet_pitch(60),
            },
        );
        let v3_piece = verify_signed_op(&v3_put).unwrap().id();
        v3.commit(
            &key_b,
            TOPIC,
            TS + 1,
            WalkieOp::MovePiece {
                piece: v3_piece,
                pitch: tet_pitch(64),
            },
        );
        v3.commit(
            &key_b,
            TOPIC,
            TS + 2,
            WalkieOp::MovePiece {
                piece: v3_piece,
                pitch: other_pitch,
            },
        );

        let v4_view = room.view();
        assert_eq!(
            v4_view.pieces[&piece].pitch,
            tet_pitch(64),
            "the piece must NOT vanish: the other-tuning register winner is \
             irrelevant to the active tuning's assertion"
        );
        assert_views_equivalent(&v4_view, &v3.view(), "under 12-TET");

        // Switch the active tuning to M_B's: the piece (created under 12-TET)
        // is hidden on BOTH wires — an other-tuning move never conjures it.
        room.commit_music(
            &key_a,
            TOPIC,
            TS + 3,
            MusicOp::SetTuning {
                definition: other_tuning.clone(),
            },
        );
        v3.commit(
            &key_a,
            TOPIC,
            TS + 3,
            WalkieOp::SetTuning {
                definition: other_tuning.clone(),
            },
        );
        assert!(
            room.view().pieces.is_empty(),
            "hidden under the other tuning"
        );
        assert_views_equivalent(&room.view(), &v3.view(), "under the other tuning");

        // Switch back: it resurrects at M_A's position on both wires.
        room.commit_music(
            &key_a,
            TOPIC,
            TS + 4,
            MusicOp::SetTuning {
                definition: TuningDefinition::twelve_tet(),
            },
        );
        v3.commit(
            &key_a,
            TOPIC,
            TS + 4,
            WalkieOp::SetTuning {
                definition: TuningDefinition::twelve_tet(),
            },
        );
        assert_eq!(room.view().pieces[&piece].pitch, tet_pitch(64));
        assert_views_equivalent(&room.view(), &v3.view(), "after switching back");
    }

    /// H1, the review's exact fixture: M_A (active tuning) and M_B (other
    /// tuning) are CONCURRENT — neither observed the other — so pre-fix the
    /// global register winner was an entry-hash coin toss that could hide the
    /// piece. Per-tuning scoping makes the outcome tiebreak-independent: M_B
    /// is a non-event, M_A dominates the put, and v4 matches v3 exactly.
    #[test]
    fn concurrent_moves_across_tunings_v3_v4_equivalence() {
        use crate::room::test_support::Peer;
        use tutti_core::{LogHead, VersionedOpG, sign_versioned_op};

        let key_a = signing_key_from_seed(&SEED_A);
        let key_b = signing_key_from_seed(&SEED_B);
        let other_pitch = TunedPeriodicPitch::new(
            &tuning_with_step(700).validate("other tuning").unwrap(),
            1,
            0,
        )
        .unwrap();

        // Hand-sign the v4 extension ops so the two moves are truly concurrent.
        let sign_ext = |key: &SigningKey,
                        head: &LogHead,
                        ts: u64,
                        observed: Vec<[u8; 32]>,
                        op: ExtensionOp| {
            sign_versioned_op(
                key,
                head,
                VersionedOpG::<ExtensionLang>::current_for_topic(op, ts, TOPIC).observing(observed),
            )
        };
        let (put, head_a) = sign_ext(
            &key_a,
            &LogHead::genesis(),
            TS,
            vec![],
            ExtensionOp::PutPiece {
                emoji: "🌵".into(),
                pitch: tet_pitch(60),
            },
        );
        let put_verified = verify_extension_op(&put, TOPIC).unwrap();
        let piece = put_verified.id();
        let put_hash = put_verified.hash();
        let (m_a, _) = sign_ext(
            &key_a,
            &head_a,
            TS + 1,
            vec![put_hash],
            ExtensionOp::MovePiece {
                piece,
                pitch: tet_pitch(64),
            },
        );
        // B observed only the put -> M_B is concurrent with M_A.
        let (m_b, _) = sign_ext(
            &key_b,
            &LogHead::genesis(),
            TS + 2,
            vec![put_hash],
            ExtensionOp::MovePiece {
                piece,
                pitch: other_pitch,
            },
        );

        let mut room = Room::new();
        for signed in [&put, &m_a, &m_b] {
            room.ingest_extension(verify_extension_op(signed, TOPIC).unwrap());
        }

        // v3: the same causal shape through the single-lane wire.
        let mut peer_a = Peer::new(&SEED_A);
        let mut peer_b = Peer::new(&SEED_B);
        use crate::room::ops::WalkieOp;
        use crate::room::store::RoomStore;
        let v3_put = peer_a.sign(
            TS,
            vec![],
            WalkieOp::PutPiece {
                emoji: "🌵".into(),
                pitch: tet_pitch(60),
            },
        );
        let v3_piece = v3_put.id();
        let v3_m_a = peer_a.sign(
            TS + 1,
            vec![v3_put.hash()],
            WalkieOp::MovePiece {
                piece: v3_piece,
                pitch: tet_pitch(64),
            },
        );
        let v3_m_b = peer_b.sign(
            TS + 2,
            vec![v3_put.hash()],
            WalkieOp::MovePiece {
                piece: v3_piece,
                pitch: other_pitch,
            },
        );
        let mut v3 = RoomStore::new();
        for op in [v3_put, v3_m_a, v3_m_b] {
            v3.ingest_verified(op);
        }

        let v4_view = room.view();
        assert_eq!(
            v4_view.pieces[&piece].pitch,
            tet_pitch(64),
            "M_A dominates the put within the put's tuning; concurrent M_B \
             cannot displace (or hide) it"
        );
        assert_views_equivalent(&v4_view, &v3.view(), "concurrent cross-tuning moves");
    }

    /// Ingress requires the room's topic on BOTH lanes — v3's
    /// `verify_signed_op_for_topic` gate carried into v4. In production the
    /// expected string is the DERIVED topic's hex (see `verify_music_op` docs
    /// and `tests/music_lane_interop.rs`, which pins the derivation); this
    /// test pins the mechanism: wrong topic and missing topic both refuse.
    #[test]
    fn lane_verification_enforces_the_room_topic() {
        use tutti_core::{LogHead, VersionedOpG, sign_versioned_op};

        let key = signing_key_from_seed(&SEED_A);
        let mut room = Room::new();
        let music = room.commit_music(
            &key,
            TOPIC,
            TS,
            MusicOp::AddDegree {
                degree: tet_degree(0),
            },
        );
        let ext = room.commit_extension(
            &key,
            TOPIC,
            TS + 1,
            ExtensionOp::SetConfig {
                pieces_locked: Some(true),
                available_emojis: None,
            },
        );

        assert!(verify_music_op(&music, TOPIC).is_ok());
        assert!(verify_extension_op(&ext, TOPIC).is_ok());
        assert!(matches!(
            verify_music_op(&music, "another-room-topic"),
            Err(OpVerifyError::TopicMismatch { .. })
        ));
        assert!(matches!(
            verify_extension_op(&ext, "another-room-topic"),
            Err(OpVerifyError::TopicMismatch { .. })
        ));

        // A topicless op refuses room ingress outright.
        let (topicless, _) = sign_versioned_op(
            &key,
            &LogHead::genesis(),
            VersionedOpG::<MusicLang>::current(
                MusicOp::AddDegree {
                    degree: tet_degree(0),
                },
                TS,
            ),
        );
        assert!(matches!(
            verify_music_op(&topicless, TOPIC),
            Err(OpVerifyError::MissingTopic)
        ));
    }

    /// The parking containment for a NON-conforming author (module docs: the
    /// separate-frontier invariant binds Store-based authors, not arbitrary
    /// signers). A validly signed music op whose horizon cites an EXTENSION
    /// entry hash verifies — causal refs are untyped, so no ingress check can
    /// tell it from an op citing a music entry not yet received — but strict
    /// deferral parks it forever: it never lifts and the fold never sees it.
    /// Fails closed, not open.
    #[test]
    fn cross_lane_ref_parks_forever_and_fails_closed() {
        use tutti_core::{LogHead, VersionedOpG, sign_versioned_op};

        let key_a = signing_key_from_seed(&SEED_A);
        let key_b = signing_key_from_seed(&SEED_B);
        let mut room = Room::new();
        let ext = room.commit_extension(
            &key_a,
            TOPIC,
            TS,
            ExtensionOp::PutPiece {
                emoji: "🌵".into(),
                pitch: tet_pitch(60),
            },
        );
        let ext_entry = room
            .extension()
            .lifted_entry(verify_extension_op(&ext, TOPIC).unwrap().id())
            .expect("extension op is lifted");
        let before = room.view();

        let versioned = VersionedOpG::<MusicLang>::current_for_topic(
            MusicOp::AddDegree {
                degree: tet_degree(4),
            },
            TS + 1,
            TOPIC,
        )
        .observing([*ext_entry.as_bytes()]);
        let (signed, _) = sign_versioned_op(&key_b, &LogHead::genesis(), versioned);

        let verified = verify_music_op(&signed, TOPIC)
            .expect("verification cannot type-check an untyped causal ref");
        let lifted = room.ingest_music(verified);
        assert!(lifted.is_empty(), "the op never lifts");
        assert_eq!(room.music().pending_len(), 1, "parked, permanently");
        assert_eq!(
            room.view(),
            before,
            "the fold never sees the parked op — fails closed"
        );
    }

    /// Composition is total over BTreeMap moves — no leftover clone of the
    /// extension pieces survives in the composed view when the tuning hides
    /// them all (defensive shape check for the `Err` arm in `Room::view`).
    #[test]
    fn empty_room_composes_the_default_view() {
        let room = Room::new();
        let view = room.view();
        assert_eq!(
            view,
            RoomView {
                pitches: BTreeSet::new(),
                pitch_authors: BTreeMap::new(),
                pieces: BTreeMap::new(),
                tuning: Some(TuningDefinition::twelve_tet()),
                pieces_locked: false,
                available_emojis: None,
            }
        );
    }
}
