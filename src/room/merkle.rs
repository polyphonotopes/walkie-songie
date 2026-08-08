//! Walkie's `state_root` — a canonical Merkle commitment to the projected
//! [`RoomView`] (feature `merkle`).
//!
//! [`state_root_of`] is a canonical Merkle over the projected `RoomView`, a pure
//! function of the view's fields. Leaf grammar documented on [`state_trie`].
//!
//! The domain-agnostic `ops_root`/`prove_op` (over the entry-hash identity set)
//! moved to [`tutti_core`] (tutti extraction Track-D step 3) — they name no
//! domain, so they are generic `Store<L>` methods there. `state_root` stays here
//! because it folds `RoomView`: hoisting it onto `Store<L>` needs an
//! `L::View: Canonical` bound that is not wired yet, so it is deferred with the
//! rest of tutti-core's Merkle work.
//!
//! ## What this layer does NOT touch
//!
//! RBSR, the gossip path, the salted-XOR range fingerprints, and `sync_root` are
//! all unchanged; the roots stand BESIDE the anti-entropy machinery.
//!
//! ## Recompute, not incremental
//!
//! The root is recomputed on demand from the view rather than maintained in a
//! persistent trie field. At room scale this is microseconds; should score-sized
//! logs make it hot, a persistent structurally-shared trie is a drop-in with no
//! change to the root's byte format.

use radix_immutable::{BytesKeyConverter, Trie};

use super::store::RoomView;
use crate::tuning::{TunedDegree, TunedPeriodicPitch};

/// The `state_root` trie: byte keys (section-tagged, see [`state_trie`]) → byte
/// values (canonical field encodings).
pub type StateTrie = Trie<Vec<u8>, Vec<u8>, BytesKeyConverter<Vec<u8>>>;

// --- state_root canonical view encoding --------------------------------------

/// Section tag: room config (tuning identity, lock, emoji palette).
const SEC_CONFIG: u8 = 0x01;
/// Section tag: live degree keys with their holder authors.
const SEC_PITCHES: u8 = 0x02;
/// Section tag: live pieces.
const SEC_PIECES: u8 = 0x03;

/// Canonical bytes for a [`TunedDegree`]: `tuning_id(32) ‖ degree(u16 le)`.
fn tuned_degree_bytes(degree: &TunedDegree) -> Vec<u8> {
    let mut out = Vec::with_capacity(34);
    out.extend_from_slice(degree.tuning_id.as_bytes());
    out.extend_from_slice(&degree.degree.index().to_le_bytes());
    out
}

/// Canonical bytes for a [`TunedPeriodicPitch`]:
/// `tuning_id(32) ‖ degree(u16 le) ‖ period(i32 le)`.
fn tuned_pitch_bytes(pitch: &TunedPeriodicPitch) -> Vec<u8> {
    let mut out = Vec::with_capacity(38);
    out.extend_from_slice(pitch.tuning_id.as_bytes());
    out.extend_from_slice(&pitch.pitch.degree().index().to_le_bytes());
    out.extend_from_slice(&pitch.pitch.period().to_le_bytes());
    out
}

/// Build the `state_root` trie: one leaf per view fact, section-tagged so leaves
/// from different facets share no key space. The trie is a canonical Merkle over
/// its sorted `(key, value)` leaves, so the root is a pure, deterministic function
/// of the [`RoomView`] alone.
///
/// Leaf grammar (`key -> value`, all integers little-endian, lengths fixed-width
/// except the trailing variable-length UTF-8 fields):
///
/// * config — `[SEC_CONFIG, b'T'] -> tuning_id(32)` (omitted when `tuning` is
///   `None`); `[SEC_CONFIG, b'L'] -> [pieces_locked as u8]`;
///   `[SEC_CONFIG, b'E'] -> available_emojis (UTF-8)` (omitted when `None`).
///   Committing the 32-byte `TuningId` binds the full tuning: the id *is* blake3
///   of the canonical Scala/KBM bytes.
/// * pitches — `[SEC_PITCHES] ‖ tuned_degree_bytes(d) -> concat of the sorted
///   32-byte holder AuthorIds`. The key set is exactly `view.pitches` (an
///   invariant of `view()`: `pitches == pitch_authors.keys()`), so encoding
///   `pitch_authors` captures both fields.
/// * pieces — `[SEC_PIECES] ‖ piece_id(32) -> owner(32) ‖ tuned_pitch_bytes(38)
///   ‖ emoji (UTF-8, trailing)`. Fixed-width prefix + trailing emoji is injective
///   in `(owner, pitch, emoji)`.
pub fn state_trie(view: &RoomView) -> StateTrie {
    let mut trie = StateTrie::new_bytes_key();

    // --- config ---
    if let Some(tuning) = &view.tuning {
        trie = trie.insert(vec![SEC_CONFIG, b'T'], tuning.id.as_bytes().to_vec());
    }
    trie = trie.insert(vec![SEC_CONFIG, b'L'], vec![u8::from(view.pieces_locked)]);
    if let Some(emojis) = &view.available_emojis {
        trie = trie.insert(vec![SEC_CONFIG, b'E'], emojis.as_bytes().to_vec());
    }

    // --- pitches (keys are the live degrees; value = sorted holder authors) ---
    for (degree, authors) in &view.pitch_authors {
        let mut key = Vec::with_capacity(1 + 34);
        key.push(SEC_PITCHES);
        key.extend_from_slice(&tuned_degree_bytes(degree));
        let mut val = Vec::with_capacity(authors.len() * 32);
        for author in authors {
            val.extend_from_slice(&author.0);
        }
        trie = trie.insert(key, val);
    }

    // --- pieces ---
    for (id, piece) in &view.pieces {
        let mut key = Vec::with_capacity(1 + 32);
        key.push(SEC_PIECES);
        key.extend_from_slice(&id.0);
        let mut val = Vec::with_capacity(32 + 38 + piece.emoji.len());
        val.extend_from_slice(&piece.owner.0);
        val.extend_from_slice(&tuned_pitch_bytes(&piece.pitch));
        val.extend_from_slice(piece.emoji.as_bytes());
        trie = trie.insert(key, val);
    }

    trie
}

/// The `state_root`: a canonical blake3-256 Merkle commitment to the projected
/// [`RoomView`]. Pure function of the view (see [`state_trie`] for the grammar).
pub fn state_root_of(view: &RoomView) -> [u8; 32] {
    state_trie(view).merkle_root()
}
