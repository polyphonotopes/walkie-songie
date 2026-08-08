//! Walkie's room read model: the [`WalkieLang`] fold over `tutti_core`'s generic
//! signed-op store.
//!
//! The lift/deferral/heads machinery, the `Store<L>` container, and the causal
//! [`FoldCtx`] seam now live in [`tutti_core`] (tutti extraction Track-D step 3);
//! `RoomStore` is `tutti_core::Store<WalkieLang>`. What stays here is the music:
//! [`walkie_fold`] and the `with_*` builders that materialize a [`RoomView`] —
//! pitches are a content-keyed add-wins set resolved by causal ancestry, pieces
//! are SHARED (cross-author observed-remove for lifecycle plus a causal-maxima
//! position register; owner is attribution, `pieces_locked` is the consent gate),
//! and tuning/config are cross-author registers resolved by causal maxima.
//!
//! The fold reads the decoded op-set through the public [`FoldCtx`] surface
//! ([`FoldCtx::decoded`], [`FoldCtx::op_id`], [`FoldCtx::is_ancestor`],
//! [`FoldCtx::resolve`]) — the domain never touches the store's private indexes.

use std::collections::{BTreeMap, BTreeSet};

use hhhs::EntryHash;

use crate::room::ops::{AuthorId, OpId, WalkieLang, WalkieOp};
use crate::tuning::{TunedDegree, TunedPeriodicPitch, TuningDefinition};

/// The generic substrate types, re-exported so `RoomStore` and the sync layer name
/// them through `room::store` unchanged. [`Reach`]/[`CausalPast`] back the
/// equivalence tests; [`sync_root_of`] is the convergence digest the RBSR session
/// cross-checks; [`Store`]/[`FoldCtx`]/[`DecodedOp`] are the store + fold seam.
pub use tutti_core::{CausalPast, DecodedOp, FoldCtx, Reach, Store, sync_root_of};

/// Walkie-songie's room store — [`Store`] fixed at [`WalkieLang`]. Every call site
/// outside `store.rs`/`ops.rs` keeps the pre-extraction spelling `RoomStore`.
pub type RoomStore = Store<WalkieLang>;

/// Walkie's [`OpLanguage::fold`](tutti_core::OpLanguage::fold): the register →
/// add-wins → object composition that materializes a [`RoomView`] from the causal
/// indexes in `ctx`. The register fold runs first so the tuning-scoped set/object
/// folds can filter by the resolved tuning (the staged fold — facets are not
/// independent, and the API admits it). The projected view is bit-for-bit the
/// pre-extraction one.
pub(crate) fn walkie_fold(ctx: &FoldCtx<'_, WalkieLang>) -> RoomView {
    RoomView {
        pitches: BTreeSet::new(),
        pitch_authors: BTreeMap::new(),
        pieces: BTreeMap::new(),
        tuning: Some(TuningDefinition::twelve_tet()),
        pieces_locked: false,
        available_emojis: None,
    }
    .with_registers(ctx)
    .with_pitches(ctx)
    .with_pieces(ctx)
}

impl RoomView {
    /// Pitches: content-keyed ADD-WINS. An add is live iff no same-key remove
    /// causally observed it (`is_ancestor(add, remove)`).
    fn with_pitches(mut self, ctx: &FoldCtx<'_, WalkieLang>) -> Self {
        let Some(active_tuning) = self
            .tuning
            .as_ref()
            .and_then(|definition| definition.validate("active room tuning").ok())
        else {
            return self;
        };
        let mut adds: BTreeMap<TunedDegree, Vec<EntryHash>> = BTreeMap::new();
        let mut removes: BTreeMap<TunedDegree, Vec<EntryHash>> = BTreeMap::new();
        for (entry, decoded) in ctx.decoded() {
            match decoded.op() {
                WalkieOp::AddDegree { pitch } if pitch.validate(&active_tuning).is_ok() => {
                    adds.entry(*pitch).or_default().push(*entry)
                }
                WalkieOp::RemoveDegree { pitch } if pitch.validate(&active_tuning).is_ok() => {
                    removes.entry(*pitch).or_default().push(*entry)
                }
                _ => {}
            }
        }
        for (key, add_entries) in &adds {
            let key_removes = removes.get(key).map(Vec::as_slice).unwrap_or(&[]);
            let mut authors: BTreeSet<AuthorId> = BTreeSet::new();
            for add in add_entries {
                let killed = key_removes
                    .iter()
                    .any(|remove| ctx.is_ancestor(add, remove));
                if !killed {
                    authors.insert(ctx.decoded()[add].author());
                }
            }
            if !authors.is_empty() {
                self.pitches.insert(*key);
                self.pitch_authors.insert(*key, authors);
            }
        }
        self
    }

    /// Pieces: SHARED, resolved as a pure function of the op-set — any author's
    /// `Move`/`Remove`/`UnremovePiece` counts. `owner` is attribution only (the
    /// `PutPiece` author), never a gate; `pieces_locked` is the consent gate.
    ///
    /// * **Lifecycle — observed-remove + add-wins, mirroring degrees.** The
    ///   `PutPiece` and every valid `MovePiece` of the piece are *adds*. A
    ///   `RemovePiece` `R` **kills** exactly the adds in its causal past
    ///   (`is_ancestor(add, R)`) — so a remove concurrent with an add cannot kill
    ///   it (add-wins), and a move/put causally *after* a remove resurrects the
    ///   piece. An `UnremovePiece` `U` **overrides** the remove it observed
    ///   (`U.remove == R` and `is_ancestor(R, U)`). The piece is alive iff at
    ///   least one add survives every effective remove.
    /// * **Position — a register across ALL authors.** Concurrent moves are
    ///   resolved by [`FoldCtx::resolve`] over the *surviving* adds: causal
    ///   precedence where comparable, then the max raw-bytes [`EntryHash`]
    ///   tiebreak. No wall-clock, no seqs.
    /// * **`pieces_locked` — the consent gate, applied per op by causal past.** A
    ///   `Move`/`Remove`/`UnremovePiece` is suppressed iff the lock register
    ///   resolved over that op's causal ancestors reads `true`. A move/remove
    ///   *concurrent* with a lock still applies. `PutPiece` is never suppressed.
    fn with_pieces(mut self, ctx: &FoldCtx<'_, WalkieLang>) -> Self {
        let Some(active_tuning) = self
            .tuning
            .as_ref()
            .and_then(|definition| definition.validate("active room tuning").ok())
        else {
            return self;
        };

        // (put_entry, piece_id, owner, emoji, put_pitch)
        let mut puts: Vec<(EntryHash, OpId, AuthorId, String, TunedPeriodicPitch)> = Vec::new();
        // (move_entry, target_piece) — pitch is read back from `ctx.decoded()`.
        let mut moves: Vec<(EntryHash, OpId)> = Vec::new();
        // (remove_entry, remove_op_id, target_piece)
        let mut removes: Vec<(EntryHash, OpId, OpId)> = Vec::new();
        // (unremove_entry, target_remove_op_id)
        let mut unremoves: Vec<(EntryHash, OpId)> = Vec::new();
        // SetConfig writes that carry a `pieces_locked` value (the lock register).
        let mut lock_writes: BTreeSet<EntryHash> = BTreeSet::new();

        for (entry, decoded) in ctx.decoded() {
            let op_id = ctx.op_id(entry);
            match decoded.op() {
                WalkieOp::PutPiece { emoji, pitch } if pitch.validate(&active_tuning).is_ok() => {
                    puts.push((*entry, op_id, decoded.author(), emoji.clone(), *pitch))
                }
                WalkieOp::MovePiece { piece, pitch } if pitch.validate(&active_tuning).is_ok() => {
                    moves.push((*entry, *piece))
                }
                WalkieOp::RemovePiece { piece } => removes.push((*entry, op_id, *piece)),
                WalkieOp::UnremovePiece { remove } => unremoves.push((*entry, *remove)),
                WalkieOp::SetConfig {
                    pieces_locked: Some(_),
                    ..
                } => {
                    lock_writes.insert(*entry);
                }
                _ => {}
            }
        }

        // Whether the pieces-lock register, resolved over ONLY the causal ancestors
        // of `op`, reads `true`. A move/remove/unremove is suppressed exactly when
        // this holds — i.e. an active lock sits in its causal past.
        let locked_as_of = |op: &EntryHash| -> bool {
            let observed: BTreeSet<EntryHash> = lock_writes
                .iter()
                .copied()
                .filter(|write| ctx.is_ancestor(write, op))
                .collect();
            ctx.resolve(&observed).is_some_and(|winner| {
                matches!(
                    ctx.decoded()[&winner].op(),
                    WalkieOp::SetConfig {
                        pieces_locked: Some(true),
                        ..
                    }
                )
            })
        };

        for (put_entry, piece_id, owner, emoji, put_pitch) in &puts {
            // Effective removes of this piece: not lock-suppressed, and not
            // overridden by an unremove that observed them (and is itself unlocked).
            let effective_removes: Vec<EntryHash> = removes
                .iter()
                .filter(|(_, _, target)| target == piece_id)
                .filter(|(rem_entry, rem_id, _)| {
                    if locked_as_of(rem_entry) {
                        return false;
                    }
                    let overridden = unremoves.iter().any(|(un_entry, target_rem)| {
                        target_rem == rem_id
                            && ctx.is_ancestor(rem_entry, un_entry)
                            && !locked_as_of(un_entry)
                    });
                    !overridden
                })
                .map(|(rem_entry, _, _)| *rem_entry)
                .collect();

            // Adds = the put + every non-suppressed move of this piece; an add
            // survives iff no effective remove causally observed it (add-wins).
            let survives = |add: &EntryHash| {
                !effective_removes
                    .iter()
                    .any(|rem| ctx.is_ancestor(add, rem))
            };
            let mut surviving: BTreeSet<EntryHash> = BTreeSet::new();
            if survives(put_entry) {
                surviving.insert(*put_entry);
            }
            for (move_entry, _) in moves.iter().filter(|(_, target)| target == piece_id) {
                if !locked_as_of(move_entry) && survives(move_entry) {
                    surviving.insert(*move_entry);
                }
            }
            if surviving.is_empty() {
                // Every assertion of this piece was observed-removed: it is gone.
                continue;
            }

            // Position = the register winner among the surviving adds' pitches.
            let pitch = ctx
                .resolve(&surviving)
                .map(|winner| match ctx.decoded()[&winner].op() {
                    WalkieOp::PutPiece { pitch, .. } | WalkieOp::MovePiece { pitch, .. } => *pitch,
                    _ => unreachable!("a surviving add is a PutPiece or MovePiece"),
                })
                .unwrap_or(*put_pitch);

            self.pieces.insert(
                *piece_id,
                Piece {
                    id: *piece_id,
                    owner: *owner,
                    emoji: emoji.clone(),
                    pitch,
                },
            );
        }
        self
    }

    /// Tuning / config: cross-author registers resolved by causal maxima then max
    /// raw-bytes entry hash. Each config field is resolved independently.
    fn with_registers(mut self, ctx: &FoldCtx<'_, WalkieLang>) -> Self {
        let mut tuning_writes: BTreeSet<EntryHash> = BTreeSet::new();
        let mut locked_writes: BTreeSet<EntryHash> = BTreeSet::new();
        let mut emoji_writes: BTreeSet<EntryHash> = BTreeSet::new();
        for (entry, decoded) in ctx.decoded() {
            match decoded.op() {
                WalkieOp::SetTuning { .. } => {
                    tuning_writes.insert(*entry);
                }
                WalkieOp::SetConfig {
                    pieces_locked,
                    available_emojis,
                } => {
                    if pieces_locked.is_some() {
                        locked_writes.insert(*entry);
                    }
                    if available_emojis.is_some() {
                        emoji_writes.insert(*entry);
                    }
                }
                _ => {}
            }
        }

        self.tuning = ctx
            .resolve(&tuning_writes)
            .map(|winner| match ctx.decoded()[&winner].op() {
                WalkieOp::SetTuning { definition } => definition.clone(),
                _ => unreachable!("tuning candidate is a SetTuning"),
            })
            .or_else(|| Some(TuningDefinition::twelve_tet()));
        self.pieces_locked = ctx
            .resolve(&locked_writes)
            .map(|winner| match ctx.decoded()[&winner].op() {
                WalkieOp::SetConfig {
                    pieces_locked: Some(locked),
                    ..
                } => *locked,
                _ => unreachable!("locked candidate carries pieces_locked"),
            })
            .unwrap_or(false);
        self.available_emojis = ctx
            .resolve(&emoji_writes)
            .map(|winner| match ctx.decoded()[&winner].op() {
                WalkieOp::SetConfig {
                    available_emojis: Some(emojis),
                    ..
                } => emojis.clone(),
                _ => unreachable!("emoji candidate carries available_emojis"),
            });
        self
    }
}

/// A live emoji piece.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Piece {
    pub id: OpId,
    pub owner: AuthorId,
    pub emoji: String,
    pub pitch: TunedPeriodicPitch,
}

/// The materialized room read model.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoomView {
    /// Live tuning-scoped degree keys.
    pub pitches: BTreeSet<TunedDegree>,
    /// For each live key, the authors that hold a live add of it.
    pub pitch_authors: BTreeMap<TunedDegree, BTreeSet<AuthorId>>,
    /// Live pieces keyed by their PutPiece op id.
    pub pieces: BTreeMap<OpId, Piece>,
    /// Resolved canonical room tuning. Built-in 12-TET is the default.
    pub tuning: Option<TuningDefinition>,
    /// Whether pieces are locked (default false).
    pub pieces_locked: bool,
    /// Room-wide available-emoji palette, if set.
    pub available_emojis: Option<String>,
}

#[cfg(feature = "merkle")]
impl RoomView {
    /// M2 — the additive `state_root`: a canonical blake3-256 Merkle commitment to
    /// this projected view. A pure, deterministic function of the view's fields;
    /// see [`crate::room::merkle::state_trie`] for the leaf grammar.
    pub fn state_root(&self) -> [u8; 32] {
        crate::room::merkle::state_root_of(self)
    }
}

/// The walkie-facing `state_root` on the store (feature `merkle`).
///
/// It commits to the projected [`RoomView`], so it needs `L::View` — it cannot be
/// a generic `Store<L>` method until `L::View: Canonical` is wired (deferred with
/// the rest of tutti-core's Merkle work). Provided as a walkie extension trait
/// rather than an inherent `impl Store<WalkieLang>` because `Store` is a foreign
/// type; `ops_root`/`prove_op` (entry-hash only, domain-agnostic) ARE inherent on
/// `Store<L>` in tutti-core and need no trait. Bring this trait into scope to call
/// `store.state_root()`.
#[cfg(feature = "merkle")]
pub trait RoomStoreStateRoot {
    fn state_root(&self) -> [u8; 32];
}

#[cfg(feature = "merkle")]
impl RoomStoreStateRoot for RoomStore {
    fn state_root(&self) -> [u8; 32] {
        self.view().state_root()
    }
}

#[cfg(test)]
mod tests {
    use super::WalkieOp::*;
    use super::*;

    use super::super::ops::{
        VerifiedOp, signing_key_from_seed, verify_signed_op, verify_signed_op_for_topic,
    };
    use super::super::test_support::{
        Peer, SEED_A, SEED_B, SEED_C, TOPIC, entryhash_set, oracle, tet_definition, tet_degree,
        tet_pitch, tuning_with_step,
    };

    fn ingest_in_order(base: &[VerifiedOp], order: &[usize]) -> RoomStore {
        let mut store = RoomStore::new();
        for &i in order {
            store.ingest_verified(base[i].clone());
        }
        store
    }

    #[test]
    fn prepared_commit_is_invisible_until_persistence_boundary_ingests_it() {
        let mut store = RoomStore::new();
        let key = signing_key_from_seed(&SEED_A);
        let signed = store.prepare_commit(
            &key,
            TOPIC,
            1,
            AddDegree {
                pitch: tet_degree(4),
            },
        );
        assert!(store.is_empty());
        assert!(store.view().pitches.is_empty());

        let verified = verify_signed_op_for_topic(&signed, TOPIC).unwrap();
        store.ingest_verified(verified);
        assert!(store.view().pitches.contains(&tet_degree(4)));
    }

    fn ingest(ops: &[VerifiedOp]) -> RoomStore {
        let order: Vec<usize> = (0..ops.len()).collect();
        ingest_in_order(ops, &order)
    }

    fn assert_parity(ops: &[VerifiedOp]) {
        assert_eq!(ingest(ops).view(), oracle(ops));
    }

    // ---------------------------------------------------------------------
    // Scenario builders
    // ---------------------------------------------------------------------

    /// Two concurrent adds of the same key + a remove observing only one.
    /// Add-wins: the unobserved add survives, so the key stays live.
    fn concurrent_add_remove() -> (Vec<VerifiedOp>, AuthorId) {
        let mut a = Peer::new(&SEED_A);
        let mut b = Peer::new(&SEED_B);
        let survivor = b.author();
        let a0 = a.sign(
            1,
            vec![],
            AddDegree {
                pitch: tet_degree(5),
            },
        );
        let b0 = b.sign(
            2,
            vec![],
            AddDegree {
                pitch: tet_degree(5),
            },
        );
        // remove: backlink = a0 (observes only its own add), never sees b0.
        let a1 = a.sign(
            3,
            vec![],
            RemoveDegree {
                pitch: tet_degree(5),
            },
        );
        (vec![a0, b0, a1], survivor)
    }

    fn rich_history() -> Vec<VerifiedOp> {
        let mut a = Peer::new(&SEED_A);
        let mut b = Peer::new(&SEED_B);
        let mut c = Peer::new(&SEED_C);

        // Degrees: 0 add-wins keeps B; 7 is fully removed.
        let a_add0 = a.sign(
            1,
            vec![],
            AddDegree {
                pitch: tet_degree(0),
            },
        );
        let b_add0 = b.sign(
            2,
            vec![],
            AddDegree {
                pitch: tet_degree(0),
            },
        );
        let a_rem0 = a.sign(
            3,
            vec![],
            RemoveDegree {
                pitch: tet_degree(0),
            },
        );
        let c_add7 = c.sign(
            4,
            vec![],
            AddDegree {
                pitch: tet_degree(7),
            },
        );
        let a_add7 = a.sign(
            5,
            vec![c_add7.hash()],
            AddDegree {
                pitch: tet_degree(7),
            },
        );
        // remove7: backlink = c_add7, observes a_add7 -> both adds in its past.
        let c_rem7 = c.sign(
            6,
            vec![a_add7.hash()],
            RemoveDegree {
                pitch: tet_degree(7),
            },
        );

        // Pieces (SHARED): A creates; non-owner B moves it (takes effect); A then
        // moves it later, observing B's move, so A's position wins by causal
        // recency (not by ownership); remove then unremove by A -> alive.
        let a_put = a.sign(
            7,
            vec![],
            PutPiece {
                emoji: "🌵".into(),
                pitch: tet_pitch(60),
            },
        );
        let piece = a_put.id();
        // B (non-owner) moves first, observing the put.
        let b_mov = b.sign(
            8,
            vec![a_put.hash()],
            MovePiece {
                piece,
                pitch: tet_pitch(61),
            },
        );
        // A moves later, observing B's move -> causally dominates it -> A wins.
        let a_mov = a.sign(
            9,
            vec![b_mov.hash()],
            MovePiece {
                piece,
                pitch: tet_pitch(72),
            },
        );
        let a_rem_p = a.sign(10, vec![], RemovePiece { piece });
        let a_unrem = a.sign(
            11,
            vec![],
            UnremovePiece {
                remove: a_rem_p.id(),
            },
        );

        // Tuning: two concurrent writers -> register tiebreak.
        let a_tune = a.sign(
            12,
            vec![],
            SetTuning {
                definition: tet_definition(),
            },
        );
        let b_tune = b.sign(
            13,
            vec![],
            SetTuning {
                definition: tet_definition(),
            },
        );

        // Config: independent fields from different authors.
        let a_cfg = a.sign(
            14,
            vec![],
            SetConfig {
                pieces_locked: Some(true),
                available_emojis: None,
            },
        );
        let c_cfg = c.sign(
            15,
            vec![],
            SetConfig {
                pieces_locked: None,
                available_emojis: Some("🌵🎵".into()),
            },
        );

        vec![
            a_add0, b_add0, a_rem0, c_add7, a_add7, c_rem7, a_put, b_mov, a_mov, a_rem_p, a_unrem,
            a_tune, b_tune, a_cfg, c_cfg,
        ]
    }

    // ---------------------------------------------------------------------
    // Parity tests over adversarial histories
    // ---------------------------------------------------------------------

    #[test]
    fn add_wins_two_concurrent_adds_and_a_remove() {
        let (ops, survivor) = concurrent_add_remove();
        let view = ingest(&ops).view();
        assert!(
            view.pitches.contains(&tet_degree(5)),
            "add-wins keeps the key live"
        );
        assert_eq!(
            view.pitch_authors[&tet_degree(5)],
            BTreeSet::from([survivor])
        );
        assert_parity(&ops);
    }

    #[test]
    fn retract_then_recreate_is_live() {
        let mut a = Peer::new(&SEED_A);
        let author = a.author();
        let add0 = a.sign(
            1,
            vec![],
            AddDegree {
                pitch: tet_degree(3),
            },
        );
        let rem = a.sign(
            2,
            vec![],
            RemoveDegree {
                pitch: tet_degree(3),
            },
        );
        let add1 = a.sign(
            3,
            vec![],
            AddDegree {
                pitch: tet_degree(3),
            },
        );
        let ops = vec![add0, rem, add1];
        let view = ingest(&ops).view();
        assert!(
            view.pitches.contains(&tet_degree(3)),
            "re-add after remove resurrects"
        );
        assert_eq!(view.pitch_authors[&tet_degree(3)], BTreeSet::from([author]));
        assert_parity(&ops);
    }

    #[test]
    fn concurrent_remove_does_not_kill_add() {
        // A adds; B removes WITHOUT observing A's add (no backlink, no observed).
        let mut a = Peer::new(&SEED_A);
        let mut b = Peer::new(&SEED_B);
        let add = a.sign(
            1,
            vec![],
            AddDegree {
                pitch: tet_degree(9),
            },
        );
        let rem = b.sign(
            2,
            vec![],
            RemoveDegree {
                pitch: tet_degree(9),
            },
        );
        let ops = vec![add, rem];
        let view = ingest(&ops).view();
        assert!(
            view.pitches.contains(&tet_degree(9)),
            "a concurrent remove cannot kill the add"
        );
        assert_parity(&ops);
    }

    #[test]
    fn tuning_change_hides_old_contributions_without_reinterpreting_them() {
        let mut a = Peer::new(&SEED_A);
        let old = a.sign(
            1,
            vec![],
            AddDegree {
                pitch: tet_degree(7),
            },
        );
        let definition = tuning_with_step(700);
        let tuning = definition.validate("test").unwrap();
        let set = a.sign(
            2,
            vec![],
            SetTuning {
                definition: definition.clone(),
            },
        );
        let new_pitch = TunedDegree::new(&tuning, 1).unwrap();
        let new = a.sign(3, vec![], AddDegree { pitch: new_pitch });
        let ops = vec![old, set, new];
        let view = ingest(&ops).view();
        assert_eq!(view.tuning, Some(definition));
        assert!(!view.pitches.contains(&tet_degree(7)));
        assert!(view.pitches.contains(&new_pitch));
        assert_parity(&ops);
    }

    #[test]
    fn piece_move_remove_unremove_by_owner() {
        let mut a = Peer::new(&SEED_A);
        let owner = a.author();
        let put = a.sign(
            1,
            vec![],
            PutPiece {
                emoji: "🎵".into(),
                pitch: tet_pitch(60),
            },
        );
        let piece = put.id();
        let mov = a.sign(
            2,
            vec![],
            MovePiece {
                piece,
                pitch: tet_pitch(72),
            },
        );
        let rem = a.sign(3, vec![], RemovePiece { piece });
        let unrem = a.sign(4, vec![], UnremovePiece { remove: rem.id() });
        let ops = vec![put, mov, rem, unrem];
        let view = ingest(&ops).view();
        let p = &view.pieces[&piece];
        assert_eq!(p.owner, owner);
        assert_eq!(p.emoji, "🎵");
        assert_eq!(
            p.pitch,
            tet_pitch(72),
            "the move wins the position register (it causally dominates the put); \
             the unremove overrides the remove -> alive"
        );
        assert_parity(&ops);
    }

    /// Shared pieces: a NON-owner's `MovePiece` takes effect. `owner` stays A as
    /// attribution only — never a gate. (This is the flip of the old
    /// `non_owner_piece_ops_are_ignored`.)
    #[test]
    fn non_owner_move_takes_effect() {
        let mut a = Peer::new(&SEED_A);
        let mut b = Peer::new(&SEED_B);
        let owner = a.author();
        let put = a.sign(
            1,
            vec![],
            PutPiece {
                emoji: "🌵".into(),
                pitch: tet_pitch(60),
            },
        );
        let piece = put.id();
        // B (not the owner) moves A's piece, observing the put.
        let b_mov = b.sign(
            2,
            vec![put.hash()],
            MovePiece {
                piece,
                pitch: tet_pitch(72),
            },
        );
        let ops = vec![put, b_mov];
        let view = ingest(&ops).view();
        let p = &view.pieces[&piece];
        assert_eq!(p.pitch, tet_pitch(72), "non-owner move takes effect");
        assert_eq!(
            p.owner, owner,
            "owner is attribution only, unchanged by B's move"
        );
        assert_parity(&ops);
    }

    /// W17 (shared-pieces update) — the semantics flipped: A creates a piece,
    /// non-owner B moves it, and BOTH peers ingest BOTH ops (in opposite orders).
    /// Every store — including A's — now converges on B's MOVED position, because
    /// any author may move a piece; `owner` is attribution only. The reversed
    /// order also exercises strict deferral of the move behind the put.
    #[test]
    fn w17_non_owner_move_converges_to_the_moved_position() {
        let mut a = Peer::new(&SEED_A);
        let mut b = Peer::new(&SEED_B);
        let owner = a.author();
        let put = a.sign(
            1,
            vec![],
            PutPiece {
                emoji: "🌵".into(),
                pitch: tet_pitch(60),
            },
        );
        let piece = put.id();
        // B is not the owner; it moves A's piece while observing the put.
        let b_mov = b.sign(
            2,
            vec![put.hash()],
            MovePiece {
                piece,
                pitch: tet_pitch(64),
            },
        );
        let ops = vec![put, b_mov];

        // Two independent peers ingest the same ops in opposite orders (the
        // reversed order also exercises strict deferral of the move).
        let store_a = ingest_in_order(&ops, &[0, 1]);
        let store_b = ingest_in_order(&ops, &[1, 0]);

        assert_eq!(store_a.view(), store_b.view(), "peers converge");
        assert_eq!(entryhash_set(&store_a), entryhash_set(&store_b));
        for (name, store) in [("A", &store_a), ("B", &store_b)] {
            let view = store.view();
            let held = &view.pieces[&piece];
            assert_eq!(
                held.pitch,
                tet_pitch(64),
                "{name} holds B's moved position, not the original"
            );
            assert_eq!(held.owner, owner, "{name} keeps A as attribution");
        }
        assert_parity(&ops);
    }

    /// Shared-pieces update: a non-owner's move is no longer inert — it produces a
    /// real view delta (the piece moves), so diff-driven projections update
    /// normally and there is nothing to "snap back". (Flip of the old
    /// `non_owner_move_produces_no_view_delta`.)
    #[test]
    fn non_owner_move_produces_a_view_delta() {
        // B's store ingests A's put, then commits B's own (non-owner) move.
        let mut a = Peer::new(&SEED_A);
        let put = a.sign(
            1,
            vec![],
            PutPiece {
                emoji: "🌵".into(),
                pitch: tet_pitch(60),
            },
        );
        let piece = put.id();

        let mut store = RoomStore::new();
        store.ingest_verified(put);
        let before = store.view();
        assert_eq!(before.pieces[&piece].pitch, tet_pitch(60));

        let b_key = signing_key_from_seed(&SEED_B);
        store.commit(
            &b_key,
            TOPIC,
            2,
            MovePiece {
                piece,
                pitch: tet_pitch(64),
            },
        );

        let after = store.view();
        assert_ne!(after, before, "a non-owner move now changes the view");
        assert_eq!(
            after.pieces[&piece].pitch,
            tet_pitch(64),
            "piece moved to B's target"
        );
    }

    /// Two DIFFERENT authors move the same piece concurrently (neither observed
    /// the other's move). Ingested in OPPOSITE orders on two stores, both compute
    /// the identical deterministic position — the cross-author position register's
    /// entry-hash tiebreak — which is one of the two proposed pitches.
    #[test]
    fn concurrent_moves_by_two_authors_converge_deterministically() {
        let mut a = Peer::new(&SEED_A);
        let mut b = Peer::new(&SEED_B);
        let put = a.sign(
            1,
            vec![],
            PutPiece {
                emoji: "🌵".into(),
                pitch: tet_pitch(60),
            },
        );
        let piece = put.id();
        // A and B each move the piece observing only the put -> mutually concurrent.
        let a_mov = a.sign(
            2,
            vec![put.hash()],
            MovePiece {
                piece,
                pitch: tet_pitch(67),
            },
        );
        let b_mov = b.sign(
            3,
            vec![put.hash()],
            MovePiece {
                piece,
                pitch: tet_pitch(65),
            },
        );
        let ops = vec![put, a_mov, b_mov];

        let store_fwd = ingest_in_order(&ops, &[0, 1, 2]);
        let store_rev = ingest_in_order(&ops, &[2, 1, 0]);

        assert_eq!(
            store_fwd.view(),
            store_rev.view(),
            "opposite ingest orders converge"
        );
        assert_eq!(entryhash_set(&store_fwd), entryhash_set(&store_rev));
        let pitch = store_fwd.view().pieces[&piece].pitch;
        assert!(
            pitch == tet_pitch(67) || pitch == tet_pitch(65),
            "position is one of the two concurrent moves, chosen deterministically"
        );
        assert_parity(&ops);
    }

    /// Author A creates a piece; a DIFFERENT author B removes it (observing the
    /// put). Both peers, in opposite ingest orders, converge to the piece being
    /// gone — shared removes are cross-author observed-removes.
    #[test]
    fn non_owner_remove_converges_removed() {
        let mut a = Peer::new(&SEED_A);
        let mut b = Peer::new(&SEED_B);
        let put = a.sign(
            1,
            vec![],
            PutPiece {
                emoji: "🌵".into(),
                pitch: tet_pitch(60),
            },
        );
        let piece = put.id();
        let b_rem = b.sign(2, vec![put.hash()], RemovePiece { piece });
        let ops = vec![put, b_rem];

        let store_fwd = ingest_in_order(&ops, &[0, 1]);
        let store_rev = ingest_in_order(&ops, &[1, 0]);

        assert_eq!(store_fwd.view(), store_rev.view(), "peers converge");
        assert_eq!(entryhash_set(&store_fwd), entryhash_set(&store_rev));
        for (name, store) in [("fwd", &store_fwd), ("rev", &store_rev)] {
            assert!(
                !store.view().pieces.contains_key(&piece),
                "{name}: a non-owner remove takes effect (piece gone)"
            );
        }
        assert_parity(&ops);
    }

    /// A move whose causal past is locked is suppressed: the piece stays at its
    /// original position on both peers. `pieces_locked` is the consent gate.
    #[test]
    fn move_under_pieces_locked_is_a_noop() {
        let mut a = Peer::new(&SEED_A);
        let mut b = Peer::new(&SEED_B);
        let put = a.sign(
            1,
            vec![],
            PutPiece {
                emoji: "🌵".into(),
                pitch: tet_pitch(60),
            },
        );
        let piece = put.id();
        // Lock pieces, observing the put.
        let lock = a.sign(
            2,
            vec![put.hash()],
            SetConfig {
                pieces_locked: Some(true),
                available_emojis: None,
            },
        );
        // B moves the piece AFTER observing the lock -> suppressed.
        let b_mov = b.sign(
            3,
            vec![lock.hash()],
            MovePiece {
                piece,
                pitch: tet_pitch(64),
            },
        );
        let ops = vec![put, lock, b_mov];

        let store_fwd = ingest_in_order(&ops, &[0, 1, 2]);
        let store_rev = ingest_in_order(&ops, &[2, 1, 0]);

        assert_eq!(store_fwd.view(), store_rev.view(), "peers converge");
        assert_eq!(entryhash_set(&store_fwd), entryhash_set(&store_rev));
        for (name, store) in [("fwd", &store_fwd), ("rev", &store_rev)] {
            let view = store.view();
            assert!(view.pieces_locked, "{name}: the room is locked");
            assert_eq!(
                view.pieces[&piece].pitch,
                tet_pitch(60),
                "{name}: a move whose past is locked is a no-op"
            );
        }
        assert_parity(&ops);
    }

    /// The lock is a CAUSAL gate, not a global freeze: a move CONCURRENT with the
    /// lock (neither observed the other) still applies — an op cannot be
    /// retroactively frozen by a lock it did not causally precede. The room still
    /// ends up locked; only the concurrent move slips through. Deterministic on
    /// both peers.
    #[test]
    fn move_concurrent_with_lock_still_applies() {
        let mut a = Peer::new(&SEED_A);
        let mut b = Peer::new(&SEED_B);
        let put = a.sign(
            1,
            vec![],
            PutPiece {
                emoji: "🌵".into(),
                pitch: tet_pitch(60),
            },
        );
        let piece = put.id();
        // A locks the room observing only the put.
        let lock = a.sign(
            2,
            vec![put.hash()],
            SetConfig {
                pieces_locked: Some(true),
                available_emojis: None,
            },
        );
        // B moves observing only the put -> concurrent with the lock.
        let b_mov = b.sign(
            3,
            vec![put.hash()],
            MovePiece {
                piece,
                pitch: tet_pitch(64),
            },
        );
        let ops = vec![put, lock, b_mov];

        let store_fwd = ingest_in_order(&ops, &[0, 1, 2]);
        let store_rev = ingest_in_order(&ops, &[2, 1, 0]);

        assert_eq!(store_fwd.view(), store_rev.view(), "peers converge");
        for (name, store) in [("fwd", &store_fwd), ("rev", &store_rev)] {
            let view = store.view();
            assert!(view.pieces_locked, "{name}: the room ends up locked");
            assert_eq!(
                view.pieces[&piece].pitch,
                tet_pitch(64),
                "{name}: a move concurrent with the lock still applies"
            );
        }
        assert_parity(&ops);
    }

    #[test]
    fn concurrent_set_tuning_uses_register_rule() {
        let mut a = Peer::new(&SEED_A);
        let mut b = Peer::new(&SEED_B);
        let a_tune = a.sign(
            1,
            vec![],
            SetTuning {
                definition: tuning_with_step(600),
            },
        );
        let b_tune = b.sign(
            2,
            vec![],
            SetTuning {
                definition: tuning_with_step(700),
            },
        );
        let ops = vec![a_tune, b_tune];
        let view = ingest(&ops).view();
        assert!(view.tuning.is_some(), "some concurrent writer wins");
        assert_parity(&ops);
    }

    #[test]
    fn set_config_fields_resolve_independently() {
        let mut a = Peer::new(&SEED_A);
        let mut b = Peer::new(&SEED_B);
        let locked = a.sign(
            1,
            vec![],
            SetConfig {
                pieces_locked: Some(true),
                available_emojis: None,
            },
        );
        let emojis = b.sign(
            2,
            vec![],
            SetConfig {
                pieces_locked: None,
                available_emojis: Some("🎹".into()),
            },
        );
        let ops = vec![locked, emojis];
        let view = ingest(&ops).view();
        assert!(view.pieces_locked, "locked field comes from A");
        assert_eq!(
            view.available_emojis.as_deref(),
            Some("🎹"),
            "emoji field comes from B"
        );
        assert_parity(&ops);
    }

    #[test]
    fn rich_history_matches_oracle_with_expected_values() {
        let ops = rich_history();
        let store = ingest(&ops);
        let view = store.view();

        assert_eq!(
            view.pitches,
            BTreeSet::from([tet_degree(0)]),
            "degree 7 is fully removed and degree 0 survives add-wins"
        );
        let b_author = Peer::new(&SEED_B).author();
        assert_eq!(
            view.pitch_authors[&tet_degree(0)],
            BTreeSet::from([b_author])
        );
        assert_eq!(view.pieces.len(), 1);
        let piece = view.pieces.values().next().unwrap();
        assert_eq!(piece.emoji, "🌵");
        assert_eq!(
            piece.pitch,
            tet_pitch(72),
            "later causal move wins under shared pieces (A observed B's move)"
        );
        assert!(view.tuning.is_some());
        assert!(view.pieces_locked);
        assert_eq!(view.available_emojis.as_deref(), Some("🌵🎵"));

        assert_eq!(view, oracle(&ops));
    }

    // ---------------------------------------------------------------------
    // Out-of-order convergence
    // ---------------------------------------------------------------------

    #[test]
    fn out_of_order_ingest_converges_and_is_deterministic() {
        let base = rich_history();
        let n = base.len();
        let expected = oracle(&base);

        let identity: Vec<usize> = (0..n).collect();
        let reversed: Vec<usize> = (0..n).rev().collect();
        let mut interleave: Vec<usize> = (0..n).step_by(2).collect();
        interleave.extend((1..n).step_by(2));

        let baseline = ingest_in_order(&base, &identity);
        let baseline_view = baseline.view();
        let baseline_hashes = entryhash_set(&baseline);
        assert_eq!(baseline_view, expected);
        assert_eq!(baseline.pending_len(), 0, "everything drains");

        for order in [identity.clone(), reversed, interleave] {
            let store = ingest_in_order(&base, &order);
            assert_eq!(
                store.pending_len(),
                0,
                "order {order:?} must fully drain"
            );
            assert_eq!(
                store.view(),
                baseline_view,
                "view differs for order {order:?}"
            );
            assert_eq!(
                entryhash_set(&store),
                baseline_hashes,
                "entry-hash identity must be order-independent for {order:?}"
            );
        }
    }

    // ---------------------------------------------------------------------
    // Golden vector: pins the framing/lift so a format change is caught.
    // ---------------------------------------------------------------------

    #[test]
    fn golden_vector_entry_hash_and_pitches() {
        let key = signing_key_from_seed(&SEED_A);
        let mut store = RoomStore::new();
        let signed0 = store.commit(
            &key,
            TOPIC,
            1_700_000_000_000_000,
            AddDegree {
                pitch: tet_degree(0),
            },
        );
        store.commit(
            &key,
            TOPIC,
            1_700_000_000_000_001,
            AddDegree {
                pitch: tet_degree(4),
            },
        );

        let id0 = verify_signed_op(&signed0).unwrap().id();
        let eh0 = store.lifted_entry(id0).expect("first op is lifted");
        assert_eq!(
            eh0.to_hex(),
            "9e217937915d7f0969a214c904ab6adb00da97c873d89407d82b7e5bf0bf3568",
            "golden entry hash for the first committed op",
        );
        assert_eq!(
            store.view().pitches,
            BTreeSet::from([tet_degree(0), tet_degree(4)])
        );
    }

    // ---------------------------------------------------------------------
    // Mutation test: prove the parity check has teeth.
    // ---------------------------------------------------------------------

    /// A DELIBERATELY WRONG oracle: pitches follow remove-wins (any remove kills
    /// the key), which disagrees with add-wins on a concurrent remove.
    fn wrong_oracle_remove_wins(ops: &[VerifiedOp]) -> RoomView {
        let mut view = oracle(ops);
        let mut adds: BTreeMap<TunedDegree, BTreeSet<AuthorId>> = BTreeMap::new();
        let mut has_remove: BTreeSet<TunedDegree> = BTreeSet::new();
        for op in ops {
            match op.payload() {
                AddDegree { pitch } => {
                    adds.entry(*pitch).or_default().insert(op.author());
                }
                RemoveDegree { pitch } => {
                    has_remove.insert(*pitch);
                }
                _ => {}
            }
        }
        view.pitches = BTreeSet::new();
        view.pitch_authors = BTreeMap::new();
        for (key, authors) in adds {
            if !has_remove.contains(&key) {
                view.pitches.insert(key);
                view.pitch_authors.insert(key, authors);
            }
        }
        view
    }

    #[test]
    fn parity_check_detects_a_broken_rule() {
        let (ops, _) = concurrent_add_remove();
        let real = ingest(&ops).view();

        // The correct oracle agrees with the real view (add-wins keeps the key).
        assert_eq!(real, oracle(&ops));
        assert!(real.pitches.contains(&tet_degree(5)));

        // The broken oracle drops the key -> it MUST disagree, proving the parity
        // assertion is not vacuously true.
        let wrong = wrong_oracle_remove_wins(&ops);
        assert!(!wrong.pitches.contains(&tet_degree(5)));
        assert_ne!(real, wrong, "a broken pitch rule must be caught by parity");
    }

    // ---------------------------------------------------------------------
    // M2 — the Merkle commitment / proof layer (ops_root / state_root).
    // ADDITIVE: RBSR, the `sync_root` convergence digest, and view() are all
    // untouched; these tests only add commitments beside them. `ops_root`/
    // `prove_op` are now generic tutti-core `Store<L>` methods; `state_root` is
    // the walkie `RoomStoreStateRoot` extension (in scope via `use super::*`).
    // ---------------------------------------------------------------------

    #[cfg(feature = "merkle")]
    fn hex32(bytes: &[u8; 32]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Golden vector: a fixed op-set pins concrete `ops_root` and `state_root`,
    /// and both are history-independent — the SAME roots regardless of ingest
    /// order (the whole point of a canonical Merkle commitment).
    #[cfg(feature = "merkle")]
    #[test]
    fn merkle_golden_roots_and_history_independence() {
        let base = rich_history();
        let n = base.len();

        let identity: Vec<usize> = (0..n).collect();
        let reversed: Vec<usize> = (0..n).rev().collect();
        let mut interleave: Vec<usize> = (0..n).step_by(2).collect();
        interleave.extend((1..n).step_by(2));

        let baseline = ingest_in_order(&base, &identity);
        let ops_root = baseline.ops_root();
        let state_root = baseline.state_root();

        // Pinned golden vectors (blake3-256, radix_immutable merkle v1 format).
        const GOLDEN_OPS_ROOT: &str =
            "0c06563b3c948882383459c249421e0f5b809af3d843d3f7f362fe527ea3ca01";
        const GOLDEN_STATE_ROOT: &str =
            "d4d3e5b6c1e3d407d0cc744101d2f0d06deb9a671ee4b7e3aac3ba6d8d2454b4";
        assert_eq!(hex32(&ops_root), GOLDEN_OPS_ROOT, "ops_root golden vector");
        assert_eq!(hex32(&state_root), GOLDEN_STATE_ROOT, "state_root golden vector");

        // History-independence: every ingest order yields the identical roots.
        for order in [reversed, interleave] {
            let store = ingest_in_order(&base, &order);
            assert_eq!(store.ops_root(), ops_root, "ops_root differs for {order:?}");
            assert_eq!(
                store.state_root(),
                state_root,
                "state_root differs for {order:?}"
            );
        }

        // The two roots are distinct digests, and ops_root is a DIFFERENT
        // (stronger) digest than the legacy sync_root over the same set.
        assert_ne!(ops_root, state_root);
        assert_ne!(ops_root, baseline.sync_root());
    }

    /// Inclusion-proof round-trip: prove every lifted op is in `ops_root`, verify
    /// each proof STANDALONE against the root, survive a wire round-trip, and
    /// reject a tampered root.
    #[cfg(feature = "merkle")]
    #[test]
    fn merkle_inclusion_proofs_verify_standalone() {
        use radix_immutable::{Proof, verify};

        let ops = rich_history();
        let store = ingest(&ops);
        let root = store.ops_root();

        for entry in store.entry_hashes() {
            let proof = store.prove_op(&entry);
            assert!(proof.is_inclusion(), "a present op proves inclusion");
            // Standalone verify: the `()` leaf value encodes to empty bytes.
            assert!(
                verify(&root, entry.as_bytes(), Some(&[]), &proof),
                "inclusion proof verifies against the root"
            );
            // Wire round-trip, then re-verify the decoded proof.
            let bytes = proof.to_bytes();
            let decoded = Proof::from_bytes(&bytes).expect("proof decodes");
            assert_eq!(decoded, proof, "proof survives a serialize round-trip");
            assert!(verify(&root, entry.as_bytes(), Some(&[]), &decoded));
            // A tampered root must reject.
            let mut bad = root;
            bad[0] ^= 1;
            assert!(!verify(&bad, entry.as_bytes(), Some(&[]), &proof));
        }
    }

    /// Non-inclusion: an op ABSENT from the store proves exclusion against its
    /// `ops_root`, verified standalone; the same hash proves inclusion in a store
    /// that DOES hold it — one key, opposite verdicts.
    #[cfg(feature = "merkle")]
    #[test]
    fn merkle_non_inclusion_proof_for_absent_op() {
        use radix_immutable::verify;

        let mut a = Peer::new(&SEED_A);
        let op0 = a.sign(
            1,
            vec![],
            AddDegree {
                pitch: tet_degree(0),
            },
        );
        let op1 = a.sign(
            2,
            vec![],
            AddDegree {
                pitch: tet_degree(4),
            },
        );

        // store_all holds both ops; store_missing holds only op0.
        let store_all = ingest(&[op0.clone(), op1.clone()]);
        let store_missing = ingest(&[op0.clone()]);

        // The entry hash op1 lifts to (learned from the full store).
        let missing = store_all
            .lifted_entry(op1.id())
            .expect("op1 is lifted in store_all");

        // Absent from store_missing -> non-inclusion, verifies with value=None.
        let np = store_missing.prove_op(&missing);
        assert!(!np.is_inclusion(), "an absent op proves non-inclusion");
        let missing_root = store_missing.ops_root();
        assert!(
            verify(&missing_root, missing.as_bytes(), None, &np),
            "non-inclusion proof verifies against the root"
        );
        // Demanding inclusion of an absent key must fail.
        assert!(!verify(&missing_root, missing.as_bytes(), Some(&[]), &np));

        // The SAME hash proves inclusion in the store that holds it.
        let all_root = store_all.ops_root();
        let ip = store_all.prove_op(&missing);
        assert!(ip.is_inclusion());
        assert!(verify(&all_root, missing.as_bytes(), Some(&[]), &ip));
    }

    /// The Merkle layer is purely additive: querying the roots leaves the view,
    /// the entry-hash set, and the legacy `sync_root` unchanged, and `ops_root`
    /// covers the SAME set as `sync_root` (both order-independent, no skew).
    #[cfg(feature = "merkle")]
    #[test]
    fn merkle_roots_do_not_perturb_existing_behavior() {
        let ops = rich_history();
        let store = ingest(&ops);

        let view_before = store.view();
        let hashes_before = store.entry_hashes();
        let sync_before = store.sync_root();

        let _ = store.ops_root();
        let _ = store.state_root();
        let _ = store.prove_op(store.entry_hashes().iter().next().unwrap());

        assert_eq!(store.view(), view_before, "view unchanged by merkle queries");
        assert_eq!(store.entry_hashes(), hashes_before, "entry set unchanged");
        assert_eq!(store.sync_root(), sync_before, "sync_root unchanged");

        // Same input set on any ingest order -> same ops_root AND same sync_root
        // (the "one entry set, one capture" invariant: no skew between the two).
        let reversed: Vec<usize> = (0..ops.len()).rev().collect();
        let store_rev = ingest_in_order(&ops, &reversed);
        assert_eq!(store_rev.ops_root(), store.ops_root());
        assert_eq!(store_rev.sync_root(), store.sync_root());
    }
}

// =====================================================================
// Correctness gate for the cheap `Reach` ancestry backend.
//
// The whole optimization rests on ONE claim: the lazy `Reach::is_ancestor`
// (and the register `resolve` derived from it) answers IDENTICALLY to the
// kernel `hhhs::cover::ReachIndex` it replaced, for every DAG. These
// tests hammer that claim over thousands of seeded-random causal histories:
//
//   1. `reach_is_ancestor_matches_kernel_oracle` — for ALL (a, b) pairs.
//   2. `resolve_matches_kernel_register`          — over random candidate sets.
//   3. `view_equals_reference_and_oracle`         — the whole projection, i.e.
//      `view()` (cheap `Reach`) == `view_reference()` (kernel `ReachIndex` +
//      real `register::resolve`) == the INDEPENDENT op-graph `oracle`.
//
// `Reach`/`CausalPast`/`Store::view_reference`/`Store::dag` now live in
// tutti-core; the reference surface is enabled here through the `test-support`
// dev-dependency. The generator, oracle, and `WalkieLang` projection stay in
// walkie — this is a walkie-domain gate over the substrate's ancestry backend.
// =====================================================================
#[cfg(test)]
mod reach_equiv {
    use std::collections::BTreeSet;

    use hhhs::cover::ReachIndex;
    use hhhs::{DagRead, EntryHash, register};

    use super::super::ops::{OpId, VerifiedOp, WalkieOp};
    use super::super::test_support::{
        Peer, oracle, tet_definition, tet_degree, tet_pitch, tuning_with_step,
    };
    use super::{CausalPast, Reach, RoomStore};

    /// A tiny deterministic splitmix64 PRNG, so every case is reproducible from
    /// its seed and the whole suite is byte-stable across runs.
    struct Rng(u64);
    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03)
        }
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn upto(&mut self, n: usize) -> usize {
            if n == 0 { 0 } else { (self.next() % n as u64) as usize }
        }
        fn pct(&mut self, p: u64) -> bool {
            self.next() % 100 < p
        }
    }

    fn pick(ids: &[OpId], rng: &mut Rng) -> Option<OpId> {
        if ids.is_empty() {
            None
        } else {
            Some(ids[rng.upto(ids.len())])
        }
    }

    /// A seeded-random causal history over `authors` peers and `steps` ops.
    ///
    /// Each op's `observed` horizon is a random subset of PRIOR op hashes, so
    /// concurrency and forks arise naturally; piece/remove references target
    /// already-created ids so lifecycles are exercised. Emitted in a valid causal
    /// order (every reference precedes its use), so ingest never parks.
    fn random_history(seed: u64, authors: usize, steps: usize) -> Vec<VerifiedOp> {
        let mut rng = Rng::new(seed);
        let mut peers: Vec<Peer> = (0..authors)
            .map(|i| Peer::new(&[(i + 1) as u8; 32]))
            .collect();
        let emojis = ["🌵", "🎵", "🎹", "🎸"];

        let mut out: Vec<VerifiedOp> = Vec::with_capacity(steps);
        let mut op_hashes: Vec<[u8; 32]> = Vec::new();
        let mut piece_ids: Vec<OpId> = Vec::new();
        let mut remove_ids: Vec<OpId> = Vec::new();

        for step in 0..steps {
            let author = rng.upto(authors);

            // Random observed horizon from prior ops (fork/concurrency source).
            let mut observed: Vec<[u8; 32]> = Vec::new();
            for h in &op_hashes {
                if observed.len() >= 4 {
                    break;
                }
                if rng.pct(28) {
                    observed.push(*h);
                }
            }

            // Degrees are drawn from a SMALL keyspace so add/remove races land on
            // the same key (the add-wins path that runs `is_ancestor`).
            let op = match rng.upto(10) {
                0 | 1 => WalkieOp::AddDegree {
                    pitch: tet_degree(rng.upto(3) as u16),
                },
                2 => WalkieOp::RemoveDegree {
                    pitch: tet_degree(rng.upto(3) as u16),
                },
                3 => WalkieOp::PutPiece {
                    emoji: emojis[rng.upto(emojis.len())].into(),
                    pitch: tet_pitch(60 + rng.upto(5) as i32),
                },
                4 => match pick(&piece_ids, &mut rng) {
                    Some(piece) => WalkieOp::MovePiece {
                        piece,
                        pitch: tet_pitch(60 + rng.upto(7) as i32),
                    },
                    None => WalkieOp::AddDegree { pitch: tet_degree(0) },
                },
                5 => match pick(&piece_ids, &mut rng) {
                    Some(piece) => WalkieOp::RemovePiece { piece },
                    None => WalkieOp::AddDegree { pitch: tet_degree(1) },
                },
                6 => match pick(&remove_ids, &mut rng) {
                    Some(remove) => WalkieOp::UnremovePiece { remove },
                    None => WalkieOp::AddDegree { pitch: tet_degree(2) },
                },
                7 => WalkieOp::SetConfig {
                    pieces_locked: Some(rng.pct(50)),
                    available_emojis: None,
                },
                8 => WalkieOp::SetConfig {
                    pieces_locked: None,
                    available_emojis: Some(emojis[rng.upto(emojis.len())].into()),
                },
                _ => WalkieOp::SetTuning {
                    definition: if rng.pct(70) {
                        tet_definition()
                    } else {
                        tuning_with_step(500 + 100 * rng.upto(3) as u16)
                    },
                },
            };

            let signed = peers[author].sign(1_000 + step as u64, observed, op.clone());
            match &op {
                WalkieOp::PutPiece { .. } => piece_ids.push(signed.id()),
                WalkieOp::RemovePiece { .. } => remove_ids.push(signed.id()),
                _ => {}
            }
            op_hashes.push(signed.hash());
            out.push(signed);
        }
        out
    }

    fn store_of(ops: &[VerifiedOp]) -> RoomStore {
        let mut store = RoomStore::new();
        for op in ops {
            store.ingest_verified(op.clone());
        }
        store
    }

    /// (1) `Reach::is_ancestor` == `ReachIndex::is_ancestor` for EVERY pair, over
    /// thousands of random DAGs plus a batch of deep (N≈80) ones.
    #[test]
    fn reach_is_ancestor_matches_kernel_oracle() {
        let mut cases = 0usize;
        for seed in 0..1500u64 {
            let authors = 2 + (seed as usize % 3);
            let steps = 5 + (seed as usize % 10);
            check_pairs(seed, authors, steps, &mut cases);
        }
        // Deep chains: exercise long ancestor walks and the memo.
        for seed in 0..30u64 {
            check_pairs(seed ^ 0xDEED_BEEF, 2, 60, &mut cases);
        }
        assert!(cases > 100_000, "expected a large pair count, got {cases}");
    }

    fn check_pairs(seed: u64, authors: usize, steps: usize, cases: &mut usize) {
        let ops = random_history(seed, authors, steps);
        let store = store_of(&ops);
        assert_eq!(store.pending_len(), 0, "seed {seed}: all ops lift");
        let reach = Reach::new(store.dag());
        let kernel = ReachIndex::new(&store.dag().snapshot());
        let hashes: Vec<EntryHash> = store.entry_hashes().into_iter().collect();
        for a in &hashes {
            for b in &hashes {
                assert_eq!(
                    CausalPast::is_ancestor(&reach, a, b),
                    ReachIndex::is_ancestor(&kernel, a, b),
                    "seed {seed}: is_ancestor({}, {}) disagreement",
                    a.to_hex(),
                    b.to_hex()
                );
                *cases += 1;
            }
        }
    }

    /// (2) The `Reach`-derived register `resolve` == the kernel
    /// `register::resolve`, over random candidate subsets of each DAG.
    #[test]
    fn resolve_matches_kernel_register() {
        for seed in 0..1500u64 {
            let ops = random_history(
                seed ^ 0xA5A5_A5A5,
                2 + (seed as usize % 3),
                5 + (seed as usize % 10),
            );
            let store = store_of(&ops);
            let reach = Reach::new(store.dag());
            let kernel = ReachIndex::new(&store.dag().snapshot());
            let hashes: Vec<EntryHash> = store.entry_hashes().into_iter().collect();
            let mut rng = Rng::new(seed ^ 0x1357_9BDF);
            for _ in 0..8 {
                let candidates: BTreeSet<EntryHash> = hashes
                    .iter()
                    .copied()
                    .filter(|_| rng.pct(35))
                    .collect();
                assert_eq!(
                    CausalPast::resolve(&reach, &candidates),
                    register::resolve(&candidates, &kernel),
                    "seed {seed}: register resolve disagreement",
                );
            }
        }
    }

    /// (3) The whole projection: `view()` (cheap `Reach`) is bit-for-bit the
    /// `view_reference()` (kernel `ReachIndex` + real `register::resolve`) AND the
    /// INDEPENDENT op-graph `oracle`, over thousands of random histories spanning
    /// every op kind with concurrent forks.
    #[test]
    fn view_equals_reference_and_oracle() {
        for seed in 0..2000u64 {
            let authors = 2 + (seed as usize % 3);
            let steps = 6 + (seed as usize % 12);
            let ops = random_history(seed, authors, steps);
            let store = store_of(&ops);
            assert_eq!(store.pending_len(), 0, "seed {seed}: all ops lift");

            let produced = store.view();
            assert_eq!(
                produced,
                store.view_reference(),
                "seed {seed}: view() (Reach) != view_reference() (kernel ReachIndex)"
            );
            assert_eq!(
                produced,
                oracle(&ops),
                "seed {seed}: view() != independent op-graph oracle"
            );
        }
    }
}
