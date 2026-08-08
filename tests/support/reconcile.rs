//! Range-based set reconciliation (RBSR) anti-entropy driver — the real
//! algorithm, today.
//!
//! This is the executable spec for the sync layer (HHHS H6). It builds a
//! [`hhhs_sync::reconciliation::Index`] per peer over the peer's `entry_hashes()`
//! (sort key = the 32-byte entry hash, which already satisfies the RBSR
//! op-hash-suffix invariant and makes the index injective), then drives
//! [`opening`](hhhs_sync::reconciliation::opening) /
//! [`respond`](hhhs_sync::reconciliation::respond) to a fixpoint. On each terminal
//! `Items` message it transfers the sender's VERBATIM `SignedOp` bytes for the ids
//! the receiver lacks and re-ingests them through the production ingress
//! (`ingest_verified`).
//!
//! Causal completion: an advertised op is transferred together with its full
//! causal past (its `backlink` + `observed` predecessors, walked over the op
//! graph). This is the role of the kernel's
//! [`completion_plan`](hhhs_sync::reconciliation::completion_plan): it guarantees a
//! transferred op LIFTS immediately rather than parking behind a predecessor that
//! lives in a different RBSR range — which keeps the round count at the RBSR tree
//! depth instead of stalling one range while another slowly delivers its parents.
//! The store's strict-deferral drain then handles any residual ordering within the
//! batch. When H6's Fetch/Entries messages land, the byte transfer below is the
//! seam they replace, assertions unchanged.

use std::collections::{BTreeMap, BTreeSet};

use hhhs_sync::SortKey;
use hhhs_sync::reconciliation::{self, Config, Index, Message};

use walkie_songie::room::ops::{OpId, VerifiedOp};
use walkie_songie::room::store::RoomStore;

use super::{SimNet, TraceEvent};

/// A fixed per-session salt. Only keys the fingerprint monoid (collision
/// resistance), never correctness, so a constant keeps reconcile fully
/// deterministic and independent of the bus scheduler's RNG stream.
const SESSION_SALT: [u8; 16] = [0x5a; 16];

/// Build an RBSR index for a store: `SortKey(entry_hash_bytes) -> EntryHash`.
fn build_index(store: &RoomStore) -> Index {
    let mut idx = Index::new(SESSION_SALT);
    for h in store.entry_hashes() {
        idx.insert(SortKey(h.as_bytes().to_vec()), h);
    }
    idx
}

/// Collect `root` together with its full causal past (walked over `by_id`, the
/// bus's already-verified op set) into `out`, skipping ids already seen this
/// session. Order is irrelevant — the receiver's strict-deferral drain resolves it
/// — but the closure is complete, so every collected op lifts.
fn collect_with_past(
    root: OpId,
    by_id: &BTreeMap<OpId, VerifiedOp>,
    seen: &mut BTreeSet<OpId>,
    out: &mut Vec<VerifiedOp>,
) {
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        let Some(verified) = by_id.get(&id) else {
            continue;
        };
        if let Some(backlink) = verified.backlink() {
            stack.push(OpId(backlink));
        }
        for observed in verified.observed() {
            stack.push(OpId(*observed));
        }
        out.push(verified.clone());
    }
}

impl SimNet {
    /// Reconcile peers `a` and `b` to a fixpoint over one sans-io RBSR session
    /// (`a` initiates). Both stores end holding the union of their lifted ops.
    /// Returns the number of RBSR rounds.
    pub fn reconcile(&mut self, a: &str, b: &str) -> usize {
        let cfg = Config::default();
        let mut idx_a = build_index(self.store(a));
        let mut idx_b = build_index(self.store(b));

        // The opening full-range fingerprint, from a, addressed to b.
        let mut msgs = vec![reconciliation::opening(&idx_a)];
        let mut to_b = true;
        let mut rounds = 0usize;
        let mut guard = 0usize;

        while !msgs.is_empty() {
            guard += 1;
            assert!(
                guard < 100_000,
                "reconcile({a},{b}) failed to terminate\ntrace: {:#?}",
                self.trace()
            );
            rounds += 1;

            let (recv, send) = if to_b { (b, a) } else { (a, b) };

            let mut replies = Vec::new();
            for m in &msgs {
                if let Message::Items(_range, ids) = m {
                    // Resolve advertised entry hashes to op ids on the sender, then
                    // transfer each op the receiver lacks with its full causal past,
                    // reusing the bus's already-verified ops (no re-verification).
                    let hash_to_id = self.store(send).lifted_op_ids();
                    let mut collected: Vec<VerifiedOp> = Vec::new();
                    let mut seen: BTreeSet<OpId> = BTreeSet::new();
                    {
                        let have = self.store(recv).entry_hashes();
                        for id in ids {
                            if have.contains(id) {
                                continue;
                            }
                            if let Some(op_id) = hash_to_id.get(id) {
                                collect_with_past(
                                    *op_id,
                                    &self.ops_by_id,
                                    &mut seen,
                                    &mut collected,
                                );
                            }
                        }
                    }
                    let store = self.store_mut(recv);
                    for verified in collected {
                        store.ingest_verified(verified);
                    }
                }
                // Respond against the receiver's CURRENT index (rebuilt to reflect
                // anything the transfer above lifted).
                let recv_idx = if to_b { &mut idx_b } else { &mut idx_a };
                *recv_idx = build_index(self.store(recv));
                replies.extend(reconciliation::respond(recv_idx, m, &cfg));
            }

            msgs = replies;
            to_b = !to_b;
        }

        self.trace.push(TraceEvent::Reconciled {
            a: a.to_string(),
            b: b.to_string(),
            rounds,
        });
        rounds
    }
}
