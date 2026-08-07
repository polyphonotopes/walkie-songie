//! L0 deterministic in-memory gossip bus.
//!
//! A seeded, sockets-free simulation that exchanges real `SignedOp` wire bytes
//! between real [`RoomStore`]s over the production ingress path
//! (`verify_signed_op_for_topic` -> `ingest_verified`). Every scheduling decision
//! derives from one master seed, so a run reproduces from its seed alone; a failed
//! assertion prints the [`TraceEvent`] log so the schedule is a recipe, not a
//! puzzle. Blueprint: potluck's `crates/potluck-sim`.
//!
//! The unit under test is `RoomStore` + bytes, not sockets — turmoil/madsim buy
//! nothing here — and because there are no sockets it also compiles to wasm (L3).

#![allow(dead_code, unused_imports)]

use std::collections::{BTreeMap, BTreeSet, HashSet};

use walkie_songie::room::ops::{
    AuthorId, OpId, SignedOp, SigningKey, VerifiedOp, WalkieOp, signing_key_from_seed,
    verify_signed_op, verify_signed_op_for_topic,
};
use walkie_songie::room::store::{RoomStore, RoomView};

pub use walkie_songie::room::test_support::{
    Peer, SEED_A, SEED_B, SEED_C, TOPIC, entryhash_set, oracle, tet_definition, tet_degree,
    tet_pitch, tuning_with_step,
};

pub mod reconcile;

// ---------------------------------------------------------------------------
// Seeded RNG — a self-contained SplitMix64, so the harness needs no rand crate
// and every schedule is bit-reproducible.
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n`. Panics if `n == 0`.
    pub fn gen_range(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    /// A biased coin: true with probability `prob` (clamped to `[0, 1]`).
    pub fn gen_bool(&mut self, prob: f64) -> bool {
        let u = (self.next_u64() >> 11) as f64 / ((1u64 << 53) as f64);
        u < prob
    }

    fn fill(&mut self, buf: &mut [u8; 32]) {
        for chunk in buf.chunks_mut(8) {
            let bytes = self.next_u64().to_le_bytes();
            for (slot, b) in chunk.iter_mut().zip(bytes.iter()) {
                *slot = *b;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Clock, peers, envelopes, trace.
// ---------------------------------------------------------------------------

/// A per-peer monotonic microsecond counter. Author timestamps are forgeable by
/// design (they ride in the signed body and nothing verifies them against
/// reality), so the sim treats clocks as an input to vary, not a fact to trust.
#[derive(Clone)]
struct SimClock {
    now: u64,
    step: u64,
}

impl SimClock {
    fn new(start: u64, step: u64) -> Self {
        Self { now: start, step }
    }
    fn next(&mut self) -> u64 {
        let issued = self.now;
        self.now += self.step;
        issued
    }
}

/// One simulated participant: an identity plus its own real [`RoomStore`].
pub struct SimPeer {
    pub name: String,
    pub key: SigningKey,
    pub store: RoomStore,
    clock: SimClock,
}

impl SimPeer {
    pub fn author(&self) -> AuthorId {
        AuthorId(*self.key.verifying_key().as_bytes())
    }
    pub fn view(&self) -> RoomView {
        self.store.view()
    }
}

/// A signed-op record in flight from one peer to another.
#[derive(Clone)]
struct Envelope {
    id: u64,
    from: String,
    to: String,
    signed: SignedOp,
    deliver_at: u64,
}

/// How the scheduler picks which in-flight record to deliver next.
///
/// `Fifo` is the well-behaved baseline. `RandomSeeded` explores orderings.
/// `Adversarial` prefers the newest queued record — with per-author chains that
/// tends to arrive before its own predecessor, which is exactly the delivery
/// order a positional projection mishandles and a causal one does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    Fifo,
    RandomSeeded,
    Adversarial,
}

/// What happened, in order. Printed alongside the seed when an assertion fails.
#[derive(Debug, Clone, PartialEq)]
pub enum TraceEvent {
    Wrote {
        peer: String,
        op: String,
    },
    Delivered {
        from: String,
        to: String,
        op: String,
        outcome: String,
    },
    Dropped {
        from: String,
        to: String,
        op: String,
        reason: String,
    },
    Partitioned {
        a: String,
        b: String,
    },
    Healed,
    Reconciled {
        a: String,
        b: String,
        rounds: usize,
    },
}

// ---------------------------------------------------------------------------
// The bus.
// ---------------------------------------------------------------------------

/// N peers, a delivery queue, and a seeded scheduler.
pub struct SimNet {
    peers: BTreeMap<String, SimPeer>,
    order: Vec<String>,
    inflight: Vec<Envelope>,
    next_id: u64,
    now: u64,
    partitions: BTreeSet<(String, String)>,
    policy: Policy,
    rng: Rng,
    drop_prob: f64,
    dup_prob: f64,
    delay: u64,
    /// Every op the sim has ever seen that VERIFIES, deduped by id. The
    /// independent oracle is computed over exactly this set.
    ops_by_id: BTreeMap<OpId, VerifiedOp>,
    /// Header bytes already handed to `register_op`, so re-offering the same op
    /// (broadcast fan-out, regossip) does not re-run signature verification.
    /// Delivery still verifies every time — that is the production ingress path.
    seen_headers: HashSet<Vec<u8>>,
    trace: Vec<TraceEvent>,
}

impl SimNet {
    /// Build a room with the given participants, all fully connected. Peer keys
    /// derive from the master seed, so identities — and therefore every op hash
    /// and hash-order tiebreak — are stable across runs of the same seed.
    pub fn new(seed: u64, names: &[&str], policy: Policy) -> Self {
        let mut rng = Rng::new(seed);
        let mut peers = BTreeMap::new();
        let mut order = Vec::new();
        for (index, name) in names.iter().enumerate() {
            let mut key_seed = [0u8; 32];
            rng.fill(&mut key_seed);
            let key = signing_key_from_seed(&key_seed);
            // Stagger start times and step by a prime-ish amount so timestamp skew
            // accumulates rather than staying in lockstep.
            let clock = SimClock::new(1_700_000_000_000_000 + (index as u64) * 997, 1_009);
            peers.insert(
                name.to_string(),
                SimPeer {
                    name: name.to_string(),
                    key,
                    store: RoomStore::new(),
                    clock,
                },
            );
            order.push(name.to_string());
        }
        Self {
            peers,
            order,
            inflight: Vec::new(),
            next_id: 0,
            now: 0,
            partitions: BTreeSet::new(),
            policy,
            rng,
            drop_prob: 0.0,
            dup_prob: 0.0,
            delay: 0,
            ops_by_id: BTreeMap::new(),
            seen_headers: HashSet::new(),
            trace: Vec::new(),
        }
    }

    // --- fault configuration (all seeded) ---

    pub fn set_drop_prob(&mut self, p: f64) {
        self.drop_prob = p;
    }
    pub fn set_dup_prob(&mut self, p: f64) {
        self.dup_prob = p;
    }
    /// Enqueued records land `0..=delay` steps in the future (seeded), a delay
    /// that also reorders delivery.
    pub fn set_delay(&mut self, delay: u64) {
        self.delay = delay;
    }

    // --- accessors ---

    pub fn names(&self) -> &[String] {
        &self.order
    }
    pub fn peer(&self, name: &str) -> &SimPeer {
        self.peers.get(name).expect("known peer")
    }
    pub fn store(&self, name: &str) -> &RoomStore {
        &self.peers.get(name).expect("known peer").store
    }
    pub fn store_mut(&mut self, name: &str) -> &mut RoomStore {
        &mut self.peers.get_mut(name).expect("known peer").store
    }
    pub fn view(&self, name: &str) -> RoomView {
        self.store(name).view()
    }
    pub fn author(&self, name: &str) -> AuthorId {
        self.peer(name).author()
    }
    pub fn trace(&self) -> &[TraceEvent] {
        &self.trace
    }
    pub fn pending(&self) -> usize {
        self.inflight.len()
    }
    /// Every verified op the sim has recorded, for hand-rolled oracle checks.
    pub fn all_ops(&self) -> Vec<VerifiedOp> {
        self.ops_by_id.values().cloned().collect()
    }

    /// The number of lifted entries at each peer, in peer order. Used to assert
    /// monotone growth under flapping links.
    pub fn entry_hash_sizes(&self) -> Vec<usize> {
        self.order
            .iter()
            .map(|n| self.store(n).entry_hashes().len())
            .collect()
    }

    pub(crate) fn topic(&self) -> &str {
        TOPIC
    }

    // --- authoring ---

    /// Author an op as `name`, commit it locally (production `commit` path), and
    /// queue its bytes to every reachable peer. Returns the signed bytes.
    pub fn act(&mut self, name: &str, op: WalkieOp) -> SignedOp {
        let signed = {
            let peer = self.peers.get_mut(name).expect("known peer");
            let ts = peer.clock.next();
            peer.store.commit(&peer.key, TOPIC, ts, op)
        };
        let id = self.register_op(&signed);
        self.trace.push(TraceEvent::Wrote {
            peer: name.to_string(),
            op: id.map(|i| i.to_hex()).unwrap_or_else(|| "<invalid>".into()),
        });
        self.broadcast(name, &signed);
        signed
    }

    /// Author an op as `name` and commit it locally, but DROP it in transit — it
    /// never reaches any other peer until an explicit [`reconcile`](Self::reconcile).
    /// Models deterministic loss of a specific op (gossip has no retransmit buffer).
    pub fn act_dropped(&mut self, name: &str, op: WalkieOp) -> SignedOp {
        let signed = {
            let peer = self.peers.get_mut(name).expect("known peer");
            let ts = peer.clock.next();
            peer.store.commit(&peer.key, TOPIC, ts, op)
        };
        let id = self.register_op(&signed);
        let hex = id.map(|i| i.to_hex()).unwrap_or_else(|| "<invalid>".into());
        self.trace.push(TraceEvent::Wrote {
            peer: name.to_string(),
            op: hex.clone(),
        });
        for to in self.reachable_targets(name) {
            self.trace.push(TraceEvent::Dropped {
                from: name.to_string(),
                to,
                op: hex.clone(),
                reason: "act_dropped".to_string(),
            });
        }
        signed
    }

    // --- lower-level delivery of arbitrary bytes (hand-crafted causal tests) ---

    /// Immediately verify+ingest `signed` into `to`'s store (production ingress).
    /// Bypasses the queue, so a test controls exact per-peer arrival order.
    pub fn inject(&mut self, to: &str, signed: &SignedOp) -> String {
        self.register_op(signed);
        let outcome = self.deliver(to, signed);
        self.trace.push(TraceEvent::Delivered {
            from: "inject".to_string(),
            to: to.to_string(),
            op: op_hex(signed),
            outcome: outcome.clone(),
        });
        outcome
    }

    /// Queue arbitrary bytes from `from` to `to` (respecting partitions and the
    /// seeded drop/dup/delay faults). `from` is a trace label only.
    pub fn enqueue(&mut self, from: &str, to: &str, signed: SignedOp) {
        self.register_op(&signed);
        if self.is_partitioned(from, to) {
            self.trace.push(TraceEvent::Dropped {
                from: from.to_string(),
                to: to.to_string(),
                op: op_hex(&signed),
                reason: "partitioned".to_string(),
            });
            return;
        }
        if self.drop_prob > 0.0 && self.rng.gen_bool(self.drop_prob) {
            self.trace.push(TraceEvent::Dropped {
                from: from.to_string(),
                to: to.to_string(),
                op: op_hex(&signed),
                reason: "lossy-link".to_string(),
            });
            return;
        }
        self.push(from, to, signed.clone());
        if self.dup_prob > 0.0 && self.rng.gen_bool(self.dup_prob) {
            self.push(from, to, signed);
        }
    }

    /// Re-offer every op in `name`'s store to every peer (queued). Models a peer
    /// dumping its whole history on reconnect (dedup/idempotency exercise).
    pub fn regossip(&mut self, name: &str) {
        let ops: Vec<SignedOp> = self.store(name).signed_ops().into_values().collect();
        let targets = self.reachable_targets(name);
        for signed in ops {
            for to in &targets {
                self.enqueue(name, to, signed.clone());
            }
        }
    }

    fn broadcast(&mut self, from: &str, signed: &SignedOp) {
        for to in self.reachable_targets(from) {
            self.enqueue(from, &to, signed.clone());
        }
    }

    fn reachable_targets(&self, from: &str) -> Vec<String> {
        self.order
            .iter()
            .filter(|n| n.as_str() != from)
            .cloned()
            .collect()
    }

    fn push(&mut self, from: &str, to: &str, signed: SignedOp) {
        let jitter = if self.delay > 0 {
            self.rng.gen_range((self.delay + 1) as usize) as u64
        } else {
            0
        };
        let id = self.next_id;
        self.next_id += 1;
        self.inflight.push(Envelope {
            id,
            from: from.to_string(),
            to: to.to_string(),
            signed,
            deliver_at: self.now + jitter,
        });
    }

    /// Record an op in the oracle set iff it is valid FOR THIS ROOM (verifies AND
    /// is bound to `TOPIC`). Tampered, wrong-key, or wrong-topic bytes are excluded,
    /// so the independent oracle never counts something a real peer would reject.
    fn register_op(&mut self, signed: &SignedOp) -> Option<OpId> {
        // Skip re-verifying an op we've already classified (broadcast fan-out
        // offers the same bytes to every peer; regossip re-offers whole logs). No
        // caller uses the returned id on a repeat — only `act`, on its fresh op.
        if !self.seen_headers.insert(signed.header.clone()) {
            return None;
        }
        match verify_signed_op_for_topic(signed, TOPIC) {
            Ok(v) => {
                let id = v.id();
                self.ops_by_id.entry(id).or_insert(v);
                Some(id)
            }
            Err(_) => None,
        }
    }

    /// Deliver bytes to a store via the production ingress path.
    fn deliver(&mut self, to: &str, signed: &SignedOp) -> String {
        let peer = self.peers.get_mut(to).expect("known peer");
        match verify_signed_op_for_topic(signed, TOPIC) {
            Ok(verified) => {
                peer.store.ingest_verified(verified);
                "ingested".to_string()
            }
            Err(e) => format!("rejected: {e}"),
        }
    }

    // --- partitions ---

    fn norm(a: &str, b: &str) -> (String, String) {
        if a <= b {
            (a.to_string(), b.to_string())
        } else {
            (b.to_string(), a.to_string())
        }
    }

    fn is_partitioned(&self, a: &str, b: &str) -> bool {
        self.partitions.contains(&Self::norm(a, b))
    }

    /// Cut the link between two peers. In-flight and future records for this link
    /// DROP rather than buffer — gossip reality.
    pub fn partition(&mut self, a: &str, b: &str) {
        self.partitions.insert(Self::norm(a, b));
        // Drop anything already queued across the cut link.
        let cut: Vec<Envelope> = std::mem::take(&mut self.inflight);
        for env in cut {
            if Self::norm(&env.from, &env.to) == Self::norm(a, b) {
                self.trace.push(TraceEvent::Dropped {
                    from: env.from,
                    to: env.to,
                    op: op_hex(&env.signed),
                    reason: "partitioned".to_string(),
                });
            } else {
                self.inflight.push(env);
            }
        }
        self.trace.push(TraceEvent::Partitioned {
            a: a.to_string(),
            b: b.to_string(),
        });
    }

    /// Restore every link. Does NOT re-offer anything — dropped records stay lost
    /// until [`reconcile`](Self::reconcile) (or a fresh commit / regossip).
    pub fn heal(&mut self) {
        self.partitions.clear();
        self.trace.push(TraceEvent::Healed);
    }

    // --- scheduling ---

    /// Deliver exactly one deliverable record. Returns false when the queue is
    /// empty.
    pub fn step(&mut self) -> bool {
        if self.inflight.is_empty() {
            return false;
        }
        let min_at = self
            .inflight
            .iter()
            .map(|e| e.deliver_at)
            .min()
            .expect("non-empty");
        if min_at > self.now {
            self.now = min_at;
        }
        let ready: Vec<usize> = self
            .inflight
            .iter()
            .enumerate()
            .filter(|(_, e)| e.deliver_at <= self.now)
            .map(|(i, _)| i)
            .collect();
        let choice = match self.policy {
            Policy::Fifo => ready[0],
            Policy::RandomSeeded => ready[self.rng.gen_range(ready.len())],
            Policy::Adversarial => *ready
                .iter()
                .max_by_key(|&&i| self.inflight[i].id)
                .expect("non-empty"),
        };
        let env = self.inflight.remove(choice);
        let outcome = self.deliver(&env.to, &env.signed);
        self.trace.push(TraceEvent::Delivered {
            from: env.from,
            to: env.to,
            op: op_hex(&env.signed),
            outcome,
        });
        true
    }

    /// Deliver until the queue drains. Bounded so a scheduling bug fails loudly.
    pub fn step_until_quiescent(&mut self) {
        let mut budget = 1_000_000;
        while self.step() {
            budget -= 1;
            assert!(
                budget > 0,
                "delivery did not reach quiescence\ntrace: {:#?}",
                self.trace
            );
        }
    }

    // --- reconcile_all convenience (pairwise gather + scatter) ---

    /// Bring every peer to convergence by pairwise anti-entropy against a hub:
    /// one gather pass (hub learns the union) then one scatter pass (hub teaches
    /// the union to all). Returns the total RBSR rounds.
    pub fn reconcile_all(&mut self) -> usize {
        let mut total = 0;
        if self.order.len() < 2 {
            return 0;
        }
        let hub = self.order[0].clone();
        let rest: Vec<String> = self.order[1..].to_vec();
        for name in &rest {
            total += self.reconcile(&hub, name);
        }
        for name in &rest {
            total += self.reconcile(&hub, name);
        }
        total
    }

    // --- the oracle ---

    /// Convergence oracle: every peer's `view()` equal, every peer's
    /// `entry_hashes()` equal, every peer's `pending_len() == 0`, and the shared
    /// state equals an INDEPENDENT oracle computed over every op the sim recorded.
    pub fn assert_converged(&self) {
        let expected = oracle(&self.all_ops());
        let first_name = &self.order[0];
        let first = self.store(first_name);
        let first_view = first.view();
        let first_hashes = first.entry_hashes();

        for name in &self.order {
            let store = self.store(name);
            assert_eq!(
                store.pending_len(),
                0,
                "peer {name} still has deferred ops after quiescence\ntrace: {:#?}",
                self.trace
            );
            assert_eq!(
                store.view(),
                first_view,
                "peer {name} view diverges from {first_name}\ntrace: {:#?}",
                self.trace
            );
            assert_eq!(
                store.entry_hashes(),
                first_hashes,
                "peer {name} entry-hash set diverges from {first_name}\ntrace: {:#?}",
                self.trace
            );
        }
        assert_eq!(
            first_view, expected,
            "converged view disagrees with the independent oracle\ntrace: {:#?}",
            self.trace
        );
    }
}

/// A cheap, stable, deterministic label for `signed`, for trace lines only. It is
/// NOT the op id — deriving that means a full signature verification, and the trace
/// is written for every delivered/dropped record on the hot path. A fixed-key hash
/// of the header bytes is distinctive enough to follow a record through a trace and
/// costs nothing next to `verify_signed_op`.
pub fn op_hex(signed: &SignedOp) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    signed.header.hash(&mut hasher);
    format!("op:{:016x}", hasher.finish())
}

/// The op id of a signed record (panics on unverifiable bytes — call only on
/// records the test just authored).
pub fn op_id_of(signed: &SignedOp) -> OpId {
    verify_signed_op(signed).expect("authored op verifies").id()
}

/// A random ref-free op (no Move/Remove/Unremove-piece, which need an existing
/// piece id) — for the catch-up and property scenarios.
pub fn random_op(rng: &mut Rng) -> WalkieOp {
    match rng.gen_range(6) {
        0 => WalkieOp::AddDegree {
            pitch: tet_degree(rng.gen_range(12) as u16),
        },
        1 => WalkieOp::RemoveDegree {
            pitch: tet_degree(rng.gen_range(12) as u16),
        },
        2 => WalkieOp::PutPiece {
            emoji: "🎵".to_string(),
            pitch: tet_pitch(60 + rng.gen_range(12) as i32),
        },
        3 => WalkieOp::SetTuning {
            definition: tuning_with_step(100 + rng.gen_range(10) as u16 * 50),
        },
        4 => WalkieOp::SetConfig {
            pieces_locked: Some(rng.gen_range(2) == 1),
            available_emojis: None,
        },
        _ => WalkieOp::SetConfig {
            pieces_locked: None,
            available_emojis: Some(if rng.gen_range(2) == 0 {
                "🎵".to_owned()
            } else {
                "🌵".to_owned()
            }),
        },
    }
}
