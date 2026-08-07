# Tasks — rewrite on iroh + p2panda + HHHS (HHHS = full data layer)

Living checklist. `[x]` done+tested, `[~]` in progress, `[ ]` not started.
Design consolidated per the Fable review (2026-07-29): HHHS owns mutations + conflict +
reads; p2panda/iroh own signing + transport; `RoomView` hand fold and the opaque-entry
mirror are both removed.

## 0. Foundation (done)
- [x] 0.1 Vendor `hhhs-rs` (SHA-pinned); `p2panda-core 0.7` + hhhs + `futures-signals` link in-tree
- [x] 0.2 Wasm32 build of the stack is clean (browser feasibility proven)
- [x] 0.3 `ops.rs` signed op-log (sign/verify, `VerifiedOp`, topic-binding) — KEEP
- [~] 0.4 `view.rs` fold + `mirror.rs` opaque mirror — PROVISIONAL, being replaced (see §2–§3)

## 1. Fix HHHS itself — UPSTREAM `/laboratory/fe-stuff/hhhs-rs` FIRST, then re-vendor to walkie AND potluck (peer guidance). Strictly additive; do NOT reshape `VoidPolicy`/`verdict` (potluck implements it @62f5c4d).
- [x] 1.1 **H1** `hhhs_core::cover::ReachIndex` (`is_ancestor`/`ancestors`/`observed_at`/`causal_cover`/`concurrent_cover`) — upstream `fe-stuff/hhhs-rs` (working-tree, uncommitted; HEAD dae9453), re-vendored to walkie. 6 tests, reviewed + green.
- [x] 1.2 **H4** `hhhs_core::register::resolve` — causal-maxima + max raw-bytes `EntryHash` (diverges from hhs3-ts base64 tiebreak, deliberate); no cache/seniority. 5 tests, reviewed + green.
- [ ] 1.2b Commit the two modules UPSTREAM (needs user OK — shared repo) so potluck can re-vendor; refresh walkie vendor `HHHS_SOURCE_REV`/SHA manifests at the re-pin
- [ ] 1.3 **H2** bound `VoidPolicy`/`removers_of` by ancestors-of-`at` (uses H1), or narrow API to frontier + drop the misleading param
- [ ] 1.4 **H7** (perf, for live jam) store `Arc<Entry>` internally (pointer-copy snapshots) + delta-aware reactive view adapter (use `DagDelta::appended_since`)
- [x] 1.5 **H6 DONE** — `sync_session.rs` (`SyncMessage`/`SyncSession`/`EntrySource`, `wire` feature) additive over `reconciliation`; 14 tests (40 w/ wire), items-parity vs `replica::reconcile`. Committed, **pushed** to gitlab (`add-cover-register` + `master` = `7d0dd3f0e7f62f44d414c58c6d7a877709416bd1`), walkie **re-pinned** to `7d0dd3f…` — 70 tests green against it. Ready for the sync-over-stream wiring. (Potluck can re-pin `ce9e30d`→`7d0dd3f` to get H6.)
- [ ] 1.6 Follow-on: H3 (lens resurrection/authority split), H5 (two-horizon `(at,from)`), H8 (removers-by-target index), H9 (iterative void DFS/depth budget), H10
- [ ] 1.7 Re-vendor into walkie (and coordinate potluck re-vendor); own golden identity vectors (hhs3-ts preimage differs)

## 2. Op alphabet v2 (`ops.rs`) — DONE (8 tests green; view.rs/mirror.rs removed)
- [x] 2.1 `WalkieOp` v2: `AddPitch`/`RemovePitch{pc,of}`, `SetVoice`/`ClearVoice`, `PutPiece`/`MovePiece`/`RemovePiece`/`UnremovePiece` (piece id = op id), `SetTuning`, `SetConfig`; `OP_SCHEMA_VERSION=2`; `pc,of` denormalized; `OpId` newtype
- [x] 2.2 `observed` load-bearing: `sign_op_for_topic_observing` stamps it (store fills from frontier)
- [x] 2.3 `u64→u32` seq guard (`try_from().expect`); `ts_micros` display-only

## 3. RoomStore — deterministic lift + HHHS-native reads (replaces `mirror.rs`) — DONE (12 tests, reviewed)
- [x] 3.1 `src/room/store.rs`: `RoomStore` — payload = framed VERBATIM signed bytes; `prevs = lift(backlink) ∪ lift(observed)` with STRICT deferral (`pending`+drain, never omit); dual `opId↔EntryHash` maps; per-author head tracking; `commit()`/`ingest_verified()`/`observed_frontier()`
- [x] 3.2 Golden entry-hash vector (`e3f567…`) + out-of-order convergence (identity/reversed/interleaved → identical view AND identical entry-hash set + full drain)
- [x] 3.3 Pitch = content-keyed add-wins via `cover::is_ancestor`; voice = per-author seq register; pieces = owner-gated per-owner seq register (simpler than GraphVoid — owner log is totally ordered); tuning/config = `register::resolve`. (GraphVoid only needed if non-owner removes are ever allowed.)
- [x] 3.4 Materialized `view() -> RoomView` + INDEPENDENT oracle (OpId-graph ancestry + own entry-hash recompute) parity over adversarial histories + mutation-tested parity. Reactive `signal_vec_view` wrapping deferred to UI wiring (§6.2).
- [ ] 3.5 `canonical_root()` for the sync fixpoint predicate (deferred to sync, §5.4)

## 4. Spatial/music strategy
- [ ] 4.1 Bespoke `WalkiePitch` `AddressStrategy` (~100 ln, toyfacet-modeled; chain kind→pc→pitch, `PrefixNestedKey`); `AddressIndex` backs the pitch cover filter (perf)
- [ ] 4.2 (later, with text-input mode) riffcat `WalkieMusicProjection` for chords/scales/pc-set equivalence (parses via vibe-grammars); addresses never = identity, never gate liveness

## 5. Transport (iroh + p2panda)
- [ ] 5.1 Persistent Ed25519 identity (author = iroh id) — IndexedDB / plugin state
- [ ] 5.2 **Topology A (see `transport-design.md`): raw iroh 1.0 + iroh-gossip EVERYWHERE (browser + native), NO p2panda-net.** Two ALPNs: iroh-gossip (broadcast `SignedOp` on commit, 64KiB max msg) + `walkie/rbsr/1` (H6 sync stream). Inbound → `verify_signed_op_for_topic` → `ingest_verified`.
- [ ] 5.3 Build-env for wasm iroh: `CC_wasm32_unknown_unknown=clang` (ring) + `--cfg getrandom_backend="wasm_js"` (.cargo/config.toml). Verify walkie compiles native + wasm with iroh+iroh-gossip.
- [ ] 5.4 Room name → `Topic`; anti-entropy = kernel `hhhs_core::reconciliation` RBSR (via H6 `SyncSession`); fixpoint = canonical-root equality
- [ ] 5.4b **Relay: self-hosted `iroh-relay` at `relay.wondering.xyz`** via `RelayMode::Custom(RelayMap)` — configurable (env/URL override; n0 `presets::N0` as dev fallback). Requires deploying n0's `iroh-relay` binary + TLS (browsers use HTTPS/WSS) — NOT the old libp2p relay (different protocol). Replaces the deleted `relay-server/`. Discovery stays separate (n0 pkarr OR `WalkieTicket` carries full addr).
- [ ] 5.5 Replace `src/web/libp2p_sync.rs` → `src/net/`; drop libp2p behaviour/relay

## 5b. Browser-direct WebRTC (post-v1 milestone; see transport-design.md Addendum B)
- [ ] 5b.1 Custom iroh transport carrying QUIC over a WebRTC data channel (iroh's `add_custom_transport` + `PathSelector` preferring WebRTC; on `wasm_browser` the transport set is `Custom + Relay` — this IS the designed non-relay path). Signaling = JSEP over an iroh bidi stream through the relay. **Additive: zero changes to gossip/RBSR/H6/tickets/identity, one integration point (`src/net/endpoint.rs`).** ~2–4 wk + unstable-API churn; or harden a community crate (anchalshivank / SuddenlyHazel `iroh-webrtc-transport`, both alpha, pinned iroh 0.97/0.98 — need porting to 1.0). Field data: WebRTC-direct = 100% same-LAN/hotspot (walkie's jam case), 0% CGNAT. v1 ships relay-only; add this next. Revisit-now trigger: n0 ships/blesses a WebRTC transport.

## 6. Rewire web app
- [ ] 6.1 `app.rs`: local edits → signed ops via RoomStore; reads ← HHHS views
- [ ] 6.2 `keyboard.rs`/`components.rs`: bind to view `SignalVec`s; MIDI deltas re-source from `Revision` added/retracted
- [ ] 6.3 `storage.rs`: persist verbatim signed bytes + signing key + log heads (replace yrs state vector)

## 7. Plugin → nice-plug; 8. Remove old stack (yrs/libp2p/relay-server); 9. Verify (native + 2-device wasm jam)
- [ ] (unchanged from prior plan)

## 10. Integration tests — see `integration-tests.md` (Fable plan; layered L0→L1→L2→L3)
- [x] 10.1 L0 DONE + reviewed: `tests/support/{mod,reconcile}.rs` (SimNet bus, real RBSR driver) + W1–W16 across `l0_{convergence,faults,late_join,properties}.rs`; `src/room/test_support.rs` refactor + `entry_hashes()`/`pending_len()`/`signed_ops()`/`lifted_op_ids()` accessors. 70 tests green (54 lib + 16 L0). Also **un-vendored hhhs → git dep** (`ce9e30dd…`), `vendor/` removed.
- [ ] 10.2 L1 real iroh test-utils + p2panda-net `TestNode` two-node + **interop (raw-iroh↔p2panda-net)** — gated on transport §5
- [ ] 10.3 L2 patchbay network conditions (Linux) — gated on §5
- [ ] 10.4 L3 `wasm-bindgen-test` (data-layer + golden vector proves wasm==native hashing)
