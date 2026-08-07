# Peer discovery & addressing for the iroh browser transport

Status: design review, 2026-08-07. Grounded in walkie-songie source at HEAD,
`iroh 1.0.3`, and `iroh-gossip 0.101.0` (crate sources under
`~/.cargo/registry/src/`). No code changed yet.

The symptom under diagnosis, from the tab that pasted a ticket:

```
WARN iroh-gossip-0.101.0 src/net.rs:436 dial failed: No addressing information available peer = 6ea6a17087
[native:repair_complete] HHHS H6 repair with 6ea6…daa completed; ingested 0 operations
UI: "⏳ Connecting to 1 discovered peer(s)…"
```

---

## 1. Root cause of the addressing bug

There is one bug with four compounding parts. The headline: **the ticket the
UI hands out usually contains an `EndpointAddr` with an empty address set**,
and the browser endpoint has **no way to resolve an endpoint id to an address
except that ticket**, because the code strips iroh's built-in discovery.

### 1.1 The ticket is minted before the relay home exists, and never refreshed

- On wasm, an endpoint's address is **relay-only and empty until the home
  relay handshake completes**. `Endpoint::watch_addr` under `cfg(wasm_browser)`
  maps only the home-relay watcher (`iroh-1.0.3/src/endpoint.rs:1299-1309`);
  `Endpoint::addr()` is `watch_addr().get()` (`endpoint.rs:1199-1201`). Before
  the relay connects, `addr()` = `EndpointAddr { id, addrs: {} }`.
- `BrowserNetHandle::settle_ticket` waits at most **750 ms** for
  `endpoint.online()` (`src/net/browser.rs:103-106`, called from
  `src/web/browser_host.rs:328` with `Duration::from_millis(750)`).
  `online()` resolves only after a relay **completed its handshake**
  (`endpoint.rs:1358-1374`); iroh's own docs recommend a timeout near
  `NET_REPORT_TIMEOUT` = **5 s** (`src/net_report/defaults.rs:14`). A cold
  browser doing DNS + TLS + WebSocket upgrade to `relay.wondering.xyz` easily
  blows 750 ms — the console's `home is now relay …, was None` arriving *after*
  the dial failures is exactly this race, observed.
- Worse, the ticket is a **one-shot snapshot**: `start_room` stringifies it
  once (`browser_host.rs:328-329`), stores it in the snapshot
  (`browser_host.rs:702`) and emits `RoomChanged` once
  (`browser_host.rs:718-722`). Even after the relay settles seconds later,
  every copy of the ticket from the UI is still the address-less one.
  `BrowserNetHandle::ticket()` (`browser.rs:96-98`), which would return a
  fresh address, is never re-consulted.
- The ticket codec happily round-trips an empty `EndpointAddr` — see the
  project's own test constructing `EndpointAddr::new(pubkey)` with no addrs
  (`src/net/iroh_common.rs:399-407`). Nothing anywhere refuses to *share* an
  undialable ticket.

### 1.2 The receiving side has no other resolver — by explicit choice

`BrowserRoomNetwork::bind` builds the endpoint with:

```rust
Endpoint::builder(presets::N0)
    …
    .clear_address_lookup()
    .address_lookup(memory.clone())   // MemoryLookup only
```

(`src/net/browser.rs:178-186`; `src/net/native.rs:87-95` is identical.)

This is the decisive line. `presets::N0` **already installs**
`PkarrPublisher::n0_dns()` and `PkarrResolver::n0_dns()` — both fully
wasm-capable — and `.clear_address_lookup()` throws them away
(`iroh-1.0.3/src/endpoint/presets.rs`, `N0::apply`: publisher + resolver
always; `DnsAddressLookup` additionally on non-wasm). So the endpoint's entire
id→address knowledge is one `MemoryLookup` seeded from the (empty) ticket.

### 1.3 The dial path, precisely

`join_ticket`/bind seeds `MemoryLookup` with the ticket's `EndpointAddr`
(`browser.rs:171-174`, `115-121`), then gossip `subscribe(topic, [id])`
queues a bootstrap dial. The chain:

1. gossip `Dialer::queue_dial` → `endpoint.connect(endpoint_id, GOSSIP_ALPN)`
   (`iroh-gossip-0.101.0/src/net.rs:1014-1031`).
2. `connect_with_opts` → `self.inner.resolve_remote(endpoint_addr)`
   (`iroh-1.0.3/src/endpoint.rs:1130`).
3. `resolve_remote` asks the `RemoteStateActor` for a known path; with none, it
   runs address lookup and waits (`src/socket.rs:1304-1336`).
4. `MemoryLookup::resolve` returns the stored entry **even when its addr set is
   empty** (`src/address_lookup/memory.rs`, `impl AddressLookup … resolve`);
   `insert_multiple` inserts zero paths — explicitly a no-op for an empty
   iterator (`src/socket/remote_map/remote_state/path_state.rs:128-155`).
5. Lookup stream ends → `address_lookup_finished` → paths still empty →
   `AddressLookupFailed::NoResults` (`path_state.rs:176-206`,
   `src/address_lookup.rs:264-279`).
6. Surfaces as `ConnectWithOptsError::NoAddress` — display string **"No
   addressing information available"** (`endpoint.rs:909-910`) — logged by the
   gossip actor's dialer arm (`iroh-gossip/src/net.rs:436`).

### 1.4 The failed dial is terminal — gossip never retries a bootstrap peer

On dial failure the gossip actor feeds `InEvent::PeerDisconnected`
(`net.rs:437-446`); HyParView's `handle_connection_closed` just removes the
peer from views (`src/proto/hyparview.rs:346-354`). There is no backoff, no
re-dial. `join_peers` is the only re-trigger, and nothing in walkie calls it
again. So a single 750 ms race at ticket time bricks the room join forever,
silently — the dial error never crosses the gossip API into an `AppEvent`.

### 1.5 Why HHHS repair could still reach that endpoint id

Two asymmetries explain repair succeeding while gossip dials fail:

- **Accepting needs no addressing.** The RBSR responder path
  (`BrowserRepairProtocol::accept`, `browser.rs:337-377` →
  `spawn_repair(…, initiator=false)`, `browser_host.rs:494-502`) logs the same
  `repair_complete` diagnostic (`browser_host.rs:1133-1139`). An inbound
  connection from `6ea6…` requires only that *the other side* had *our*
  address — e.g. our ticket pasted in the other direction carried a settled
  relay URL (the second tab often wins the 750 ms race that the first tab,
  binding on page load, loses).
- **Dial-by-id succeeds the moment any path is known.** `resolve_remote`
  returns immediately when the remote's path set is non-empty
  (`path_state.rs:161-167`), and paths accrue from sources other than the
  ticket: any inbound connection registers its path
  (`socket.rs:1352-1376`, `register_connection`), and gossip itself gossips
  addresses — each peer's `EndpointAddr` rides HyParView `PeerData`
  (`iroh-gossip/src/net.rs:484-487` publish side, `net.rs:737-750` receive
  side, stored in a `GossipAddressLookup` that gossip registers into the
  endpoint at spawn, `net.rs:186-194`). So a *later* repair dial by bare id
  can succeed even though the *initial* bootstrap dial — racing an empty
  `MemoryLookup` before any connection existed — failed permanently.

The UI's terminal "⏳ Connecting to 1 discovered peer(s)…" is consistent with
either timeline: the bootstrap peer is seeded into the peers map as
`Connecting` (`browser_host.rs:340-350`), and `classify_peer_path` also
returns `Connecting` after a connection dies (remote_info present, no Active
addrs — `iroh_common.rs:218-244`), so both "never connected" and "connected
once, then dropped" park the status there.

---

## 2. The fix for addressing: stop deleting iroh's discovery (pkarr in wasm)

### 2.1 What works in a relay-only browser

In iroh 1.0.3 the discovery system is the `iroh::address_lookup` module
(renamed from `discovery`; services implement `AddressLookup` with `publish` /
`resolve`, are composed by `AddressLookupServices`, and the endpoint calls
`publish` on every own-address change — `src/socket.rs:695`).

| Service | wasm? | Notes |
|---|---|---|
| `MemoryLookup` | yes | what we have; manual entries only |
| `PkarrPublisher` | **yes** | HTTP `PUT https://dns.iroh.link/pkarr/<z32-endpoint-id>` via reqwest→fetch; publishes **relay URLs only** by default (`AddrFilter::relay_only()`); auto-republish every 5 min, 1 s/2 s/… backoff on failure (`src/address_lookup/pkarr.rs`) |
| `PkarrResolver` | **yes** | HTTP `GET https://dns.iroh.link/pkarr/<z32>` → signed packet → `EndpointInfo` (`pkarr.rs`, `impl AddressLookup for PkarrResolver`) |
| `DnsAddressLookup` | **no** — `#[cfg(not(wasm_browser))]` (`src/address_lookup.rs:120-125`) | native-only DNS query path |
| mDNS / mainline | no | separate crates, native only |

Browser-friendliness of pkarr is not speculative: `presets::N0` uses exactly
publisher+resolver on wasm, and n0's `iroh-dns-server` mounts a permissive
CORS layer (`CorsLayer … allow_origin(cors::Any)`,
`n0-computer/iroh-dns-server`, `iroh-dns-server/src/http.rs:190-231`), so
cross-origin GET/PUT from `micahscopes.github.io` works.

Publishing is relay-only-compatible by construction: on wasm the published
`EndpointData` comes from `watch_addr`, which is exactly `{Relay(home_url)}`.
Peers resolve `6ea6… → https://relay.wondering.xyz/` and dial through the
relay both ends already share.

### 2.2 Exact changes

**`src/net/browser.rs` (`BrowserRoomNetwork::bind`, lines 178-186)** — keep the
`MemoryLookup` (instant ticket fast-path) but stop clearing the preset, or
equivalently re-add pkarr explicitly (explicit is better here; it survives
preset changes and documents intent):

```rust
use iroh::address_lookup::{PkarrPublisher, PkarrResolver};

let endpoint = Endpoint::builder(presets::N0)
    .secret_key(secret_key)
    .alpns(vec![GOSSIP_ALPN.to_vec(), RBSR_ALPN.to_vec()])
    .relay_mode(relay_mode)
    .clear_address_lookup()
    .address_lookup(memory.clone())
    .address_lookup(PkarrPublisher::n0_dns())   // publish id -> relay url
    .address_lookup(PkarrResolver::n0_dns())    // resolve id -> relay url
    .bind()
    .await…
```

`PkarrPublisher::n0_dns()` / `PkarrResolver::n0_dns()` return builders that
implement `AddressLookupBuilder`, so they slot straight into
`Builder::address_lookup` (`pkarr.rs`, `PkarrPublisherBuilder`,
`PkarrResolverBuilder`). Default `AddrFilter::relay_only()` is already what we
want; do not widen it.

**`src/net/native.rs` (lines 87-95)** — same two lines. Native keeps mDNS as
before (added post-bind via `endpoint.address_lookup()?.add(mdns)`,
`native.rs:97-104`); pkarr gives native↔browser and cross-network native↔native
resolution. On native, `DnsAddressLookup::n0_dns()` can be added too (cheaper
reads than pkarr HTTP), but pkarr alone is sufficient and keeps both targets
symmetric.

**Effect**: once any code path knows an endpoint *id* — ticket, gossip
`join_peers`, a rendezvous (§3), a future `#room@<endpoint-id>` link
(`app.rs:1431-1440` already parses that form and currently drops the suffix) —
iroh resolves the address by itself. An address-less ticket becomes merely
"a ticket", not a brick.

Later, optionally: self-host `iroh-dns-server` (e.g. `dns.wondering.xyz`) and
switch to `PkarrPublisher::builder(url)` / `PkarrResolver::builder(url)` to
remove the n0 dependency. Not needed to ship.

### 2.3 Ticket hygiene (still worth doing)

1. Raise `settle_ticket` to **5 s** (match `NET_REPORT_TIMEOUT` guidance) in
   `browser_host.rs:328`. Room entry can render immediately; only the ticket
   string waits.
2. Make the ticket **live**: spawn a task on `endpoint.watch_addr().stream()`
   (or re-run `online()` → `handle.ticket()`) that re-emits
   `AppEvent::RoomChanged` with the fresh ticket string when the relay URL
   appears, replacing the one-shot snapshot at `browser_host.rs:328/702/718`.
3. App-level join retry: after `JoinTicket`, if the bootstrap peer hasn't
   produced `NeighborUp` within ~5 s, call
   `gossip_sender.join_peers(vec![id])` again with backoff (cap ~30 s).
   `join_peers` (`iroh-gossip/src/api.rs:193`) re-queues the dial
   (`net.rs:682-698`), and with the resolver installed the dial now blocks on
   pkarr instead of failing instantly. This also papers over the "peer
   published a moment after we dialed" window. Surface each failed round as an
   `AppEvent::Diagnostic` — today the dial error dies inside gossip's tracing
   and the app never learns the join failed.

---

## 3. Topic rendezvous: who is in room `groovy-field-garden`?

iroh-gossip's `subscribe(topic, bootstrap)` needs at least one live peer *id*;
there is no "who's subscribed to T" anywhere in iroh. Something outside iroh
must map **topic → endpoint ids**. With §2 in place, ids are all a rendezvous
needs to return — addresses resolve automatically. (Confirmed: that split —
rendezvous returns ids, address_lookup resolves them — is the clean
architecture. We still put the relay URL in the announce as a fast-path so a
join needs zero extra round trips.)

### Option 1 — reuse the y-webrtc signaling server at `signal.wondering.xyz` (recommended)

Already deployed, already Origin-tolerant, and this repo has spoken its
protocol before: the pre-iroh client is in git history at
`25bcf36:src/net/yjs_signaller.rs`. The protocol (y-webrtc `bin/server.js`)
is a trivial topic pub/sub over one WebSocket; the server never inspects
`data`, it fans every `publish` out to every subscriber of that topic:

```
→ {"type":"subscribe","topics":["walkie-rdv-v1-<topic-hex>"]}
→ {"type":"publish","topic":"walkie-rdv-v1-<topic-hex>","data":<opaque>}
← {"type":"publish","topic":"walkie-rdv-v1-<topic-hex>","data":<opaque>}   (fan-out, sender included)
← {"type":"ping"}   → {"type":"pong"}                                       (~30 s keepalive)
```

Our `data` payload (new, ignores/ignored-by y-webrtc peers):

```json
{ "kind": "walkie-hello", "v": 1,
  "id": "<endpoint id, 64 hex>",
  "relay": "https://relay.wondering.xyz/" }
```

Client behavior (new module, e.g. `src/net/rendezvous.rs` + wasm WS impl):

- On room join: connect, `subscribe` to `walkie-rdv-v1-` + the existing
  `RoomTopic` hex (**never** the human room name — same privacy stance as
  `room_mdns_service_name`, `iroh_common.rs:98-101`), then `publish` a hello.
- On hello from an unknown id: `memory_lookup.add_endpoint_info(EndpointAddr
  {id, {Relay(relay)}})`, `gossip_sender.join_peers(vec![id])`, seed the peers
  map with `DiscoverySource::AddressLookup` (variant already exists,
  `src/client.rs:101-106`; app.rs:1160 already renders it), and **reply with
  our own hello** so the newcomer learns us — this handles late joiners with
  zero server state.
- Re-hello every ~30 s as keepalive; answer `ping` with `pong`; skip
  `join_peers` for ids that are already active neighbors to avoid HyParView
  Join spam.
- wasm: `web_sys`/`gloo-net` WebSocket (the historical client is a working
  reference). Native: same protocol over `tokio-tungstenite`, so desktop peers
  also meet across networks (today native only has mDNS = LAN-only).

Threat note: hellos are unauthenticated; a topic-hash-knowing attacker can
spray fake ids. Ids are self-certifying (the QUIC handshake proves the key),
so garbage ids only cost bounded failed dials — cap concurrent pending joins.

Why ranked first: zero new deploys, push (no polling latency), protocol
already proven from this codebase, and it degrades into Option 2 cleanly.

### Option 2 — tiny owned rendezvous next to the relay (successor)

A second binary in `relay/` (workspace member `walkie-rendezvous`), ~120 lines
of axum, behind traefik on wondering.xyz:

```
POST /v1/rooms/{topic-hex}/announce   body {"id":"<hex>","relay":"<url>"}  → 204
GET  /v1/rooms/{topic-hex}            → {"peers":[{"id":"…","relay":"…"},…]}
```

In-memory `HashMap<Topic, HashMap<EndpointId, (RelayUrl, Instant)>>`, 60 s
TTL, GC sweep, caps (≤64 peers/topic, ≤1 KiB body, per-IP rate limit), CORS
allowlist matching the relay's Origin allowlist, no persistence. Client:
announce on join + every 30 s; GET on join + poll 5 s (jittered) while the
room has no neighbors, slow to 60 s once connected. Same client-side handling
of results as Option 1.

Take it when: the y-webrtc server is to be retired, or its multi-tenant JS
deploy becomes a reliability/abuse concern. The client module's transport is
the only swap (WS pub/sub → HTTP announce/poll).

### Option 3 — pkarr/DNS as the rendezvous: no

pkarr records are keyed by an ed25519 keypair and signed by it. A
topic-derived keypair (everyone in the room derives the same secret from the
room name) is technically expressible, but: one record per key with
last-writer-wins means N members permanently overwrite each other; the shared
secret means any member (or name-guesser) can wipe the record; signed-packet
size (~1 KiB DNS packet) caps membership; `PkarrResolver` validates packets
as *that endpoint's own info* (`EndpointInfo::from_pkarr_signed_packet`), so
a custom client would be needed; and it repurposes n0's public infra as a
mutable room registry. A known-node bootstrap derived from the room name has
the same shape with one seat. Reject; rendezvous is a different primitive
than id→address lookup, and forcing pkarr to do both breaks both.

---

## 4. Implementation plan (each step independently testable)

1. **Pkarr address lookup** — `browser.rs` + `native.rs` builder changes from
   §2.2. Test: two browser tabs, ticket flow; deliberately keep the 750 ms
   settle so the ticket is empty — join must now succeed anyway (watch the
   `PUT/GET dns.iroh.link/pkarr/<z32>` in devtools; expect `NeighborUp` and
   status leaving "Connecting"). This alone un-bricks tickets.
2. **Live tickets** — settle 5 s + re-emit `RoomChanged` on `watch_addr`
   change (§2.3.1-2). Test: UI ticket string visibly gains the relay URL
   after connect; copy → paste still works with pkarr disabled locally
   (temporarily) to prove the ticket itself is again self-sufficient.
3. **Join retry + diagnostics** — bounded `join_peers` re-dial with backoff,
   dial failures surfaced as diagnostics (§2.3.3). Test: paste a ticket for a
   peer that comes online 20 s later; expect eventual join, and visible
   diagnostics meanwhile.
4. **Rendezvous client (Option 1)** — `src/net/rendezvous.rs` + wasm WS
   backend, wired into `start_room` for the `EnterRoom` (room-name) path;
   hellos as in §3. Test: two tabs, same three-word code, no ticket anywhere,
   different networks — auto-meet; then a third tab late-joins. Then the
   native (Tauri) client over `tokio-tungstenite` against a browser tab.
5. **(Later) owned rendezvous (Option 2)** — second binary in `relay/`,
   traefik route, flip the client transport. Test: same matrix as step 4 with
   `signal.wondering.xyz` blocked.

Step 1 is the root-cause fix; steps 2-3 are hardening the manual path; step 4
is the product feature (three-word code auto-peering); step 5 is an ops
decision, not a blocker.
