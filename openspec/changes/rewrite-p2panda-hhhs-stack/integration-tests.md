# Integration-test plan — walkie-songie on iroh + p2panda + HHHS

From the Fable research pass (2026-07-29). Layered: **L0** deterministic in-memory bus
(workhorse, unblocked today) → **L1** real iroh/p2panda-net (gated on transport §5) →
**L2** patchbay network conditions (gated on §5) → **L3** wasm.

## Framework decision (per layer — do NOT force one tool)
- **L0** — walkie-owned deterministic in-memory gossip bus over real `SignedOp` wire
  bytes between real `RoomStore`s. Blueprint: potluck's `crates/potluck-sim`. Seeded
  (ChaCha8 / small xorshift) so every schedule is bit-reproducible; no sockets, so it
  also compiles to wasm (L3 reuse). **turmoil/madsim buy nothing here** (the unit under
  test is `RoomStore`+bytes, not sockets).
- **L1** — iroh 1.0 `test-utils` (`run_relay_server`, `MemoryLookup`,
  `direct_pair`/`relay_pair` fixtures in `iroh/src/{socket.rs:2342,protocol.rs:820}`) for
  the raw-iroh path; p2panda-net `test_utils` (`TestNode::spawn` +
  `AddressBook::insert_node_info`, `tests/e2e.rs`) for the native path. **Eventually-
  converges-within-timeout** (poll 200ms/30s), never exact-schedule — real QUIC isn't
  deterministic. Constraint: `SyncHandle::initiate_session` is `#[cfg(test)]`-gated in
  p2panda-net; must use the automatic membership path.
- **L2** — **patchbay 0.7** (iroh's own Linux userns+veth+tc harness): `LinkLimits`
  {latency,jitter,loss,reorder,rate,dup,corrupt}, `link_up/down`, `replug`, `Nat`.
  Linux-only, realistic (not deterministic) → recovery/robustness tests. Optional: fork
  iroh's in-process `TestNetwork` custom transport under `tokio(start_paused)`.
- **L3** — `wasm-bindgen-test` (`run_in_browser`, headless Chrome). Data-layer +
  golden-vector only (proves wasm hashes identically to native). Two-browser networking
  is nightly E2E, not `cargo test`.

**Honest caveat:** deterministic partition/reorder/loss coverage lives ONLY at L0.
L1/L2 prove wiring + recovery under real/realistic networks.

## Refactor L0 needs (only blocker; ~half day)
- Move `Peer`, `oracle()`, `entryhash_set()` out of `store.rs`'s `#[cfg(test)]` module
  into `src/room/test_support.rs` behind `#[cfg(any(test, feature = "test-support"))]`
  so `tests/` can use them.
- Add two **permanent public** accessors on `RoomStore` (the sync layer needs them too):
  `entry_hashes() -> BTreeSet<EntryHash>` (the RBSR index is built from exactly this) and
  `pending_len() -> usize`.
- `Cargo.toml`: `[features] test-support = []`; self-dev-dep
  `walkie-songie = { path = ".", features = ["test-support"] }`.

## L0 harness (`tests/support/`)
- `SimPeer { name, key, store: RoomStore, clock }` — seeds from a master seed (stable op
  hashes → stable register tiebreaks). Staggered per-peer µs clocks (exercise ts skew).
- `act(peer, op) -> SignedOp` = `store.commit(&key, TOPIC, clock.now(), op)`, enqueue
  bytes to every reachable peer.
- Delivery = `verify_signed_op_for_topic(&signed, TOPIC)` → `ingest_verified` (the
  production path). `Envelope { from, to, signed }`.
- Faults (all seeded): `Policy::{Fifo, RandomSeeded, Adversarial(newest-first)}`;
  `partition(a,b)` **drops** (gossip reality — never buffers); `heal()`; per-link
  `drop_prob`/`dup_prob`/`delay_steps`; `step()`/`step_until_quiescent(budget)`; a
  `TraceEvent` log embedded in every failure so a seed reproduces from the trace.
- **`reconcile(a,b)` — the real anti-entropy, today**: build a
  `hhhs_core::reconciliation::Index` per peer over `entry_hashes()` (sort key = entry-hash
  bytes), drive `opening()`/`respond()`/`completion_plan()` to fixpoint, transfer the
  missing peer's verbatim `SignedOp` bytes (`VerifiedOp::signed()`), re-ingest (strict
  deferral drains causal order). **This is the executable spec for H6** — when H6's
  Fetch/Entries messages land, swap the byte transfer for the kernel messages; assertions
  unchanged.
- **Oracle**: `assert_converged(&peers)` = all `view()` equal + all `entry_hashes()`
  equal + all `pending_len()==0` + equality with independent `oracle(all ops)`. Add
  `canonical_root()` equality when task 3.5 lands (one line).

## Test matrix (W1–W18) — each ends in `assert_converged`
L0 = bus; ↑L1/↑L2 re-run at higher layers.
- **W1** add-wins survives partition+heal (↑L1,L2): {A}|{B,C}; A&B both `AddPitch(5,12)`,
  A `RemovePitch` (observes only own add), heal → key live, authors `{B}`.
- **W2** remove lost in transit: drop A's `RemovePitch`; diverge; `reconcile()` repairs → key dead.
- **W3** register recency after reorder (↑L1): A `SetTuning t1`→`t2`; deliver t2 before t1 (defer) → tuning=t2, pending empty.
- **W4** concurrent tuning tiebreak across partition (↑L2): all peers pick same winner (`register::resolve`) == oracle.
- **W5** owner gating cross-node (↑L1): B's move/remove on A's piece ignored, every order incl. before-put (defer).
- **W6** remove/unremove race across partition: greatest-seq owner lifecycle op decides (both variants).
- **W7** late joiner catch-up (↑L1,L2): C joins after N≈50 ops; reconcile → identical view/hashes/root; RBSR round count small.
- **W8** loss stalls a deterministic prefix: drop v3 of a 5-op voice chain → view=v2 (not gap-jumped v5), v4/v5 parked; repair → v5. Pins the strict-deferral liveness invariant.
- **W9** dedup/idempotency on reconnect (↑L1): re-deliver all history + reconcile → len/hashes/view unchanged; second reconcile transfers zero.
- **W10** dup-while-pending: op X (parent missing) twice then parent → single lift.
- **W11** forged/tampered rejected (↑L1): payload-flip / wrong-key / wrong-topic → `OpVerifyError`; converged state byte-identical to clean run.
- **W12** offline peer full catch-up (↑L2 flap): C offline for N=100 ops, heal+reconcile → converges.
- **W13** intermittent flap (↑L2): A–B link flaps ×10 (seeded) while all commit; final heal+reconcile → converge; entry-hash sets grow monotonically.
- **W14** cross-key causal coupling: op on key Y defers where an X-op is missing (frontier-observed coupling); repair releases both.
- **W15** N-peer randomized property: 4–6 peers, 6–30 random ops, random partitions/policy per seed 0..64 under `catch_unwind` printing the seed + trace-determinism guard.
- **W16** wire-bytes cross-store identity (↑L3): commit on A, ingest bytes on B (and wasm) → identical `EntryHash` (golden `e3f567…`).
- **W17** relay-vs-direct migration (L2 only): no op loss/dup across hole-punch migration.
- **W18** restart/persistence (deferred to §6.3 storage): replay from signed bytes+key+heads → entry-hash set == pre-restart.

## Suite layout
```
tests/support/mod.rs        # SimNet bus, SimPeer, TraceEvent, assert_converged
tests/support/reconcile.rs  # RBSR driver over hhhs_core::reconciliation (H6 spec)
tests/l0_convergence.rs     # W1, W3–W6, W16
tests/l0_faults.rs          # W2, W8–W11, W13(bus), W14
tests/l0_late_join.rs       # W7, W12
tests/l0_properties.rs      # W15
tests/l1_two_node.rs        # L1a/b/c — with §5 transport PR; cfg(feature="net-tests")
tests/patchbay_walkie.rs    # L2 — cfg(linux, not(skip_patchbay))
tests/wasm.rs               # L3 — cfg(target_arch="wasm32")
```

## Dev-deps
- L0: `rand`+`rand_chacha` (or a 20-line xorshift); self-dev-dep `test-support`.
- L1: `iroh = { version="1.0.3", default-features=false, features=["tls-ring","test-utils"] }`, `p2panda-net = { version="0.7", features=["test_utils"] }`, `tokio` test-util.
- L2: `[target.'cfg(target_os="linux")'.dev-dependencies] patchbay="0.7", ctor="1"`.
- L3: `[target.'cfg(target_arch="wasm32")'.dev-dependencies] wasm-bindgen-test="0.3.62"`.

## CI
- PR: `cargo test --no-default-features --features test-support` (L0, zero flake budget).
- PR (linux): patchbay via nextest, mild/moderate levels; harsh levels `#[ignore]` → nightly.
- PR: `wasm-pack test --headless --chrome -- --no-default-features` (data-layer only).
- L1 native with 30s poll-timeouts, two-gate (transport-ready, then data-arrival), never `sleep`.

## Build order
1. `test_support.rs` extraction + `entry_hashes()`/`pending_len()` accessors.
2. L0 bus + W1–W14, W16.
3. W15 seeded property loop + trace-determinism guard.
4. `support/reconcile.rs` RBSR driver + W2/W7/W9 repair variants (doubles as H6 spec).
5. Wire `canonical_root()` into `assert_converged` when 3.5 lands.
6. With §5 transport PR: L1a/b fixtures + L1c interop + reconnection.
7. patchbay L2 (flap+partition+degrade 0–1 first; graduate like iroh).
8. wasm L3 (golden vector + mini-convergence).

## Blockers
- L0: none but the refactor. `canonical_root` (3.5) and H6 (1.5) are NOT blockers (interim
  oracle sound; reconcile driver transfers bytes test-side).
- L1/L2 gated on transport §5. **Interop flag:** raw-iroh↔p2panda-net must speak the same
  ALPN — surface in the §5 design, test in L1c.
- W18 gated on storage §6.3.
