//! Additive Merkle commitment / proof layer for [`RoomStore`] (feature `merkle`).
//!
//! Two roots stand **beside** the anti-entropy machinery, never replacing it:
//!
//! * [`ops_root_of`] — a canonical [`radix_immutable`] Merkle trie keyed by the
//!   **entry hash** of every lifted op. Its input is exactly
//!   `RoomStore::entry_to_source.keys()` — the *same* identity set
//!   [`sync_root_of`](super::store::sync_root_of) digests — so the two
//!   commitments can never skew (the "one entry set, one capture" invariant of
//!   `docs/research/reconciliation-tree-fit.md` risk 3). Root equality iff
//!   entry-set equality, **plus** O(log n) inclusion / non-inclusion proofs
//!   ([`RoomStore::prove_op`](super::store::RoomStore::prove_op) +
//!   standalone [`radix_immutable::verify`]).
//! * [`state_root_of`] — a canonical Merkle over the projected
//!   [`RoomView`](super::store::RoomView), a pure function of the view's fields.
//!   Leaf grammar documented on [`state_trie`].
//!
//! ## What this layer does NOT touch
//!
//! RBSR (`src/net/sync.rs`), the gossip path, the per-session salted-XOR range
//! fingerprints, and `sync_root`/`sync_root_of` are all **unchanged**. `ops_root`
//! is a strictly stronger digest than `sync_root` (adds proofs), so `sync_root`
//! is *superseded for proofs* — but it remains the value the RBSR session
//! cross-checks on `Done`, and is not removed here. See the deprecation note on
//! [`RoomStore::sync_root`](super::store::RoomStore::sync_root).
//!
//! ## Recompute, not incremental
//!
//! Both roots are recomputed on demand from the store's existing maps rather than
//! maintained in a persistent trie field. At room scale this is microseconds
//! (immutable-trie inserts are ~200 ns each), and — decisively — recomputing
//! `ops_root` from the same `entry_to_source.keys()` iterator that `sync_root_of`
//! consumes makes skew between the two commitments *structurally impossible*.
//! Should score-sized logs ever make this profile hot, a persistent
//! structurally-shared trie updated on lift is a drop-in (the crate is immutable
//! and built for exactly that), with no change to these roots' byte format.

use hhhs_core::EntryHash;
use radix_immutable::{BytesKeyConverter, Proof, Trie};

use super::store::RoomView;
use crate::tuning::{TunedDegree, TunedPeriodicPitch};

/// The `ops_root` trie: key = 32-byte entry hash, value = `()` (presence only).
///
/// Presence-only leaves (`ValueToBytes for () == b""`) suffice because the key
/// *is* the op's cryptographic identity; the value carries no extra information.
pub type OpsTrie = Trie<[u8; 32], (), BytesKeyConverter<[u8; 32]>>;

/// The `state_root` trie: byte keys (section-tagged, see [`state_trie`]) → byte
/// values (canonical field encodings).
pub type StateTrie = Trie<Vec<u8>, Vec<u8>, BytesKeyConverter<Vec<u8>>>;

/// Build the `ops_root` trie from an entry-hash identity set.
///
/// Feed it `entry_to_source.keys()` so the committed set is byte-for-byte the set
/// `sync_root_of` digests. Insertion order is irrelevant — the trie shape (and
/// thus the root) is a pure function of the final key set.
pub fn ops_trie<'a>(hashes: impl IntoIterator<Item = &'a EntryHash>) -> OpsTrie {
    let mut trie = OpsTrie::new_bytes_key();
    for hash in hashes {
        trie = trie.insert(*hash.as_bytes(), ());
    }
    trie
}

/// The `ops_root`: a canonical blake3-256 Merkle commitment to the entry-hash set.
pub fn ops_root_of<'a>(hashes: impl IntoIterator<Item = &'a EntryHash>) -> [u8; 32] {
    ops_trie(hashes).merkle_root()
}

/// An inclusion / non-inclusion proof for `entry` against `ops_root_of(hashes)`.
///
/// Verify standalone (no store) with
/// `radix_immutable::verify(&root, entry.as_bytes(), Some(&[]), &proof)` for an
/// inclusion (the `()` value encodes to empty bytes), or
/// `radix_immutable::verify(&root, entry.as_bytes(), None, &proof)` for a
/// non-inclusion.
pub fn prove_op<'a>(
    hashes: impl IntoIterator<Item = &'a EntryHash>,
    entry: &EntryHash,
) -> Proof {
    ops_trie(hashes).prove(entry.as_bytes())
}

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
/// its sorted `(key, value)` leaves, so the root is a pure, deterministic
/// function of the [`RoomView`] alone.
///
/// Leaf grammar (`key -> value`, all integers little-endian, lengths fixed-width
/// except the trailing variable-length UTF-8 fields):
///
/// * config — `[SEC_CONFIG, b'T'] -> tuning_id(32)` (omitted when `tuning` is
///   `None`); `[SEC_CONFIG, b'L'] -> [pieces_locked as u8]`;
///   `[SEC_CONFIG, b'E'] -> available_emojis (UTF-8)` (omitted when `None`).
///   Committing the 32-byte `TuningId` binds the full tuning: the id *is*
///   blake3 of the canonical Scala/KBM bytes.
/// * pitches — `[SEC_PITCHES] ‖ tuned_degree_bytes(d) -> concat of the sorted
///   32-byte holder AuthorIds`. The key set is exactly `view.pitches` (an
///   invariant of `view()`: `pitches == pitch_authors.keys()`), so encoding
///   `pitch_authors` captures both fields.
/// * pieces — `[SEC_PIECES] ‖ piece_id(32) -> owner(32) ‖ tuned_pitch_bytes(38)
///   ‖ emoji (UTF-8, trailing)`. Fixed-width prefix + trailing emoji is
///   injective in `(owner, pitch, emoji)`.
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
