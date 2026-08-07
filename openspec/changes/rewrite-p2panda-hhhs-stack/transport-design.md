# Transport design (§5) — raw iroh everywhere, one protocol

Status: proposed (resolves the §5 interop/topology question BEFORE transport code is
written). Companion to `proposal.md`, `tasks.md` §5, `integration-tests.md` (L1).

## 0. Decision

**Topology A: raw `iroh` 1.0 endpoints on BOTH browser and native, with walkie's own
thin two-ALPN protocol.** `p2panda-net` is not used anywhere. p2panda stays exactly
where it already is: `p2panda-core` signing/verification in `src/room/ops.rs`.

- **ALPN 1 — live gossip:** `iroh-gossip` 0.101 (`/iroh-gossip/1`), same crate on both
  targets, broadcasting verbatim `SignedOp` bytes on commit.
- **ALPN 2 — anti-entropy:** `walkie/rbsr/1`, our own protocol carrying
  `hhhs_core::reconciliation` messages (plus the H6 `Fetch`/`Entries` additions) over
  one iroh bidi stream, transferring verbatim `SignedOp` bytes.
- **Membership/bootstrap:** invite ticket in the room URL (iroh-tickets, modeled on
  potluck's `TableTicket` minus the p2panda network-id field) + iroh's pkarr/DNS
  address lookup (`presets::N0`, which works in browsers over HTTPS) + gossip's
  HyParView peer exchange. No ALPN mixing, no derived topics, no PSI discovery.

This is interop-free by construction: one wire protocol, both targets compile from the
same `src/net/` module, and the L0 test suite's `reconcile.rs` driver is the executable
spec for the sync half.

## 1. Why not (B) browser raw-iroh + native p2panda-net

### 1.1 p2panda-net 0.7 cannot run in a browser — so B is only about native

`~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/p2panda-net-0.7.0/Cargo.toml`:
every feature (`iroh_endpoint`, `gossip`, `discovery`, `sync`, `iroh_mdns`) requires
`address_book`, and `address_book = ["p2panda-store/macros", "p2panda-store/sqlite"]`
— i.e. sqlx/SQLite. Unconditional deps include `tokio` (default features, `rt`),
`ractor` (tokio actor framework), `tokio-util`. None of this compiles for
`wasm32-unknown-unknown`. The browser must be raw iroh no matter what; B buys nothing
for the browser and creates an interop seam in the middle of the room.

### 1.2 The potluck precedent shows browser↔native p2panda-net interop is NOT real

The exact question was: can a raw-iroh peer join p2panda-net's gossip + sync overlay?
Potluck is the only precedent, and it **punted** — deliberately:

- `potluck/crates/potluck-transport-iroh/src/lib.rs` reproduces only p2panda's **ALPN
  mixing** (`mixed_protocol_id = blake3(protocol_id ‖ network_id)`, matching
  `p2panda-net-0.7.0/src/iroh_endpoint/actors/endpoint.rs:227`
  `hash_protocol_id_with_network_id`) so that a raw-iroh browser endpoint can dial a
  **custom application ALPN** (`potluck/table-rpc/3`) that the native node registered
  on its p2panda-managed endpoint. That is client↔server RPC over the shared endpoint,
  not membership in p2panda's overlay.
- `potluck/crates/potluck-transport-iroh/src/browser.rs` (`bind_host` / `connect`)
  confirms it: the browser subscribes to no gossip topic, joins no HyParView overlay,
  and speaks neither p2panda's `Discovery` (PSI topic-interest random walk) nor
  `LogSync`. It length-frames postcard RPC over one bidi stream.
- `potluck/crates/potluck-store-p2panda/src/node.rs` documents how much machinery a
  *native* p2panda-net node needs just to sync with another native node
  (AddressBook + MdnsDiscovery + `Discovery` + Gossip + LogSync, the
  `derived_gossip_topic` = `Hash(sync_topic ‖ GOSSIP_TOPIC_MIX_VALUE)` overlay from
  `p2panda-net .../sync/actors/manager.rs:35,143,557`, and the P9 post-mortem about
  the overlay never forming without `Discovery`). A browser peer would have to
  reimplement all of that in wasm to be a first-class member — that is writing a
  second p2panda-net.

Potluck could live with browser-as-client because every table has a native host node.
Walkie cannot: the primary jam is **browser↔browser with no native peer present**. So
under B we would still have to build the full raw-iroh gossip+sync path for browsers,
then ALSO bridge it to p2panda-net's overlay for natives. B is A plus extra work plus
an interop risk.

### 1.3 What p2panda-net would buy us — and why it's redundant here

| p2panda-net battery | walkie's need | verdict |
|---|---|---|
| `LogSync` (per-author log heights over `p2panda-store`, `p2panda-sync-0.7.0/src/protocols/log_sync.rs:36` `ReceiveHave { local: LogHeights }`) | already have RBSR anti-entropy over `RoomStore::entry_hashes()` (`tests/support/reconcile.rs`), and `RoomStore` re-emits verbatim signed bytes | redundant; worse, it forces a **parallel SQLite OperationStore** as a second source of truth beside `RoomStore` |
| `Gossip` | needed — but it's just **iroh-gossip 0.101 re-exported behind actors** (`p2panda-net-0.7.0/Cargo.toml: iroh-gossip = "0.101.0"`) | use iroh-gossip directly on both targets |
| `Discovery` (PSI topic-interest) | rooms are joined by explicit invite/URL, not by confidential topic scanning | not needed |
| mDNS (`iroh-mdns-address-lookup 0.4`) | nice for plugin LAN jams | usable **directly with raw iroh** on native; it's an iroh address-lookup crate, not p2panda-specific |
| AddressBook/supervision | — | iroh `MemoryLookup` + pkarr covers it |

Conclusion: p2panda-net does not earn its complexity for walkie. (The `proposal.md`
line "Replace libp2p transport with iroh 1.0 (browser-direct) + p2panda-net 0.7
(native)" and `tasks.md` 5.3 should be amended to topology A; `integration-tests.md`
L1c "interop raw-iroh↔p2panda-net" becomes "browser-wasm ↔ native, same protocol".)

### 1.4 Could a raw peer speak iroh-gossip's ALPN into p2panda-net's overlay anyway?

Technically the gossip wire is iroh-gossip's, but p2panda-net (a) mixes the ALPN with
its network id, (b) gates overlay join on AddressBook topic-interest
(`node_infos_by_topics`), populated only by its own `Discovery` actors, and (c) runs
sync sessions via its `SyncManager` membership task. A raw peer could reproduce (a)
(potluck proves the mixing is reproducible) but not (b)/(c) without reimplementing the
actors. Not worth it when A removes the problem.

## 2. Protocol design

### 2.1 Identity — one Ed25519 key (task 5.1)

One 32-byte seed per participant; the same seed feeds both keys, so the p2panda
author ID and the iroh endpoint ID are **the same 32 public-key bytes**:

```rust
// src/net/identity.rs
pub struct Identity {
    seed: [u8; 32],
}
impl Identity {
    /// p2panda signing key (ops.rs). ed25519-dalek from seed.
    pub fn signing_key(&self) -> room::ops::SigningKey { signing_key_from_seed(&self.seed) }
    /// iroh endpoint secret. Same curve, same seed ⇒ same public key bytes.
    pub fn iroh_secret(&self) -> iroh::SecretKey { iroh::SecretKey::from_bytes(&self.seed) }
    pub fn author_id(&self) -> AuthorId { AuthorId(*self.signing_key().verifying_key().as_bytes()) }
    // author_id().0 == endpoint_id.as_bytes(): asserted by a unit test.
}
```

- **Browser:** seed generated with `crypto.getRandomValues` (getrandom 0.3 `wasm_js`),
  persisted in IndexedDB via the existing `src/web/storage.rs` plumbing (replaces the
  per-session `Keypair::generate_ed25519()` in `libp2p_sync.rs:72`).
- **Native plugin:** seed file in the plugin state dir (0600), created on first run
  (replaces `SwarmBuilder::with_new_identity()` in `src/plugin/mod.rs:429`).
- Domain-separation note: the key signs p2panda CBOR headers and authenticates iroh's
  TLS handshake. These signature domains do not overlap (TLS 1.3 signs
  context-prefixed transcript hashes); acceptable, and it is precisely potluck's P12
  "one key = NodeId + author" precedent (`node.rs` `NetworkOptions::signing_key`).

### 2.2 Room name → topic (task 5.4)

- Op-level binding stays the **human room name string** (`VersionedOp::topic`,
  verified by `verify_signed_op_for_topic`) — unchanged, already tested.
- Transport-level topic: `TopicId = Hash::digest(b"walkie-songie/room/v1\0" ++ room_name)`
  (blake3, 32 bytes; `p2panda_core::Hash` and `iroh_gossip::proto::TopicId` are both
  32-byte blake3-friendly). One function in `src/net/topic.rs`, golden-vector tested.
  The namespace prefix means a walkie room can never collide with another app's topic.

### 2.3 Invite ticket (bootstrap)

```rust
// src/net/ticket.rs — modeled on potluck's TableTicket (potluck-transport-iroh/src/lib.rs),
// minus the p2panda network-id (we have no ALPN mixing).
pub struct WalkieTicket {
    room: String,                    // reject cross-room pastes before dialing
    endpoint: iroh_tickets::endpoint::EndpointTicket, // EndpointAddr: id + relay URL(+ ips)
    proto_version: u16,              // wire generation; bump = incompatible
}
impl iroh_tickets::Ticket for WalkieTicket { const KIND: &'static str = "walkie"; /* postcard */ }
```

- Carried in the room URL fragment (`#room=<name>&t=<ticket>`) and in the existing QR
  code path (`qrcode` dep already present). Replaces today's `room@peer-id` hack
  (`app.rs:963-967`).
- Any peer can mint one **after** its endpoint reports a relay address (potluck's
  `relay_table_ticket` wait-for-relay pattern, `node.rs:239-269`).
- Joining: insert the ticket's `EndpointAddr` into the endpoint's `MemoryLookup`
  (`iroh::address_lookup::memory::MemoryLookup` — the documented API for out-of-band
  addresses), then `gossip.subscribe_and_join(topic_id, vec![ticket.endpoint_id()])`.
- Ticketless join (same room name typed on two devices) is NOT in v1. If wanted later:
  a ~200-line HTTP rendezvous exactly like `potluck/crates/potluck-rendezvous`
  (publish `{room_hash → endpoint hints, TTL 60 s}`), far simpler than the current
  libp2p `relay-server/`.

### 2.4 Endpoint setup

```rust
// src/net/endpoint.rs
// BOTH targets: iroh = { version="1.0.3", default-features=false, features=["tls-ring"] }.
// presets::N0 = Minimal(ring crypto) + PkarrPublisher + PkarrResolver (HTTPS, works in
// browsers: iroh-1.0.3/src/endpoint/presets.rs:116-140) + n0 default relays
// (+ DnsAddressLookup natively).
pub async fn bind(identity: &Identity, seeds: MemoryLookup) -> Result<Endpoint, NetError> {
    let builder = Endpoint::builder(presets::N0)
        .secret_key(identity.iroh_secret())
        .address_lookup(seeds)                  // ticket-seeded addresses
        .alpns(vec![iroh_gossip::net::GOSSIP_ALPN.to_vec(), WALKIE_RBSR_ALPN.to_vec()]);
    #[cfg(not(target_arch = "wasm32"))]
    let builder = builder.address_lookup(iroh_mdns_address_lookup::MdnsAddressLookup::/*…*/);
    builder.bind().await
}
```

- Browser: relay-mediated only (no UDP in wasm) — the endpoint is dialable at its
  relay URL; timeouts via `gloo_timers` exactly as potluck's `browser.rs` does.
  A self-hosted relay stays possible via `RelayMode::Custom` (potluck's
  `browser_relay_mode`, `browser.rs:86-101`).
- Native: full QUIC + hole-punching; optional mDNS address lookup for LAN plugin jams
  (feature `lan`, default on for the plugin build).
- Protocol registration: `iroh::protocol::Router` with two handlers — `Gossip`
  (implements `ProtocolHandler`, `iroh-gossip-0.101.0/src/net.rs:129`) and our
  `RbsrProtocol`.

### 2.5 Live gossip path (tasks 5.2/5.3 unified)

```rust
// src/net/gossip.rs
let gossip = iroh_gossip::net::Gossip::builder()
    .max_message_size(64 * 1024)     // default is 4096 (proto.rs:69); SetTuning scl text needs more
    .spawn(endpoint.clone());
let topic = gossip.subscribe(topic_id, bootstrap_ids).await?;
let (sender, mut receiver) = topic.split();
```

- **Outbound:** `RoomStore::commit(&key, topic, ts, op) -> SignedOp` (store.rs:258)
  → `wire::frame(&signed)` (postcard `{header, payload}` — the exact bytes the author
  signed, never re-encoded) → `sender.broadcast(bytes)`.
- **Inbound:** `Event::Received(msg)` → `wire::unframe` →
  `verify_signed_op_for_topic(&signed, room)` → `ingest_verified` (ops.rs:383,
  store.rs:156). Idempotent by the store's dedup; wrong-topic/forged bytes are dropped
  exactly as the L0 suite (W11) asserts.
- **Membership events:** `Event::NeighborUp(id)` / `NeighborDown(id)` surface as
  `NetEvent::PeerJoined/PeerLeft` (AuthorId == EndpointId bytes) and trigger
  anti-entropy (below). Plumtree gives fast fanout; HyParView + gossip's internal
  `GossipAddressLookup` spreads peer addresses beyond the bootstrap ticket.
- Oversize guard: `commit` callers cap `SetTuning`/`SetConfig` payloads under the
  gossip limit; anything larger still converges via RBSR (which has no such limit).

### 2.6 Anti-entropy path (task 5.4)

Trigger points: on `NeighborUp` (a peer appeared), on a jittered periodic timer
(~30 s), on browser `visibilitychange`/reconnect, and once at room join. One in-flight
session per peer, initiator = the side that noticed (sessions are idempotent; a
crossed pair is only wasted bytes).

```
initiator                            responder            (one bidi stream, ALPN walkie/rbsr/1,
---------                            ---------             frames = u32-be length + postcard)
Hello{strategy:"walkie-entryhash/1", salt} ─▶              responder adopts salt (kernel rule)
Recon(Ranges[full-range fp])        ─▶
                                    ◀─ Recon(Ranges|Items) …RBSR descent (reconciliation.rs respond)
Fetch[missing entry-hashes]         ─▶
                                    ◀─ Entries[(hash, signed-op bytes) + causal closure]
  …ingest (verify → ingest_verified), rebuild index, continue…
Done                                ─▶
                                    ◀─ Done                 close stream
```

- Index per side: `SortKey(entry_hash bytes) → EntryHash` over
  `RoomStore::entry_hashes()` — verbatim `tests/support/reconcile.rs::build_index`.
- `Entries` carries the sender's verbatim `SignedOp` bytes for each hash **plus its
  causal closure** (backlink + observed walk — `reconcile.rs::collect_with_past`,
  kernel `completion_plan` semantics) so every transferred op lifts immediately
  instead of parking across ranges. Receiver ingests through the production ingress
  only — a peer cannot inject unverified entries because the entry is *re-derived*
  from the verified op (store.rs frame/lift), never trusted from the wire.
- Fixpoint: `respond` returns no replies and no `Fetch` is outstanding ⇒ `Done`.
  When task 3.5 lands, both sides also exchange `canonical_root()` inside `Done` as a
  convergence check (mismatch ⇒ log + rerun, never panic in prod).
- Budgets: 1 MiB max frame, 512 hashes per `Fetch`, 60 s session timeout, guard
  counter mirroring the L0 driver's `guard < 100_000`.

## 3. H6 — exact `hhhs_core::reconciliation` additions (task 1.5)

> Refined by **Addendum A** below, which firms this up as a SHARED walkie+potluck
> kernel primitive (full state machine, budgets, potluck-generic notes). Where the
> two differ, Addendum A wins.

Kernel changes go to `/laboratory/fe-stuff/hhhs-rs` first (working tree), then:
commit → push GitLab → re-pin `rev` in walkie's `Cargo.toml` (both `hhhs-core` and
`hhhs-reactive`, currently `ce9e30dd…`) → coordinate the same re-pin in potluck.
Strictly additive — no reshaping of `Message`, `respond`, `VoidPolicy`, or `verdict`.

### 3.1 New wire messages (new enum, wrapping — not extending — `Message`)

Extending `Message` itself would break every downstream exhaustive match; instead add:

```rust
// hhhs-core/src/reconciliation.rs (additions)

/// Transport-level envelope for a full sync session. `Recon` wraps the existing
/// pure set-difference messages unchanged.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SyncMessage {
    Hello(SessionHello),                      // existing struct, now actually on the wire
    Recon(Message),                           // Ranges / Items / Done (unchanged)
    Fetch(Vec<EntryHash>),                    // "send bytes for these hashes"
    Entries(Vec<(EntryHash, Vec<u8>)>),       // hash → opaque app bytes (walkie: framed SignedOp)
    Done { root: Option<[u8; 32]> },          // fixpoint; optional canonical-root cross-check
}
```

The kernel stays byte-agnostic: `Entries` values are **opaque app bytes** — the app
decides what "the bytes for an entry" are (walkie: the verbatim signed op that lifts
to that entry; a future app could ship kernel `Entry` encodings). This preserves the
A7 invariant (no derived fields on the wire) and keeps verification app-side.

### 3.2 Sans-io session driver

```rust
/// One peer's half of a sync session. No IO, no clock, no store handle:
/// the app pumps messages in and ships the returned messages out.
pub struct SyncSession {
    index: Index, cfg: Config, salt: [u8; 16],
    awaiting: Option<Message>,        // Items held while the app ingests fetched entries
    outstanding_fetch: usize,
    done_sent: bool, done_seen: bool,
}

pub trait EntrySource {
    fn have(&self, h: &EntryHash) -> bool;
    /// Bytes for `h` PLUS its causal closure (order irrelevant; receiver drains).
    /// Returning the closure here is what keeps rounds at RBSR tree depth.
    fn bytes_with_closure(&self, h: &EntryHash) -> Vec<(EntryHash, Vec<u8>)>;
}

impl SyncSession {
    pub fn initiate(index: Index, cfg: Config, salt: [u8;16]) -> (Self, Vec<SyncMessage>);
    pub fn accept(hello: &SessionHello, index: Index, cfg: Config) -> Self; // adopts initiator's salt
    /// Pump one inbound message. `source` answers peers' Fetches; entries the PEER
    /// sent come back in `SessionOutput::ingest` for the app to verify+apply.
    pub fn on_message(&mut self, msg: SyncMessage, source: &impl EntrySource) -> SessionOutput;
    /// After the app ingests `SessionOutput::ingest` bytes, hand back the rebuilt
    /// index; the held `Items` reply is then produced (mirrors the L0 driver's
    /// rebuild-before-respond, reconcile.rs:130-132).
    pub fn resume(&mut self, index: Index) -> Vec<SyncMessage>;
    pub fn is_complete(&self) -> bool;        // done_sent && done_seen && outstanding_fetch == 0
}
pub struct SessionOutput {
    pub send: Vec<SyncMessage>,
    pub ingest: Vec<(EntryHash, Vec<u8>)>,    // verify & apply, then call resume()
}
```

Termination inherits from `respond`'s Items-equality guard; a `rounds` guard mirrors
`replica.rs:231`. Unit tests in-kernel: two `SyncSession`s over an in-memory duplex
reach fixpoint on the same corpora as `replica::reconcile` (equal `Stats.items`).

### 3.3 Wire encoding

Kernel gains a `wire` cargo feature (`postcard` + `serde` derives on `SyncMessage`,
`Message`, `KeyRange`, `FpBytes`, `SessionHello`, `SortKey`, `StrategyId`,
`EntryHash`) — dependency-free by default, so potluck's no-serde build is unaffected.
`Bound<SortKey>` serializes via a 3-variant tag (postcard handles `std::ops::Bound`).
Walkie frames each message as u32-be length + postcard bytes (potluck's framing,
`browser.rs:631-669`), 1 MiB cap.

### 3.4 Walkie-side adapter (`src/net/sync.rs`)

`EntrySource` implemented on the store bridge: `bytes_with_closure` =
`lifted_op_ids()` → `collect_with_past` over verified ops → `signed_ops()` bytes
(exactly `tests/support/reconcile.rs:53-75,108-127`). When H6 lands, the L0 driver's
hand-rolled transfer block is replaced by `SyncSession` — **assertions unchanged**
(that suite is the spec; reconcile.rs:20-23 already promises this swap).

## 4. `src/net/` module design (task 5.5)

Single crate, target-gated (walkie is `cdylib+rlib` — unchanged):

```
src/net/
  mod.rs        RoomNet facade, NetEvent, NetConfig, spawn glue (spawn_local vs tokio::spawn)
  identity.rs   §2.1  (browser IndexedDB / native seed file)
  topic.rs      §2.2  room name → TopicId (pure, golden-tested, wasm-safe)
  ticket.rs     §2.3  WalkieTicket + URL fragment codec (pure)
  endpoint.rs   §2.4  target-gated builders (the ONLY file with cfg forks besides identity persistence)
  gossip.rs     §2.5  broadcast/receive SignedOp frames, membership events
  sync.rs       §2.6+§3.4  RbsrProtocol (accept side) + initiate_sync(peer) (dial side)
  wire.rs       SignedOp frame codec (postcard {header, payload}) + length framing
```

```rust
pub enum NetEvent {
    Op(VerifiedOp),                                  // verified inbound; caller ingests
    PeerJoined(AuthorId), PeerLeft(AuthorId),
    Synced { peer: AuthorId, sent: usize, received: usize },
    Status(NetStatus),                               // Offline / RelayOnline / Direct(n)
}

/// The seam RoomNet needs from the store owner. The app keeps single-threaded
/// ownership of RoomStore (browser: Rc<RefCell<…>> on the main thread; plugin:
/// the store lives on the networking tokio task, exactly like today's
/// run_networking_thread channel pattern in src/plugin/mod.rs:366).
pub trait StoreBridge {
    fn entry_hashes(&self) -> BTreeSet<EntryHash>;
    fn bytes_with_closure(&self, want: &[EntryHash]) -> Vec<(EntryHash, SignedOp)>;
    fn ingest(&self, signed: SignedOp) -> Result<Option<VerifiedOp>, OpVerifyError>;
        // = verify_signed_op_for_topic + ingest_verified (+ §6.3 persistence hook); None = duplicate
}

impl RoomNet {
    pub async fn spawn(cfg: NetConfig, bridge: impl StoreBridge + Clone + 'static)
        -> Result<(RoomNet, impl Stream<Item = NetEvent>), NetError>;
    pub fn broadcast(&self, signed: &SignedOp);        // fire-and-forget after commit()
    pub fn ticket(&self) -> Option<WalkieTicket>;      // None until a relay is confirmed
    pub fn sync_now(&self, peer: EndpointId);          // manual/periodic anti-entropy kick
    pub async fn shutdown(self);                       // endpoint.close() (potluck's drop lesson)
}
```

Browser/native differences, exhaustively: (1) identity persistence (IndexedDB vs
file), (2) endpoint extras (mDNS lookup native-only; relay-only reachability on wasm),
(3) task spawning (`wasm_bindgen_futures::spawn_local` vs `tokio::spawn`) behind one
`net::spawn_task` shim, (4) timers (`gloo_timers` vs `tokio::time`) behind
`n0_future`/`web-time` (already a dep). Everything else — gossip, sync, wire, ticket,
topic — is shared, which is the point of topology A.

### Cargo changes

```toml
[dependencies]                                   # both targets — ONE iroh in the tree (1.0.3)
iroh          = { version = "1.0.3", default-features = false, features = ["tls-ring"] }
iroh-gossip   = { version = "0.101", default-features = false, features = ["net"] }
iroh-base     = "1.0"
iroh-tickets  = "1.0"
postcard      = { version = "1", features = ["alloc", "use-std"] }
hhhs-core     = { git = …, rev = "<H6 re-pin>" }  # + hhhs-reactive, same rev

[target.'cfg(not(target_arch = "wasm32"))'.dependencies]
iroh-mdns-address-lookup = { version = "0.4", optional = true }   # feature "lan"
# REMOVED: all six libp2p git-fork lines (both targets), gloo-net
```

Enforce the single-iroh rule with `cargo tree -i iroh` in CI (proposal.md already
mandates mirroring potluck's pins).

## 5. Migration

1. **`src/web/libp2p_sync.rs` → deleted.** `app.rs:984 start_libp2p_room_sync(room, room_topic, iroh_peer_id)`
   becomes `net::RoomNet::spawn` + a `spawn_local` loop that pumps `NetEvent::Op` into
   `RoomStore` and pushes `commit()` results through `RoomNet::broadcast`. Note the
   coupling: the yrs `RoomState` dies with it, so §5 wiring and §6.1 (app.rs reads ←
   HHHS views) land in the same PR or behind a feature flag — there is no useful
   "iroh transport, yrs state" intermediate.
2. **Room URL scheme:** `?room=name@peer-id` → `#room=<name>&t=<walkie-ticket>`;
   `get_or_generate_room_name` (app.rs:964) loses the `@peer-id` split; QR/invite UI
   mints `RoomNet::ticket()`.
3. **`relay-server/` → deleted** (with `Dockerfile.relay`, `docker-compose.relay.yml`,
   deploy scripts). Its roles map to: circuit relay → n0 public relays (or a
   self-hosted `iroh-relay` later via `RelayMode::Custom` — one binary, no walkie
   code); gossipsub backbone → iroh-gossip overlay itself; signaling → gone (QUIC via
   relay needs none). No walkie-owned server remains in v1; the optional future
   rendezvous (§2.3) would be a new tiny crate, not a revival of relay-server.
4. **Plugin (`src/plugin/mod.rs`):** keep the thread + crossbeam channel architecture
   (`run_networking_thread`, cmd/evt channels) — only `run_networking_loop`'s body
   changes: libp2p swarm loop → `RoomNet::spawn` on the same tokio runtime, store
   owned by that task, `RELAY_ADDR`/`libp2p.wondering.xyz` constants deleted. Identity
   from the plugin-state seed file (§2.1). The nice-plug move (§7) is orthogonal and
   NOT gated on this.
5. **Docs:** amend `proposal.md` (§What Changes bullet 1) and `tasks.md` 5.3 from
   "native: p2panda-net node" to "native: same raw-iroh stack"; retitle
   `integration-tests.md` L1c interop to "wasm↔native same-ALPN" (the interop flag it
   raises is hereby resolved: there is nothing to interop — one protocol).

## 6. Build order

1. **H6 in `fe-stuff/hhhs-rs`** (§3): `SyncMessage` + `SyncSession` + `wire` feature +
   in-kernel duplex tests → commit/push → re-pin in walkie; port
   `tests/support/reconcile.rs` onto `SyncSession` (assertions unchanged — this is the
   proof the kernel driver is faithful, before any sockets exist). Coordinate potluck
   re-pin (additive, so no potluck breakage expected).
2. **Pure modules:** `net/{topic,ticket,identity,wire}.rs` + unit tests (all wasm-safe;
   the AuthorId==EndpointId byte-equality test lives here).
3. **Native first:** `endpoint.rs` + `gossip.rs`; L1a (two native endpoints,
   `iroh test-utils` relay: commit on A appears on B via gossip within timeout).
4. **Sync over streams:** `sync.rs` (RbsrProtocol accept + initiate); L1b late-join
   catch-up (= W7 at L1) + reconnection; wrong-topic/forged rejection at L1 (W11).
5. **Wasm build + browser jam:** compile the same crate for wasm32 (build env below),
   manual two-tab jam via ticket URL; L3 golden vector already pins wasm==native
   hashing (W16).
6. **Swap call sites** (§5 migration 1–4), delete libp2p/relay-server (§8), then L2
   patchbay (W13/W17) on the native path.

## 7. Honest caveats

- **Browser latency floor:** wasm iroh is relay-only (no UDP) — every browser byte
  crosses an iroh relay via WebSocket. For a live jam the op-plane tolerates this
  (walkie synthesizes locally; ops are small and Plumtree-fanned), but two browsers on
  the same LAN still round-trip a possibly-distant n0 relay. Mitigation lever, later:
  self-hosted `iroh-relay` near users; native↔native does hole-punch to direct QUIC.
- **`ring` needs clang for wasm32.** The probe build fails without a C compiler for
  wasm (`error occurred in cc-rs: failed to find tool "clang"`). Fix is potluck's
  flake precedent (`potluck/flake.nix:30`):
  `CC_wasm32_unknown_unknown = ${llvmPackages.clang-unwrapped}/bin/clang` in walkie's
  dev shell + CI. Also keep `--cfg getrandom_backend="wasm_js"` (getrandom 0.3).
- **iroh-gossip-on-wasm is the load-bearing bet of §2.5 — and it is VERIFIED.**
  A probe crate with exactly `iroh = { version = "1.0.3", default-features = false,
  features = ["tls-ring"] }` + `iroh-gossip = { version = "0.101.0", default-features
  = false, features = ["net"] }` passes `cargo check --target wasm32-unknown-unknown`
  cleanly (2026-07-30, rustc 1.98 nightly, with `CC_wasm32_unknown_unknown=clang` and
  `--cfg getrandom_backend="wasm_js"`). Structurally it holds because iroh-gossip runs
  on `n0-future` and its tokio dep is only `io-util`+`sync` (both wasm-supported).
  **Fallback if it ever regresses:** `gossip.rs` is the only consumer — replace with a hand-rolled flood
  (broadcast to every known peer over persistent bidi streams on a `walkie/flood/1`
  ALPN, peer set = ticket + peers learned in RBSR sessions). For ≤ 8-person jam rooms
  flood is entirely adequate; the RBSR layer is unaffected either way.
- **Gossip is lossy by design** — no retransmit buffer (the L0 suite's W2/W8 models
  this). Convergence is owned by the RBSR layer; the periodic timer bounds the repair
  window (~30 s worst case, immediate on NeighborUp).
- **pkarr publish lag:** a fresh browser endpoint takes seconds to become resolvable
  via n0 DNS; tickets carry the full `EndpointAddr` (relay URL) so the first dial
  never waits on pkarr.
- **4 KiB default gossip message cap** raised to 64 KiB; `SetTuning` scl text must be
  capped at commit ingress anyway (a hostile peer's oversize op simply won't gossip —
  it still syncs via RBSR, which is fine).
- **Persistence interlock (§6.3):** RBSR serves only lifted ops; a restarted client
  must replay persisted signed bytes into `RoomStore` before its first session, or it
  will re-download its own history (correct but wasteful).
- **Test attach points:** L1 uses iroh `test-utils` (`run_relay_server`,
  `MemoryLookup` fixtures — `iroh/src/{socket.rs:2342,protocol.rs:820}`) — the
  p2panda-net `TestNode` path in integration-tests.md is dropped with topology B.
  L2 patchbay exercises native QUIC (relay→direct migration W17); browser-path L2
  does not exist (relay-only, nothing to migrate).

---

# Addendum A — sync layer confirmed: HHHS reconciliation, shared with potluck

Status: supersedes §3 where they differ. Context: the maintainer has directed potluck
to move to **full HHHS r/w** — HHHS owning reads, writes, AND sync via
`hhhs_core::reconciliation`, off p2panda LogSync. Both projects therefore converge on
the same sync primitive, and the "home-grown vs maintained sync" question dissolves:
the sync protocol is **HHHS's own reconciliation kernel** (walkie's L0
`tests/support/reconcile.rs` already drives it verbatim — we did not invent a diff
protocol, we drive the kernel's). H6 is now a shared kernel deliverable consumed by
two projects, which raises the bar on its API design.

## A.0 Closing the p2panda-sync question (evidence, for the record)

Investigated before the direction landed; it confirms the direction independently:

- **`p2panda-sync` 0.7's `Protocol` trait IS transport-agnostic** — `fn run(self,
  sink: impl Sink<Message>, stream: impl Stream<Item = Result<Message>>)`
  (`p2panda-sync-0.7.0/src/traits.rs:15-25`), so LogSync *could* in principle be
  driven over an iroh bidi stream with a codec.
- **But the published crate cannot reach a browser.** Its Cargo.toml hard-depends on
  `p2panda-store = { version = "0.7.0", default-features = true }` — and
  p2panda-store's default features are `["sqlite", "macros"]` where
  `sqlite = ["dep:sqlx", "dep:tokio", …]` (`p2panda-store-0.7.0/Cargo.toml:48-72`).
  A dependency edge with default features on cannot be disabled downstream without
  forking. **Probe verified** (2026-07-30): a crate depending only on
  `p2panda-sync = "0.7.0"` fails `cargo check --target wasm32-unknown-unknown` deep in
  `mio` (UDP sockets, pulled via sqlx/tokio-net) — 48 errors, unfixable from outside.
- **With default features off, p2panda-store 0.7 is traits-only** (`logs::traits`,
  `operations::traits` — no memory or IndexedDB backend; the only impl is
  `SqliteStore`, `src/lib.rs:136-143`). `RoomStore` *could* implement
  `LogStore`/`OperationStore` (async trait methods over author/log-id/seq —
  `logs/traits.rs:42`, `operations/traits.rs:18`), but that buys entry into a sync
  protocol we can't compile for the browser and whose per-author-log model duplicates
  what `entry_hashes()` already gives us.
- **No published p2panda beyond 0.7.0 exists** (crates.io sparse index, 2026-07-30:
  latest of p2panda-{core,net,store,sync} is 0.7.0). Anything newer means pinning an
  unreleased git rev of a stack that potluck's own `node.rs` header warns "differs
  meaningfully" from its releases — exactly the supply-chain posture (fork/unreleased
  pins) this rewrite exists to escape.

## A.1 Decision (confirmed)

**Sync = `hhhs_core::reconciliation` over raw iroh, ALPN `walkie/rbsr/1`, identical on
browser and native.** RBSR set reconciliation over `RoomStore::entry_hashes()`,
verbatim `SignedOp` bytes as the transfer payload, verification only at the app
ingress. p2panda-core signing stays; p2panda net/store/sync are not adopted, in either
project. Topology A stands unchanged.

## A.2 H6 as a shared kernel primitive — implementable spec

Everything below lands in `/laboratory/fe-stuff/hhhs-rs` `hhhs-core/src/reconciliation.rs`
(plus a sibling `reconciliation/session.rs` if preferred), **strictly additive**:
`Message`, `respond`, `opening`, `Index`, `completion_plan`, `VoidPolicy`, `verdict`
are untouched. Process: edit upstream → commit → push GitLab → re-pin `rev` in walkie
(`hhhs-core` + `hhhs-reactive`, currently `ce9e30dd…`) AND in potluck.

### A.2.1 Wire messages

```rust
/// Transport envelope for one sync session. `Recon` wraps the existing pure
/// set-difference messages UNCHANGED — extending `Message` itself would break
/// downstream exhaustive matches (potluck matches on it).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SyncMessage {
    /// First frame, initiator → responder. Responder MUST adopt the salt
    /// (reconciliation.rs:28-30 rule) or `Abort` on strategy mismatch.
    Hello(SessionHello),
    /// The existing RBSR messages: Ranges / Items / Done.
    Recon(Message),
    /// "Send me the bytes for these entry hashes." ≤ FETCH_MAX_HASHES per message;
    /// issue multiple Fetches for more.
    Fetch(Vec<EntryHash>),
    /// Answer to exactly one `Fetch`: requested hashes PLUS causal closure, as
    /// (entry hash, opaque app bytes) pairs. One `Entries` per `Fetch`, 1:1.
    Entries(Vec<(EntryHash, Vec<u8>)>),
    /// "I have nothing more to compare or request." A peer that sent `Done` MUST
    /// still answer inbound `Fetch`es until the session closes. Optional
    /// canonical-root for a convergence cross-check (log-only on mismatch).
    Done { root: Option<[u8; 32]> },
    /// Terminal refusal: strategy mismatch, budget exceeded, shutting down.
    Abort { reason: String },
}
```

Opaque-bytes contract (the A7 invariant, kept): the kernel never interprets `Entries`
values. Walkie: framed verbatim `SignedOp` (postcard `{header, payload}`); potluck:
its source-op record bytes (`potluck-store-p2panda/src/source_ops.rs` shape). The
receiving APP verifies and ingests; the kernel only learns the result via the rebuilt
index handed to `resume()`. A hostile peer can therefore waste bytes but can never
inject an entry: the entry hash is re-derived from verified content, never trusted
from the wire.

### A.2.2 The sans-io session driver

```rust
/// One peer's half of a sync session. No IO, no clock, no store handle, no Send
/// bounds (must be usable on wasm's single thread).
pub struct SyncSession {
    index: Index,
    cfg: Config,
    salt: [u8; 16],
    role: Role,                       // Initiator | Responder
    awaiting: VecDeque<Message>,      // Items held while the app ingests fetched bytes
    outstanding_fetches: usize,       // my Fetches not yet answered by Entries
    done_sent: bool,
    done_seen: bool,
    budget: SessionBudget,            // counters checked on every transition
}

/// The ONLY integration surface an app implements. Walkie: over RoomStore
/// (`lifted_op_ids` → collect_with_past → `signed_ops` bytes, exactly
/// tests/support/reconcile.rs:53-75,108-127). Potluck: over its HHHS mirror's
/// source-op bytes. Deliberately store-agnostic and synchronous (both stores are
/// in-memory mirrors; an async store would wrap this with a snapshot).
pub trait EntrySource {
    fn have(&self, hash: &EntryHash) -> bool;
    /// Bytes for `hash` PLUS its causal closure (predecessors the peer may lack),
    /// deduped against `already_included`. Closure-on-send keeps round count at
    /// RBSR tree depth (the completion_plan guarantee, reconciliation.rs:247).
    fn bytes_with_closure(
        &self,
        hash: &EntryHash,
        already_included: &mut BTreeSet<EntryHash>,
    ) -> Vec<(EntryHash, Vec<u8>)>;
}

pub struct SessionOutput {
    pub send: Vec<SyncMessage>,                 // ship these, in order
    pub ingest: Vec<(EntryHash, Vec<u8>)>,      // verify+apply, then call resume()
}

impl SyncSession {
    /// Initiator: returns the session plus the opening frames
    /// [Hello{strategy, salt}, Recon(opening(&index))].
    pub fn initiate(strategy: StrategyId, index: Index, cfg: Config, salt: [u8; 16])
        -> (SyncSession, Vec<SyncMessage>);

    /// Responder: called with the peer's Hello. Err(reason) ⇒ send Abort + close.
    /// `index` must already be built under `hello.session_salt` (the app builds it
    /// after reading Hello — the initiator's salt wins).
    pub fn accept(hello: &SessionHello, expected: StrategyId, index: Index, cfg: Config)
        -> Result<SyncSession, String>;

    /// Pump one inbound frame. Never blocks, never ingests: entries the peer sent
    /// arrive in `SessionOutput::ingest`; if non-empty the app MUST verify+apply
    /// them and then call `resume()` before pumping further frames.
    pub fn on_message(&mut self, msg: SyncMessage, source: &impl EntrySource)
        -> Result<SessionOutput, SessionError>;

    /// Hand back the index rebuilt after ingest (mirrors the L0 driver's
    /// rebuild-before-respond, tests/support/reconcile.rs:130-132). Drains
    /// `awaiting` through `respond()` and may emit Fetch/Done.
    pub fn resume(&mut self, index: Index) -> Result<Vec<SyncMessage>, SessionError>;

    /// done_sent && done_seen && outstanding_fetches == 0.
    pub fn is_complete(&self) -> bool;
}
```

### A.2.3 State machine (normative)

On `on_message(msg)`:

1. `Recon(Ranges(..))` → `respond(&self.index, msg, &self.cfg)` → wrap replies in
   `Recon`; if replies are empty AND nothing pending anywhere, emit `Done` (rule 4).
2. `Recon(Items(range, theirs))` → `missing = theirs.iter().filter(|h| !source.have(h))`.
   - `missing` empty → `respond()` immediately (its equality guard yields `[]` or my
     `Items`), continue as (1).
   - else → push the `Items` onto `awaiting`, emit `Fetch(missing)` (chunked at
     `FETCH_MAX_HASHES`), `outstanding_fetches += chunks`.
3. `Fetch(hashes)` → answer with exactly one `Entries`: for each hash,
   `source.bytes_with_closure(hash, &mut included)`; unknown hashes are silently
   omitted (the peer's next `Items` round self-corrects). Answer even after having
   sent `Done`.
4. `Entries(pairs)` → `outstanding_fetches -= 1`; return the pairs as
   `SessionOutput::ingest` (app verifies → applies → `resume(new_index)`).
   `resume` then: re-runs `respond()` over every held `awaiting` Items with the new
   index, emits those replies, and if replies are empty, `awaiting` is empty, and
   `outstanding_fetches == 0`, emits `Done { root }` (once; `done_sent = true`).
5. `Done { root }` → `done_seen = true`; if roots were exchanged and differ, surface
   `SessionOutput` with a diagnostic flag (app logs; NEVER a protocol error — the next
   periodic session repairs). If `is_complete()`, the transport closes the stream.
6. `Recon(Message::Done)` (the legacy pure-layer variant) → treated as a no-op frame
   (the session-level `Done` is authoritative); kept only so `respond()`'s output can
   be forwarded blindly.
7. `Hello` after the first frame, `Entries` without outstanding fetch, or any frame
   after `Abort` → `SessionError::Protocol` (transport closes).

Termination: initiator closes the stream once `is_complete()`; responder treats
stream-close after `done_seen && done_sent` as normal completion, anything else as an
aborted session (no state to roll back — ingest is idempotent, sessions are cheap to
rerun). Liveness inherits from `respond`'s Items-equality guard plus the budget below;
a `rounds` guard mirrors `replica.rs:231`'s `guard < 100_000`.

### A.2.4 Budgets (`SessionBudget`, kernel-enforced, app-tunable)

| knob | default | exceeded ⇒ |
|---|---|---|
| `FETCH_MAX_HASHES` | 512 | chunk automatically |
| `max_rounds` | 4 096 | `Abort{"rounds"}` |
| `max_entries_ingested` | 65 536 | `Abort{"entries"}` |
| `max_outstanding_fetches` | 64 | `SessionError::Protocol` |
| frame size (app framing) | 1 MiB | transport closes |

### A.2.5 Wire encoding — `wire` cargo feature

`hhhs-core` today has zero serde dependency; keep the default build that way. New
feature `wire = ["dep:serde", "dep:postcard"]` adds derives on `SyncMessage`,
`Message`, `KeyRange`, `FpBytes`, `SessionHello`, `SortKey`, `StrategyId`,
`EntryHash` (serde has `Bound<T>` impls; postcard handles the enums; `FpBytes`/
`EntryHash` as fixed 32-byte arrays). Enum evolution rule: **append variants only** —
postcard tags are ordinal. Protocol *generation* bumps live in the app's ALPN
(`walkie/rbsr/1` → `/2`), never in reshaped kernel variants. Apps frame each message
as u32-be length + postcard bytes (potluck's framing, `browser.rs:631-669`).

### A.2.6 Shared-consumer requirements (potluck)

- No walkie types anywhere in the kernel API: `EntrySource` speaks
  `EntryHash`/bytes only; `Index` construction (and therefore sort-key strategy)
  stays app-side — walkie indexes by raw entry-hash bytes, potluck may index by its
  facet strategy; `SessionHello.strategy` guards the mismatch at accept-time.
- No `Send`/`Sync` bounds on `SyncSession`/`EntrySource` (wasm single-thread);
  potluck's tokio node wraps the session in its own task and that works with
  `Send`-free types because the session lives entirely inside one task.
- Kernel tests prove parity with the in-process driver: two `SyncSession`s pumped
  over an in-memory duplex must fetch the same item count as `replica::reconcile`
  (`Stats.items`) on the same corpora, plus salt-adoption, Abort-on-mismatch,
  budget-exhaustion, and closure-dedup cases.
- Acceptance in walkie: swap `tests/support/reconcile.rs`'s hand-rolled transfer loop
  onto `SyncSession` — every W1–W16 assertion unchanged (reconcile.rs:20-23 already
  reserves this seam). Acceptance in potluck: its store-p2panda mirror implements
  `EntrySource` over source-op bytes and reaches fixpoint in its two-node test.

## A.3 Experimental p2panda — anything still worth wanting? (brief)

Nothing published exists beyond 0.7.0 (sparse index, 2026-07-30), so "newer p2panda"
means pinning an unreleased git rev — the supply-chain posture this rewrite exists to
end. Capability-wise, raw iroh 1.0 + `presets::N0` already covers what p2panda-net's
batteries would give walkie: relay + hole-punching (iroh core), address
publish/resolve including in-browser (pkarr over HTTPS, `presets.rs:116-140`), LAN
lookup natively (`iroh-mdns-address-lookup`, an iroh crate p2panda merely re-uses),
and membership/fanout (iroh-gossip, wasm-verified §7). The one genuinely unique
p2panda-net capability — `Discovery`'s confidential PSI topic-interest exchange
(learn who shares a topic without revealing yours) — solves a privacy problem walkie's
explicit-invite rooms don't have; if ticketless room join is ever wanted, the
potluck-rendezvous pattern (~200-line HTTP hint service) covers it without adopting a
stack. Re-evaluate only if p2panda ships a *released* browser-capable net/sync (watch
for: p2panda-store growing a non-sqlite backend and p2panda-net dropping the
`address_book`→sqlx coupling).

---

# Addendum B — browser↔browser DIRECT on iroh: state, roadmap, and the walkie plan

Question: walkie's core case is browser↔browser LAN jams; plain iroh browser peers are
relay-only today. libp2p-webrtc (the fork) gave browser-direct. Can the iroh stack
deliver it — now or on a credible path? Everything below is **verified** against
sources unless marked *speculation*; dates are as of 2026-07-30.

## B.0 Verdict up front

**Browser-direct on iroh is real but not shipped: not first-party-imminent, buildable
by us against a designed-for-this extension point, and — critically — worth exactly
walkie's LAN case and little more.** Recommendation: **(d) now, (b) staged** — v1
ships relay-only browsers (identical protocol semantics), and browser-direct arrives
later as an *additive* WebRTC custom transport that changes zero protocol code.
Removing libp2p forecloses **nothing** (§B.6). n0's own field data (§B.3) shows
WebRTC direct succeeds ~100% on the same LAN — walkie's jam case — and ~0–20% across
real WANs, where the relay stays the path regardless.

## B.1 What iroh has today (source-verified)

- **No WebRTC anywhere**: `grep -ri webrtc` over `iroh-1.0.3/src`, `iroh-relay-1.0.3/src`
  and `noq-1.1.1/src` (n0's QUIC) finds nothing; same for `webtransport`. No
  first-party `iroh-webrtc` crate exists on crates.io.
- **Browser = relay-only, by design, documented**: the wasm build compiles the IP
  transport out entirely; official docs and the "Iroh & the Web" post (2024-07-01,
  https://www.iroh.computer/blog/iroh-and-the-web) describe browser mode as
  relay-via-WebSocket, E2E-encrypted, no hole-punching. That post already names
  WebRTC data channels as the eventual direct path ("We can leverage the WebRTC data
  channel APIs to send packets directly") and already flags the cost ("equipping
  native iroh clients with WebRTC stacks, and those are *heavy*").

## B.2 The custom-transport extension point — how real is it? (source-verified)

This is not a stub; it is a complete datagram-path abstraction that the wasm build
treats as first-class:

- **API** (iroh 1.0.3, feature `unstable-custom-transports`):
  `CustomTransport::bind() -> Box<dyn CustomEndpoint>`;
  `CustomEndpoint::{watch_local_addrs, create_sender, poll_recv, max_transmit_segments}`;
  `CustomSender::{is_valid_send_addr, poll_send}` — plain poll-based QUIC-datagram
  send/recv to opaque addresses (`src/socket/transports/custom.rs`). Wired via
  `Builder::add_custom_transport` (`src/endpoint.rs:813`) and
  `Builder::path_selector` (`endpoint.rs:840`) so an app can force "custom path wins
  over relay".
- **Addresses are first-class**: `TransportAddr::Custom(CustomAddr)` sits beside
  `Ip`/`Relay` in `EndpointAddr` (`iroh-base-1.0.3/src/endpoint_addr.rs:61,186`) —
  custom addrs ride tickets and address lookups like any other.
- **The wasm architecture anticipates exactly this**: on `wasm_browser`, the
  endpoint's transport set is literally *Custom + Relay* (`src/socket/transports.rs:92-96`
  — `Tuple<CustomTransportsWatcher, RelayTransportsWatcher>`; IP is compiled out).
  A browser custom transport is the designed non-relay path, not a hack.
- **Reference implementations ship**: a 744-line in-tree `TestTransport`/`TestNetwork`
  (`src/test_utils/test_transport.rs`, pluggable as a `Preset`) and
  `examples/custom-transport.rs` (in the published crate, including a custom
  `PathSelector`). The repo keeps a public **transport-ID registry**
  (`TRANSPORTS.md`: Test `0x20`; **Tor `0x544F52` → n0-computer/iroh-tor,
  "experimental", pushed 2026-06-15** — n0 dogfoods out-of-tree transports; BLE
  reserved).
- **Constraints (honest)**: (1) the 0.97 announcement
  (https://iroh.computer/blog/iroh-0-97-0-custom-transports-and-noq, 2026-03-16)
  says the API "is unstable and **will remain so for some time even after iroh 1.0
  is released**" — semver-exempt, may churn per release; (2) contract is "any
  unreliable datagram transport with ≥1200-byte packets" — WebRTC data channels in
  unreliable/unordered mode satisfy this (SCTP), *verified as a spec fact, not yet by
  us in code*; (3) the traits demand `Send + Sync + 'static`, but JS
  `RTCDataChannel` handles are `!Send` — a wasm impl needs a `SendWrapper`-style
  shim (sound on the single-threaded browser runtime, ugly); (4) **signaling is not
  provided** — iroh contributor flub, in discussion #4024: "WebRTC needs more than
  just custom transports… a need to carry the coordination payloads via another
  path. This is not something that iroh generically offers right now." The known
  solution (proven by community code, §B.4): run JSEP/SDP over an iroh QUIC bidi
  stream **through the relay connection you already have** — bootstrap direct from
  relay, which maps perfectly onto walkie (every room peer is already
  relay-connected).

## B.3 n0's roadmap + their own field data (dated, quoted)

- **Issue #3250 "WebRTC datachannels"** (opened 2025-03-31, closed COMPLETED
  2026-01-05, https://github.com/n0-computer/iroh/issues/3250): n0 field-tested
  WebRTC in real homes/offices. Their numbers: **same LAN / intranet / VPN /
  hotspot / same host (incl. between browser tabs): 100% success**; different
  residential/office ISP lines: **20%**; mobile/CGNAT/corporate/guest Wi-Fi: **0%**.
  Their framing: WebRTC's real use-case is "closest to… local network discovery with
  mDNS" — i.e. **it buys the LAN case, in browsers** — precisely walkie's jam.
- **PR #3440** "initial scaffolding for WebRTC support" (community, opened
  2025-08-18, **closed unmerged 2026-03-30**): closed with "conflicts… too much now
  with multipath. The recommended approach is to do this probably using custom
  transports. See discussion #4024."
- **PR #3845 "feat: Implement custom transports"** merged 2026-03-06 → shipped in
  0.97 (2026-03-16) and present in 1.0.x.
- **Discussion #4024** "Implementing a WebRTC Transport: iroh team or external?"
  (b5, n0 founder, 2026-03-17,
  https://github.com/n0-computer/iroh/discussions/4024): "We want to support WebRTC
  as an iroh transport… **the iroh team definitely wants to see this happen, and we
  will write an implementation if someone doesn't beat us to it, but at the moment
  the earliest we could get to is in the May–July 2026 timeframe**."
- **Where that stands today (2026-07-30)**: no n0-side WebRTC PR, branch, or crate
  has appeared; the newest WebRTC-touching PR in the repo is still the closed #3440;
  last activity on #4024 is a community post of 2026-05-10. The stated
  earliest-window is closing with nothing visible landed. *Speculation:* it slips to
  H2 2026 at best; treat first-party as unscheduled.

## B.4 Community / prior art (verified)

- **anchalshivank/iroh-webrtc-transport** (GitHub, pushed 2026-05-10, 8 stars, the
  effort n0's #4024 tracks): str0m-based; **claims working native↔native,
  browser↔browser, browser↔native**; JSEP signaling over **iroh QUIC bidi streams**
  (ALPN `iroh-webrtc-transport/signal/0`) or WebSocket; browser wasm crate built on
  `presets::N0`; bridges SCTP ↔ iroh custom transport (`poll_send`/`poll_recv`).
  Pins **iroh 0.97** (pre-1.0). Prototype-grade, single author.
- **`iroh-webrtc-transport` 0.1.0-alpha.2 on crates.io** (SuddenlyHazel, published
  2026-05-02, 33 downloads, https://github.com/SuddenlyHazel/iroh-webrtc-transport —
  a *different* project, same name): "WebRTC channel bootstrapping with signaling
  using Iroh's relays… it's still 'just Iroh'"; browser wasm facade + native impl.
  Pins **iroh 0.98.2**. Self-described "very much experimental".
- **iroh-examples #113** (open since 2025-03-23): browser↔browser via
  **matchbox_socket** (mature Rust WebRTC full-mesh crate, v0.14) with iroh
  replacing matchbox's signaling server — prior art for the hybrid path (c).
- **WebTransport does not help**: it is browser→*server* only (requires a
  server-side cert/listener); no browser↔browser mode exists in any shipping
  browser. n0's own 2024 post merely *hoped* ("Perhaps by then WebTransport will be
  extended to support direct connections?"). Dead end for this purpose today.

Two independent implementations both converge on the same architecture — relay
connection first, SDP over it, data channel as a custom-transport path — which is
strong evidence the design is right; neither is production-grade or tracks iroh 1.0.x.

## B.5 Blunt maturity verdict

- **(a) exists/ships soon (first-party)?** No. Zero WebRTC in iroh 1.0.3; n0's
  "earliest May–July 2026" window is expiring with no visible artifact. Wanting is
  documented; scheduling is not. Do not plan v1 around it.
- **(b) buildable-now by us?** Yes, genuinely: the extension point is designed for
  this (wasm transport set is Custom+Relay), two community prototypes prove
  browser↔browser end-to-end, and signaling-over-relay is a solved pattern.
  **Effort estimate: 2–4 focused weeks** to port/adopt against iroh 1.0.3
  (transport bridge + JSEP-over-iroh ALPN + wasm `RTCDataChannel` glue + Send-shim +
  `PathSelector`), **plus ongoing churn tax**: the API is explicitly semver-exempt
  ("unstable… for some time even after 1.0"), so every iroh upgrade may break the
  transport. Risk: medium; payoff: LAN + easy-NAT only (n0's field data), which *is*
  walkie's case.
- **(c) hybrid (matchbox data channel beside iroh)?** Works (examples#113) but
  strictly worse for walkie: a second connection system with its own auth/framing,
  the gossip/RBSR protocols would need a parallel non-iroh path, and none of iroh's
  connection model (ALPNs, streams, migration) applies. Rejected.
- **(d) relay-only v1?** This is the reality **for v1**, and it is not a semantic
  compromise: every design element in this document (gossip, RBSR, tickets,
  identity) is path-agnostic — a future direct path only lowers RTT.

## B.6 What walkie does — and the libp2p question

1. **v1 ships relay-only browsers** (this document unchanged). Latency mitigation
   levers that need no new transport: self-hosted `iroh-relay` near users
   (`RelayMode::Custom`, §2.4), and native↔native already hole-punches direct.
2. **Browser-direct is a planned additive follow-up, not a foreclosed dream**: an
   `iroh-webrtc` custom transport (adopt/port anchalshivank's or SuddenlyHazel's
   design onto 1.0.x, or n0's if they ship first — watch
   https://github.com/n0-computer/iroh/discussions/4024) slots in via
   `Builder::add_custom_transport` + a `PathSelector` preferring the WebRTC path.
   **Zero changes** to ALPNs, gossip, RBSR/H6, tickets, RoomStore, or identity: a
   transport adds a *path*, not a protocol. `src/net/endpoint.rs` is the single
   integration point (one builder call per target).
3. **Removing libp2p forecloses nothing.** The fork's WebRTC gave browser-direct
   only *inside libp2p's* connection model (gossipsub over libp2p streams) — it is
   unusable as a path under iroh QUIC and ties us to the unmaintained fork.
   Browser-direct on the new stack arrives through iroh's own designed extension
   point, which exists today in the exact iroh version we pin, with a public
   registry inviting third-party transports (`TRANSPORTS.md`). The two stacks'
   browser-direct paths share no code; keeping libp2p would buy nothing toward (b).
4. **Trigger to revisit**: n0 ships or blesses a WebRTC transport, or the
   custom-transport API is declared stable — then (b) drops from "2–4 weeks + churn
   tax" to "add a dependency".

Sources: https://www.iroh.computer/blog/iroh-and-the-web ·
https://iroh.computer/blog/iroh-0-97-0-custom-transports-and-noq ·
https://docs.iroh.computer/languages/wasm-browser ·
https://github.com/n0-computer/iroh/issues/3250 ·
https://github.com/n0-computer/iroh/pull/3440 ·
https://github.com/n0-computer/iroh/pull/3845 ·
https://github.com/n0-computer/iroh/discussions/4024 ·
https://github.com/n0-computer/iroh/blob/main/TRANSPORTS.md ·
https://github.com/n0-computer/iroh-tor ·
https://github.com/anchalshivank/iroh-webrtc-transport ·
https://github.com/SuddenlyHazel/iroh-webrtc-transport ·
https://github.com/n0-computer/iroh-examples/issues/113 ·
iroh-1.0.3 / iroh-base-1.0.3 / noq-1.1.1 sources in `~/.cargo/registry/src/`.

# Addendum C — Pluggable multi-transport (Agregore/Peersky; libp2p / hyperswarm / mDNS)

> **Status:** design + scaffold. The `Transport` trait in `src/net/mod.rs` is landed and
> dependency-free; no backend beyond iroh is implemented. Everything about Agregore and
> Peersky below is tagged **VERIFIED** (read from their source or docs at the cited path)
> or **SPECULATIVE** (inference that must be checked before any code is written).
>
> Nothing here changes the wire format. Every mode carries the same `SignedOp` bytes and
> the same `hhhs_core::sync_session::SyncMessage` frames, because §2/§3 and Addendum A
> already made the data layer transport-agnostic. A mode is a *carrier*, never a protocol.

---

## C.0 Why a trait at all

Three unrelated pressures converge on the same seam:

1. **The default stack is fine but its only end-to-end test is `#[ignore]`d.**
   `two_offline_endpoints_exchange_gossip_over_direct_ip` (native.rs:640) is the sole test
   that exercises gossip + discovery, and it needs real UDP + multicast, so CI never runs
   it. `cargo test --lib --no-default-features --features native-net` = 94 passed, **1
   ignored**. A `Transport` trait with an in-process `Loopback` impl makes the sync driver
   testable with zero sockets — which is the cheapest correctness win available.
2. **The HHHS sync driver does not exist yet.** `SyncSession` is driven *nowhere* in
   `src/` (only in `tests/support/reconcile.rs`, an in-memory harness). `repair.rs` gives
   framing + `EntrySource`; `native.rs` hands out a bare `iroh::endpoint::Connection` in
   `NativeNetworkEvent::IncomingRepair` and nothing consumes it. The missing `src/net/sync.rs`
   has to name *some* stream type — making that type abstract costs nothing today and is
   the whole ballgame later.
3. **P2P browsers are a real deployment target** and they do not offer QUIC/UDP to a page.
   If walkie is ever to run inside Agregore or Peersky as a first-class peer rather than a
   relayed web page, it must speak whatever those runtimes expose. That is a carrier swap,
   not a rewrite — provided the carrier is behind an interface.

---

## C.1 Audit: `src/net/native.rs` + `src/net/repair.rs`

These were written by an unreviewed agent run. They compile, and their unit tests pass
(8 of the 94 `native-net` tests). **Verdict: sound foundation, keep the shape, fix six
concrete defects before it carries traffic.** The event/command shape is genuinely good —
`bind` / `broadcast` / `next_event` / `begin_repair` / `ticket` / `peer_path` / `shutdown`
is almost exactly the right trait, which is why C.2 formalizes it nearly verbatim. The
defects are all in the *implementation*, not the interface.

### C.1.1 What is right (keep as-is)

- **Room-scoped mDNS with a truncated topic hash** (`room_mdns_service_name`, native.rs:78)
  — advertises 80 bits of `blake3::derive_key(…, room_name)`, never the room name.
  The test asserting `!service.contains("quiet")` is the right test to have written.
- **Ticket codec is defensive**: version pin, explicit length field cross-checked against
  the buffer, `MAX_ROOM_TICKET_BYTES` cap, and a `decode_bytes` test that tampers the
  length field. `Ticket::KIND = "walkieroom"` gives cross-kind rejection for free.
- **`peer_path` is honest** — it reads `TransportAddrUsage::Active` off iroh's own remote
  info rather than inferring a path from a heuristic. This is exactly what §2.4 asked for.
- **`RelayPolicy` is explicit, with `Disabled` a first-class variant** so offline-LAN tests
  provably never touch Internet infrastructure. `Custom(vec![])` is rejected rather than
  silently meaning "none". Good.
- **`repair.rs` framing is correct**: length is validated *before* the buffer is allocated,
  `read_sync_frame` distinguishes clean EOF (`Ok(None)`) from a truncated frame (`Err`),
  and `write_sync_frames` refuses oversize frames rather than emitting a length a reader
  would reject.
- **`build_repair_index` matches the L0 harness byte-for-byte** — both use
  `SortKey(entry_hash.as_bytes().to_vec())` (repair.rs:92 vs
  `tests/support/reconcile.rs:44`). A divergence here would make production sessions fail
  to reconcile while the test suite stayed green; it is worth stating that they agree.
- **`RoomRepairSnapshot` as an immutable horizon** is the right idea (§2.6 wants a stable
  `EntrySource` for the life of a session). See C.1.2(6) for the way it is currently unsafe.

### C.1.2 Defects — verified, ordered by severity

**(1) mDNS stream closure is a hot loop that will wedge the whole event task.** *(native.rs:356-361)*

```rust
let Some(mdns) = mdns else {
    let _ = events_tx.send(NativeNetworkEvent::Diagnostic("room mDNS event stream closed".into())).await;
    continue;   // <-- the stream is permanently exhausted; select! re-polls immediately
};
```

`MdnsAddressLookup::subscribe()` returns a `tokio_stream::wrappers::ReceiverStream`
(iroh-mdns-address-lookup-0.4.0/src/lib.rs:462-469). Once its sender drops, `next()`
returns `None` *forever*. So `continue` re-enters `select!`, that branch is instantly
ready again, and the loop spins emitting `Diagnostic` frames until the 512-slot channel
fills — at which point `send().await` blocks forever and **gossip delivery stops too**,
because it shares the same channel. Fix: latch the stream closed and replace the branch
with `std::future::pending()`, or `break`.

**(2) `join_ticket` leaks an address-lookup service on every call.** *(native.rs:484-489)*

Each call builds a fresh `MemoryLookup` and calls `endpoint.address_lookup().add(memory)`.
`AddressLookupServices::add` is `self.services.write().push(service)` with **no dedup and
no removal API** (iroh-1.0.3/src/address_lookup.rs:483-497). N ticket joins ⇒ N permanently
retained services, each consulted on every resolve and published to on every address change.
Fix: keep the `MemoryLookup` built in `bind()` on the struct and call `add_endpoint_info`
on that one instance.

**(3) One 512-slot channel carries gossip payloads, mDNS events, *and* live QUIC connections.** *(native.rs:297)*

`mpsc::channel(512)` with `MAX_GOSSIP_MESSAGE_BYTES = 1_200_000` is a ~600 MB worst-case
buffer. Worse, `RepairProtocol::accept` (native.rs:560-571) `.await`s a send on that same
channel, so a peer opening repair connections competes with — and can head-of-line block —
op delivery. And iroh's docs are explicit that *"Once `accept()` returns, the connection is
dropped"* (iroh-1.0.3/src/protocol.rs:258-268); here the `Connection` survives only because
it was moved into the queued event, so an undrained queue holds live QUIC state open with
no timeout and no cap on concurrent inbound sessions. Fix: separate channels (a small one
for connections, byte-budgeted for messages), a concurrency cap, and an accept-side timeout.

**(4) `MAX_GOSSIP_MESSAGE_BYTES` is 293× the iroh-gossip default, and larger ops have no fallback.** *(native.rs:37)*

`iroh_gossip::proto::DEFAULT_MAX_MESSAGE_SIZE = 4096` (iroh-gossip-0.101.0/src/proto.rs:69);
§2.5 of this document proposed 64 KiB. native.rs uses 1,200,000. Plumtree eager-pushes each
message to every eager neighbour, so one large `SetTuning` is multi-megabyte egress per hop.
Separately, `ops::MAX_SIGNED_PAYLOAD_BYTES = 2 * 1024 * 1024` **exceeds** the gossip cap, so
a legal op can be rejected by `broadcast()`; §2.5 says such ops "still converge via RBSR",
but no periodic anti-entropy exists yet, so today the failure is a silent divergence with a
returned `Err` nobody is positioned to act on. Also note every peer must be built with the
same `max_message_size` or large frames are dropped at the receiver.

**(5) `RepairProtocol` sits outside the router's shutdown accounting.** *(native.rs:560-571)*

Because `accept()` returns as soon as the event is queued, `Router::shutdown()` — which
"aborts the futures returned by `accept`" after `ProtocolHandler::shutdown` completes — has
nothing to wait for, and `NativeRoomNetwork::shutdown` will close the endpoint out from
under an in-flight repair session. Whatever drives the session must be tracked explicitly.

**(6) `RoomRepairSnapshot` + a live index can silently "complete" a divergent sync.** *(repair.rs:63-87)*

`bytes_with_closure` swallows a miss:

```rust
let Some((bytes, predecessors)) = self.records.get(&candidate) else { continue; };
```

The snapshot is captured once at session start, but `SyncSession` requires the app to call
`resume(index)` with a **rebuilt** index after every ingest. If that rebuild reads the live
store while the `EntrySource` remains the frozen snapshot, the index advertises hashes the
source cannot serve; the peer's `Fetch` is answered with an incomplete `Entries`,
`outstanding_fetches` still decrements, both sides reach `Done`, and the session reports
success while the peers have not converged. The `Done{root}` cross-check would *notice*
(`SessionOutput::root_mismatch`), but it is advisory and currently unwired. Fix: rebuild
snapshot and index together as one unit, and make a missing hash an explicit `Abort` rather
than a silent skip.

**(7) Unbounded closure vs. a hard frame cap — a large room can never finish a repair.** *(repair.rs:68-86, 97-107)*

`bytes_with_closure` walks the *entire* ancestor DAG of each requested hash. For a
late-joining peer that is the whole room history in one `Entries` frame. With
`fetch_max_hashes: 8` the intent is clearly "few roots, closure does the work" — but
`write_sync_frames` then hits `MAX_REPAIR_FRAME_BYTES` (4 MiB) and returns
`FrameTooLarge`, killing the session instead of chunking. There is no room size at which
this recovers on its own. Fix: chunk `Entries` across frames, or bound the closure walk
and let RBSR make another pass.

**(8) Budget arithmetic is internally inconsistent.** *(repair.rs:97-107)*

`max_entries_ingested: 65_536` is unreachable under `max_rounds: 4096`: each `Fetch` and
each `Entries` consumes a round, and `fetch_max_hashes: 8` caps roots at 8 per `Fetch`, so
the round budget binds first by a wide margin. Either raise `max_rounds`, raise
`fetch_max_hashes`, or drop the entry ceiling to something the session can actually reach.

**(9) Minor: `discovery_sources` never shrinks.** *(native.rs:339-369)* Entries are inserted
on mDNS discovery and never removed on `Expired`/`NeighborDown`, so the map grows with
every peer ever seen and a peer re-found via gossip after mDNS expiry is still reported
`DiscoverySource::Mdns`.

**(10) Footgun, not a bug: `impl From<EndpointId> for AuthorId`.** *(native.rs:573-577)*

This is *sound for the local participant* — `identity.rs` derives the p2panda `SigningKey`
and the `iroh::SecretKey` from one seed, so the public bytes coincide by construction. But
under Plumtree relaying, `NativeNetworkEvent::Message::delivered_from` is the **forwarder**,
not the **author**, and this blanket `From` makes `delivered_from.into()` compile into a
plausible-looking authorship claim. Authorship must only ever come from
`verify_signed_op_for_topic`. C.2 keeps `PeerId` and `AuthorId` deliberately unconvertible
for exactly this reason.

### C.1.3 One design-level note (not a defect)

`bind()` calls `.clear_address_lookup()` and then installs only `MemoryLookup` + room-scoped
mDNS, discarding everything `presets::N0` supplies — `PkarrPublisher::n0_dns()`,
`PkarrResolver::n0_dns()`, and `DnsAddressLookup::n0_dns()`
(iroh-1.0.3/src/endpoint/presets.rs:116-138). The comment says this is deliberate, and it is
a genuine privacy win: the endpoint never publishes its addresses to n0's global pkarr
directory. The undocumented cost is that iroh warns *"If no Address Lookup is set, connecting
to an endpoint without providing its direct addresses or relay URLs will fail"*
(endpoint.rs:579-583) — so a peer whose ticket has gone stale (address *and* relay changed)
has **no recovery path** except a fresh ticket or being on the same LAN. Worth writing down
as an accepted trade-off, with an opt-in pkarr escape hatch as a possible future flag.

Also structural: `NativeRoomNetwork` binds a whole `Endpoint` **per room**, because the room
topic is baked into the mDNS service name at bind time. Two rooms = two QUIC sockets, two
relay homes, and a full rebind (losing every hole-punched path) to switch rooms. The trait
in C.2 is deliberately per-room so this stays an implementation detail, but if multi-room
ever matters, the endpoint must be hoisted above the room.

---

## C.2 The `Transport` trait

Landed in `src/net/mod.rs`. It is **dependency-free by construction** — no iroh, no tokio,
no `wasm-bindgen` — so it compiles under every feature/target combination the repo builds,
including `cargo build --lib --no-default-features --target wasm32-unknown-unknown`.
Backends implement it behind their own cfg gates.

```rust
/// A peer's *transport* identity: the raw 32-byte Ed25519 public key its
/// connection is authenticated under. Deliberately NOT convertible to AuthorId.
pub struct PeerId(pub [u8; 32]);

pub enum TransportMode { Iroh /* default */, Libp2p, Hyperswarm, Loopback }

pub enum TransportEvent<S> {
    PeerUp { peer: PeerId, discovery: DiscoverySource },
    PeerDown { peer: PeerId },
    Message { from: PeerId, bytes: Vec<u8> },   // verbatim SignedOp wire bytes
    SyncRequested { peer: PeerId, stream: S },  // drive as HHHS responder
    Lagged, Closed, Diagnostic(String),
}

/// One bidirectional, ordered, peer-authenticated FRAME channel = one SyncSession.
pub trait SyncStream {
    fn send_frame(&mut self, frame: &[u8]) -> impl Future<Output = Result<(), TransportError>>;
    fn recv_frame(&mut self)               -> impl Future<Output = Result<Option<Vec<u8>>, TransportError>>;
    fn close(self)                         -> impl Future<Output = ()>;
}

pub trait Transport {
    type Stream: SyncStream;
    fn mode(&self) -> TransportMode;
    fn max_broadcast_bytes(&self) -> usize;
    fn broadcast(&self, frame: Vec<u8>)    -> impl Future<Output = Result<(), TransportError>>;
    fn next_event(&mut self)               -> impl Future<Output = Option<TransportEvent<Self::Stream>>>;
    fn open_sync(&self, peer: PeerId)      -> impl Future<Output = Result<Self::Stream, TransportError>>;
    fn peer_path(&self, peer: PeerId)      -> impl Future<Output = PeerPath>;
    fn shutdown(self)                      -> impl Future<Output = Result<(), TransportError>>;
}
```

### C.2.1 Five decisions, and why

**No `Send`/`Sync` bounds anywhere.** Browser backends hold `!Send` JS handles
(`js_sys::Function`, `web_sys::EventSource`), so a `Send` bound would make the browser modes
unimplementable. This follows the precedent already set by the kernel:
`hhhs_core::sync_session::EntrySource` is documented as *"No `Send`/`Sync` bounds — the
session lives entirely inside one task."* The same discipline, one layer down.

**Async methods are `impl Future`, so the trait is not dyn-compatible — and that is fine.**
Runtime mode selection is an `enum AnyTransport { Iroh(..), Hyperswarm(..), … }` with a
delegating `impl Transport`, not `Box<dyn Transport>`. Enum dispatch is what the situation
actually wants: the mode set is closed, known at compile time, and per-target cfg-gated
(the iroh variant does not exist in a wasm build; the hyperswarm variant does not exist in a
native build). `async-trait`'s boxing tax buys nothing here.

**`SyncStream` is framed, not byte-oriented.** A `Read`/`Write` seam would be the obvious
choice for iroh alone, but it is the wrong one at the boundary: a JS-bridged socket crosses
into wasm as discrete `Uint8Array` messages, and every message-oriented carrier
(`pubsub://` SSE events, hypercore extension messages, libp2p streams behind a JS shim)
would have to fake a byte stream that the driver immediately re-framed. Since
`SyncMessage::encode()`/`decode()` already work on whole frames, framing belongs to the
backend: iroh keeps `repair::{write_sync_frames, read_sync_frame}` as-is, and the JS
backends do nothing at all. This also means a backend can enforce its own frame cap
honestly instead of inheriting a 4 MiB constant chosen for QUIC.

**`PeerId` is `[u8; 32]` and does not convert to `AuthorId`.** 32 raw Ed25519 bytes is the
common denominator: iroh's `EndpointId` *is* one; a hyperswarm connection is keyed by a
32-byte Noise static public key; a libp2p `PeerId` for an Ed25519 identity is an
identity-multihash wrapper around exactly these 32 bytes, so it is recoverable both ways.
(Caveat, stated plainly: this pins libp2p mode to Ed25519 identities — which walkie already
mints, so it costs nothing.) The missing `From<PeerId> for AuthorId` is the deliberate fix
for C.1.2(10): a delivering peer is not an author, and the type system should say so.

**`max_broadcast_bytes()` is on the trait.** Carriers disagree wildly — iroh-gossip's
default is 4 KiB, a `pubsub://` POST body is bounded by whatever the handler accepts, an
Ed25519-signed op may be up to 2 MiB. Exposing the limit lets the one driver decide
*"too big to gossip ⇒ leave it to anti-entropy"* uniformly, instead of every backend
inventing a policy. This directly closes C.1.2(4).

### C.2.2 How `RoomStore` wires to it — the missing `src/net/sync.rs`

The trait exists to let exactly one driver serve every mode. Pseudocode, transport-generic:

```rust
pub async fn run<T: Transport>(mut net: T, store: Rc<RefCell<RoomStore>>, topic: String) {
    while let Some(event) = net.next_event().await {
        match event {
            // Ingress — the ONLY path into the store, identical in every mode.
            TransportEvent::Message { bytes, .. } => {
                let Ok(signed) = SignedOp::from_wire_bytes(&bytes) else { continue };
                let Ok(verified) = verify_signed_op_for_topic(&signed, &topic) else { continue };
                store.borrow_mut().ingest_verified(verified);
            }
            // Anti-entropy, initiator half.
            TransportEvent::PeerUp { peer, .. } => {
                if let Ok(stream) = net.open_sync(peer).await { drive(stream, Role::Initiator, &store).await }
            }
            // Anti-entropy, responder half.
            TransportEvent::SyncRequested { stream, .. } => drive(stream, Role::Responder, &store).await,
            TransportEvent::Lagged => { /* gossip dropped frames: schedule a sync */ }
            _ => {}
        }
    }
}
```

and `drive` is pure HHHS — `SyncSession::{initiate, accept}`, `on_message`, `resume`, with
`RoomRepairSnapshot` as the `EntrySource` — touching `SyncStream::{send_frame, recv_frame}`
and nothing else. Egress is the mirror image: `RoomStore::commit` returns the `SignedOp`,
the caller checks `to_wire_bytes().len() <= net.max_broadcast_bytes()` and either
`broadcast`es it or lets the next session carry it.

**The immediate payoff is testability.** `TransportMode::Loopback` — a pair of in-process
channel endpoints implementing `Transport`/`SyncStream` — lets the whole driver, including
the RBSR session and the snapshot/index rebuild that C.1.2(6) says is currently unsafe, run
under `cargo test` with no sockets, no multicast, and no `#[ignore]`. That test does not
exist today at any price; behind the trait it is cheap.

### C.2.3 Where a mode is chosen

Mode is **runtime**, not compile-time, but the *set* of available modes is compile-time:

| Build | Modes linked in |
|---|---|
| `--features native-net` (Tauri desktop, plugin) | `Iroh`, `Loopback` |
| wasm + `web-ui`, ordinary browser | `Iroh` (relay-only, per Addendum B), `Loopback` |
| wasm + `web-ui` + `p2p-browser` | `Hyperswarm`, `Libp2p`, `Loopback` — and `Iroh` if the page can still reach a relay |

Selection order at startup: explicit config (Tauri setting / `#mode=` URL fragment) →
runtime capability probe (does the injected JS API exist? does a relay answer?) →
`TransportMode::default()` = `Iroh`. The probe is what makes a single wasm bundle work
both in Chrome and inside Agregore. `client::Capabilities` already carries
`native_iroh` / `mdns` / `relay` booleans for exactly this kind of report and should gain a
`transport_mode` field rather than growing a fourth boolean.

---

---

## C.3 Per-mode design

### C.3.0 The headline research result

**Neither Agregore nor Peersky exposes a js-libp2p or hyperswarm handle to a web page.**
VERIFIED, by exhaustion, in both:

- Agregore's preload scripts are `src/llm-preload.js`, `src/localsites-preload.js`,
  `src/settings-preload.js` — an LLM bridge and browser-settings shims. There is no
  `window.agregore` P2P object. (`agregore` *is* a registered URL scheme,
  `src/protocols/index.js:75`, which is the browser's own UI — not a JS global.)
- Peersky's single preload (`src/pages/unified-preload.js`) is the **only** file in the repo
  calling `contextBridge.exposeInMainWorld`. It exposes `window.peersky` (environment/css/
  print), `window.electronAPI` (key-allowlisted settings IPC), `window.llm`. Tab content runs
  `contextIsolation=yes` (`src/pages/tab-bar.js:857`). The `hyper-sdk` instance is
  module-scoped in the main process (`src/protocols/hyper-handler.js`) and never bridged.

And it could not be otherwise for hyperswarm: `holepunchto/hyperdht` declares
`"browser": "browser.js"`, and that file is a class whose **constructor throws**
`'hyperdht is not supported in browsers'`. `hyperswarm/index.js` constructs `new DHT(...)`
unconditionally, so it cannot instantiate in a renderer at all. Underneath, `udx-native` is
a UDP native addon whose prebuild workflow targets linux/darwin/win32/android/ios and
**has no wasm target**. The only browser route is `@hyperswarm/dht-relay@0.4.3`, whose
README says verbatim *"🧪 This project is still experimental. Do not use it in production"*
and which needs a trusted always-on relay host — i.e. client-server with extra steps.

**So there is no "call js-libp2p from wasm" design to write.** What exists instead is
better for our purposes: both browsers register their P2P stacks as **privileged URL
schemes**, so the access path from Rust→wasm is the ordinary web platform —
`fetch()` and streaming responses, already in `web-sys`. VERIFIED, and identical in both
browsers (Agregore `src/protocols/index.js:17-25`; Peersky `src/main.js:34-42`):

```js
const P2P_PRIVILEGES = {           // byte-identical in both projects
  standard: true, secure: true, allowServiceWorkers: true,
  supportFetchAPI: true, bypassCSP: false, corsEnabled: true, stream: true
}
```

Consequences worth naming, because they decide feasibility:

- `secure: true` ⇒ a page served from `hyper://…` is a **secure context**: `WebAssembly`,
  `crypto.subtle`, and service workers all work. Rust→wasm runs normally.
- `supportFetchAPI` + `corsEnabled` ⇒ a page on one P2P scheme can `fetch()` another.
- `stream: true` ⇒ streaming response bodies, which is what makes SSE viable.
- **`bypassCSP: false`** ⇒ the page's own CSP still applies. Walkie's `index.html` must list
  `pubsub:` and `hyper:` in `connect-src` or every request is blocked. Concrete and easy to
  get wrong.

Registered schemes, VERIFIED from source:

| | Agregore 2.24.0 (`src/protocols/index.js:63-78`) | Peersky 1.0.0-beta.27 (`src/main.js:145-158`) |
|---|---|---|
| schemes | `https+raw hyper gemini ipfs ipns ipld pubsub bittorrent bt ssb web3 agregore browser magnet did` | `peersky browser ipfs ipns pubsub hyper hs web3 file bittorrent bt magnet` |
| IPFS impl | **go-ipfs/Kubo 0.22 daemon** via `ipfs-http-client@^60` + `js-ipfs-fetch@^5.3.0` | **Helia 6 / js-libp2p 3** in-process (`src/protocols/helia/helia.js`) |
| hyper impl | `hyper-sdk@^6.2.1` + `hypercore-fetch@^10.2.0` | `hyper-sdk@^6.2.2` + `hypercore-fetch@^10.1.0` |

### C.3.1 Mode `Iroh` — the default (recap)

Unchanged from §2 and Addendum B: iroh 1.0 QUIC + relay + hole punching, `iroh-gossip`
Plumtree for `SignedOp` broadcast, a `walkie/rbsr/1` ALPN bi-stream per `SyncSession`,
room-scoped mDNS on native. `NativeRoomNetwork` becomes `impl Transport` almost mechanically:

| `Transport` | `NativeRoomNetwork` today |
|---|---|
| `broadcast` | `broadcast(Vec<u8>)` — already exact |
| `next_event` | `next_event()` — already exact |
| `open_sync` | `begin_repair(peer)` + `connection.open_bi()` |
| `SyncRequested` | `NativeNetworkEvent::IncomingRepair` + `connection.accept_bi()` |
| `Stream` | `(SendStream, RecvStream)` + `repair::{write_sync_frames, read_sync_frame}` |
| `peer_path` / `shutdown` | already exact |
| `max_broadcast_bytes` | `MAX_GOSSIP_MESSAGE_BYTES` — **and see C.1.2(4)** |

This is the whole argument for the trait shape: it is what native.rs already is, minus the
leaked `iroh::endpoint::Connection`. Land the C.1.2 fixes first — mode Iroh is the only mode
that will carry real traffic for the foreseeable future.

### C.3.2 Mode `Libp2p` — Agregore's `pubsub://` (libp2p gossipsub)

**Agregore only.** Peersky registers the `pubsub` scheme (`src/main.js:150`) and routes it to
its IPFS handler (`:442`), but `src/protocols/ipfs-handler.js` contains **no** pubsub/publish/
subscribe/topic handling — a `pubsub://` URL falls through to CID parsing and returns
`400 Invalid CID in URL`. Its libp2p `services` list (`src/protocols/helia/helia.js:84-106`)
has no gossipsub, and there is no `@chainsafe/libp2p-gossipsub` in `package.json`. **The
scheme is vestigial** — almost certainly copied from Agregore's list without the
implementation. (VERIFIED twice, independently.)

In Agregore it is real, implemented by `js-ipfs-fetch@^5.3.0` against the Kubo daemon.
Routes, VERIFIED verbatim from `RangerMauve/js-ipfs-fetch` README:

```js
// SUBSCRIBE
new EventSource('pubsub://TOPIC/?format=base64')
//   or: fetch('pubsub://TOPIC/', { headers: { Accept: 'text/event-stream' } })
//   `message` events; e.data is JSON { from, topics, data }
//   ?format = base64 (default) | json | utf8

// PUBLISH
await fetch('pubsub://TOPIC/', { method: 'POST', body })   // body sent as a binary buffer
```

(The README's heading writes `{method:'POST', data}`, but its prose says *"The `body` will be
sent as a binary buffer"* — `body` is the real field.)

Mapping onto the trait:

- **`broadcast`** — `POST pubsub://walkie-<room-topic-hex>/` with `signed.to_wire_bytes()` as
  a **binary** body. No encoding tax. Clean.
- **`Message`** — subscribe with `?format=base64`, `atob` → `SignedOp::from_wire_bytes`.
- **`PeerId`** — `from` is the **Kubo daemon's libp2p PeerId string**, which is (a) not
  walkie's author key and (b) not necessarily 32 bytes (an RSA identity is a sha256
  multihash). Map it as `PeerId(blake3::derive_key("walkie transport peer id v1", from))`.
  This is sound precisely because transport peer identity is advisory — authorship comes
  from `verify_signed_op_for_topic` and nothing else. It is also the clearest vindication of
  C.2's refusal to convert `PeerId` into `AuthorId`.
- **`PeerUp`/`PeerDown`** — **no membership events exist.** Gossipsub tells you nothing about
  join/leave through this API. Synthesize: first message from an unseen `from` ⇒ `PeerUp`;
  a silence timeout ⇒ `PeerDown`. This means anti-entropy cannot be triggered by "a peer
  appeared" and must fall back to the periodic timer.
- **`open_sync`** — there is no per-peer duplex primitive. The workable design is a
  **per-pair rendezvous topic**: both sides derive
  `pubsub://walkie-sync-<blake3(min(a,b) ++ max(a,b) ++ nonce)>/`, subscribe, and POST
  `SyncMessage` frames; each POST is exactly one frame, so `SyncStream` maps 1:1 with no
  framing layer at all. Gossipsub will route it and only the two subscribers join that mesh.

**The sharp edge: gossipsub guarantees neither ordering nor delivery.** RBSR is a round-based
request/response protocol, so a dropped or reordered frame stalls a session that then never
completes. Two honest options: (a) build a small seq+ack/retransmit shim over the pair topic —
real work, and reinventing a transport; or (b) exploit the fact that `SyncSession` is
idempotent and cheap to restart — put a hard timeout on the session, abandon it, and let the
periodic timer try again. **(b) is the right call** for a room-sized DAG: convergence becomes
probabilistic-but-eventual instead of guaranteed-per-session, which is exactly what
anti-entropy is for. Say so in the code rather than pretending the channel is reliable.

### C.3.3 Mode `Hyperswarm` — `hyper://<key>/$/extensions/` (both browsers)

Strictly, this is **hypercore extension messages riding the replication stream**, which in
turn rides a hyperswarm-brokered NoiseSecretStream. Naming the mode `Hyperswarm` is fair —
hyperswarm is what finds the peers — but the API is hypercore's.

**It is enabled in both browsers, and in both cases by accident.**
`hypercore-fetch/index.js:93-96` defaults `extensionMessages = writable`, and both browsers
construct the fetch with `writable: true` — Agregore at `src/protocols/hyper-protocol.js:20`,
Peersky at `src/protocols/hyper-handler.js:102`. Neither passes `extensionMessages`
explicitly, and neither documents the routes. **They are live today and could regress
silently** if either project ever flips that default. First thing to pin in a smoke test.

Route table, VERIFIED from `RangerMauve/hypercore-fetch` README + `index.js:111-115`:

| Verb | URL | Purpose |
|---|---|---|
| `GET` | `hyper://NAME/$/extensions/` | list registered extension names |
| `GET` | `hyper://NAME/$/extensions/` + `Accept: text/event-stream` | **subscribe** (SSE) |
| `GET` | `hyper://NAME/$/extensions/EXT` | list peers — **and registers `EXT` if new** |
| `POST` | `hyper://NAME/$/extensions/EXT` | **broadcast** to all replicating peers |
| `POST` | `hyper://NAME/$/extensions/EXT/REMOTE_PUBLIC_KEY` | **send to one peer** |

Mapping onto the trait — and it fits far better than pubsub does:

- **`broadcast`** — `POST …/$/extensions/walkie-ops-1`.
- **`open_sync`** — `POST …/$/extensions/walkie-sync-1/<remotePublicKey>` is a **genuine
  per-peer send primitive**. Paired with the SSE stream (whose `id:` field is the sender's
  hex public key, `index.js:228`), that is a real duplex channel per peer.
- **`PeerUp`/`PeerDown`** — the SSE stream emits **`peer-open` and `peer-remove`** special
  events. Direct mapping; no synthesis needed.
- **`PeerId`** — `remotePublicKey` is a **32-byte hypercore Noise public key**. Hex-decode
  straight into `PeerId([u8; 32])`. Exact fit, no hashing fudge.
- **Reliability — the decisive advantage.** Extension messages ride the hypercore
  replication stream, i.e. a NoiseSecretStream: ordered and reliable per peer. So unlike
  pubsub, `SyncStream` here has the semantics `SyncSession` actually assumes, and the
  round-based RBSR descent works as designed.

Costs and caveats, stated plainly:

- **utf8-only bodies.** The README says it twice, verbatim: *"only utf8 encoded text is
  currently supported due to limitations of the event-stream encoding."* Every `SignedOp`
  and every `SyncMessage` must be base64'd — **+33% on the wire**, on top of a
  hypercore-fetch layer that is itself HTTP-shaped. For a 4 MiB repair frame that is 5.3 MiB.
- **Registration is a side effect of a GET.** An extension is only created — and only becomes
  visible in the SSE stream — after a `GET …/$/extensions/EXT`. Startup must GET both
  extension names before subscribing, or messages silently never arrive.
- **Per-peer POST 404s** with "Peer Not Found" if that peer is not currently replicating.
  That is a usable `PeerDown` signal, but it must be handled rather than logged.
- **SSE is not native over `hyper://`.** The docs route browsers through
  `@rangermauve/fetch-to-eventsource` rather than claiming `new EventSource('hyper://…')`
  works. From Rust, the robust path is `fetch` with `Accept: text/event-stream`, then read
  `Response::body()` as a `ReadableStream` and parse SSE frames by hand — perhaps 60 lines,
  and it avoids depending on an undocumented behaviour.
- **Rendezvous.** Extension messages only reach peers already replicating the same core, and
  hyper-sdk joins that core's discovery key on hyperswarm — so **the hyper key *is* the room
  rendezvous**, and the invite ticket should carry it. *SPECULATIVE, and attractive if true:*
  because walkie already treats the room name as the capability, a core whose keypair is
  derived from `blake3(room_name)` would give rendezvous with **no ticket at all**. Verify
  that hyper-sdk permits constructing a core from an arbitrary caller-supplied keypair before
  designing around this.
- **No LAN story.** See C.3.4 — this mode requires internet + DHT bootstrap, full stop.

Not to be copied: Peersky's own peerchat writes newline-delimited JSON **directly onto the
socket that corestore is already multiplexing replication over** (`peerchat/p2p.js:625-637`),
surviving only because both readers skip garbage. If a native-runtime (Bare/Pear) walkie peer
is ever built, the sanctioned equivalent is `Protomux.from(socket).createChannel(…)` — a real
named protocol alongside `hypercore/alpha` — or `hyper-sdk`'s `doReplicate: false`.
`hypercore.registerExtension` still works in v10 and v11, but hypercore's README now calls it
*"a legacy implementation … no longer recommended"* in favour of protomux.

### C.3.4 mDNS — a discovery axis, not a transport

mDNS answers "who is on this LAN", not "how do bytes move", so it composes with a carrier
rather than replacing one. That is why `TransportMode` has no `Mdns` variant and discovery is
a separate option.

- **Native (`Iroh`): already built and already good.** `iroh-mdns-address-lookup` with a
  room-scoped service name, per C.1.1. One caveat to check: the advertised name is
  `<endpoint-id>._walkie-<20-hex>-v1._udp.local`, and that 30-character service label exceeds
  the **15-character limit RFC 6763 §7 places on a service name**. `swarm-discovery` does not
  validate it (only TXT attributes are checked), so walkie peers will find each other; whether
  Avahi and Bonjour tolerate registering it is **unverified and worth an actual LAN test**.
- **Inside the P2P browsers: not reachable from a page, in either.** Peersky's libp2p does
  configure `peerDiscovery: [mdns(), bootstrap(…)]` (`src/protocols/helia/helia.js:80-83`)
  and Agregore's Kubo daemon does its own — but that is *below* the protocol handler, serving
  content discovery. A page can neither observe nor steer it. You get whatever LAN benefit it
  incidentally provides on `ipfs://`, and no control.
- **The hyper stack has no mDNS at all.** VERIFIED by exhaustion: no `mdns|multicast` in
  hyperswarm, hyperdht, or dht-rpc; `udx-native` *implements* `addMembership`/`dropMembership`
  (`lib/socket.js:253`) but **no consumer in the stack ever calls them**; and no
  `dns-discovery`/`bare-mdns` exists across ~200 repos in the holepunch org. What hyperdht has
  is a **DHT-brokered same-NAT shortcut** (`lib/connect.js:84,248-251` — peers advertise
  private LAN IPs *through the DHT*, then shortcut), which is an optimization, not offline
  discovery.

**Therefore: mode `Hyperswarm` cannot work offline.** For an app whose best demo is several
people in one room with no internet, that is not a footnote — it is the reason mode `Iroh`
stays the default everywhere and the P2P-browser modes are a bonus, not a migration.

---

## C.4 Feasibility and effort

Effort assumes the C.1.2 fixes and `src/net/sync.rs` already exist, since every mode needs them.

| Mode | Works? | Access path | Reliable stream for RBSR | Peer up/down | LAN | Effort |
|---|---|---|---|---|---|---|
| **`Iroh`** (default) | **Yes**, shipping | native QUIC / wasm+relay | Yes — QUIC bi-stream | Yes — gossip neighbours | **Yes** (mDNS) | fixes only: **~1 wk** |
| **`Loopback`** (tests) | Yes, trivial | in-process channels | Yes | Synthetic | n/a | **~1 day** |
| **`Hyperswarm`** = `hyper://…/$/extensions/` | **Yes**, Agregore **and** Peersky | `fetch` + SSE | **Yes** — Noise stream | **Yes** — `peer-open`/`peer-remove` | **No** | **2–3 wks** |
| **`Libp2p`** = `pubsub://` | **Agregore only** — dead in Peersky | `fetch` + `EventSource` | **No** — gossipsub is unordered/lossy | **No** — must synthesize | indirect only | **3–4 wks** |
| direct js-libp2p / hyperswarm handle | **No — does not exist** | — | — | — | — | **n/a** |

Reading the table honestly: **`Hyperswarm` (hypercore extensions) is the better of the two
browser modes on every axis that matters** — a reliable ordered per-peer channel, real
membership events, a `PeerId` that is already 32 bytes, and it works in *both* browsers. Its
costs (base64 tax, no LAN) are known and bounded. `pubsub://` has the nicer broadcast story
(binary bodies, no encoding tax) but no reliable per-peer channel, no membership events, and
only one of the two browsers implements it. **If only one browser mode is ever built, build
`Hyperswarm`.** A defensible hybrid is to use `pubsub://` for live gossip and
`$/extensions/` for anti-entropy in Agregore, which plays to each one's strength — but that
doubles the surface for a deployment target with no users yet.

### C.4.1 What must be verified before writing any browser-mode code

Roughly in the order a smoke test should establish them. Everything above is source-verified;
these are the things source-reading **cannot** settle.

1. **That a Rust→wasm bundle loads and runs at all** from `hyper://<key>/` in each browser —
   correct `application/wasm` MIME for `instantiateStreaming`, and no CSP surprise. This is
   the go/no-go and costs an afternoon.
2. **That `$/extensions/` is actually reachable from page JS** in shipped 2.24.0 and
   beta.27 builds. It is enabled only by hypercore-fetch's `extensionMessages = writable`
   default, which neither browser sets deliberately or documents. Assert it in a smoke test
   so a silent upstream regression is caught.
3. **The `peer-open` / `peer-remove` payload shape.** The README names the two events; the
   exact `data:` body was not confirmed. `PeerUp`/`PeerDown` depend on it.
4. **Whether two independent Agregore instances actually exchange `pubsub://` messages**
   over the public DHT within a usable time, and what the practical body-size ceiling is.
   Kubo gossipsub in the field is not the same thing as a README.
5. **Whether hyper-sdk accepts a caller-supplied keypair**, which decides whether the room
   name alone can be the rendezvous (C.3.3) or an invite ticket must carry the hyper key.
6. **Real-world base64 frame cost** on a room-sized DAG — whether a first sync into an
   established room is seconds or minutes.
7. **The mDNS 30-character service label against Avahi and Bonjour** (C.3.4) — this one
   affects the *default* mode and should be checked regardless of any browser work.
8. **Cross-scheme CORS in practice.** Both browsers set `corsEnabled: true`, but Electron's
   documentation for that flag is literally an empty description, and Peersky's chat handler
   sets no `Access-Control-Allow-Origin` at all. Whether a page on `hyper://` may `fetch()`
   `pubsub://` is unconfirmed and gates the hybrid design.

### C.4.2 Recommendation

1. **Fix `native.rs` / `repair.rs` (C.1.2) and write `src/net/sync.rs` behind the trait.**
   This is the only work that is unconditionally needed, it removes the project's sharpest
   real risk (a repair path that has never run), and it is prerequisite to every mode.
2. **Implement `TransportMode::Loopback` and test the sync driver with it.** Cheap, and it
   converts the currently-`#[ignore]`d convergence story into something CI proves.
3. **Spend one afternoon on verification item (1)** — get a wasm build running from
   `hyper://` in Agregore and Peersky. Everything downstream is contingent on it, and it is
   the cheapest possible way to find out.
4. **Only then, if that succeeds, build `TransportMode::Hyperswarm`.** Both browsers, reliable
   streams, real membership events. Treat `Libp2p`/`pubsub://` as optional and Agregore-only.

The load-bearing point: none of steps 2–4 change a wire format, a `RoomStore` API, an op
schema, or an entry hash. That is what the trait bought, and it is why this can wait until
the default mode is actually solid.

---

## C.5 Nix dev setup for Agregore and Peersky

**Neither is in nixpkgs.** VERIFIED: `gh search code --repo NixOS/nixpkgs agregore` and
`… peersky` both return zero hits (2026-08). So this is a local flake output, not an
override.

Both ship an `x86_64` Linux AppImage per release, which makes `appimageTools.wrapType2` the
right tool — it unpacks the AppImage into an FHS-ish wrapper and handles the Electron
shared-library set, which is the whole reason not to try `buildNpmPackage` (both are
Electron apps with native modules: `udx-native`/`sodium-native` for the hyper stack, plus
prebuilt Electron itself — a source build means patching `electron-builder`, and `npm`
lockfile hashes for a ~200 MB dependency tree, for zero benefit when we only want to *run*
the browser).

VERIFIED release artifacts and digests (`gh release view`, 2026-08):

| App | Version | Date | Asset | sha256 |
|---|---|---|---|---|
| Agregore | 2.24.0 | 2026-04-23 | `agregore-browser-2.24.0-linux-x86_64.AppImage` | `d3a28c5e…4c85c0` |
| Peersky | 1.0.0-beta.27 | 2026-07-06 | `peersky-browser-1.0.0-beta.27-linux-x86_64.AppImage` | `6ede8fcc…7a6b82` |

Add to the existing `flake.nix` (which already provides the Rust toolchain with the
`wasm32-unknown-unknown` target, trunk, and pnpm) as extra packages:

```nix
# flake.nix — inside the eachDefaultSystem `let`, alongside `rust = …`
mkP2PBrowser = { pname, version, url, sha256 }: let
  fhs = pkgs.appimageTools.wrapType2 {
    inherit pname version;
    src = pkgs.fetchurl { inherit url sha256; };
  };
in
  # appimage-exec.sh ends in `exec "$APPDIR/AppRun" "$@"`, so flags pass straight
  # through. See the --no-sandbox note below for why this indirection exists.
  pkgs.writeShellScriptBin pname ''
    exec ${fhs}/bin/${pname} "$@"
  '';

agregore = mkP2PBrowser {
  pname = "agregore-browser"; version = "2.24.0";
  url = "https://github.com/AgregoreWeb/agregore-browser/releases/download/v2.24.0/agregore-browser-2.24.0-linux-x86_64.AppImage";
  sha256 = "d3a28c5ed6654117840e710289309b8db0672b589e294adb01b636f7234c85c0";
};

peersky = mkP2PBrowser {
  pname = "peersky-browser"; version = "1.0.0-beta.27";
  url = "https://github.com/p2plabsxyz/peersky-browser/releases/download/v1.0.0-beta.27/peersky-browser-1.0.0-beta.27-linux-x86_64.AppImage";
  sha256 = "6ede8fcccee1639b0fddcd6982fe83e8aaeac5d4d391409b8dc1abe1077a6b82";
};
```

then `packages = with pkgs; [ rust trunk … ] ++ [ agregore peersky ];`, plus
`packages.${system} = { inherit agregore peersky; }` so `nix run .#agregore-browser` works
without entering the shell.

Caveats, honestly:

- **`wrapType2` builds on `buildFHSEnv`, not `mkDerivation`** — VERIFIED at
  `nixpkgs/pkgs/build-support/appimage/default.nix`. So `extraPkgs` (a `pkgs: [ … ]`
  *function*) and `extraInstallCommands` are both supported, but `wrapProgram` is not
  available unless you wire in `makeWrapper` yourself; the `writeShellScriptBin` indirection
  above sidesteps that. Note also that `libsecret` (Electron `safeStorage`, which Peersky
  uses for chat seeds) is **already** in `defaultFhsEnvArgs.targetPkgs`, so no `extraPkgs`
  is needed for these two.
- **`--no-sandbox` may be required.** Chromium's `chrome-sandbox` helper must be root-owned
  and SUID, which the Nix store cannot provide; whether Electron falls back cleanly depends
  on unprivileged user namespaces being enabled on the host. Try it without first, and add
  `--add-flags "--no-sandbox"` to the wrapper only if it fails — it is a real (if
  development-only) weakening, and it should never ship.
- These digests pin one release each and came from `gh release view`. `latest-linux.yml` in
  the same release carries the publisher's own hashes for cross-checking.
- aarch64: Agregore publishes `linux-arm64.AppImage`; Peersky (as of beta.27) publishes
  **no** aarch64 Linux asset — so on ARM the Peersky output must be omitted rather than
  silently broken. macOS uses the `.dmg`/`.zip` assets and a different derivation shape; not
  covered here.
- **Peersky pulls its P2P apps as git submodules at `postinstall` with
  `--remote --merge`**, i.e. tracking submodule branch tips. That does not affect the
  AppImage (already built), but it means Peersky builds are not reproducible from a parent
  commit — one more reason to consume the release binary rather than build from source.
- **The dev loop.** `trunk build` already emits a plain static bundle. Serving it to these
  browsers over `http://localhost` is fine for UI work but does **not** exercise the P2P
  modes — see C.4 on why a page's origin decides which protocol handlers it may reach.
  The real loop is: `trunk build` → publish `dist/` into a hyperdrive → open the resulting
  `hyper://` URL in both browsers. Budget a small `xtask` for the publish step.

---

## C.6 Implementation status (landed)

C.1's severe defects are fixed and the driver C.0 said was missing now exists.

### C.6.1 Walkie-side changes

| Area | Change |
|---|---|
| **`src/net/sync.rs`** (new, ungated) | The real `SyncSession` driver: `drive_initiator` / `drive_responder`, `RoomSyncSource`, `SyncLimits`. Transport-neutral, so it compiles on wasm and serves every mode. |
| **`src/net/loopback.rs`** (new, ungated) | In-process `Transport` + `SyncStream` pair over `async-channel`. |
| **`repair.rs` C.1.2(6)** | `RoomRepairSnapshot` + free-standing `build_repair_index` are **gone**, replaced by `RoomSyncSource`, which owns the records and derives the index *from the same map that backs `have()`*. Drift is now unrepresentable rather than merely avoided. The driver rebuilds the pair as a unit after every ingest, before `resume`. |
| **`repair.rs` C.1.2(7)** | `bytes_with_closure` is byte-budgeted and **always includes the requested hash** (see C.6.2). |
| **`repair.rs` C.1.2(8)** | `repair_budget()`'s contradictory numbers replaced by `SyncLimits::default()`: `fetch_max_hashes` 8 → 256, so `max_entries_ingested` is reachable in ~512 rounds against a 4096 ceiling instead of needing >16k. |
| **`repair.rs` framing** | `MAX_REPAIR_FRAME_BYTES` is now *defined as* `MAX_SYNC_FRAME_BYTES` (1 MiB, was 4 MiB), so the wire cap and the session cap cannot drift; a test asserts it. Added `IrohSyncStream` implementing `SyncStream` over a QUIC bi-stream. |
| **`native.rs` C.1.2(1)** | mDNS hot loop fixed: the exhausted `ReceiverStream` is latched with a `select!` guard (`if mdns_open`) and the diagnostic uses `try_send`, so a closed stream can neither spin nor block the event task. |
| **`native.rs` C.1.2(2)** | One `MemoryLookup` is held on the struct and fed by `join_ticket`, instead of registering a fresh service per call into a `Vec` that never dedups or removes. |
| **`native.rs` C.1.2(3)** | Repair connections moved to their own `mpsc` queue (depth 16) with `try_send` and an explicit `connection.close()` on overflow; gossip/mDNS keep a separate depth-64 queue. A peer opening repair sessions can no longer head-of-line block op delivery, and an unbounded backlog of live QUIC state is refused rather than queued. |
| **`native.rs` C.1.2(9)** | `discovery_sources` entries are removed on `Expired` and `NeighborDown`. |
| **`native.rs` C.1.2(10)** | `From<EndpointId> for AuthorId` **deleted**, with a comment recording why it must not come back. |

Tests: **96** lib tests (was 86), **105** with `native-net` (was 94). Builds green on
`--no-default-features`, `--target wasm32-unknown-unknown`, and `--features native-net`.

### C.6.2 The liveness bug the loopback test caught

The e2e test earned its keep immediately. A byte-budgeted closure that emits *ancestors
first and truncates* — the obvious reading of C.1.2(7) — **does not terminate**. Under a
budget too small for the full closure, the responder repeatedly received the same deepest
prefix, because the source picks entries by depth while the peer asks by hash, and the
source has no idea what the peer already holds. `Fetch(12) → Entries(12) → Fetch(12) → …`
forever, with the requested hashes never delivered.

The fix is a one-line invariant with a real proof obligation behind it: **the requested
hash is always included, budget notwithstanding.** The peer asked for it precisely because
it is absent from its lifted set, so delivering it always shrinks the peer's missing set —
which no ancestor guarantees. Ancestors still go first while they fit, so the common
(unbudgeted) case still lifts everything immediately; truncated entries park and drain via
`RoomStore`'s strict deferral. `pump` also carries an iteration cap so any future stall is
a loud error rather than a hung task.

### C.6.3 KERNEL FLAGS — `hhhs_core` changes for upstream

Not made here (pinned git dep). Ordered by value.

1. **`bytes_with_closure` must document the liveness contract.** *(doc/contract, no code)*
   The trait doc says "Bytes for `hash` PLUS its causal closure" and never states that an
   implementation which truncates **MUST** still include `hash`. C.6.2 is what that omission
   costs. Make it a normative MUST — cheapest possible fix, highest value.

2. **`EntrySource`/`Index` have no consistency guard, and the failure is silent.**
   `on_message(msg, source)` and `resume(index)` accept an arbitrary pair with nothing
   checking they describe the same state. On drift: the index advertises hashes the source
   cannot serve, `answer_fetch` silently omits them (*"Unknown hashes are silently
   omitted"*, sync_session.rs:437-440), `outstanding_fetches` still decrements, both halves
   reach `Done`, and **the session reports success while the peers have diverged**.
   Suggested: bundle the two into one app-supplied view (`resume(view)` / `on_message(msg,
   view)`) so drift is unrepresentable, or at minimum a `debug_assert!` that every indexed
   hash satisfies `source.have(..)`. Walkie works around this with `RoomSyncSource`, but
   every consumer currently has to rediscover the hazard.

3. **`SessionBudget::max_frame_bytes` is declared, defaulted, and never read.** VERIFIED:
   the only occurrences in the whole crate are the field declaration (sync_session.rs:136)
   and its default (`:146`). It is documented "advisory (framing itself is the app's job)",
   so it is effectively a dead field that reads as a guarantee.

4. **The app cannot chunk a large `Entries`, so the byte budget has to live in the app.**
   `answer_fetch` emits exactly one `Entries` per `Fetch` (`:455`) and the receiver does
   `outstanding_fetches -= 1` on every `Entries` (`:349`) behind an
   `Entries without an outstanding Fetch` guard (`:344`) — so splitting one answer across
   frames is a protocol error. That forces the truncation into `EntrySource`, where the app
   is guessing at a budget the kernel owns. Two options:
   - *(a) wire change, append-only:* `Entries { pairs, more: bool }`, decrementing
     `outstanding_fetches` only on the final frame, letting the kernel enforce
     `max_frame_bytes` itself; or
   - *(b) trait change, no wire change:* pass the remaining byte budget into
     `bytes_with_closure` so the app truncates with the kernel's number instead of its own.
   (b) is the smaller change and would let flag 1's contract be enforced rather than merely
   documented.

5. **Consider surfacing unanswered fetches.** A `SessionOutput` counter for hashes a `Fetch`
   could not answer would let a driver log or abort instead of silently completing — a
   cheap independent check on flag 2. Related: `root_mismatch` is documented as log-only,
   so an app that silently diverges still looks healthy.


## C.7 Sources

Read at source unless marked otherwise; all accessed 2026-08.

**Agregore** (v2.24.0, released 2026-04-23) ·
`AgregoreWeb/agregore-browser` `src/protocols/index.js` (scheme + privilege registration,
`:17-25`, `:63-78`) · `src/protocols/hyper-protocol.js:20` (`hyperFetch({sdk, writable:true})`) ·
`src/protocols/` file listing (browser, bt, did, gemini, hyper, ipfs, magnet, raw-http, ssb,
web3) · `package.json` (`hyper-sdk@^6.2.1`, `hypercore-fetch@^10.2.0`, `js-ipfs-fetch@^5.3.0`,
`go-ipfs@^0.22.0`, `ipfs-http-client@^60`, `bt-fetch`, `gemini-fetch`, `ssb-fetch`,
`make-fetch`) · preloads `src/llm-preload.js`, `src/localsites-preload.js`,
`src/settings-preload.js` · https://agregore.mauve.moe/docs/ · `AgregoreWeb/agregore-chat-example`

**Peersky** (v1.0.0-beta.27, released 2026-07-06) ·
`p2plabsxyz/peersky-browser` `src/main.js:34-42` (privileges), `:145-158` (schemes),
`:416-459` (`setupProtocols`, incl. `:442` `handle('pubsub', ipfsProtocolHandler)`) ·
`src/protocols/hyper-handler.js:102` (`makeHyperFetch({sdk, writable:true})`) ·
`src/protocols/ipfs-handler.js` (**no** pubsub/publish/subscribe/topic handling) ·
`src/protocols/helia/helia.js:53-112` (libp2p services — no gossipsub; `:80-83`
`peerDiscovery: [mdns(), bootstrap(…)]`) · `src/pages/unified-preload.js` (sole
`contextBridge.exposeInMainWorld`) · `src/pages/tab-bar.js:857` (`contextIsolation=yes`) ·
`src/protocols/hs-handler.js` (Holesail + Yjs) · `p2plabsxyz/peerchat` `p2p.js:591-637`
(NDJSON on the replication socket) · `package.json` (`helia@^6.0.20`, `libp2p@^3.1.3`,
`hyper-sdk@^6.2.2`, `hypercore-fetch@^10.1.0`, `hyperswarm@^4.14.0`, `holesail@^2.4.1`)

**Hyper ecosystem** ·
`RangerMauve/hypercore-fetch@10.2.0` `index.js:93-96` (`extensionMessages = writable`),
`:111-115` (route registration), `:171-176` (utf-8 registration), `:228` (SSE `id:` = peer
hex key), `:335-343` (per-peer 404), `:1052-1062` (`formatPeers`) + README `$/extensions/`
section · `RangerMauve/js-ipfs-fetch` README (`pubsub://` subscribe/publish routes) ·
`RangerMauve/hyper-sdk@6.2.2` `index.js:116-122` (`doReplicate`), `:125-135` (getters),
`:460` (`join`), `:553` (`create`) · `holepunchto/hyperswarm@4.17.0` README + `index.js`,
`lib/peer-info.js`, `lib/peer-discovery.js` · `holepunchto/hyperdht` **`browser.js`**
(constructor throws), `lib/connect.js:84,248-251` (LAN shortcut) · `holepunchto/udx-native`
`lib/socket.js:253` (unused multicast), `.github/workflows/prebuild.yml` (no wasm target) ·
`holepunchto/hypercore` README:410 (`registerExtension` "legacy … no longer recommended"),
`index.js:1249` (v11) / `:1037` (v10), `lib/replicator.js:667,673-680,3248` ·
`holepunchto/protomux` · `@hyperswarm/dht-relay@0.4.3` README ("still experimental")

**iroh / local crates** ·
`iroh-1.0.3` `src/protocol.rs:228-290` (`ProtocolHandler::accept` lifetime),
`src/address_lookup.rs:483-497` (`add` pushes, never dedups/removes),
`src/endpoint.rs:579-588` (`clear_address_lookup`), `src/endpoint/presets.rs:113-138` (N0) ·
`iroh-gossip-0.101.0` `src/proto.rs:69` (`DEFAULT_MAX_MESSAGE_SIZE = 4096`) ·
`iroh-mdns-address-lookup-0.4.0` `src/lib.rs:189-194` (name shape), `:462-469`
(`subscribe` → `ReceiverStream`) · `swarm-discovery-0.6.3` (validates TXT only) ·
`hhhs-core@7d0dd3f` `src/sync_session.rs` · RFC 6763 §7 (15-char service-name limit)

**Nix** · `nixpkgs/pkgs/build-support/appimage/default.nix` (`wrapType2` → `buildFHSEnv`;
`defaultFhsEnvArgs.targetPkgs` already includes `libsecret`) ·
`pkgs/build-support/appimage/appimage-exec.sh` (`exec "$APPDIR/AppRun" "$@"`) ·
`pkgs/build-support/build-fhsenv-bubblewrap/default.nix:345` (`extraInstallCommands`) ·
`gh search code --repo NixOS/nixpkgs {agregore,peersky}` → no results
