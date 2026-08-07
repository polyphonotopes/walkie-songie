//! Transport-neutral HHHS anti-entropy driver (transport-design §2.6 / §3.4).
//!
//! Drives [`hhhs_core::sync_session::SyncSession`] to completion over any
//! [`SyncStream`], for both halves: the initiator (dialled via
//! [`Transport::open_sync`](super::Transport::open_sync)) and the responder
//! (accepted from [`TransportEvent::SyncRequested`](super::TransportEvent)).
//! Nothing here names a backend, so the same driver serves iroh, a loopback
//! pair, and any browser-bridged carrier.
//!
//! # The consistency invariant
//!
//! The kernel requires that the [`EntrySource`] answering `Fetch`es and the
//! [`Index`] advertising hashes describe the *same* store state. If they drift,
//! the index advertises hashes the source cannot serve; `Fetch`es come back
//! partially answered, `outstanding_fetches` still decrements, both sides reach
//! `Done`, and the session reports success **while the peers have not
//! converged**. [`RoomSyncSource`] makes that failure unrepresentable: it owns
//! both, and builds the index from the very map that backs `have()`.
//!
//! # Liveness
//!
//! Convergence is not enough — the session must also always *end*. Three
//! separate mechanisms, because each covers a hole the others do not:
//!
//! * **A receive deadline** ([`SyncLimits::recv_timeout`]). A peer that simply
//!   stops talking must be a loud error, never a hung task. The iteration cap
//!   below cannot do this: it only advances when frames arrive.
//! * **`resume_admitted` after every `Entries`**, empty or not — see [`pump`].
//! * **Close on `status() != Exchanging`, never on `is_complete()`** — a
//!   root-divergent run finishes with `is_complete()` false FOREVER, so a
//!   driver waiting on it hangs on exactly the sessions it must end and
//!   re-try. [`SessionStatus::Divergent`] is "closed; the periodic
//!   anti-entropy re-syncs", never an error.

use core::future::Future;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use futures::future::{Either, select};
use hhhs_core::{
    EntryHash, SortKey,
    reconciliation::{Config, Index, SessionHello},
    strategy::StrategyId,
    sync_session::{
        EntrySource, SessionBudget, SessionError, SessionStatus, SyncMessage, SyncSession,
    },
};

use super::{SyncStream, TransportError};
use crate::room::{
    ops::{MAX_SIGNED_OP_WIRE_BYTES, MAX_SIGNED_PAYLOAD_BYTES, SignedOp, verify_signed_op_for_topic},
    store::{RoomStore, sync_root_of},
};

/// Protocol generation. A change here is an ALPN/mode change, never a silent
/// reshape — peers compare this for equality and `Abort` on mismatch.
///
/// Version 2 is the hardened kernel's wire generation: `Recon(Message)` became
/// `Question { id, msg }` with a mandatory per-question `Ack(id)`, and
/// `Entries(Vec<..>)` became the chunkable `Entries { pairs, more }`. The ALPN
/// (`net::native::RBSR_ALPN`) bumps in lockstep so old and new peers never
/// attempt to interop.
pub const SYNC_STRATEGY_NAME: &str = "walkie-entryhash";
pub const SYNC_STRATEGY_VERSION: u32 = 2;

/// Hard cap on one encoded `SyncMessage`.
///
/// Derived from the op size ladder, not chosen: anti-entropy cannot split one op
/// across frames, and `bytes_with_closure` must include the hash the peer asked
/// for whatever the budget says, so a frame cap below the largest legal op is a
/// permanent convergence failure for the whole room. The `const` assertion below
/// is the enforcement.
pub const MAX_SYNC_FRAME_BYTES: usize = 2 * 1024 * 1024;

const _: () = assert!(
    MAX_SYNC_FRAME_BYTES >= MAX_SIGNED_OP_WIRE_BYTES + 64 * 1024,
    "one Entries must carry the largest legal op plus postcard framing, or a \
     single big-but-legal op permanently poisons anti-entropy for every peer"
);
const _: () = assert!(
    MAX_SIGNED_PAYLOAD_BYTES < MAX_SYNC_FRAME_BYTES,
    "the largest legal payload must fit one sync frame"
);

/// How long the driver waits for one inbound frame before giving up.
///
/// Matches the deadline the previous iroh-specific driver applied to every read.
/// A session with no deadline anywhere is a task that a silent peer parks
/// forever.
pub const DEFAULT_RECV_TIMEOUT: Duration = Duration::from_secs(20);

pub fn sync_strategy() -> StrategyId {
    StrategyId::new(SYNC_STRATEGY_NAME, SYNC_STRATEGY_VERSION)
}

/// A runtime's sleep.
///
/// The driver is runtime-neutral as well as transport-neutral — one session runs
/// inside a native tokio task or on wasm's single thread — so it cannot name a
/// timer, and the caller supplies one. Tests inject a deterministic timer and
/// assert on the deadline without a real clock.
pub trait SyncTimer {
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()>;
}

/// Session limits.
///
/// The default budget is internally consistent: draining `max_entries_ingested`
/// at `fetch_max_hashes` per `Fetch` costs `2 * 65_536 / 256 = 512` rounds,
/// leaving most of `max_rounds` for the RBSR descent itself.
///
/// The queue/flood budgets the hardened kernel added
/// (`max_pending_fetches`, `max_held_items`, `max_requested_hashes`,
/// `max_fruitless_fetches`) ride the kernel defaults, which are sized for the
/// same `max_entries_ingested` this budget keeps. Scaling rule (kernel module
/// docs): an app that raises the size budgets must raise
/// `max_requested_hashes` in step — it doubles as the outstanding-question
/// ceiling, and the honest peak grows with the divergence.
#[derive(Debug, Clone, Copy)]
pub struct SyncLimits {
    pub budget: SessionBudget,
    /// Hard cap on one encoded frame.
    ///
    /// The kernel chunks `Entries` answers to `budget.max_frame_bytes` using a
    /// deliberately over-estimated size model, so an encoded frame never
    /// exceeds it; [`send_all`] keeps this as the loud last resort.
    pub max_frame_bytes: usize,
    /// Deadline for a single inbound frame.
    pub recv_timeout: Duration,
}

impl Default for SyncLimits {
    fn default() -> Self {
        Self {
            budget: SessionBudget {
                fetch_max_hashes: 256,
                max_rounds: 4_096,
                max_entries_ingested: 65_536,
                max_outstanding_fetches: 64,
                max_frame_bytes: MAX_SYNC_FRAME_BYTES,
                // max_requested_hashes (65_536) matches max_entries_ingested
                // above, plus max_pending_fetches / max_held_items /
                // max_fruitless_fetches — all sized by the kernel for exactly
                // these size budgets.
                ..SessionBudget::default()
            },
            max_frame_bytes: MAX_SYNC_FRAME_BYTES,
            recv_timeout: DEFAULT_RECV_TIMEOUT,
        }
    }
}

/// A consistent (`EntrySource`, `Index`, root) triple captured at one store
/// horizon.
///
/// Replaces the previous `RoomRepairSnapshot` + separate `build_repair_index`,
/// whose split let a caller pair a fresh index with a stale snapshot.
pub struct RoomSyncSource {
    /// entry hash -> (verbatim signed-op wire bytes, causal predecessors)
    records: BTreeMap<EntryHash, (Vec<u8>, Vec<EntryHash>)>,
    index: Index,
    /// Convergence digest over `records`' key set, for the `Done` cross-check.
    root: [u8; 32],
}

impl RoomSyncSource {
    /// Capture the store's current horizon. The index and root are derived from
    /// exactly the records that back `have()`, so the three cannot disagree.
    pub fn capture(store: &RoomStore, salt: [u8; 16]) -> Self {
        let records: BTreeMap<EntryHash, (Vec<u8>, Vec<EntryHash>)> = store
            .repair_records()
            .into_iter()
            .filter_map(|(hash, (signed, predecessors))| match signed.to_wire_bytes() {
                Ok(bytes) => Some((hash, (bytes, predecessors))),
                // A lifted entry that cannot be re-serialized would be advertised
                // by neither index nor root, so both peers would "agree" while
                // one silently holds an entry the other can never obtain.
                // `verify_signed_op` enforces the same limits at ingress, so this
                // is a store-invariant violation, not a peer's doing.
                Err(error) => {
                    debug_assert!(false, "lifted entry {} is unserializable: {error}", hash.to_hex());
                    None
                }
            })
            .collect();
        let mut index = Index::new(salt);
        for hash in records.keys() {
            index.insert(SortKey(hash.as_bytes().to_vec()), *hash);
        }
        let root = sync_root_of(records.keys());
        Self {
            records,
            index,
            root,
        }
    }

    /// Fold entries the store has just lifted into this snapshot.
    ///
    /// O(lifted), against [`Self::capture`]'s O(entries) topo sort plus full
    /// re-serialization plus full index rebuild — which the driver used to run
    /// once per `Entries` frame. Index, records and root move together, so the
    /// consistency invariant holds through the update as well as at capture.
    pub fn absorb(&mut self, store: &RoomStore, lifted: &[EntryHash]) {
        let mut changed = false;
        for hash in lifted {
            if self.records.contains_key(hash) {
                continue;
            }
            let Some((signed, predecessors)) = store.repair_record(hash) else {
                debug_assert!(false, "store reported lifting {} but has no record", hash.to_hex());
                continue;
            };
            let Ok(bytes) = signed.to_wire_bytes() else {
                debug_assert!(false, "lifted entry {} is unserializable", hash.to_hex());
                continue;
            };
            self.index.insert(SortKey(hash.as_bytes().to_vec()), *hash);
            self.records.insert(*hash, (bytes, predecessors));
            changed = true;
        }
        if changed {
            self.root = sync_root_of(self.records.keys());
        }
    }

    /// A clone of the index for `SyncSession::{initiate, accept, resume}`.
    pub fn index(&self) -> Index {
        self.index.clone()
    }

    /// The convergence digest to carry on `Done`.
    pub fn root(&self) -> [u8; 32] {
        self.root
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

impl EntrySource for RoomSyncSource {
    fn have(&self, hash: &EntryHash) -> bool {
        self.records.contains_key(hash)
    }

    /// The WHOLE causal closure of `hash`, ancestors first (post-order), always
    /// containing `hash` itself.
    ///
    /// The hardened kernel owns the byte discipline that used to live here:
    ///
    /// * `already_included` is now SESSION-scoped (the kernel's `sent` set), so
    ///   each entry's bytes travel at most once per session and honest transfer
    ///   is O(|union|). The walkie-side wave byte budget this replaced solved
    ///   the same blowup one layer too low, at the price of parking entries.
    /// * The kernel chunks the assembled answer across as many
    ///   `Entries { more }` frames as `budget.max_frame_bytes` requires, so an
    ///   unbounded closure can no longer blow the frame cap.
    /// * The kernel removes `hash` from `already_included` before every call,
    ///   so the liveness MUST — a served `have()` hash is ALWAYS in the answer
    ///   — stays satisfiable even after its bytes went out earlier: a peer
    ///   whose app refused a damaged delivery can re-ask and be re-served.
    ///
    /// Post-order still matters: ancestors skipped later via
    /// `already_included` were then emitted as part of a causally closed
    /// prefix, which is the receiver's only guarantee that nothing parks
    /// forever behind a predecessor that never ships.
    fn bytes_with_closure(
        &self,
        hash: &EntryHash,
        already_included: &mut BTreeSet<EntryHash>,
    ) -> Vec<(EntryHash, Vec<u8>)> {
        if !self.records.contains_key(hash) {
            return Vec::new();
        }

        // Iterative post-order DFS, so ancestors precede descendants.
        let mut order: Vec<EntryHash> = Vec::new();
        let mut visited: BTreeSet<EntryHash> = BTreeSet::new();
        let mut stack: Vec<(EntryHash, bool)> = vec![(*hash, false)];
        while let Some((candidate, expanded)) = stack.pop() {
            if expanded {
                order.push(candidate);
                continue;
            }
            if already_included.contains(&candidate) || !visited.insert(candidate) {
                continue;
            }
            let Some((_, predecessors)) = self.records.get(&candidate) else {
                continue;
            };
            stack.push((candidate, true));
            for predecessor in predecessors {
                stack.push((*predecessor, false));
            }
        }

        let mut output = Vec::new();
        for candidate in order {
            // The requested entry is appended separately below, unconditionally.
            if candidate == *hash || already_included.contains(&candidate) {
                continue;
            }
            let Some((bytes, _)) = self.records.get(&candidate) else {
                continue;
            };
            already_included.insert(candidate);
            output.push((candidate, bytes.clone()));
        }

        // The entry the peer actually asked for — the liveness MUST.
        if !already_included.contains(hash)
            && let Some((bytes, _)) = self.records.get(hash)
        {
            already_included.insert(*hash);
            output.push((*hash, bytes.clone()));
        }
        output
    }
}

/// The store side of one session, borrowed only for the duration of a single
/// call.
///
/// The driver must never hold the store across a network round trip: a durable
/// runtime keeps its `RoomStore` behind a lock that the room loop also needs, so
/// a `&mut RoomStore` held for a whole session freezes gossip ingest, local
/// commits and the UI for as long as the peer takes to answer. Implementations
/// re-acquire per call; `RoomStore` itself implements it directly for tests and
/// single-threaded runtimes.
pub trait SyncStoreAccess {
    /// Capture a consistent horizon.
    fn capture(&mut self, salt: [u8; 16]) -> impl Future<Output = RoomSyncSource>;

    /// Ingest peer entries through the production ingress and fold everything
    /// newly lifted into `source`, so the answer set and the advertised set move
    /// together. The returned verdict feeds `SyncSession::resume_admitted`
    /// directly — see [`SyncApply`] for what belongs in `admitted`.
    fn apply(
        &mut self,
        topic: &str,
        pairs: &[(EntryHash, Vec<u8>)],
        source: &mut RoomSyncSource,
    ) -> impl Future<Output = SyncApply>;
}

/// The app's verdict on one delivered `Entries` frame, in the exact shape
/// `SyncSession::resume_admitted` consumes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncApply {
    /// Every hash the store verified and KEPT — lifted into the index or parked
    /// behind a missing predecessor. Everything delivered and NOT named here is
    /// treated by the kernel as REFUSED and re-queued for fetching, so:
    ///
    /// * a pair that fails decode/verification is left out (refused — the peer
    ///   can re-serve the real bytes, or run into `max_fruitless_fetches`);
    /// * a pair whose verified op LIFTS reports the store-derived entry hash
    ///   (never the wire claim; if the two differ the wire hash stays refused
    ///   and is honestly re-asked);
    /// * a pair whose verified op PARKS reports the wire hash — a parked op
    ///   cannot resolve its `prevs` yet, so the wire claim is the only name it
    ///   has, and leaving it out would re-fetch bytes already in hand until
    ///   the fruitless-fetch budget killed an honest session;
    /// * an op already lifted or parked (gossip raced the session, duplicate
    ///   frame) reports the same way — it is KEPT, and marking it refused
    ///   would spin an honest re-serve loop into `Abort{"no progress"}`.
    ///
    /// Over-reporting is the dangerous direction (a hash named here is never
    /// asked for again this session), and nothing here over-reports: every
    /// admitted hash names an op the store actually holds.
    pub admitted: Vec<EntryHash>,
    /// Ops newly LIFTED — parked ops are not counted, since they are neither
    /// advertised nor servable. Outcome bookkeeping only.
    pub lifted: usize,
}

impl SyncStoreAccess for RoomStore {
    async fn capture(&mut self, salt: [u8; 16]) -> RoomSyncSource {
        RoomSyncSource::capture(self, salt)
    }

    async fn apply(
        &mut self,
        topic: &str,
        pairs: &[(EntryHash, Vec<u8>)],
        source: &mut RoomSyncSource,
    ) -> SyncApply {
        let report = ingest_pairs(self, topic, pairs);
        source.absorb(self, &report.lifted);
        SyncApply {
            admitted: report.admitted,
            lifted: report.lifted.len(),
        }
    }
}

/// What [`ingest_pairs`] did with one frame's pairs.
#[derive(Debug, Clone, Default)]
pub struct IngestReport {
    /// The admitted set for `resume_admitted` — see [`SyncApply::admitted`].
    pub admitted: Vec<EntryHash>,
    /// Entries newly LIFTED, for folding into the snapshot via
    /// [`RoomSyncSource::absorb`].
    pub lifted: Vec<EntryHash>,
}

/// Ingest peer-supplied entries through the production ingress only.
///
/// The entry is re-derived from the verified op by [`RoomStore`], never trusted
/// from the wire, so a peer cannot inject an unverified entry. Frames that fail
/// to decode or verify are dropped: a hostile peer wastes its own bandwidth,
/// and the kernel counts the un-admitted hash as refused (re-fetchable).
pub fn ingest_pairs(
    store: &mut RoomStore,
    topic: &str,
    pairs: &[(EntryHash, Vec<u8>)],
) -> IngestReport {
    let mut report = IngestReport::default();
    for (wire_hash, bytes) in pairs {
        let Ok(signed) = SignedOp::from_wire_bytes(bytes) else {
            continue;
        };
        let Ok(verified) = verify_signed_op_for_topic(&signed, topic) else {
            continue;
        };
        let id = verified.id();
        if let Some(entry) = store.lifted_entry(id) {
            // Already materialized (gossip raced the session, or a duplicate
            // pair): kept, under the entry hash the store derived for it.
            report.admitted.push(entry);
            continue;
        }
        if store.knows_op(id) {
            // Already parked: kept, and the wire claim is its only name until
            // its causal past resolves.
            report.admitted.push(*wire_hash);
            continue;
        }
        let newly = store.ingest_verified(verified);
        if newly.is_empty() {
            // Parked just now — kept, so it must be admitted or the kernel
            // re-fetches bytes already in hand until "no progress" aborts.
            report.admitted.push(*wire_hash);
        } else {
            // Lifted, possibly unblocking earlier parked deliveries; admit the
            // store-derived hashes (all of them are kept AND indexed).
            report.admitted.extend(newly.iter().copied());
            report.lifted.extend(newly);
        }
    }
    report
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("sync transport failed: {0}")]
    Transport(#[from] TransportError),
    #[error("sync session failed: {0}")]
    Session(String),
    #[error("sync frame could not be decoded: {0}")]
    Decode(String),
    #[error("peer opened a sync session we refused: {0}")]
    Rejected(String),
    #[error("outbound sync frame is {actual} bytes; the limit is {max}")]
    FrameTooLarge { actual: usize, max: usize },
    #[error("peer sent no frame within {}s", .after.as_secs())]
    TimedOut { after: Duration },
    #[error("session exchanged {frames} frames without completing")]
    Stalled { frames: usize },
}

impl From<SessionError> for SyncError {
    fn from(value: SessionError) -> Self {
        Self::Session(value.to_string())
    }
}

/// What one completed session did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncOutcome {
    /// Ops newly LIFTED through the production ingress. Parked ops are excluded:
    /// they are not yet part of the identity set peers reconcile over.
    pub ingested: usize,
    pub frames_sent: usize,
    pub frames_received: usize,
    /// The `Done` cross-check disagreed: both halves went idle while their entry
    /// sets differ. Diagnostic only — the next session repairs it. Never treated
    /// as a protocol error, but it is the ONLY signal that a session completed
    /// without converging, so callers should log it.
    pub root_mismatch: bool,
    /// The session ended without both halves exchanging `Done` (peer hung up,
    /// or a budget tripped). The store is still consistent; retry later.
    pub incomplete: bool,
}

/// Dial side: open with `Hello`, then pump to completion.
///
/// Takes the stream by value so it can be closed on every exit — success,
/// refusal, transport error, timeout.
pub async fn drive_initiator<S, T, K>(
    mut stream: S,
    timer: &T,
    store: &mut K,
    topic: &str,
    limits: SyncLimits,
) -> Result<SyncOutcome, SyncError>
where
    S: SyncStream,
    T: SyncTimer,
    K: SyncStoreAccess,
{
    let result = initiate(&mut stream, timer, store, topic, limits).await;
    stream.close().await;
    result
}

async fn initiate<S, T, K>(
    stream: &mut S,
    timer: &T,
    store: &mut K,
    topic: &str,
    limits: SyncLimits,
) -> Result<SyncOutcome, SyncError>
where
    S: SyncStream,
    T: SyncTimer,
    K: SyncStoreAccess,
{
    let mut outcome = SyncOutcome::default();
    // Fresh per session, never caller-supplied. The salt keys the range
    // fingerprints; reuse lets an adversary precompute two entries whose salted
    // leaves XOR to zero, so a divergent range prunes as "agreed" and both halves
    // reach `Done` silently unconverged — permanently, since the same collision
    // works in every later session too.
    let salt: [u8; 16] = rand::random();
    let mut source = store.capture(salt).await;
    let (session, opening) =
        SyncSession::initiate(sync_strategy(), source.index(), Config::default(), salt);
    let mut session = session.with_budget(limits.budget);
    // The INITIAL root only; after any ingest it rides `resume_admitted`.
    session.set_root(Some(source.root()));
    send_all(stream, &opening, &limits, &mut outcome).await?;
    pump(
        stream,
        timer,
        &mut session,
        &mut source,
        store,
        topic,
        limits,
        &mut outcome,
    )
    .await?;
    Ok(outcome)
}

/// Accept side: adopt the initiator's salt (the kernel's "initiator's salt wins"
/// rule), then pump to completion.
pub async fn drive_responder<S, T, K>(
    mut stream: S,
    timer: &T,
    store: &mut K,
    topic: &str,
    limits: SyncLimits,
) -> Result<SyncOutcome, SyncError>
where
    S: SyncStream,
    T: SyncTimer,
    K: SyncStoreAccess,
{
    let result = respond_to_dial(&mut stream, timer, store, topic, limits).await;
    stream.close().await;
    result
}

async fn respond_to_dial<S, T, K>(
    stream: &mut S,
    timer: &T,
    store: &mut K,
    topic: &str,
    limits: SyncLimits,
) -> Result<SyncOutcome, SyncError>
where
    S: SyncStream,
    T: SyncTimer,
    K: SyncStoreAccess,
{
    let mut outcome = SyncOutcome::default();
    let Some(first) = recv_frame(stream, timer, &limits).await? else {
        outcome.incomplete = true;
        return Ok(outcome);
    };
    outcome.frames_received += 1;
    let hello = match SyncMessage::decode(&first).map_err(|e| SyncError::Decode(e.to_string()))? {
        SyncMessage::Hello(hello) => hello,
        other => {
            // Never format the frame itself: an adversary chooses its size.
            let reason = format!("first frame was {}, expected Hello", frame_kind(&other));
            refuse(stream, &reason, &limits, &mut outcome).await;
            return Err(SyncError::Rejected(reason));
        }
    };
    // The index MUST be built under the initiator's salt.
    let salt = hello.session_salt;
    let mut source = store.capture(salt).await;
    let mut session = match accept_session(&hello, &source, limits) {
        Ok(session) => session,
        Err(reason) => {
            refuse(stream, &reason, &limits, &mut outcome).await;
            return Err(SyncError::Rejected(reason));
        }
    };
    // The INITIAL root only; after any ingest it rides `resume_admitted`.
    session.set_root(Some(source.root()));
    pump(
        stream,
        timer,
        &mut session,
        &mut source,
        store,
        topic,
        limits,
        &mut outcome,
    )
    .await?;
    Ok(outcome)
}

fn accept_session(
    hello: &SessionHello,
    source: &RoomSyncSource,
    limits: SyncLimits,
) -> Result<SyncSession, String> {
    SyncSession::accept(hello, sync_strategy(), source.index(), Config::default())
        .map(|session| session.with_budget(limits.budget))
}

/// Tell the peer why we are hanging up.
///
/// The kernel's contract for a refused `accept` is "the app sends `Abort{reason}`
/// and closes"; dropping the stream instead leaves the dialler blocked on a read
/// until its own deadline. Best effort — we are closing regardless.
async fn refuse<S: SyncStream>(
    stream: &mut S,
    reason: &str,
    limits: &SyncLimits,
    outcome: &mut SyncOutcome,
) {
    let abort = SyncMessage::Abort {
        reason: reason.to_owned(),
    };
    let _ = send_all(stream, std::slice::from_ref(&abort), limits, outcome).await;
}

/// A frame's variant name, for error text that an adversary cannot inflate.
fn frame_kind(message: &SyncMessage) -> &'static str {
    match message {
        SyncMessage::Hello(_) => "Hello",
        SyncMessage::Question { .. } => "Question",
        SyncMessage::Fetch(_) => "Fetch",
        SyncMessage::Entries { .. } => "Entries",
        SyncMessage::Done { .. } => "Done",
        SyncMessage::Abort { .. } => "Abort",
        SyncMessage::Ack(_) => "Ack",
    }
}

#[allow(clippy::too_many_arguments)]
async fn pump<S, T, K>(
    stream: &mut S,
    timer: &T,
    session: &mut SyncSession,
    source: &mut RoomSyncSource,
    store: &mut K,
    topic: &str,
    limits: SyncLimits,
    outcome: &mut SyncOutcome,
) -> Result<(), SyncError>
where
    S: SyncStream,
    T: SyncTimer,
    K: SyncStoreAccess,
{
    // The kernel's `max_rounds` bounds frames it processes; this bounds frames we
    // exchange, so a driver bug that keeps both halves talking without converging
    // is a loud error rather than an endless conversation. It does NOT cover a
    // peer that stops talking — nothing here advances while blocked on a read —
    // which is what `recv_timeout` is for.
    let iteration_cap = limits.budget.max_rounds.saturating_mul(4).max(1_024);
    let mut iterations = 0_usize;
    loop {
        // The kernel close rule: close the stream the moment the status is no
        // longer Exchanging — NEVER wait on `is_complete()`, which a
        // root-divergent run denies forever while both halves sit idle.
        match session.status() {
            SessionStatus::Exchanging => {}
            SessionStatus::Complete => return Ok(()),
            SessionStatus::Divergent => {
                // Finished, but the `Done{root}` cross-check caught the peer's
                // store disagreeing. Close; the periodic anti-entropy re-syncs.
                outcome.root_mismatch = true;
                return Ok(());
            }
            SessionStatus::Aborted => {
                // A budget tripped or the peer was refused mid-exchange (the
                // Abort frame itself already went out via `output.send`). The
                // store is still consistent; retry later.
                outcome.incomplete = true;
                return Ok(());
            }
        }
        iterations += 1;
        if iterations > iteration_cap {
            return Err(SyncError::Stalled {
                frames: iteration_cap,
            });
        }
        let Some(frame) = recv_frame(stream, timer, &limits).await? else {
            // Peer hung up without the closing handshake.
            outcome.incomplete = true;
            outcome.root_mismatch = session.root_divergence();
            return Ok(());
        };
        outcome.frames_received += 1;
        let message = SyncMessage::decode(&frame).map_err(|e| SyncError::Decode(e.to_string()))?;
        let answered_a_fetch = matches!(message, SyncMessage::Entries { .. });

        let output = session.on_message(message, &*source)?;
        send_all(stream, &output.send, &limits, outcome).await?;

        if answered_a_fetch {
            // EVERY `Entries` frame must be followed by `resume_admitted` —
            // including an empty final one, which is how a peer says "I hold
            // none of those" and still retires a `Fetch`; only the resume
            // drains the `Items` the kernel is holding behind that wave. The
            // `admitted` set is the store's verdict on this delivery (verified
            // AND kept — lifted or parked; see [`SyncApply::admitted`]), and
            // the root is derived from the SAME post-ingest state as the index
            // riding beside it, so the finalizing `Done` can never advertise a
            // stale root.
            let admitted = if output.ingest.is_empty() {
                Vec::new()
            } else {
                let applied = store.apply(topic, &output.ingest, source).await;
                outcome.ingested += applied.lifted;
                applied.admitted
            };
            let more = session.resume_admitted(source.index(), &admitted, Some(source.root()))?;
            send_all(stream, &more, &limits, outcome).await?;
        }
    }
}

/// One inbound frame, or [`SyncError::TimedOut`].
///
/// Cancelling the read on timeout leaves the stream unusable, which is fine and
/// intended: every caller closes it immediately afterwards.
async fn recv_frame<S: SyncStream, T: SyncTimer>(
    stream: &mut S,
    timer: &T,
    limits: &SyncLimits,
) -> Result<Option<Vec<u8>>, SyncError> {
    let receive = stream.recv_frame();
    let deadline = timer.sleep(limits.recv_timeout);
    futures::pin_mut!(receive);
    futures::pin_mut!(deadline);
    // `select` polls the read first, so a frame already in hand always wins over
    // an elapsed deadline.
    match select(receive, deadline).await {
        Either::Left((frame, _)) => Ok(frame?),
        Either::Right(((), _)) => Err(SyncError::TimedOut {
            after: limits.recv_timeout,
        }),
    }
}

async fn send_all<S: SyncStream>(
    stream: &mut S,
    messages: &[SyncMessage],
    limits: &SyncLimits,
    outcome: &mut SyncOutcome,
) -> Result<(), SyncError> {
    for message in messages {
        let bytes = message.encode();
        if bytes.len() > limits.max_frame_bytes {
            return Err(SyncError::FrameTooLarge {
                actual: bytes.len(),
                max: limits.max_frame_bytes,
            });
        }
        stream.send_frame(&bytes).await?;
        outcome.frames_sent += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::loopback::loopback_pair;
    use crate::net::{Transport, TransportEvent};
    use crate::room::ops::{MAX_EMOJI_PALETTE_BYTES, WalkieOp, signing_key_from_seed};
    use crate::tuning::{TunedDegree, Tuning, TuningDefinition};
    use hhhs_core::encoding::Digest;
    use hhhs_core::reconciliation::{KeyRange, Message};

    const TOPIC: &str = "sync-driver-test";

    // ---------------------------------------------------------------------
    // Timers. Deterministic, no wall clock: the deadline is exercised by
    // controlling whether the sleep resolves, never by waiting.
    // ---------------------------------------------------------------------

    /// Never fires. Any test that completes under this proves it did not need
    /// the deadline.
    struct NoTimeout;
    impl SyncTimer for NoTimeout {
        async fn sleep(&self, _duration: Duration) {
            futures::future::pending::<()>().await
        }
    }

    /// Already elapsed. `recv_frame` polls the read first, so buffered frames
    /// still arrive and only a genuinely blocking read trips the deadline.
    struct Expired;
    impl SyncTimer for Expired {
        async fn sleep(&self, _duration: Duration) {}
    }

    fn add_degrees(store: &mut RoomStore, seed: &[u8; 32], degrees: &[u16], start_ts: u64) {
        let key = signing_key_from_seed(seed);
        let tuning = Tuning::twelve_tet();
        for (offset, degree) in degrees.iter().enumerate() {
            store.commit(
                &key,
                TOPIC,
                start_ts + offset as u64,
                WalkieOp::AddDegree {
                    pitch: TunedDegree::new(&tuning, *degree).unwrap(),
                },
            );
        }
    }

    /// `count` distinct ops from one author. Ops are distinct by sequence number
    /// even when the degree repeats, so this builds an arbitrarily long causal
    /// chain within 12-TET's twelve legal degrees.
    fn add_chain(store: &mut RoomStore, seed: &[u8; 32], count: u16) {
        let degrees: Vec<u16> = (0..count).map(|i| i % 12).collect();
        add_degrees(store, seed, &degrees, 1);
    }

    /// Run both halves of one session over a loopback pair to completion.
    fn run_session(
        left: &mut RoomStore,
        right: &mut RoomStore,
        limits: SyncLimits,
    ) -> (Result<SyncOutcome, SyncError>, Result<SyncOutcome, SyncError>) {
        let (a, mut b) = loopback_pair();
        futures::executor::block_on(async {
            // The dial side opens a stream; the accept side sees SyncRequested.
            let initiator_stream = a.open_sync(a.remote_id()).await.unwrap();
            // Dispatch on event type exactly as the real room loop does: the
            // pair is seeded with a `PeerUp` ahead of the sync request.
            let responder_stream = loop {
                match b.next_event().await {
                    Some(TransportEvent::SyncRequested { stream, .. }) => break stream,
                    Some(TransportEvent::PeerUp { .. }) => continue,
                    other => panic!("expected SyncRequested, got {other:?}"),
                }
            };

            let initiator = drive_initiator(initiator_stream, &NoTimeout, left, TOPIC, limits);
            let responder = drive_responder(responder_stream, &NoTimeout, right, TOPIC, limits);
            futures::future::join(initiator, responder).await
        })
    }

    fn run_ok(
        left: &mut RoomStore,
        right: &mut RoomStore,
        limits: SyncLimits,
    ) -> (SyncOutcome, SyncOutcome) {
        let (l, r) = run_session(left, right, limits);
        (l.unwrap(), r.unwrap())
    }

    fn assert_converged(left: &RoomStore, right: &RoomStore) {
        assert_eq!(
            left.entry_hashes(),
            right.entry_hashes(),
            "entry-hash identity sets must match after sync"
        );
        assert_eq!(left.view(), right.view(), "read models must match after sync");
        assert_eq!(left.pending_len(), 0, "left must fully drain");
        assert_eq!(right.pending_len(), 0, "right must fully drain");
        assert_eq!(
            left.sync_root(),
            right.sync_root(),
            "convergence digests must match after sync"
        );
    }

    // ---------------------------------------------------------------------
    // Convergence
    // ---------------------------------------------------------------------

    #[test]
    fn diverged_stores_converge() {
        let mut left = RoomStore::new();
        let mut right = RoomStore::new();
        add_degrees(&mut left, &[1; 32], &[0, 2, 4], 1);
        add_degrees(&mut right, &[2; 32], &[5, 7, 9], 100);

        let before_left = left.entry_hashes().len();
        let before_right = right.entry_hashes().len();
        assert_ne!(left.entry_hashes(), right.entry_hashes());

        let (l, r) = run_ok(&mut left, &mut right, SyncLimits::default());

        assert_converged(&left, &right);
        assert_eq!(left.entry_hashes().len(), before_left + before_right);
        assert!(!l.incomplete && !r.incomplete, "both halves must finish");
        assert!(l.ingested > 0 && r.ingested > 0);
        // Meaningful now that both halves carry a root: the cross-check ran and
        // agreed. `root_mismatch_is_detected_on_done` proves it can also fail.
        assert!(!l.root_mismatch && !r.root_mismatch);
    }

    #[test]
    fn late_joiner_from_empty_converges() {
        let mut established = RoomStore::new();
        add_chain(&mut established, &[3; 32], 24);
        let mut joiner = RoomStore::new();
        assert!(joiner.is_empty());

        let (_, r) = run_ok(&mut established, &mut joiner, SyncLimits::default());

        assert_converged(&established, &joiner);
        assert_eq!(joiner.entry_hashes().len(), 24);
        assert_eq!(r.ingested, 24, "ingested counts lifted ops");
        assert!(!r.incomplete);
    }

    /// Narrow fetch waves: with `fetch_max_hashes` forced tiny the joiner's
    /// wave queues and drains over many `Fetch`es, later answers re-serve only
    /// their explicitly requested hashes (session-scoped send-dedup), and the
    /// duplicate-delivery `admitted` path runs for real. The frame counts are
    /// the non-vacuity check, calibrated against the same corpus under default
    /// limits.
    #[test]
    fn late_joiner_converges_with_narrow_fetch_waves() {
        let corpus = |store: &mut RoomStore| add_chain(store, &[4; 32], 32);

        let mut baseline_source = RoomStore::new();
        corpus(&mut baseline_source);
        let mut baseline_joiner = RoomStore::new();
        let (_, baseline) = run_ok(
            &mut baseline_source,
            &mut baseline_joiner,
            SyncLimits::default(),
        );

        let mut established = RoomStore::new();
        corpus(&mut established);
        let mut joiner = RoomStore::new();
        let limits = SyncLimits {
            budget: SessionBudget {
                // One `Fetch` can never name the whole chain, so the wave
                // spans many fetches and answers overlap.
                fetch_max_hashes: 3,
                ..SyncLimits::default().budget
            },
            ..SyncLimits::default()
        };
        let (_, r) = run_ok(&mut established, &mut joiner, limits);

        assert_converged(&established, &joiner);
        assert_eq!(joiner.entry_hashes().len(), 32);
        assert!(!r.incomplete, "narrow waves must still converge");
        assert!(
            r.frames_received > baseline.frames_received,
            "narrow waves must actually cost round trips: {} vs baseline {}",
            r.frames_received,
            baseline.frames_received
        );
    }

    #[test]
    fn already_converged_stores_exchange_nothing() {
        let mut left = RoomStore::new();
        add_degrees(&mut left, &[5; 32], &[1, 3], 1);
        let mut right = RoomStore::new();
        add_degrees(&mut right, &[5; 32], &[1, 3], 1);
        assert_eq!(left.entry_hashes(), right.entry_hashes());

        let (l, r) = run_ok(&mut left, &mut right, SyncLimits::default());

        assert_converged(&left, &right);
        assert_eq!(l.ingested, 0, "nothing to transfer");
        assert_eq!(r.ingested, 0, "nothing to transfer");
        assert!(!l.root_mismatch && !r.root_mismatch);
    }

    /// The kernel chunks one logical `Entries` answer across as many
    /// `{ pairs, more }` frames as `budget.max_frame_bytes` requires, so a
    /// whole-DAG closure and a small frame cap coexist — the driver-side
    /// ancestor byte budget this replaced is gone. The driver's own hard cap
    /// sits at the same value and never trips, because the kernel's size model
    /// deliberately over-estimates.
    #[test]
    fn entries_answers_are_chunked_to_the_frame_cap() {
        let corpus = |store: &mut RoomStore| add_chain(store, &[8; 32], 40);

        let mut baseline_source = RoomStore::new();
        corpus(&mut baseline_source);
        let mut baseline_joiner = RoomStore::new();
        let (_, baseline) = run_ok(
            &mut baseline_source,
            &mut baseline_joiner,
            SyncLimits::default(),
        );

        let mut established = RoomStore::new();
        corpus(&mut established);
        let one_op = op_wire_len(&established);
        let mut joiner = RoomStore::new();

        // Room for a handful of ops per frame — nowhere near the 40-op chain
        // one answer's closure carries — while still fitting the widest
        // non-`Entries` frame (a 40-hash `Items`/`Fetch` is ~1.3 KiB).
        let cap = (one_op + 64) * 4 + 2_048;

        let limits = SyncLimits {
            budget: SessionBudget {
                max_frame_bytes: cap,
                ..SyncLimits::default().budget
            },
            max_frame_bytes: cap,
            ..SyncLimits::default()
        };
        let (l, r) = run_ok(&mut established, &mut joiner, limits);

        assert_converged(&established, &joiner);
        assert_eq!(joiner.entry_hashes().len(), 40);
        assert!(!l.incomplete && !r.incomplete);
        assert!(
            r.frames_received > baseline.frames_received,
            "a sub-closure frame cap must actually chunk: {} vs baseline {}",
            r.frames_received,
            baseline.frames_received
        );
    }

    // ---------------------------------------------------------------------
    // Liveness against a peer that misbehaves
    // ---------------------------------------------------------------------

    /// A scripted peer: hands the driver canned frames, records what it sends,
    /// and then goes silent (never yields another frame).
    struct ScriptedPeer {
        inbox: Vec<Vec<u8>>,
        sent: Vec<SyncMessage>,
        closed: bool,
        /// `false` = go silent after the script (blocks forever, like a peer
        /// that stops talking); `true` = clean EOF.
        eof_when_done: bool,
    }

    impl ScriptedPeer {
        fn silent(inbox: Vec<SyncMessage>) -> Self {
            Self {
                inbox: inbox.iter().map(SyncMessage::encode).collect(),
                sent: Vec::new(),
                closed: false,
                eof_when_done: false,
            }
        }

        fn hanging_up(inbox: Vec<SyncMessage>) -> Self {
            Self {
                eof_when_done: true,
                ..Self::silent(inbox)
            }
        }

        fn kinds(&self) -> Vec<&'static str> {
            self.sent.iter().map(frame_kind).collect()
        }
    }

    impl SyncStream for &mut ScriptedPeer {
        async fn send_frame(&mut self, frame: &[u8]) -> Result<(), TransportError> {
            self.sent.push(SyncMessage::decode(frame).expect("driver emits valid frames"));
            Ok(())
        }

        async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
            if self.inbox.is_empty() {
                if self.eof_when_done {
                    return Ok(None);
                }
                return futures::future::pending().await;
            }
            Ok(Some(self.inbox.remove(0)))
        }

        async fn close(self) {
            self.closed = true;
        }
    }

    fn bogus_hash(byte: u8) -> EntryHash {
        EntryHash(Digest([byte; 32]))
    }

    /// F3. A peer advertises a hash it cannot serve and answers the resulting
    /// `Fetch` with an empty final `Entries`. The kernel has already
    /// decremented the outstanding-fetch count, so unless the driver calls
    /// `resume_admitted` anyway the held `Items` is never drained and the
    /// session can never finish.
    #[test]
    fn an_empty_entries_answer_still_completes_the_session() {
        let mut store = RoomStore::new();
        add_degrees(&mut store, &[9; 32], &[0, 1, 2], 1);

        let mut peer = ScriptedPeer::hanging_up(vec![
            // Acknowledge the driver's opening (question 0)...
            SyncMessage::Ack(0),
            // ...advertise a hash we cannot serve...
            SyncMessage::Question {
                id: 0,
                msg: Message::Items(KeyRange::full(), vec![bogus_hash(0xEE)]),
            },
            // ...answer the resulting `Fetch` with nothing...
            SyncMessage::Entries {
                pairs: Vec::new(),
                more: false,
            },
            // ...and acknowledge the driver's answer to our `Items` (its
            // question 1), so its outstanding-question ledger can drain.
            SyncMessage::Ack(1),
            SyncMessage::Done { root: None },
        ]);
        let outcome = futures::executor::block_on(drive_initiator(
            &mut peer,
            &NoTimeout,
            &mut store,
            TOPIC,
            SyncLimits::default(),
        ))
        .expect("an unserved fetch is not an error");

        assert_eq!(outcome.ingested, 0);
        assert!(
            peer.kinds().contains(&"Done"),
            "driver must resume and close the handshake, sent {:?}",
            peer.kinds()
        );
        assert!(
            !outcome.incomplete,
            "both halves exchanged Done; the session did not strand"
        );
        assert!(peer.closed, "the stream must be closed on the success path");
    }

    /// F2. A peer that simply stops talking is a loud error, not a hung task.
    #[test]
    fn a_silent_peer_trips_the_receive_deadline() {
        let mut store = RoomStore::new();
        add_degrees(&mut store, &[10; 32], &[0, 1], 1);

        // No frames at all: the driver blocks on the first read.
        let mut peer = ScriptedPeer::silent(Vec::new());
        let error = futures::executor::block_on(drive_initiator(
            &mut peer,
            &Expired,
            &mut store,
            TOPIC,
            SyncLimits::default(),
        ))
        .expect_err("a silent peer must not hang");

        assert!(
            matches!(error, SyncError::TimedOut { .. }),
            "expected a timeout, got {error:?}"
        );
        assert!(peer.closed, "the stream must be closed on the timeout path");
    }

    /// The deadline must not fire while frames are actually flowing: `select`
    /// polls the read first, so an already-elapsed timer still delivers buffered
    /// frames.
    #[test]
    fn an_elapsed_deadline_does_not_discard_buffered_frames() {
        let mut store = RoomStore::new();
        let mut peer = ScriptedPeer::hanging_up(vec![
            SyncMessage::Ack(0),
            SyncMessage::Done { root: None },
        ]);
        let outcome = futures::executor::block_on(drive_initiator(
            &mut peer,
            &Expired,
            &mut store,
            TOPIC,
            SyncLimits::default(),
        ))
        .expect("a buffered frame beats an elapsed deadline");
        assert!(!outcome.incomplete);
    }

    /// F8. A non-`Hello` opener is refused with an `Abort` on the wire, per the
    /// kernel contract — not by silently dropping the stream.
    #[test]
    fn a_non_hello_opener_is_refused_with_an_abort() {
        let mut store = RoomStore::new();
        let mut peer = ScriptedPeer::hanging_up(vec![SyncMessage::Fetch(vec![bogus_hash(1)])]);
        let error = futures::executor::block_on(drive_responder(
            &mut peer,
            &NoTimeout,
            &mut store,
            TOPIC,
            SyncLimits::default(),
        ))
        .expect_err("a non-Hello opener must be refused");

        assert!(matches!(error, SyncError::Rejected(_)), "got {error:?}");
        assert_eq!(peer.kinds(), vec!["Abort"], "the peer must be told why");
        assert!(peer.closed);
        // The reason must not echo attacker-chosen bytes back into a log line.
        assert!(format!("{error}").contains("Fetch"));
    }

    /// F8. Same for a strategy mismatch — the other refusal the kernel defines.
    #[test]
    fn a_strategy_mismatch_is_refused_with_an_abort() {
        let mut store = RoomStore::new();
        let mut peer = ScriptedPeer::hanging_up(vec![SyncMessage::Hello(SessionHello {
            strategy: StrategyId::new("some-other-protocol", 7),
            session_salt: [3; 16],
        })]);
        let error = futures::executor::block_on(drive_responder(
            &mut peer,
            &NoTimeout,
            &mut store,
            TOPIC,
            SyncLimits::default(),
        ))
        .expect_err("a foreign strategy must be refused");

        assert!(matches!(error, SyncError::Rejected(_)), "got {error:?}");
        assert_eq!(peer.kinds(), vec!["Abort"]);
        assert!(peer.closed);
    }

    // ---------------------------------------------------------------------
    // The Done cross-check and the session salt
    // ---------------------------------------------------------------------

    /// F4. The cross-check is armed: a peer whose root disagrees finishes the
    /// session `Divergent` — closed, flagged for re-sync, never an error and
    /// never a hang (the close rule is `status() != Exchanging`, not
    /// `is_complete()`, which a divergent run denies forever).
    #[test]
    fn root_mismatch_is_detected_on_done() {
        let mut store = RoomStore::new();
        add_degrees(&mut store, &[12; 32], &[0, 1], 1);

        let mut agreeing = ScriptedPeer::hanging_up(vec![
            SyncMessage::Ack(0),
            SyncMessage::Done {
                root: Some(store.sync_root()),
            },
        ]);
        let matched = futures::executor::block_on(drive_initiator(
            &mut agreeing,
            &NoTimeout,
            &mut store,
            TOPIC,
            SyncLimits::default(),
        ))
        .unwrap();
        assert!(!matched.root_mismatch, "identical roots must agree");
        assert!(!matched.incomplete, "an agreeing run finishes cleanly");

        let mut lying = ScriptedPeer::hanging_up(vec![
            SyncMessage::Ack(0),
            SyncMessage::Done { root: Some([9; 32]) },
        ]);
        let mismatched = futures::executor::block_on(drive_initiator(
            &mut lying,
            &NoTimeout,
            &mut store,
            TOPIC,
            SyncLimits::default(),
        ))
        .unwrap();
        assert!(
            mismatched.root_mismatch,
            "a Done carrying a different root is a silent-divergence signal"
        );
        assert!(
            !mismatched.incomplete,
            "a divergent run still finished; it must close, not hang or error"
        );
        assert!(lying.closed, "the stream must be closed on the divergent path");
    }

    #[test]
    fn the_snapshot_root_matches_the_store_root() {
        let mut store = RoomStore::new();
        add_degrees(&mut store, &[13; 32], &[0, 4, 7], 1);
        let source = RoomSyncSource::capture(&store, [0; 16]);
        assert_eq!(
            source.root(),
            store.sync_root(),
            "the value we put on the wire must be the value the store computes"
        );

        // And it tracks an absorb, not just a capture.
        let mut donor = RoomStore::new();
        add_degrees(&mut donor, &[14; 32], &[2], 500);
        let pairs: Vec<(EntryHash, Vec<u8>)> = donor
            .repair_records()
            .into_iter()
            .map(|(hash, (signed, _))| (hash, signed.to_wire_bytes().unwrap()))
            .collect();
        let mut source = source;
        let report = ingest_pairs(&mut store, TOPIC, &pairs);
        assert_eq!(report.lifted.len(), 1);
        assert_eq!(
            report.admitted, report.lifted,
            "one clean lifted delivery: admitted is exactly the lifted hash"
        );
        source.absorb(&store, &report.lifted);
        assert_eq!(source.root(), store.sync_root());
        assert_eq!(source.len(), store.entry_hashes().len());
    }

    /// The `admitted` derivation for `resume_admitted`, at the unit it lives
    /// at: verified-and-KEPT hashes only — lifted ops under their store-derived
    /// entry hash, parked ops under the only name they have (the wire claim),
    /// duplicates as kept — and garbage left out (refused, re-fetchable).
    #[test]
    fn admitted_reports_kept_ops_indexed_or_parked() {
        // A two-op chain from one author: the child cannot lift without the
        // parent.
        let mut donor = RoomStore::new();
        add_degrees(&mut donor, &[23; 32], &[0, 1], 1);
        let records = donor.repair_records();
        let mut in_causal_order: Vec<(EntryHash, Vec<u8>)> = donor
            .signed_ops()
            .into_iter()
            .map(|(hash, signed)| (hash, signed.to_wire_bytes().unwrap()))
            .collect();
        // Parent first = fewer prevs.
        in_causal_order.sort_by_key(|(hash, _)| records[hash].1.len());
        let (parent, child) = (in_causal_order[0].clone(), in_causal_order[1].clone());

        let mut store = RoomStore::new();

        // Garbage is refused: not admitted, so the kernel may re-ask.
        let garbage = vec![(bogus_hash(0xAA), vec![0xFF; 16])];
        let report = ingest_pairs(&mut store, TOPIC, &garbage);
        assert!(report.admitted.is_empty(), "garbage must never be admitted");
        assert!(report.lifted.is_empty());

        // The child alone parks — kept, so it MUST be admitted (under the wire
        // claim; a parked op cannot resolve its entry hash yet), or the kernel
        // would re-fetch bytes already in hand until "no progress" aborts.
        let report = ingest_pairs(&mut store, TOPIC, std::slice::from_ref(&child));
        assert_eq!(report.admitted, vec![child.0], "a parked op is kept");
        assert!(report.lifted.is_empty(), "parked is not lifted");
        assert_eq!(store.pending_len(), 1);

        // The parent lifts itself AND drains the parked child; both are
        // admitted under their store-derived hashes.
        let report = ingest_pairs(&mut store, TOPIC, std::slice::from_ref(&parent));
        assert_eq!(report.lifted.len(), 2, "parent + drained child lift");
        assert_eq!(report.admitted, report.lifted);
        assert_eq!(store.pending_len(), 0);

        // A duplicate delivery of an already-lifted op is KEPT (admitted under
        // the derived hash), never refused — marking it refused would spin an
        // honest re-serve loop into `Abort{"no progress"}`.
        let report = ingest_pairs(&mut store, TOPIC, std::slice::from_ref(&parent));
        assert_eq!(report.admitted, vec![parent.0]);
        assert!(report.lifted.is_empty(), "nothing new lifts");
    }

    /// F9. The salt is generated per session, never caller-supplied, so a
    /// fingerprint collision cannot be precomputed once and reused forever.
    #[test]
    fn every_session_draws_a_fresh_salt() {
        fn salt_of_first_hello(store: &mut RoomStore) -> [u8; 16] {
            let mut peer = ScriptedPeer::hanging_up(Vec::new());
            let _ = futures::executor::block_on(drive_initiator(
                &mut peer,
                &NoTimeout,
                store,
                TOPIC,
                SyncLimits::default(),
            ));
            match peer.sent.first() {
                Some(SyncMessage::Hello(hello)) => hello.session_salt,
                other => panic!("expected a Hello first, got {other:?}"),
            }
        }

        let mut store = RoomStore::new();
        add_degrees(&mut store, &[15; 32], &[0], 1);
        let first = salt_of_first_hello(&mut store);
        let second = salt_of_first_hello(&mut store);
        assert_ne!(first, second, "a reused salt is a permanent collision oracle");
        assert_ne!(first, [0; 16]);
    }

    // ---------------------------------------------------------------------
    // The closure contract (whole closure, session-scoped dedup)
    // ---------------------------------------------------------------------

    /// The deepest closure in the store, i.e. the causal tip.
    fn causal_tip(source: &RoomSyncSource) -> EntryHash {
        *source
            .records
            .keys()
            .max_by_key(|hash| {
                let mut included = BTreeSet::new();
                source.bytes_with_closure(hash, &mut included).len()
            })
            .expect("store must not be empty")
    }

    #[test]
    fn closure_is_whole_ancestors_first_and_deduplicated() {
        let mut store = RoomStore::new();
        add_degrees(&mut store, &[6; 32], &[0, 1, 2], 1);
        let source = RoomSyncSource::capture(&store, [0; 16]);

        // The frontier entry's closure is the whole three-op chain: the source
        // returns everything and lets the kernel chunk it to frames.
        let tip = causal_tip(&source);
        let mut included = BTreeSet::new();
        let full = source.bytes_with_closure(&tip, &mut included);
        assert_eq!(full.len(), 3, "the whole causal closure ships");
        // Every emitted entry's predecessors precede it.
        let mut emitted: BTreeSet<EntryHash> = BTreeSet::new();
        for (hash, _) in &full {
            for predecessor in &source.records[hash].1 {
                assert!(
                    emitted.contains(predecessor),
                    "ancestors must be emitted before descendants"
                );
            }
            emitted.insert(*hash);
        }

        // `already_included` is the kernel's SESSION-scoped sent set: with the
        // closure already shipped, a re-serve carries the requested hash and
        // nothing else. (The kernel removes the requested hash from the set
        // before every call — that is what makes re-serving possible at all.)
        included.remove(&tip);
        let again = source.bytes_with_closure(&tip, &mut included);
        assert_eq!(again.len(), 1, "ancestors are deduplicated, the ask is not");
        assert_eq!(again[0].0, tip, "a served hash is always in its own answer");
    }

    /// One op's wire size, for tests that need to reason in whole entries.
    fn op_wire_len(store: &RoomStore) -> usize {
        store
            .repair_records()
            .values()
            .next()
            .unwrap()
            .0
            .to_wire_bytes()
            .unwrap()
            .len()
    }

    /// Replay what the kernel's fetch answering does — ONE session-scoped
    /// `already_included` across every requested hash, cleared of the requested
    /// hash before each call — and collect what went on the wire.
    fn answer_session(
        source: &RoomSyncSource,
        requested: &[EntryHash],
    ) -> (BTreeSet<EntryHash>, usize) {
        let mut included = BTreeSet::new();
        let mut delivered = BTreeSet::new();
        let mut total_pairs = 0_usize;
        for hash in requested {
            // The kernel's re-serve exemption.
            included.remove(hash);
            for (emitted, _) in source.bytes_with_closure(hash, &mut included) {
                delivered.insert(emitted);
                total_pairs += 1;
            }
        }
        (delivered, total_pairs)
    }

    /// The dedup half of the O(|union|) transfer guarantee: honoring
    /// `already_included` across a whole session means a deep chain's ancestry
    /// costs the room once, plus at most one re-serve per explicitly requested
    /// hash — C <= 2, never O(fetches x depth).
    #[test]
    fn a_session_ships_each_entrys_bytes_at_most_once() {
        let mut store = RoomStore::new();
        add_chain(&mut store, &[16; 32], 40);
        let requested: Vec<EntryHash> = store.entry_hashes().into_iter().collect();
        let source = RoomSyncSource::capture(&store, [0; 16]);

        let (delivered, total_pairs) = answer_session(&source, &requested);
        assert_eq!(delivered.len(), 40, "the union is the whole DAG");
        assert!(
            total_pairs <= 2 * 40,
            "shipped {total_pairs} pairs for a 40-op room; the bound is 2x"
        );
    }

    /// The liveness MUST: an explicitly requested, held hash is in its answer
    /// whatever already shipped — or the peer's `missing` set never shrinks,
    /// it re-requests the identical set, and the session spins until a budget
    /// kills it.
    #[test]
    fn every_requested_hash_is_always_delivered() {
        let mut store = RoomStore::new();
        add_chain(&mut store, &[17; 32], 24);
        let requested: Vec<EntryHash> = store.entry_hashes().into_iter().collect();
        let source = RoomSyncSource::capture(&store, [0; 16]);

        let mut included = BTreeSet::new();
        for hash in &requested {
            included.remove(hash);
            let answer = source.bytes_with_closure(hash, &mut included);
            assert!(
                answer.iter().any(|(emitted, _)| emitted == hash),
                "a served hash must appear in its own answer even after its \
                 bytes already shipped"
            );
        }
    }

    /// The safety condition that makes session-scoped dedup sound: the union of
    /// everything emitted stays causally CLOSED, so whatever a receiver holds
    /// mid-session, every parked entry's past is already on the wire. Feeding
    /// the answers into an empty store in arrival order must lift everything.
    #[test]
    fn deduplicated_answers_are_still_a_liftable_stream() {
        let mut store = RoomStore::new();
        add_chain(&mut store, &[22; 32], 16);
        let requested: Vec<EntryHash> = store.entry_hashes().into_iter().collect();
        let source = RoomSyncSource::capture(&store, [0; 16]);

        let mut joiner = RoomStore::new();
        let mut included = BTreeSet::new();
        for hash in &requested {
            included.remove(hash);
            let pairs = source.bytes_with_closure(hash, &mut included);
            ingest_pairs(&mut joiner, TOPIC, &pairs);
        }
        assert_eq!(joiner.entry_hashes().len(), 16, "every entry lifts");
        assert_eq!(joiner.pending_len(), 0, "nothing may stay parked");
    }

    // ---------------------------------------------------------------------
    // The size ladder
    // ---------------------------------------------------------------------

    /// F5. The largest legal op must be syncable. It was not: the payload cap
    /// (2 MiB) sat above the sync frame cap (1 MiB), so one big-but-legal op —
    /// a large SCL tuning, a full emoji palette — permanently poisoned
    /// anti-entropy for the whole room.
    #[test]
    fn a_large_op_syncs() {
        let mut established = RoomStore::new();
        let key = signing_key_from_seed(&[18; 32]);
        // A genuinely large op: a full emoji palette, at the schema's own cap.
        let palette: String = std::iter::repeat_n('🎹', MAX_EMOJI_PALETTE_BYTES / 4 - 1).collect();
        established.commit(
            &key,
            TOPIC,
            1,
            WalkieOp::SetConfig {
                pieces_locked: Some(true),
                available_emojis: Some(palette.clone()),
            },
        );
        let wire = established
            .repair_records()
            .values()
            .next()
            .unwrap()
            .0
            .to_wire_bytes()
            .unwrap()
            .len();
        assert!(wire > MAX_EMOJI_PALETTE_BYTES / 2, "op should be sizeable");

        let mut joiner = RoomStore::new();
        let (_, r) = run_ok(&mut established, &mut joiner, SyncLimits::default());
        assert_converged(&established, &joiner);
        assert_eq!(joiner.view().available_emojis.as_deref(), Some(&*palette));
        assert!(!r.incomplete);
    }

    // The size ladder is enforced by the `const` assertions at the top of this
    // module and in `net::native`, so a constant edit that reintroduces F5 fails
    // the BUILD rather than a test run. Nothing to assert at runtime.

    #[test]
    fn an_oversize_frame_is_refused_rather_than_sent() {
        // A frame cap below one op is unsatisfiable by construction, and the
        // driver must say so instead of writing it.
        let mut established = RoomStore::new();
        add_chain(&mut established, &[19; 32], 4);
        let mut joiner = RoomStore::new();
        let limits = SyncLimits {
            max_frame_bytes: 64,
            ..SyncLimits::default()
        };
        let (l, r) = run_session(&mut established, &mut joiner, limits);
        assert!(
            matches!(l, Err(SyncError::FrameTooLarge { .. }))
                || matches!(r, Err(SyncError::FrameTooLarge { .. })),
            "expected a FrameTooLarge, got {l:?} / {r:?}"
        );
    }

    /// A large SCL tuning definition round-trips too — the payload the gossip
    /// cap's own comment says it is sized for.
    #[test]
    fn a_large_tuning_definition_syncs() {
        let mut established = RoomStore::new();
        let key = signing_key_from_seed(&[20; 32]);
        established.commit(
            &key,
            TOPIC,
            1,
            WalkieOp::SetTuning {
                definition: TuningDefinition::twelve_tet(),
            },
        );
        add_chain(&mut established, &[21; 32], 4);

        let mut joiner = RoomStore::new();
        run_ok(&mut established, &mut joiner, SyncLimits::default());
        assert_converged(&established, &joiner);
    }
}
