//! Deep-laggard courier proof exchange (M3.2 §4.5) — the wire half of
//! [`WindowedStore::lift_pending_via_courier`].
//!
//! A windowed leaf that compacted away an op keeps only a bounded cache of
//! discarded-op reach rows. When a deep laggard arrives referencing an op whose
//! row has been evicted, the leaf **defers** it ([`DeferredLift`]) rather than
//! admit a silently wrong row. This module carries the §4.5 answer over the
//! wire: the requester sends its pinned discard root plus its sorted retained
//! set, and a fuller peer answers with a real [`DiscardProof`] (reconstructed
//! from a [`TrackedDiscardHistory`] copy of the leaf's own discard journal) plus
//! an ancestor mask over the requester's retained array (answered from the full
//! store's real reach oracle).
//!
//! **Dedicated lane ALPN.** Courier frames ride
//! [`LaneSpec::COURIER_ALPN`](super::sync::LaneSpec::COURIER_ALPN), never the
//! RBSR ALPN: the RBSR pump decodes every inbound frame as a `SyncMessage`, so
//! multiplexing would be a kernel wire/state-machine change and a generation
//! bump. One request/response per stream. As with RBSR, the authenticated ALPN
//! IS the lane tag — no lane discriminator rides inside a frame.
//!
//! **The §4.5 trust ledger, exactly:**
//!
//! ```text
//! PROVEN:  the missing EntryHash belongs to the requester's own discard chain
//!          (the floor verifies the DiscardProof against the pinned root).
//! TRUSTED: the returned ancestor mask belongs to that EntryHash.
//! ```
//!
//! A valid proof with a dishonest mask is accepted by design (the floor
//! restricts it to currently retained hashes and nothing more). A bad proof,
//! root mismatch, stale context, or refusal leaves the op **pending** — never a
//! session error, never a wrong row (§4.5 defer-never-reject).

use std::collections::{BTreeMap, BTreeSet};

use hhhs_sync::EntryHash;
use tutti_core::{
    Courier, CourierAnswer, CourierContext, DeferredLift, DeferredLiftError, Digest, DiscardProof,
    OpLanguage, Reach as _, Store, WindowedStore,
};

use super::SyncStream;
use super::sync::SyncError;

// ---------------------------------------------------------------------------
// Size bounds. Enforced at decode BEFORE any allocation, and re-checked by the
// requester/responder so an oversized answer is refused rather than sent.
// ---------------------------------------------------------------------------

/// Cap on the retained hashes a request may carry (the mask width).
pub const MAX_COURIER_CONTEXT_ENTRIES: usize = 1_024;
/// Cap on `later_batches` in one answer — the chain depth one wire generation
/// serves.
pub const MAX_COURIER_LATER_BATCHES: usize = 1_024;
/// Cap on Merkle siblings in one answer (log2 of the batch size; 32 covers
/// batches beyond any journal cap this store will ever hold).
pub const MAX_COURIER_SIBLINGS: usize = 32;
/// Hard cap on one encoded courier frame.
pub const MAX_COURIER_FRAME_BYTES: usize = 64 * 1024;

// A maximal legal frame must actually fit the frame cap, or the bounds above
// are unsatisfiable and honest exchanges fail at runtime.
const _: () = assert!(
    1 + 32 + 32 + 4 + MAX_COURIER_CONTEXT_ENTRIES * 32 <= MAX_COURIER_FRAME_BYTES,
    "a maximal courier request must fit the frame cap"
);
const _: () = assert!(
    1 + 32
        + 32
        + 1
        + (4 + MAX_COURIER_SIBLINGS * 33)
        + 32
        + (4 + MAX_COURIER_LATER_BATCHES * 32)
        + (4 + MAX_COURIER_CONTEXT_ENTRIES.div_ceil(8))
        <= MAX_COURIER_FRAME_BYTES,
    "a maximal courier answer must fit the frame cap"
);

// ---------------------------------------------------------------------------
// Frames.
// ---------------------------------------------------------------------------

/// One courier wire frame. The ALPN scopes the protocol; the leading tag byte
/// distinguishes the two directions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CourierFrame {
    Request(CourierRequest),
    Response(CourierResponse),
}

/// The requester's question: ONE missing (discarded, reach-evicted) predecessor,
/// plus the exact context the answer must be produced against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CourierRequest {
    pub missing_prev: EntryHash,
    pub context: CourierContextWire,
}

/// The requester's horizon, on the wire: its pinned discard root and its sorted
/// retained set. The response's ancestor mask indexes `retained` positionally,
/// so the array must be canonical (strictly ascending, unique) and bounded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CourierContextWire {
    pub discard_root: [u8; 32],
    /// Sorted, unique. At most [`MAX_COURIER_CONTEXT_ENTRIES`].
    pub retained: Vec<EntryHash>,
}

/// The responder's verdict. `missing_prev` and `discard_root` echo the request,
/// so a response cannot be applied to another request or another context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CourierResponse {
    pub missing_prev: EntryHash,
    pub discard_root: [u8; 32],
    pub result: Result<CourierWireAnswer, CourierRefusal>,
}

/// The serialized §4.5 answer: the [`DiscardProof`] parts (verified half) plus
/// the ancestor mask over the request's retained array (trusted half).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CourierWireAnswer {
    /// Bottom-up Merkle siblings; `true` = sibling is on the left.
    pub siblings: Vec<([u8; 32], bool)>,
    pub pinned_before: [u8; 32],
    /// Batch roots folded after the member's batch, oldest first.
    pub later_batches: Vec<[u8; 32]>,
    /// Bit `i` set means `request.context.retained[i]` is a strict ancestor of
    /// `missing_prev`. Exactly `ceil(retained.len() / 8)` bytes; unused high
    /// bits must be zero.
    pub ancestor_mask: Vec<u8>,
}

/// Why the responder could not answer. The op stays deferred at the requester —
/// a refusal is never an error, exactly like an unanswering courier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CourierRefusal {
    /// The requested root is not the chain this responder tracks.
    UnknownRoot,
    /// The member's batch is not (or no longer) reconstructable here.
    HistoryEvicted,
    /// The responder's full store does not hold the entry, so it cannot answer
    /// the ancestor half.
    MissingEntry,
    /// The request's retained array exceeds this generation's bound.
    ContextTooLarge,
}

/// Why a courier frame failed to encode or decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CourierWireError(pub &'static str);

impl core::fmt::Display for CourierWireError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "courier wire: {}", self.0)
    }
}

impl std::error::Error for CourierWireError {}

impl From<CourierWireError> for SyncError {
    fn from(value: CourierWireError) -> Self {
        SyncError::Decode(value.to_string())
    }
}

// ---------------------------------------------------------------------------
// Codec. Fixed little-endian layout; strict — counts are bounds-checked before
// any allocation, the retained array must be strictly ascending, side bytes
// must be 0/1, refusal codes must be known, and trailing bytes are an error.
// ---------------------------------------------------------------------------

const FRAME_REQUEST: u8 = 1;
const FRAME_RESPONSE: u8 = 2;
const RESULT_ANSWER: u8 = 1;
const RESULT_REFUSAL: u8 = 2;

impl CourierFrame {
    pub fn encode(&self) -> Result<Vec<u8>, CourierWireError> {
        let mut out: Vec<u8> = Vec::new();
        match self {
            CourierFrame::Request(request) => {
                let retained = &request.context.retained;
                if retained.len() > MAX_COURIER_CONTEXT_ENTRIES {
                    return Err(CourierWireError("request retained set over cap"));
                }
                if !strictly_ascending(retained) {
                    return Err(CourierWireError("request retained set not sorted/unique"));
                }
                out.push(FRAME_REQUEST);
                out.extend_from_slice(request.missing_prev.as_bytes());
                out.extend_from_slice(&request.context.discard_root);
                out.extend_from_slice(&(retained.len() as u32).to_le_bytes());
                for hash in retained {
                    out.extend_from_slice(hash.as_bytes());
                }
            }
            CourierFrame::Response(response) => {
                out.push(FRAME_RESPONSE);
                out.extend_from_slice(response.missing_prev.as_bytes());
                out.extend_from_slice(&response.discard_root);
                match &response.result {
                    Ok(answer) => {
                        if answer.siblings.len() > MAX_COURIER_SIBLINGS {
                            return Err(CourierWireError("answer siblings over cap"));
                        }
                        if answer.later_batches.len() > MAX_COURIER_LATER_BATCHES {
                            return Err(CourierWireError("answer later_batches over cap"));
                        }
                        if answer.ancestor_mask.len() > MAX_COURIER_CONTEXT_ENTRIES.div_ceil(8) {
                            return Err(CourierWireError("answer mask over cap"));
                        }
                        out.push(RESULT_ANSWER);
                        out.extend_from_slice(&(answer.siblings.len() as u32).to_le_bytes());
                        for (digest, left) in &answer.siblings {
                            out.extend_from_slice(digest);
                            out.push(u8::from(*left));
                        }
                        out.extend_from_slice(&answer.pinned_before);
                        out.extend_from_slice(&(answer.later_batches.len() as u32).to_le_bytes());
                        for root in &answer.later_batches {
                            out.extend_from_slice(root);
                        }
                        out.extend_from_slice(&(answer.ancestor_mask.len() as u32).to_le_bytes());
                        out.extend_from_slice(&answer.ancestor_mask);
                    }
                    Err(refusal) => {
                        out.push(RESULT_REFUSAL);
                        out.push(match refusal {
                            CourierRefusal::UnknownRoot => 1,
                            CourierRefusal::HistoryEvicted => 2,
                            CourierRefusal::MissingEntry => 3,
                            CourierRefusal::ContextTooLarge => 4,
                        });
                    }
                }
            }
        }
        debug_assert!(out.len() <= MAX_COURIER_FRAME_BYTES, "bounds imply the frame cap");
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, CourierWireError> {
        if bytes.len() > MAX_COURIER_FRAME_BYTES {
            return Err(CourierWireError("frame over the byte cap"));
        }
        let mut r = Reader { buf: bytes, pos: 0 };
        let frame = match r.u8()? {
            FRAME_REQUEST => {
                let missing_prev = EntryHash(Digest(r.bytes32()?));
                let discard_root = r.bytes32()?;
                let count = r.u32()? as usize;
                if count > MAX_COURIER_CONTEXT_ENTRIES {
                    return Err(CourierWireError("request retained count over cap"));
                }
                // Bounds-checked BEFORE this allocation.
                let mut retained: Vec<EntryHash> = Vec::with_capacity(count);
                for _ in 0..count {
                    retained.push(EntryHash(Digest(r.bytes32()?)));
                }
                if !strictly_ascending(&retained) {
                    return Err(CourierWireError("request retained set not sorted/unique"));
                }
                CourierFrame::Request(CourierRequest {
                    missing_prev,
                    context: CourierContextWire {
                        discard_root,
                        retained,
                    },
                })
            }
            FRAME_RESPONSE => {
                let missing_prev = EntryHash(Digest(r.bytes32()?));
                let discard_root = r.bytes32()?;
                let result = match r.u8()? {
                    RESULT_ANSWER => {
                        let sibling_count = r.u32()? as usize;
                        if sibling_count > MAX_COURIER_SIBLINGS {
                            return Err(CourierWireError("answer sibling count over cap"));
                        }
                        let mut siblings = Vec::with_capacity(sibling_count);
                        for _ in 0..sibling_count {
                            let digest = r.bytes32()?;
                            let left = match r.u8()? {
                                0 => false,
                                1 => true,
                                _ => return Err(CourierWireError("sibling side byte not 0/1")),
                            };
                            siblings.push((digest, left));
                        }
                        let pinned_before = r.bytes32()?;
                        let later_count = r.u32()? as usize;
                        if later_count > MAX_COURIER_LATER_BATCHES {
                            return Err(CourierWireError("answer later count over cap"));
                        }
                        let mut later_batches = Vec::with_capacity(later_count);
                        for _ in 0..later_count {
                            later_batches.push(r.bytes32()?);
                        }
                        let mask_len = r.u32()? as usize;
                        if mask_len > MAX_COURIER_CONTEXT_ENTRIES.div_ceil(8) {
                            return Err(CourierWireError("answer mask length over cap"));
                        }
                        let ancestor_mask = r.take(mask_len)?.to_vec();
                        Ok(CourierWireAnswer {
                            siblings,
                            pinned_before,
                            later_batches,
                            ancestor_mask,
                        })
                    }
                    RESULT_REFUSAL => Err(match r.u8()? {
                        1 => CourierRefusal::UnknownRoot,
                        2 => CourierRefusal::HistoryEvicted,
                        3 => CourierRefusal::MissingEntry,
                        4 => CourierRefusal::ContextTooLarge,
                        _ => return Err(CourierWireError("unknown refusal code")),
                    }),
                    _ => return Err(CourierWireError("unknown result tag")),
                };
                CourierFrame::Response(CourierResponse {
                    missing_prev,
                    discard_root,
                    result,
                })
            }
            _ => return Err(CourierWireError("unknown frame tag")),
        };
        if r.pos != bytes.len() {
            return Err(CourierWireError("trailing bytes after frame"));
        }
        Ok(frame)
    }
}

fn strictly_ascending(hashes: &[EntryHash]) -> bool {
    hashes.windows(2).all(|pair| pair[0] < pair[1])
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], CourierWireError> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&end| end <= self.buf.len())
            .ok_or(CourierWireError("frame truncated"))?;
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, CourierWireError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, CourierWireError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn bytes32(&mut self) -> Result<[u8; 32], CourierWireError> {
        let bytes = self.take(32)?;
        let mut out = [0u8; 32];
        out.copy_from_slice(bytes);
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// The responder's journal copy.
// ---------------------------------------------------------------------------

/// A copy of one leaf's discard journal, batch by batch in order — what a
/// fuller peer accumulates (keyed by peer/lane at the call site) so it can
/// reconstruct a [`DiscardProof`] for any tracked batch member against the
/// leaf's live chained root, long after the leaf's own bounded journal (and
/// reach cache) have moved on. Everything here is a pure function of hashes the
/// tracker copied, per the floor's chain rules ([`DiscardProof::fold_pinned`] /
/// [`DiscardProof::batch_root`]).
#[derive(Clone, Debug)]
pub struct TrackedDiscardHistory {
    /// The chain value before `batches[0]` — all-zero when tracked from genesis.
    initial: Digest,
    /// Tracked batches, oldest first (each in canonical set form).
    batches: Vec<BTreeSet<EntryHash>>,
    /// The leaf-side sequence number of the next batch to copy.
    next_seq: u64,
}

impl Default for TrackedDiscardHistory {
    fn default() -> Self {
        Self {
            initial: Digest([0u8; 32]),
            batches: Vec::new(),
            next_seq: 0,
        }
    }
}

/// [`TrackedDiscardHistory::track`] found the leaf's journal past the copy's
/// horizon: a batch was evicted before it could be copied. The tracker must
/// re-anchor (or the affected members simply stay unprovable here).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JournalGap {
    pub expected_seq: u64,
    pub oldest_retained_seq: u64,
}

impl TrackedDiscardHistory {
    /// An empty from-genesis tracker (initial chain value all-zero).
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one discard batch (the next in chain order).
    pub fn record_batch(&mut self, entries: impl IntoIterator<Item = EntryHash>) {
        self.batches.push(entries.into_iter().collect());
        self.next_seq += 1;
    }

    /// Copy every not-yet-tracked batch out of `leaf`'s journal, in order.
    /// Returns how many were copied, or [`JournalGap`] if the journal's oldest
    /// retained batch is already past this copy's horizon.
    pub fn track<L: OpLanguage>(
        &mut self,
        leaf: &WindowedStore<L>,
    ) -> Result<usize, JournalGap> {
        let mut copied = 0;
        for batch in leaf.discard_batches() {
            if batch.seq.0 < self.next_seq {
                continue;
            }
            if batch.seq.0 != self.next_seq {
                return Err(JournalGap {
                    expected_seq: self.next_seq,
                    oldest_retained_seq: batch.seq.0,
                });
            }
            self.record_batch(batch.entries.iter().copied());
            copied += 1;
        }
        Ok(copied)
    }

    /// The chained discard root over everything tracked — must equal the leaf's
    /// current [`WindowedStore::discard_root`] when the copy is complete.
    pub fn current_root(&self) -> Digest {
        let mut root = self.initial;
        for batch in &self.batches {
            root = DiscardProof::fold_pinned(&root, &DiscardProof::batch_root(batch));
        }
        root
    }

    /// A membership proof for `member` against `expected_root`, or `None` for a
    /// root mismatch or a member in no tracked batch.
    pub fn prove_discarded_at(
        &self,
        member: &EntryHash,
        expected_root: Digest,
    ) -> Option<DiscardProof> {
        if self.current_root() != expected_root {
            return None;
        }
        let idx = self.batches.iter().position(|batch| batch.contains(member))?;
        let mut pinned_before = self.initial;
        for earlier in &self.batches[..idx] {
            pinned_before =
                DiscardProof::fold_pinned(&pinned_before, &DiscardProof::batch_root(earlier));
        }
        let later: Vec<Digest> = self.batches[idx + 1..]
            .iter()
            .map(DiscardProof::batch_root)
            .collect();
        DiscardProof::for_member(&self.batches[idx], member, pinned_before, later)
    }
}

// ---------------------------------------------------------------------------
// Responder.
// ---------------------------------------------------------------------------

/// The courier responder: a tracked copy of the requester's discard journal
/// (the proof source) plus a full store (the reach oracle). Pure — one
/// [`CourierResponder::answer`] per request; [`serve_courier_once`] is the
/// stream wrapper.
pub struct CourierResponder<'a, L: OpLanguage> {
    /// The requester's tracked discard journal (populated via
    /// [`TrackedDiscardHistory::track`] / `record_batch`, keyed by peer/lane at
    /// the call site).
    pub history: &'a TrackedDiscardHistory,
    /// The full DAG whose real reach oracle answers the ancestor mask.
    pub full: &'a Store<L>,
}

impl<L: OpLanguage> CourierResponder<'_, L> {
    /// Answer one request. Refusals mirror the §4.5 deferral doctrine: they are
    /// verdicts, never errors — the requester's op simply stays parked.
    pub fn answer(&self, request: &CourierRequest) -> CourierResponse {
        let respond = |result| CourierResponse {
            missing_prev: request.missing_prev,
            discard_root: request.context.discard_root,
            result,
        };
        if request.context.retained.len() > MAX_COURIER_CONTEXT_ENTRIES {
            return respond(Err(CourierRefusal::ContextTooLarge));
        }
        let requested_root = Digest(request.context.discard_root);
        if self.history.current_root() != requested_root {
            return respond(Err(CourierRefusal::UnknownRoot));
        }
        // The ancestor half needs the entry's real causal past.
        if self.full.repair_record(&request.missing_prev).is_none() {
            return respond(Err(CourierRefusal::MissingEntry));
        }
        let Some(proof) = self
            .history
            .prove_discarded_at(&request.missing_prev, requested_root)
        else {
            return respond(Err(CourierRefusal::HistoryEvicted));
        };
        if proof.siblings.len() > MAX_COURIER_SIBLINGS
            || proof.later_batches.len() > MAX_COURIER_LATER_BATCHES
        {
            // A proof this wire generation cannot carry: refuse rather than send
            // an unencodable answer.
            return respond(Err(CourierRefusal::HistoryEvicted));
        }
        debug_assert!(
            proof.verifies(&request.missing_prev, &requested_root),
            "a tracked-journal proof must verify against the tracked root"
        );

        // The trusted half: bit i <=> retained[i] is a strict ancestor.
        let reach = self.full.reach();
        let mut ancestor_mask = vec![0u8; request.context.retained.len().div_ceil(8)];
        for (i, retained) in request.context.retained.iter().enumerate() {
            if reach.is_ancestor(retained, &request.missing_prev) {
                ancestor_mask[i / 8] |= 1 << (i % 8);
            }
        }

        respond(Ok(CourierWireAnswer {
            siblings: proof
                .siblings
                .iter()
                .map(|(digest, left)| (digest.0, *left))
                .collect(),
            pinned_before: proof.pinned_before.0,
            later_batches: proof.later_batches.iter().map(|d| d.0).collect(),
            ancestor_mask,
        }))
    }
}

/// Serve ONE courier exchange on an accepted [`LaneSpec::COURIER_ALPN`]
/// (`super::sync::LaneSpec`) stream: read one request, answer it, done. A clean
/// EOF before any request is not an error.
pub async fn serve_courier_once<L: OpLanguage, S: SyncStream>(
    stream: &mut S,
    responder: &CourierResponder<'_, L>,
) -> Result<(), SyncError> {
    let Some(frame) = stream.recv_frame().await? else {
        return Ok(());
    };
    let request = match CourierFrame::decode(&frame)? {
        CourierFrame::Request(request) => request,
        CourierFrame::Response(_) => {
            return Err(SyncError::Decode("courier opener was a Response".into()));
        }
    };
    let response = responder.answer(&request);
    let bytes = CourierFrame::Response(response).encode()?;
    stream.send_frame(&bytes).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Requester.
// ---------------------------------------------------------------------------

/// Build the request for ONE missing prev under the leaf's CURRENT context.
/// `None` when the leaf has no context (M3.0 profile) or its retained set
/// exceeds the wire bound — the op then simply stays deferred.
///
/// Returns the [`CourierContext`] snapshot alongside: the caller passes it to
/// [`apply_courier_response`] after the round trip, where the store re-checks
/// it against its own current context (the request must not outlive its
/// horizon).
pub fn courier_request_for<L: OpLanguage>(
    leaf: &WindowedStore<L>,
    missing_prev: EntryHash,
) -> Option<(CourierRequest, CourierContext)> {
    let context = leaf.courier_context()?;
    if context.retained.len() > MAX_COURIER_CONTEXT_ENTRIES {
        return None;
    }
    let wire = CourierContextWire {
        discard_root: context.discard_root.0,
        retained: context.retained.to_vec(),
    };
    Some((
        CourierRequest {
            missing_prev,
            context: wire,
        },
        context,
    ))
}

/// One request/response exchange over an open courier stream. Wire-integrity
/// failures (encode/decode, echo mismatch, hang-up) are [`SyncError`]s; a
/// refusal is NOT — it comes back as the response verdict.
pub async fn exchange_courier<S: SyncStream>(
    stream: &mut S,
    request: &CourierRequest,
) -> Result<CourierResponse, SyncError> {
    let bytes = CourierFrame::Request(request.clone()).encode()?;
    stream.send_frame(&bytes).await?;
    let Some(frame) = stream.recv_frame().await? else {
        return Err(SyncError::Session("courier peer closed without answering".into()));
    };
    let response = match CourierFrame::decode(&frame)? {
        CourierFrame::Response(response) => response,
        CourierFrame::Request(_) => {
            return Err(SyncError::Decode("courier answer was a Request".into()));
        }
    };
    // The echo binds the response to THIS request and THIS context.
    if response.missing_prev != request.missing_prev
        || response.discard_root != request.context.discard_root
    {
        return Err(SyncError::Decode("courier response echo mismatch".into()));
    }
    Ok(response)
}

/// Convert verified responses back into floor [`CourierAnswer`]s and admit the
/// deferred op — the requester's final step, run after REACQUIRING the leaf
/// (never held across the round trip). `sent_context` is the snapshot
/// [`courier_request_for`] returned; the store re-checks it against its current
/// context and rejects a stale one before the floor is consulted.
///
/// Defer-never-reject, end to end: a refusal, a mask of the wrong shape, or a
/// response for the wrong prev leaves that prev unanswered (the floor reports
/// [`CourierFault::Unanswered`](tutti_core::CourierFault)); a proof that fails
/// verification reports `BadProof`. In every error case the leaf is untouched
/// and the op stays parked.
pub fn apply_courier_response<L: OpLanguage>(
    leaf: &mut WindowedStore<L>,
    deferred: &DeferredLift,
    sent_context: &CourierContext,
    responses: &[CourierResponse],
) -> Result<Vec<EntryHash>, DeferredLiftError> {
    let mut answers: BTreeMap<EntryHash, CourierAnswer> = BTreeMap::new();
    let mask_len = sent_context.retained.len().div_ceil(8);
    for response in responses {
        if !deferred.missing.contains(&response.missing_prev)
            || response.discard_root != sent_context.discard_root.0
        {
            continue; // not an answer to this deferral/context: unanswered
        }
        let Ok(answer) = &response.result else {
            continue; // refusal: that prev stays unanswered
        };
        // Mask shape validation BEFORE converting: exact length, zero unused bits.
        if answer.ancestor_mask.len() != mask_len {
            continue;
        }
        let used_bits = sent_context.retained.len();
        let valid_bits = answer
            .ancestor_mask
            .iter()
            .enumerate()
            .all(|(byte_index, byte)| {
                let first_bit = byte_index * 8;
                let live = used_bits.saturating_sub(first_bit).min(8);
                let unused = byte & !(((1u16 << live) - 1) as u8);
                unused == 0
            });
        if !valid_bits {
            continue;
        }
        let ancestors: BTreeSet<EntryHash> = sent_context
            .retained
            .iter()
            .enumerate()
            .filter(|(i, _)| answer.ancestor_mask[i / 8] >> (i % 8) & 1 == 1)
            .map(|(_, hash)| *hash)
            .collect();
        let proof = DiscardProof {
            siblings: answer
                .siblings
                .iter()
                .map(|(digest, left)| (Digest(*digest), *left))
                .collect(),
            pinned_before: Digest(answer.pinned_before),
            later_batches: answer.later_batches.iter().copied().map(Digest).collect(),
        };
        answers.insert(response.missing_prev, CourierAnswer { proof, ancestors });
    }
    let courier = MapCourier(answers);
    leaf.lift_pending_via_courier(deferred.candidate, sent_context, &courier)
}

/// The one-shot requester wiring for the common single-missing-prev deferral:
/// build the request under the current context, run one exchange on `stream`
/// (one request/response per stream), reacquire the leaf, and admit.
///
/// Outer `Err` is wire integrity (the op stays parked regardless); the inner
/// result is the floor's §4.5 verdict. A deferral with several missing prevs
/// needs one stream per prev: run [`exchange_courier`] per request and hand all
/// responses to [`apply_courier_response`] together.
pub async fn lift_deferred_over_stream<L: OpLanguage, S: SyncStream>(
    stream: &mut S,
    leaf: &mut WindowedStore<L>,
    deferred: &DeferredLift,
) -> Result<Result<Vec<EntryHash>, DeferredLiftError>, SyncError> {
    let mut prevs = deferred.missing.iter();
    let (Some(&missing_prev), None) = (prevs.next(), prevs.next()) else {
        return Err(SyncError::Session(
            "one courier stream serves one missing prev; gather multi-prev responses \
             across streams and call apply_courier_response"
                .into(),
        ));
    };
    let Some((request, sent_context)) = courier_request_for(leaf, missing_prev) else {
        return Ok(Err(DeferredLiftError::StaleContext));
    };
    let response = exchange_courier(stream, &request).await?;
    Ok(apply_courier_response(
        leaf,
        deferred,
        &sent_context,
        std::slice::from_ref(&response),
    ))
}

/// A map-backed §4.5 [`Courier`]: answers gathered ahead of the lift call.
struct MapCourier(BTreeMap<EntryHash, CourierAnswer>);

impl Courier for MapCourier {
    fn resolve_discarded(&self, prev: &EntryHash) -> Option<CourierAnswer> {
        self.0.get(prev).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use futures::executor::block_on;
    use hhhs_sync::sync_session::EntrySource;
    use tutti_core::{
        CourierFault, SignedOp, Store, VerifiedOpG, WindowedStore, signing_key_from_seed,
        sync_root_of, verify_signed_op_in,
    };
    use tutti_music::{MusicOp, MusicView};

    use crate::net::sync::{IncomingOp, LaneSpec, LaneSyncSource, MusicLane, ingest_pairs};
    use crate::net::{LaneProtocol, Transport, TransportEvent, loopback::loopback_pair};
    use crate::room::test_support::{SEED_A, SEED_B, tet_degree};
    use crate::room::v4::{MusicLang, RoomLane};

    const TOPIC: &str = "courier-deep-laggard";

    fn verify(signed: &SignedOp) -> VerifiedOpG<MusicLang> {
        verify_signed_op_in::<MusicLang>(signed).expect("op verifies")
    }

    /// A canonical digest of a [`MusicView`] — the `state_root` stand-in for the
    /// music lane (walkie's production `state_root` folds `RoomView`, not
    /// `MusicView`). Deterministic: every view field is BTree-ordered.
    fn state_root(view: &MusicView) -> [u8; 32] {
        *blake3::hash(format!("{view:?}").as_bytes()).as_bytes()
    }

    /// The deep-laggard fixture (design C):
    ///
    /// ```text
    /// leaf    = WindowedStore<MusicLang>::with_window_limits(64, 1, 8)
    /// full    = Store<MusicLang>::new()
    /// offline = Store<MusicLang>::new()
    /// ```
    ///
    /// Author A writes `P = AddDegree(0)` (seen by all three), then the
    /// `RemoveDegree(0)` chain that consumes it. `MusicLang::retain` keeps the
    /// causal-maxima remove per degree, so consuming BOTH the add and a remove
    /// takes a second remove dominating the first: the real discard batch is
    /// `{P, r1}` — two rows, and with reach cap 1 the second discarded row
    /// evicts P's. Offline author B (who saw only P) then writes the laggard
    /// `X = AddDegree(7)` with P still on its frontier; `X` reaches the leaf
    /// through the production windowed `ingest_pairs()` path and defers.
    struct Fixture {
        leaf: WindowedStore<MusicLang>,
        full: Store<MusicLang>,
        tracked: TrackedDiscardHistory,
        deferred: DeferredLift,
        p_entry: EntryHash,
        x_id: tutti_core::OpId,
        x_wire_hash: EntryHash,
    }

    fn fixture() -> Fixture {
        let key_a = signing_key_from_seed(&SEED_A);
        let key_b = signing_key_from_seed(&SEED_B);

        let mut leaf = WindowedStore::<MusicLang>::with_window_limits(64, 1, 8);
        let mut full = Store::<MusicLang>::new();
        let mut offline = Store::<MusicLang>::new();

        // 1. P, fed to leaf, full, and offline.
        let p_signed = full.commit(&key_a, TOPIC, 1, MusicOp::AddDegree { degree: tet_degree(0) });
        let p_id = verify(&p_signed).id();
        leaf.ingest_verified(verify(&p_signed));
        offline.ingest_verified(verify(&p_signed));

        // 2. The removes that consume P — fed to leaf and full, NOT offline.
        let r1_signed =
            full.commit(&key_a, TOPIC, 2, MusicOp::RemoveDegree { degree: tet_degree(0) });
        let r2_signed =
            full.commit(&key_a, TOPIC, 3, MusicOp::RemoveDegree { degree: tet_degree(0) });
        leaf.ingest_verified(verify(&r1_signed));
        leaf.ingest_verified(verify(&r2_signed));
        let p_entry = leaf.lifted_entry(p_id).expect("P lifted");

        // 3. The real compaction: {P, r1} is one non-empty discard batch; with
        //    reach cap 1 the second discarded row evicts P's.
        let stats = leaf.compact();
        assert_eq!(stats.discarded, 2, "the add and the consumed remove discard");
        assert_eq!(leaf.courier_gap_entries(), 1, "reach cap enforced");

        // 4. The fuller peer copies the leaf's exposed batch journal.
        let mut tracked = TrackedDiscardHistory::new();
        let copied = tracked.track(&leaf).expect("journal copy is seq-continuous");
        assert_eq!(copied, 1, "one discard batch journaled");
        assert_eq!(Some(tracked.current_root()), leaf.discard_root());

        // 5. Offline author B writes the deep laggard X with P on its frontier.
        let x_signed =
            offline.commit(&key_b, TOPIC, 10, MusicOp::AddDegree { degree: tet_degree(7) });
        let x_id = verify(&x_signed).id();
        full.ingest_verified(verify(&x_signed));
        let x_wire_hash = full.lifted_entry(x_id).expect("full lifts X");

        // 6. X reaches the leaf through the production windowed ingest_pairs path.
        let pairs = vec![(
            x_wire_hash,
            x_signed.to_wire_bytes_in::<MusicLang>().expect("X serializes"),
        )];
        let report = ingest_pairs::<MusicLang, _>(
            &mut leaf,
            TOPIC,
            pairs.iter().map(IncomingOp::from),
        )
        .unwrap();

        // Pre-courier assertions (design C).
        assert_eq!(leaf.pending_len(), 1);
        assert!(leaf.lifted_entry(x_id).is_none());
        assert_eq!(
            report.admitted,
            vec![x_wire_hash],
            "a courier-deferred op is KEPT under its wire hash (parked bytes in hand)"
        );
        assert!(report.lifted.is_empty());
        assert_eq!(report.courier.len(), 1);
        assert_eq!(report.courier[0].missing, BTreeSet::from([p_entry]));
        assert_eq!(leaf.courier_gap_entries(), 1, "cap enforced");

        Fixture {
            deferred: report.courier[0].clone(),
            leaf,
            full,
            tracked,
            p_entry,
            x_id,
            x_wire_hash,
        }
    }

    /// Accept the courier stream the far side opened (the loopback carrier is
    /// ALPN-less; production endpoints negotiate `LaneSpec::COURIER_ALPN`).
    async fn accept_courier(
        transport: &mut crate::net::loopback::LoopbackTransport,
    ) -> crate::net::loopback::LoopbackStream {
        loop {
            match transport.next_event().await {
                Some(TransportEvent::LaneRequested { stream, .. }) => break stream,
                Some(TransportEvent::PeerUp { .. }) => continue,
                other => panic!("expected LaneRequested, got {other:?}"),
            }
        }
    }

    /// THE M3.2 GATE (design C): a windowed leaf that deferred a deep laggard
    /// admits it over a REAL courier round trip — real compaction batch, real
    /// reach-row eviction, RBSR-side deferral, encoded exchange over the
    /// in-process transport, real `DiscardProof::for_member()` at the responder,
    /// floor verification at the requester — and reaches full-reference
    /// view/root equivalence. Then, on a fresh fixture, the SAME responder's
    /// genuine proof with ONE bit flipped stays deferred: `BadProof`, view and
    /// root byte-identical, the op still parked (§4.5 defer-never-reject).
    #[test]
    fn deep_laggard_is_admitted_over_real_courier_and_bad_proof_stays_deferred() {
        // The lane's courier channel identity (the ALPN production endpoints
        // negotiate; the RBSR ALPN never carries these frames).
        assert_eq!(MusicLane::COURIER_ALPN, b"tutti/music/courier/1");
        assert_ne!(MusicLane::COURIER_ALPN, MusicLane::ALPN);

        // ------------------------- success path ---------------------------
        let mut fx = fixture();
        let (requester_end, mut responder_end) = loopback_pair();

        let admitted = block_on(async {
            let mut requester_stream = requester_end
                .open_lane(
                    requester_end.remote_id(),
                    LaneProtocol::Courier(RoomLane::Music),
                )
                .await
                .unwrap();
            let mut responder_stream = accept_courier(&mut responder_end).await;

            let responder = CourierResponder {
                history: &fx.tracked,
                full: &fx.full,
            };
            let (request, sent_context) =
                courier_request_for(&fx.leaf, fx.p_entry).expect("leaf has a courier context");

            // Drive both halves over the wire; the responder is the REAL one
            // (tracked journal -> DiscardProof::for_member + full's reach oracle).
            let (wire, served) = futures::future::join(
                exchange_courier(&mut requester_stream, &request),
                serve_courier_once(&mut responder_stream, &responder),
            )
            .await;
            served.expect("responder serves one exchange");
            let response = wire.expect("wire round trip");
            assert!(response.result.is_ok(), "the genuine answer is not a refusal");

            // Reacquire the leaf after the round trip and admit.
            apply_courier_response(
                &mut fx.leaf,
                &fx.deferred,
                &sent_context,
                std::slice::from_ref(&response),
            )
            .expect("a real DiscardProof admits the deep laggard")
        });

        assert_eq!(admitted, vec![fx.deferred.candidate]);
        assert_eq!(fx.leaf.pending_len(), 0);
        assert_eq!(
            fx.leaf.lifted_entry(fx.x_id),
            fx.full.lifted_entry(fx.x_id),
            "byte-compatible admission: leaf and full derive the same entry hash"
        );
        assert_eq!(fx.leaf.view(), fx.full.view(), "full-reference view equivalence");
        assert_eq!(
            state_root(&fx.leaf.view()),
            state_root(&fx.full.view()),
            "state_root equivalence"
        );

        // Convergence root against the full peer's CUT-RESTRICTED LaneSyncSource
        // — windowed sync_root() is cut-scoped, never the full-history root.
        let full_source =
            LaneSyncSource::<MusicLang>::capture(&fx.full, [0u8; 16]).expect("captures");
        let leaf_retained = fx.leaf.entry_hashes();
        assert!(
            leaf_retained.iter().all(|hash| full_source.have(hash)),
            "every leaf-retained entry is servable by the full peer"
        );
        assert!(leaf_retained.contains(&fx.x_wire_hash), "X is in the cut");
        assert_eq!(
            fx.leaf.sync_root(),
            sync_root_of(leaf_retained.iter().filter(|hash| full_source.have(hash))),
            "leaf sync_root == the full source restricted to the leaf's cut"
        );

        // ------------------------- forged path ----------------------------
        let mut fx = fixture();
        let before_view = fx.leaf.view();
        let before_root = state_root(&before_view);
        let before_sync_root = fx.leaf.sync_root();

        let (requester_end, mut responder_end) = loopback_pair();
        let result = block_on(async {
            let mut requester_stream = requester_end
                .open_lane(
                    requester_end.remote_id(),
                    LaneProtocol::Courier(RoomLane::Music),
                )
                .await
                .unwrap();
            let mut responder_stream = accept_courier(&mut responder_end).await;

            let responder = CourierResponder {
                history: &fx.tracked,
                full: &fx.full,
            };
            let (request, sent_context) =
                courier_request_for(&fx.leaf, fx.p_entry).expect("leaf has a courier context");

            let (wire, ()) = futures::future::join(
                exchange_courier(&mut requester_stream, &request),
                async {
                    // The REAL responder generates the genuine proof; ONE bit of
                    // `pinned_before` is flipped before encoding.
                    let frame = responder_stream
                        .recv_frame()
                        .await
                        .unwrap()
                        .expect("request arrives");
                    let request = match CourierFrame::decode(&frame).unwrap() {
                        CourierFrame::Request(request) => request,
                        other => panic!("expected a courier Request, got {other:?}"),
                    };
                    let mut response = responder.answer(&request);
                    let answer = response.result.as_mut().expect("genuine answer");
                    answer.pinned_before[0] ^= 1;
                    let bytes = CourierFrame::Response(response).encode().unwrap();
                    responder_stream.send_frame(&bytes).await.unwrap();
                },
            )
            .await;
            let response = wire.expect("the forged frame still decodes and echoes");

            apply_courier_response(
                &mut fx.leaf,
                &fx.deferred,
                &sent_context,
                std::slice::from_ref(&response),
            )
        });

        assert!(
            matches!(
                result,
                Err(DeferredLiftError::Courier(CourierFault::BadProof(hash))) if hash == fx.p_entry
            ),
            "a bit-flipped proof must fail floor verification as BadProof, got {result:?}"
        );
        assert_eq!(fx.leaf.pending_len(), 1, "the op stays parked");
        assert!(fx.leaf.lifted_entry(fx.x_id).is_none(), "nothing admitted");
        assert_eq!(fx.leaf.view(), before_view, "view unchanged");
        assert_eq!(state_root(&fx.leaf.view()), before_root, "root unchanged");
        assert_eq!(fx.leaf.sync_root(), before_sync_root, "identity set unchanged");
    }

    /// A responder refusal (here: a root the responder does not track) is a
    /// verdict, not an error — the requester's op stays parked as Unanswered.
    #[test]
    fn a_refusal_leaves_the_laggard_deferred() {
        let mut fx = fixture();
        let foreign = TrackedDiscardHistory::new(); // tracks nothing => UnknownRoot
        let (requester_end, mut responder_end) = loopback_pair();

        let result = block_on(async {
            let mut requester_stream = requester_end
                .open_lane(
                    requester_end.remote_id(),
                    LaneProtocol::Courier(RoomLane::Music),
                )
                .await
                .unwrap();
            let mut responder_stream = accept_courier(&mut responder_end).await;
            let responder = CourierResponder {
                history: &foreign,
                full: &fx.full,
            };
            let (request, sent_context) =
                courier_request_for(&fx.leaf, fx.p_entry).expect("context");
            let (wire, served) = futures::future::join(
                exchange_courier(&mut requester_stream, &request),
                serve_courier_once(&mut responder_stream, &responder),
            )
            .await;
            served.expect("responder serves");
            let response = wire.expect("wire round trip");
            assert_eq!(response.result, Err(CourierRefusal::UnknownRoot));
            apply_courier_response(
                &mut fx.leaf,
                &fx.deferred,
                &sent_context,
                std::slice::from_ref(&response),
            )
        });

        assert!(matches!(
            result,
            Err(DeferredLiftError::Courier(CourierFault::Unanswered(hash))) if hash == fx.p_entry
        ));
        assert_eq!(fx.leaf.pending_len(), 1, "still parked; a later peer may answer");
    }

    /// The one-shot requester wiring (`lift_deferred_over_stream`) drives the
    /// same exchange end to end.
    #[test]
    fn the_one_shot_requester_wiring_admits_over_the_stream() {
        let mut fx = fixture();
        let (requester_end, mut responder_end) = loopback_pair();

        let admitted = block_on(async {
            let mut requester_stream = requester_end
                .open_lane(
                    requester_end.remote_id(),
                    LaneProtocol::Courier(RoomLane::Music),
                )
                .await
                .unwrap();
            let mut responder_stream = accept_courier(&mut responder_end).await;
            let responder = CourierResponder {
                history: &fx.tracked,
                full: &fx.full,
            };
            let (verdict, served) = futures::future::join(
                lift_deferred_over_stream(&mut requester_stream, &mut fx.leaf, &fx.deferred),
                serve_courier_once(&mut responder_stream, &responder),
            )
            .await;
            served.expect("responder serves");
            verdict.expect("wire ok").expect("floor admits")
        });

        assert_eq!(admitted, vec![fx.deferred.candidate]);
        assert_eq!(fx.leaf.pending_len(), 0);
        assert_eq!(fx.leaf.view(), fx.full.view());
    }

    // -------------------------------------------------------------------
    // Codec bounds and validation.
    // -------------------------------------------------------------------

    fn hash_of(byte: u8) -> EntryHash {
        EntryHash(Digest([byte; 32]))
    }

    #[test]
    fn courier_frames_round_trip() {
        let request = CourierFrame::Request(CourierRequest {
            missing_prev: hash_of(7),
            context: CourierContextWire {
                discard_root: [3; 32],
                retained: vec![hash_of(1), hash_of(2), hash_of(9)],
            },
        });
        let bytes = request.encode().unwrap();
        assert!(bytes.len() <= MAX_COURIER_FRAME_BYTES);
        assert_eq!(CourierFrame::decode(&bytes).unwrap(), request);

        let answer = CourierFrame::Response(CourierResponse {
            missing_prev: hash_of(7),
            discard_root: [3; 32],
            result: Ok(CourierWireAnswer {
                siblings: vec![([4; 32], true), ([5; 32], false)],
                pinned_before: [6; 32],
                later_batches: vec![[8; 32]],
                ancestor_mask: vec![0b0000_0101],
            }),
        });
        let bytes = answer.encode().unwrap();
        assert_eq!(CourierFrame::decode(&bytes).unwrap(), answer);

        for refusal in [
            CourierRefusal::UnknownRoot,
            CourierRefusal::HistoryEvicted,
            CourierRefusal::MissingEntry,
            CourierRefusal::ContextTooLarge,
        ] {
            let frame = CourierFrame::Response(CourierResponse {
                missing_prev: hash_of(1),
                discard_root: [0; 32],
                result: Err(refusal),
            });
            let bytes = frame.encode().unwrap();
            assert_eq!(CourierFrame::decode(&bytes).unwrap(), frame);
        }
    }

    #[test]
    fn the_codec_rejects_malformed_frames() {
        // Unsorted / duplicate retained sets never encode…
        for retained in [
            vec![hash_of(2), hash_of(1)],
            vec![hash_of(1), hash_of(1)],
        ] {
            let frame = CourierFrame::Request(CourierRequest {
                missing_prev: hash_of(7),
                context: CourierContextWire {
                    discard_root: [0; 32],
                    retained,
                },
            });
            assert!(frame.encode().is_err(), "unsorted/duplicate must not encode");
        }
        // …and never decode either (a hostile peer skips our encoder).
        let good = CourierFrame::Request(CourierRequest {
            missing_prev: hash_of(7),
            context: CourierContextWire {
                discard_root: [0; 32],
                retained: vec![hash_of(1), hash_of(2)],
            },
        });
        let mut swapped = good.encode().unwrap();
        // The two retained hashes start after tag(1) + prev(32) + root(32) + count(4).
        let base = 1 + 32 + 32 + 4;
        for i in 0..32 {
            swapped.swap(base + i, base + 32 + i);
        }
        assert_eq!(
            CourierFrame::decode(&swapped),
            Err(CourierWireError("request retained set not sorted/unique"))
        );

        // An over-cap retained count is rejected BEFORE allocation.
        let mut oversized = good.encode().unwrap();
        oversized[1 + 32 + 32..1 + 32 + 32 + 4]
            .copy_from_slice(&((MAX_COURIER_CONTEXT_ENTRIES as u32) + 1).to_le_bytes());
        assert_eq!(
            CourierFrame::decode(&oversized),
            Err(CourierWireError("request retained count over cap"))
        );

        // Trailing bytes are an error.
        let mut trailing = good.encode().unwrap();
        trailing.push(0);
        assert_eq!(
            CourierFrame::decode(&trailing),
            Err(CourierWireError("trailing bytes after frame"))
        );

        // A truncated frame is an error, not a panic.
        let bytes = good.encode().unwrap();
        assert_eq!(
            CourierFrame::decode(&bytes[..bytes.len() - 1]),
            Err(CourierWireError("frame truncated"))
        );

        // A sibling side byte outside {0,1} is rejected.
        let answer = CourierFrame::Response(CourierResponse {
            missing_prev: hash_of(7),
            discard_root: [3; 32],
            result: Ok(CourierWireAnswer {
                siblings: vec![([4; 32], true)],
                pinned_before: [6; 32],
                later_batches: vec![],
                ancestor_mask: vec![],
            }),
        });
        let mut bad_side = answer.encode().unwrap();
        let side_at = 1 + 32 + 32 + 1 + 4 + 32; // …result tag, sibling count, digest
        bad_side[side_at] = 2;
        assert_eq!(
            CourierFrame::decode(&bad_side),
            Err(CourierWireError("sibling side byte not 0/1"))
        );
    }

    /// Requester-side mask validation: a wrong-length mask or non-zero unused
    /// bits make the prev unanswered (still parked), never a wrong admission.
    #[test]
    fn a_malformed_mask_leaves_the_laggard_deferred() {
        let mut fx = fixture();
        let responder = CourierResponder {
            history: &fx.tracked,
            full: &fx.full,
        };
        let (request, sent_context) =
            courier_request_for(&fx.leaf, fx.p_entry).expect("context");
        let genuine = responder.answer(&request);

        // Wrong length.
        let mut wrong_len = genuine.clone();
        wrong_len.result.as_mut().unwrap().ancestor_mask.push(0);
        // Non-zero unused bits (retained.len() == 1 => only bit 0 may be set).
        let mut junk_bits = genuine.clone();
        junk_bits.result.as_mut().unwrap().ancestor_mask[0] |= 0b1000_0000;

        for response in [wrong_len, junk_bits] {
            let result = apply_courier_response(
                &mut fx.leaf,
                &fx.deferred,
                &sent_context,
                std::slice::from_ref(&response),
            );
            assert!(matches!(
                result,
                Err(DeferredLiftError::Courier(CourierFault::Unanswered(hash))) if hash == fx.p_entry
            ));
            assert_eq!(fx.leaf.pending_len(), 1);
        }
    }
}
