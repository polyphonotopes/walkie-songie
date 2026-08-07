# Transport-agnostic tutti and the direct-peering path off the relay

Status: design, 2026-08-07. Grounded in walkie-songie at HEAD (`src/net/**`,
`src/web/browser_host.rs`, `src-tauri/src/lib.rs`), the iroh 1.0.3 /
iroh-base 1.0.3 / web-sys 0.3.103 crate sources under `~/.cargo/registry`,
and the companion designs: `docs/vision/tutti-crate-architecture.md` (§2.3,
the `tutti-net` / `tutti-net-iroh` split), `docs/vision/
eventually-consistent-pitchsets.md` (§3.1 channels + device controls, §6.2
transport-swappability, §6.6 the leaf profile), and
`docs/research/peer-discovery-design.md` (the rendezvous we just shipped).
No code changed.

The motivation, verbatim from the user: *tutti should be highly transport
agnostic — USB, Bluetooth LE, WiFi via mDNS, etc. Right now the relay
situation is very unfortunate latency-wise; hopefully this drives a direct
peering solution.* This document does both jobs: it states the transport
contract a tutti backend must satisfy (and shows walkie already has it), and
it designs the one backend that actually kills the latency pain — a WebRTC
datachannel path for browsers, bootstrapped over the signaling server we
already run.

---

## 1. Why the relay hurts: the problem in round trips

A browser peer today is **relay-only by construction**. iroh's wasm build
compiles the IP transport out and tunnels QUIC over the relay's WebSocket
(`src/net/browser.rs:1-14`; only `mod ip` is `cfg(not(wasm_browser))` in
`iroh-1.0.3/src/socket/transports.rs:31-32`), so `classify_peer_path` can
honestly report nothing better than `Relayed` for a browser
(`browser.rs:10-11`). Every byte between two browser peers — even two tabs
on the same LAN — travels `A → relay.wondering.xyz → B`
(`iroh_common.rs:54`). That tax lands on all three traffic classes at once:

- **Gossip ops** — every `SignedOp` broadcast takes the relay detour twice
  (up from the author, down to each subscriber), plus Plumtree re-forwarding
  hops that each also transit the relay (`native.rs:533-538` documents that
  `delivered_from` is a forwarder, i.e. multi-hop relaying is normal).
- **Anti-entropy** — an HHHS repair session is a multi-round-trip protocol
  (`SyncLimits::budget.max_rounds`, `sync.rs:134-151`); every round pays the
  full relay RTT.
- **Presence leases** — the voice-preview tier renews on a **1.5 s default
  lease** (`src/room/presence.rs:16-17`). This is the latency-sensitive
  musical surface, and it rides the same relayed gossip.

There is a second-order cost beyond distance: the relay path is QUIC
tunneled **over WebSocket, i.e. over TCP** (`browser.rs:10-11`). Under any
loss, TCP retransmission and head-of-line blocking add jitter that a UDP
path would not — the worst possible failure mode for a jam.

Native peers are already fine: iroh does relay-assisted hole punching and
direct UDP with mDNS on the LAN (`native.rs:1-9`, `native.rs:104-112`), and
`PeerTransportPath::Direct` is reachable and reported honestly
(`iroh_common.rs:224-250`). **The whole latency problem is the browser**,
and iroh 1.0 has no WebRTC and no UDP in wasm — that gap is documented and
final in `peer-discovery-design.md` §2.1 (the wasm capability table) and is
what this design routes around.

---

## 2. The transport contract tutti-net inherits

### 2.1 The seam already exists

`src/net/mod.rs` carries a complete, deliberately dependency-free transport
abstraction ("no backend type may appear here… compiles on every target and
feature combination", `mod.rs:84-91`). Its pieces, which the tutti-net crate
(`tutti-crate-architecture.md` §2.3) adopts essentially verbatim:

- **`PeerId`** (`mod.rs:107-108`): the transport identity is a raw 32-byte
  Ed25519 key — and it is *deliberately distinct from authorship*. An
  inbound frame's `PeerId` is whoever **delivered** it; authorship comes
  from `verify_signed_op_for_topic`, never from the transport
  (`mod.rs:99-106`, reinforced at `native.rs:532-538`).
- **`TransportMode`** (`mod.rs:124-146`): the runtime backend selector.
  Already a catalog in miniature: `Iroh` (default), `Libp2p` (Agregore
  `pubsub://`, broadcast-only), `Hyperswarm` (`hyper://` extension
  channels), `Loopback` (in-process tests).
- **`TransportEvent<S>`** (`mod.rs:191-220`): everything the room layer
  learns — `PeerUp/PeerDown` (with `DiscoverySource`), `Message` (verbatim
  `SignedOp` bytes), `SyncRequested { stream }` (an *already-open*
  anti-entropy stream the consumer must spawn, `mod.rs:208-216`), `Lagged`,
  `Closed`, `Diagnostic`.
- **`SyncStream`** (`mod.rs:226-240`): one bidirectional, ordered,
  peer-scoped **frame** channel carrying a single HHHS `SyncSession`.
  Framed on purpose: framing is the backend's job (iroh adds length
  prefixes, `repair.rs:141`, `:187`; a JS-bridged socket crosses the wasm
  boundary as discrete messages anyway).
- **`Transport`** (`mod.rs:242-279`): `broadcast` (fan one frame to the
  room, with an honest `max_broadcast_bytes` cap), `next_event`
  (single-consumer, but **two separately bounded producer queues** so
  repair can never head-of-line-block op delivery, `mod.rs:261-268`,
  `native.rs:425-434`), `open_sync(peer)` (dial one anti-entropy session),
  `peer_path(peer)` (honest reachability for UI), `shutdown`. **No
  `Send`/`Sync` bounds anywhere** — browser backends hold `!Send` JS
  handles (`mod.rs:243-248`).

So "the tutti-net contract" is not aspiration; it is `mod.rs`, with two
proven implementors (`NativeRoomNetwork`, `native.rs:435-530`;
`BrowserRoomNetwork`, `browser.rs:406-496`) plus the loopback pair
(`loopback.rs:1-9`).

### 2.2 Receipts: everything above the seam is already transport-neutral

The claim that new backends are *additive* — no changes above the seam —
has receipts:

1. **The RBSR driver names no backend.** `sync.rs:1-8` says it outright:
   "Nothing here names a backend, so the same driver serves iroh, a
   loopback pair, and any browser-bridged carrier."
   `drive_initiator`/`drive_responder` (`sync.rs:504`, `sync.rs:563`) are
   generic over `S: SyncStream` and a runtime-neutral `SyncTimer`
   (`sync.rs:96-104`). The loopback transport exists precisely so this is
   *tested* transport-neutrality, not asserted (`loopback.rs:1-9`).
2. **Gossip fan-out is a backend obligation, not a shared component.** The
   contract asks only "fan one `SignedOp` frame out to the room"
   (`mod.rs:258-259`). iroh brings HyParView/Plumtree; a WebRTC mesh can
   bring flooding; Agregore brings gossipsub. The room loop consumes
   `TransportEvent::Message` and cannot tell the difference.
3. **The room loop drives exactly one `Transport`.** The browser host's
   event loop selects over one `network.next_inbound()`
   (`browser_host.rs:503`); multipath (§5) therefore composes *below* that
   line, invisible to the host.
4. **Trust does not come from the wire.** Ops are signed and verified at
   ingress (`sync.rs:410-415`: "re-derived from the verified op… never
   trusted from the wire"); presence frames are signed and leased
   (`presence.rs:1-17`). A hostile or lossy carrier can waste bandwidth,
   not forge state. The vision states the consequence as contract:
   "Transport [is swappable]. iroh, libp2p, BLE, LoRa, sneakernet. Ops are
   self-verifying bytes; the wire owes them nothing but delivery"
   (`eventually-consistent-pitchsets.md:844-845`). Transport-level
   authentication (iroh's QUIC handshake) buys DoS-resistance and privacy,
   not integrity — which is what makes cheap, weakly-authenticated links
   (BLE, serial) admissible at all.
5. **Duplicate and multi-path delivery is safe by construction.** The store
   is idempotent: an op already lifted or parked is *kept*, not refused
   (`sync.rs:365-371`; `ingest_pairs` handles "gossip raced the session, or
   a duplicate pair" explicitly, `sync.rs:429-440`). This is the
   reconciling substrate's structural advantage over an ordered-stream
   design: delivering the same op on two transports at once is not an edge
   case to deduplicate at the transport layer — it is a no-op at ingest.
   Multipath needs no coordination protocol.

### 2.3 Refinements the tutti-net extraction should make

Four deltas between "walkie's seam today" and "the tutti-net contract",
none of them reshaping the trait:

1. **Open the mode identity.** `TransportMode` is a closed enum
   (`mod.rs:127-146`); adding WebRTC/BLE/USB means touching the core. In
   tutti-net it should stay a small enum for the *shipped* backends plus a
   `Custom(&'static str)`-style escape, or become an opaque id — the
   `as_str`/`FromStr` surface (`mod.rs:148-171`) already treats it as a
   string at the edges.
2. **Report paths as a set, not a scalar.** `peer_path` returns one
   `PeerPath` (`client.rs:88-93`). With multipath, honesty means "reachable
   via {webrtc-direct (12 ms), relay (140 ms)}" — a
   `Vec<(PathKind, Option<Duration>)>` with the scalar kept as the
   summarizing projection for the existing UI. Concretely, iroh's
   `classify_peer_path` already silently drops non-IP/relay addresses
   through its `_ => {}` arm (`iroh_common.rs:238-242`) — a custom/WebRTC
   path would today be misreported as `Connecting`; that arm must learn a
   `TransportAddr::Custom` case whatever else happens.
3. **A peer-hint seam.** Today address knowledge is fed to the iroh backend
   through backend-specific handles (`MemoryLookup::add_endpoint_info`,
   `rendezvous.rs:284-296`). The generic contract wants
   `add_peer_hint(peer, bytes)` — opaque, backend-interpreted — so one
   rendezvous client can feed any backend (the same split
   `peer-discovery-design.md` §3 already drew: rendezvous returns ids +
   hints; the transport resolves them).
4. **Protocol identity as a parameter.** ALPNs, magics, the rendezvous
   channel prefix, relay/signaling URLs become the `ProtocolIds` struct of
   the crate plan (`tutti-crate-architecture.md:191-200`) — this design
   adds the WebRTC signaling message kinds and STUN/ICE config to that
   struct's inventory.

And one structural observation that shapes §7: the contract actually has
**two seams**, not one. Full peers implement `Transport` (room carriage:
broadcast + events + dialing). Constrained links only need `SyncStream` —
a framed duplex — because the sync driver runs over the stream alone. A
device link (BLE, serial) never has to be a `Transport`; it has to be a
`SyncStream` plus a thin op/presence forwarding loop on the fuller peer.

---

## 3. The backend catalog

| backend | reach | latency profile | platforms | Rust feasibility | status |
|---|---|---|---|---|---|
| iroh relay (QUIC-over-relay-WS) | universal WAN | worst: 2× haul to relay, TCP jitter | native + browser | shipped (`browser.rs`, `native.rs`) | **shipped** |
| iroh direct UDP (hole-punched) | WAN where NATs allow | best WAN | native only | shipped (iroh core) | **shipped** |
| iroh mDNS + LAN UDP | LAN | ~sub-ms LAN | native only | shipped (`iroh-mdns-address-lookup`, `native.rs:104-112`) | **shipped** |
| **WebRTC datachannel** | LAN + WAN (ICE) | direct-path; the browser fix | browser (stable web-sys) + native (webrtc-rs/str0m) | real crates, real signaling already deployed | **the headline — §4** |
| WebTransport (`serverCertificateHashes`) | browser→native, mostly LAN | direct, HTTP/3 | Chromium-only; web-sys unstable | `wtransport` native server; no ICE | research |
| Bluetooth LE | proximity (~10 m) | low latency, tiny bandwidth | native central (btleplug); ESP-32 peripheral; Web Bluetooth Chromium-only | central: yes; peripheral: platform-fragmented | device link — §7 |
| USB (serial / MIDI) | wired | lowest, most reliable | native (`serialport`); Web Serial/WebUSB Chromium-only | yes | device link — §7 |
| libp2p gossipsub via Agregore `pubsub://` | that browser's swarm | n/a (broadcast-only) | Agregore only | reserved (`TransportMode::Libp2p`, `mod.rs:132-137`) | parked, by prior design |
| hyperswarm via `hyper://` extensions | swarm | n/a | Agregore + Peersky | reserved (`TransportMode::Hyperswarm`, `mod.rs:138-143`) | parked, by prior design |

Per-backend notes, with the honesty attached:

**iroh relay.** Universal and already the fallback everything else degrades
to. Not going anywhere (§6). Its role shifts from "the only browser path"
to "the guaranteed path".

**iroh UDP + mDNS.** Native LAN and WAN direct are a solved problem in this
codebase — including honest path reporting and the room-scoped,
name-private mDNS service label (`iroh_common.rs:104-107`). Nothing to do.

**WebRTC.** The only direct browser↔browser P2P primitive that exists,
full stop, and the only browser↔native direct primitive that traverses
NATs. iroh does not provide it in wasm (`peer-discovery-design.md` §2.1),
so it is *added beside* iroh, not obtained from it — though §4.2 shows iroh
1.0.3 has an (unstable) socket-level seam that may let iroh *carry* it.
Crate reality check:

- `web-sys` 0.3.103 ships `RtcPeerConnection` / `RtcDataChannel` (+ Init,
  IceCandidate, SessionDescription types) as **stable** features
  (`web-sys-0.3.103/Cargo.toml:1718`, `:1756` — no
  `web_sys_unstable_apis` gate, unlike Bluetooth/Serial/USB/WebTransport,
  which are all unstable-gated). The browser side is plain stable Rust.
- `matchbox` (`matchbox_socket`) is a working wasm+native full-mesh
  WebRTC-datachannel crate — proof the whole shape works in Rust — but it
  brings its own signaling protocol (`matchbox_server`) and its own socket
  abstraction. Reusing it would mean running a second signaling service and
  adapting its API to `Transport`; writing the ~equivalent glue against
  `web-sys` directly, speaking *our* deployed signaling (§4.1), is
  comparable effort with no new infra. Treat matchbox as reference code.
- Native: `webrtc-rs` (a full WebRTC stack, heavy) or `str0m` (sans-io,
  lighter, needs its own ICE/UDP driving). Needed only for browser↔native
  direct (phase 2, §4.5) — native↔native is already covered by iroh UDP.

**WebTransport.** The W3C API allows a browser to connect to a self-signed
HTTP/3 server whose certificate hash it was told out of band
(`serverCertificateHashes`) — a real path for a browser to reach a *native*
walkie peer directly on the LAN without a CA. But: Chromium-only, web-sys
unstable-gated (`gen_WebTransport.rs` is `web_sys_unstable_apis`), no ICE
(so WAN NAT traversal is out), and it duplicates what WebRTC already gives
us with NAT traversal included. Catalog it; don't build it.

**Bluetooth LE.** Two distinct roles, and the crate story differs:
- *Fuller peer as central*: `btleplug` is cross-platform (Linux/macOS/
  Windows/Android/iOS) but **central-role only** — fine, because the
  device (ESP-32 leaf) is the peripheral.
- *Peripheral role in Rust on desktop* is fragmented (`bluer` is
  Linux/BlueZ-only) — avoid designing anything that needs it.
- *Browser*: Web Bluetooth is Chromium-only, central-only, user-gesture
  gated, and unstable in web-sys (`gen_Bluetooth.rs`,
  `web_sys_unstable_apis`). Usable for a demo ("browser adopts a BLE
  leaf"), not for the main path.
- Physics: with a 247-byte ATT MTU and practical GATT throughput in the
  tens of kB/s, BLE is a fine **op/lease/sync link** (ops are a few
  hundred bytes, `eventually-consistent-pitchsets.md:960-963`) and a
  non-starter as a room-scale gossip transport. It is a *device-channel
  link* (§7), which is also exactly what §3.1's device-controls story
  needs it to be.

**USB.** The wired device link: ESP-32 USB-CDC serial ↔ `serialport` on
native; Web Serial (Chromium, unstable web-sys) in a browser. Framing over
serial is COBS/length-prefix — precisely a `SyncStream` (§2.3's second
seam). USB-MIDI is the degenerate case for MIDI-only hardware: signed ops
can tunnel as 7-bit-clean SysEx at ~+15% size cost; worth a line in the
catalog because the MIDI editor/hot-plug surface already exists
(`src/midi/`, `ClientCommand::{ListMidiPorts, SelectMidiInput,…}`,
`client.rs:76-83`), but the CDC-serial link is strictly better when the
firmware is ours.

**Agregore libp2p / hyperswarm.** Already designed into the seam as
poly-modal variants with documented semantics deltas (broadcast-only vs
per-peer channels — `mod.rs:132-143`); libp2p remains allowed **only** via
Agregore's protocol handlers, not as a Rust dependency. No new work here;
this design just keeps their slots warm.

**Verdict:** real-now = WebRTC-browser (all pieces exist, including
signaling); shipped = iroh relay/UDP/mDNS; near = WebRTC-native
(webrtc-rs), USB-serial leaf link; research = BLE leaf link (needs the M3
windowed store to have a leaf at all — `performance-benchmark-suite.md`
§7.2), WebTransport, Agregore modes.

---

## 4. The WebRTC direct path, in detail

### 4.1 The signaling already exists — we deployed it for its original job

The topic rendezvous (`src/net/rendezvous.rs`) connects every peer of a
room to `wss://signal.wondering.xyz` (`iroh_common.rs:60`), subscribes them
all to the same opaque channel `walkie-rdv-v1-<topic-hex>`
(`rendezvous.rs:35`, privacy stance at `rendezvous.rs:11-13`), and
publishes JSON blobs the server fans out verbatim to every subscriber —
"the server never inspects `data`" (`rendezvous.rs:92-94`). The deployed
server is **y-webrtc's signaling server** (`iroh_common.rs:55-60`): its
designed purpose *is* brokering WebRTC connections. Walkie borrowed it for
iroh rendezvous; extending it to carry ICE/SDP is using it for exactly what
it was built for, with **zero server changes** — our messages are
discriminated by `kind` and ignore/are-ignored-by non-walkie publishers
(`rendezvous.rs:36-37`, `rendezvous.rs:270-273`).

Protocol extension (client-side only), alongside the existing `Hello`
(`rendezvous.rs:119-128`):

```json
{ "kind": "walkie-rtc", "v": 1,
  "from": "<endpoint id, 64 hex>", "to": "<endpoint id, 64 hex>",
  "payload": { "sdp": "…offer/answer…" } }            // or {"ice": {...}}
```

- Addressed fan-out: everyone on the channel receives it; non-addressees
  drop it by the `to` field (the same cost model as hellos, bounded by the
  existing `MAX_RENDEZVOUS_PEERS = 64` cap, `rendezvous.rs:48-53`).
- Deterministic roles, no glare: lower endpoint-id hex is the offerer
  (both sides already know both ids from the hello exchange,
  `rendezvous.rs:267-316`).
- The `Hello` grows an optional capability flag (`"rtc": true`) so peers
  only offer to peers that can answer — schema-compatible, since unknown
  fields are ignored and `relay` is already optional
  (`rendezvous.rs:125-127`).
- **Authentication honesty:** signaling is unauthenticated today (hellos
  too — `rendezvous.rs:49-53`). Under Option A below this is fine: the
  QUIC handshake *inside* the datachannel proves the endpoint key, so a
  hijacked signaling exchange yields a failed handshake, nothing worse.
  Under Option B, sign the `walkie-rtc` payload with the endpoint key
  (both sides know the claimed id; verification is one Ed25519 check) to
  pin the DTLS fingerprint to the peer identity.
- ICE servers: one STUN server is required for server-reflexive candidates
  — `stun:` config is a `ProtocolIds` entry; self-hosting (coturn in
  STUN-only mode next to the relay) removes the third-party dependency.
  **No TURN, ever**: TURN is a relay, and we already run a better one —
  iroh-relay *is* the fallback path (§6). Browsers on the same LAN
  additionally get host-candidate connectivity (mDNS-obfuscated host
  candidates) with no STUN at all — WebRTC quietly solves browser-LAN,
  which iroh mDNS cannot (browsers have no UDP).

### 4.2 Option A — WebRTC as an iroh custom transport (datagram carrier)

The load-bearing discovery of this design: **iroh 1.0.3 ships a pluggable
socket-level transport API**, behind the `unstable-custom-transports`
feature (`iroh-1.0.3/Cargo.toml:97`):

- `Endpoint::builder(...).add_custom_transport(Arc<dyn CustomTransport>)`
  (`iroh-1.0.3/src/endpoint.rs:812-816`);
- `CustomTransport::bind() -> Box<dyn CustomEndpoint>`; a `CustomEndpoint`
  watches its local `CustomAddr`s, creates senders, and `poll_recv`s
  datagrams (`iroh-1.0.3/src/socket/transports/custom.rs:24-74`);
- addresses travel as `TransportAddr::Custom(CustomAddr)` — a
  `(transport id: u64, opaque bytes)` pair with a string encoding — a
  first-class variant beside `Ip` and `Relay`
  (`iroh-base-1.0.3/src/endpoint_addr.rs:54-62`, `:186-296`);
- path policy is pluggable too: `Builder::path_selector(Arc<dyn
  PathSelector>)` (`endpoint.rs:839-843`; trait at
  `socket/remote_map/remote_state.rs:1419`), and the in-tree example
  (`iroh-1.0.3/examples/custom-transport.rs:37-70`) is literally "prefer
  the custom transport whenever a candidate path on it exists, else lowest
  RTT" — the exact policy we want;
- the module is **not** `cfg(not(wasm_browser))`-gated — only the IP
  transport is (`socket/transports.rs:30-32`), so the seam exists in the
  wasm build.

Under Option A, the WebRTC glue is small and everything above it is
untouched: a `WebRtcTransport` implements `CustomTransport`; each
established `RTCDataChannel` (configured unordered + zero retransmits, i.e.
datagram semantics) becomes a path that carries **QUIC packets as
datachannel messages**. Dialing works by address-book seeding, exactly like
the relay: the rendezvous hello advertises
`TransportAddr::Custom(WEBRTC_ID, endpoint-id)` beside the relay URL, the
existing `MemoryLookup.add_endpoint_info` call carries it
(`rendezvous.rs:284-296`), and a first transmit toward an unconnected
custom addr triggers the §4.1 signaling exchange lazily.

What this buys, item by item:

- **Gossip, RBSR, ALPNs, tickets, identity: unchanged.** The entire
  `Transport` impl for the browser (`browser.rs:406-496`) stays as is; the
  new path is invisible below `Endpoint`.
- **Multipath, migration, upgrade/downgrade: free.** iroh already runs
  candidate paths per remote and selects (RTT-biased, sticky) — with
  `PathSelector` to bias custom-first. Relay→direct upgrade and
  direct→relay failure downgrade are the machinery iroh runs for
  IP-vs-relay today, now covering webrtc-vs-relay.
- **Mesh scale: bounded by gossip, not by the room.** Datachannels are
  only needed toward peers iroh actually converses with — gossip's
  HyParView active view is a small partial view (the codebase already
  reasons about Join spam and Plumtree forwarding,
  `rendezvous.rs:213-215`, `native.rs:533-538`) — not O(n²) mesh edges.

The honest costs:

- **Unstable API.** "Not covered by semantic versioning… may change in any
  release" (`endpoint.rs:806-811`). Mitigated: walkie already exact-pins
  `iroh = "=1.0.3"` (`Cargo.toml:153`, `:188`), so nothing moves under us;
  the risk is upgrade friction, not breakage.
- **Send bounds vs wasm.** `CustomTransport`/`CustomEndpoint`/`CustomSender`
  are `Send + Sync + 'static` (`custom.rs:24`, `:36`, `:90`), and JS
  handles are `!Send`. The shape that satisfies both: the trait objects
  hold only channel endpoints (`futures::channel::mpsc` of `Vec<u8>` is
  `Send`), and a `spawn_local` task owns the `RtcPeerConnection`/
  `RtcDataChannel` and pumps the channels — the identical pattern the
  browser rendezvous socket already uses to bridge `!Send` JS callbacks
  into an async stream (`rendezvous.rs:473-605`). This is the part the
  spike (§8 step 1) must prove, because iroh's own wasm target only
  exercises the relay transport this way.
- **Double crypto + double congestion machinery.** QUIC (with its own
  TLS) inside DTLS-SCTP. Real, and acceptable: per-packet AEAD twice is
  CPU noise next to the latency win, and with unordered/unreliable
  datachannel config SCTP does not retransmit under QUIC. MTU is a
  non-issue (QUIC's 1200-byte minimum datagram fits any datachannel
  message limit by orders of magnitude).
- **`classify_peer_path` must learn the new arm** (`iroh_common.rs:238-242`)
  or the UI will report a fast direct path as `Connecting` (§2.3 item 2).

### 4.3 Option B — WebRTC as a parallel tutti `Transport`

The hedge if the custom-transport spike fails on wasm: implement
`Transport` (`mod.rs:249-279`) directly over a datachannel mesh —
`TransportMode::WebRtc` beside `Iroh`.

- `broadcast` = fan the frame to every open datachannel (flood; fine at
  jam-room sizes, capped by the same 64-peer rendezvous bound).
- `SyncStream` = one *reliable, ordered* datachannel per session (label
  `walkie/rbsr/2`, reusing the ALPN string as the label so the wire
  generation discipline carries over, `iroh_common.rs:46-52`) — the
  framed-message trait maps 1:1 onto datachannel messages, no length
  prefixes needed (`mod.rs:226-229` anticipates exactly this).
- `PeerUp/PeerDown` = datachannel open/close; `DiscoverySource::
  AddressLookup` attribution already exists and renders
  (`client.rs:97-102`).
- Integrity survives untouched (§2.2 item 4: ops and presence are signed;
  the transport authenticates nothing and that is admissible); privacy
  wants the signed-signaling hardening from §4.1.

What Option B must rebuild that Option A gets free: membership/gossip
(flooding + the mesh), transport-level peer authentication (or the
explicit decision to run unauthenticated), *and the cross-backend
composition of §5* — because now two `Transport`s are live at once and
something must merge them. That is not wasted work (the composite is the
tutti-net multipath layer anyway), but it is more moving parts on day one.

### 4.4 Recommendation

**Spike Option A first** (it is dramatically less new surface: signaling
messages + one datachannel pump + a `PathSelector`), with a hard,
one-week-shaped validation gate: two browser tabs, custom transport
registered, `connection.paths()`/`remote_info` showing an active custom
path, gossip flowing with the relay socket deliberately severed. If wasm
`Send` shimming or the unstable API fights back, fall to Option B — the
§4.1 signaling work and the STUN decision transfer verbatim, and the
`Transport` seam was built for exactly that insertion. Do not build both.

### 4.5 Browser↔native direct (phase 2)

Native↔native needs nothing (iroh UDP). Browser↔native direct means the
native side must speak WebRTC: `webrtc-rs` (or `str0m`) wrapped as the same
`CustomTransport` on the native endpoint, answering the same §4.1
signaling — the native rendezvous client already holds the socket
(`rendezvous.rs:416-467`, spawned at `src-tauri/src/lib.rs:276-277`). This
is a real dependency-weight decision (webrtc-rs is a large tree) and the
payoff is narrower (Tauri-desktop ↔ browser-tab in one room), so it ships
after the browser↔browser path proves out. Until then, browser↔native
pairs ride the relay exactly as today.

### 4.6 What M-milestone this is

None of §4 waits on the tutti crate extraction, the windowed store, or
anything in the M0-M4 perf ladder (`performance-benchmark-suite.md` §7.2).
It is walkie-repo work on `src/net/` with the existing seam. Given the
latency pain is *the* current UX complaint, it slots before the tutti-net
extraction — and the extraction then inherits it as the second concrete
backend, which is the best possible stress test of the `ProtocolIds`/
transport-neutral factoring ("the genericity test is a second consumer",
`tutti-crate-architecture.md` §6.1).

---

## 5. Multipath and transport selection

The model, stated once: **a peer may be reachable over several paths and
several backends at once; prefer the cheapest live direct path; keep the
relay as the always-there floor; let paths appear and vanish without the
app noticing.** The substrate makes this nearly free:

- **Correctness under duplication is already paid for** (§2.2 item 5).
  Sending one op over two paths, or receiving it via gossip *and* an RBSR
  session concurrently, converges identically (`sync.rs:429-440`). There
  is no ordering contract to protect — `TransportEvent::Message` order was
  never meaningful (causality lives in the ops, `mod.rs:99-106`). This is
  the concrete advantage over any ordered-stream substrate, where
  multipath demands sequencing/dedup machinery at the transport.
- **Within one iroh endpoint, selection is iroh's job.** Path candidates,
  RTT measurement, stickiness, migration — shipped; policy override via
  `PathSelector` (§4.2). tutti-net-iroh supplies the policy ("custom/IP
  before relay"), nothing else.
- **Across backends, selection lives in tutti-net** — never the app, never
  the domain. A `MultiTransport` composes N inner `Transport`s behind the
  same trait: `next_event` selects over the inners (dedup optional, see
  above); `broadcast` fans to all inners whose `max_broadcast_bytes`
  admits the frame (duplication is safe and buys delivery probability;
  a cost-aware policy can restrict later); `open_sync` dials in preference
  order (direct-capable backend first, relay-backed last) and takes the
  first success; `peer_path` unions the inner reports (§2.3 item 2). The
  room loop still drives exactly one `Transport`
  (`browser_host.rs:503`) — the composite preserves the single-consumer
  contract and the two-queue discipline of each inner (`mod.rs:261-268`).
- **Upgrade/downgrade is an emergent behavior, not a protocol.** A new
  path appearing (WebRTC established, mDNS peer surfaced) just changes
  which path the selector prefers; a path dying falls back. No state
  machine above the seam; at most a `Diagnostic`/path-change event for the
  UI meter.

Under Option A, note that §5's cross-backend composite has **zero
near-term users** (iroh internal multipath covers relay+UDP+WebRTC) — it
becomes real when a non-iroh backend ships (Option B fallback, or Agregore
modes). Design it in tutti-net's API (the trait already permits it);
build it when the second live backend exists, not before.

---

## 6. Composing with iroh: the dual-stack question, answered honestly

Should the browser *drop* iroh-relay once WebRTC lands? **No.** The relay
stays, for reasons that are each individually sufficient:

1. **WebRTC without TURN does not reach everyone.** Endpoint-dependent
   ("symmetric") NAT pairs defeat STUN-only ICE for a meaningful minority
   of real-world pairs. Those pairs need a relay — and running TURN would
   just be running a second, worse relay beside the one we have.
2. **Bootstrap and control.** Ticket joins, pkarr resolution, gossip
   bootstrap, and the first seconds of a room all work today through the
   relay path (`peer-discovery-design.md` §2); WebRTC setup itself takes
   an ICE round. The relay is the instant-on path while direct paths
   warm up.
3. **Cost asymmetry.** The marginal cost of keeping the relay socket is
   one WebSocket + keepalives; the cost of *not* having it is stranded
   peers. Under Option A the fallback is even automatic per-packet, not
   per-session.

So the browser posture is **WebRTC-first-with-relay-fallback in one
stack** (Option A: one endpoint, two transports, path selection), not
dual-stack. True dual-stack — two independent room networks (iroh for some
peers, WebRTC-mesh for others, Option B world) — costs double connection
state, double keepalive traffic, a composite event loop, and a peer-set
reconciliation surface; it is the contingency shape, not the goal. The
long-term reduction is the opposite direction: if iroh itself ever ships a
browser WebRTC transport upstream, our custom transport retires into it
with no seam change above the endpoint.

---

## 7. Device transports and the leaf/channel tie-in

The vision already made the structural moves; this section only routes
wires through them.

- **A device is a keypair, hence an author, hence a channel**
  (`eventually-consistent-pitchsets.md:556-566`). A hardware controller's
  ops are signed with its own key; **device-locked write is owner-gating
  where the owner is the device** — "the pieces combinator verbatim,
  pointed at hardware" (`:571-576`). Nothing transport-shaped appears in
  that story: the channel semantics are entirely in the fold, so *any*
  carrier that delivers the device's signed bytes yields the same room.
- **The leaf profile** (`:942-1005`): an ESP-32 holds its own log head +
  a bounded suffix + the union it renders, signs its own ops
  (Ed25519 in milliseconds on a 240 MHz part, `:986-988`), and syncs
  opportunistically against a fuller peer. Its perf gate is M3/M4
  (windowed store, then on-device measurement —
  `performance-benchmark-suite.md` §7.2); the *transport* design for it
  can be settled now.

The transport consequence of §2.3's two-seam observation: **a leaf link
implements `SyncStream`, not `Transport`.** Concretely:

- The physical link — USB-CDC serial (`serialport` on the host), BLE GATT
  (leaf = NimBLE peripheral on ESP-32; host = `btleplug` central), or
  later WiFi/ESP-NOW — carries length-framed messages. That *is* the
  `SyncStream` contract (`mod.rs:230-240`): `send_frame`/`recv_frame`/
  `close`, framing being the link's job. Over it run:
  1. the leaf's freshly signed ops (host ingests via the standard verify
     path and re-broadcasts to the room — the host is a *courier*, not an
     authority; it cannot forge the device's authorship, §2.2 item 4);
  2. room ops the leaf's render needs, forwarded down (filtered by the
     leaf's declared interest — its own channel + the union set);
  3. periodic anti-entropy: `drive_initiator`/`drive_responder` run over
     the link unchanged (`sync.rs:504`, `:563`) with a shrunken
     `SyncLimits` (`budget.max_frame_bytes` sized to the link MTU,
     `sync.rs:119-151` — the budget was built to be scaled);
  4. presence leases for the gesture tier (knob-drag as lease frames,
     `eventually-consistent-pitchsets.md:563-566`), which are
     RAM-resident and loss-tolerant by design (`presence.rs:1-5`).
- The fuller peer (phone/laptop/Tauri desktop) is the leaf's archive and
  gossip proxy — exactly the delegation the leaf profile prescribes
  ("deep history… and long-range repair live on full nodes", `:978-981`).
  From the room's perspective the leaf is simply another author whose
  courier happens to sit on a serial port; **no `Transport` impl, no
  gossip membership, no new event variants.**
- Ladder of links, cheapest confidence first: **USB serial** (deterministic,
  powered, flashable — the M4 micro-probe transport too), then **BLE**
  (untethered gallery grid), then WiFi (at which point the leaf could in
  principle run real iroh — but the windowed store and RAM budget, not the
  radio, are the gate).

Browser-side device access (Web Bluetooth / Web Serial / WebUSB) is
Chromium-only and web-sys-unstable across the board (§3), so device links
target the native host first; a browser demo is possible but never the
load-bearing path.

---

## 8. Staged plan

Each step independently shippable; order = leverage ÷ risk. Steps 1-4 are
pre-extraction walkie work; step 7+ ride the tutti/M-ladder gates.

1. **Custom-transport spike (the go/no-go).** `unstable-custom-transports`
   feature on the pinned iroh; native-only first (loopback
   `CustomTransport` echo per `examples/custom-transport.rs`), then the
   same registered in the wasm build with a channel-pumped fake link.
   Proves the `Send`-shim shape of §4.2 before any WebRTC code exists.
   Exit: custom path visibly selected on a wasm endpoint, or a written
   verdict flipping us to Option B.
2. **Signaling extension.** `walkie-rtc` kinds over the existing
   rendezvous socket (`rendezvous.rs`), role rule, capability flag in
   `Hello`, signed payloads. Shippable dark (no consumer yet); testable
   against two tabs with a console harness. Shared verbatim by Options A
   and B.
3. **Browser↔browser WebRTC path (the headline).** wasm `CustomTransport`
   over `web-sys` datachannels (unordered/unreliable config), lazy dial
   via step-2 signaling, `CustomAddr` advertised in hellos and seeded via
   the existing `MemoryLookup` path, `PathSelector` = custom-then-RTT,
   `classify_peer_path` learns the `Custom` arm and `PeerPath` reporting
   goes multi-path (§2.3 item 2) so the UI can *show* "direct". Acceptance:
   two tabs on one LAN and on two networks, ops + presence flowing with
   relay traffic observably idle; kill the datachannel and watch relay
   fallback without a dropped op.
4. **STUN posture.** Start on a public STUN server behind a `ProtocolIds`
   knob; decide self-hosted coturn (STUN-only) when ops preferences say
   so. Explicitly no TURN (§4.1, §6).
5. **Native WebRTC answerer** (browser↔native direct): `webrtc-rs`-backed
   `CustomTransport` behind a feature flag, answering the same signaling.
   Weight-justified only after step 3 metrics show browser↔native pairs
   matter as much as browser↔browser.
6. **tutti-net extraction alignment.** Fold the §2.3 refinements into the
   `tutti-net` / `tutti-net-iroh` split as it lands
   (`tutti-crate-architecture.md` §5 track D step 5 — which already waits
   for `src/net` to quiesce): open mode identity, peer-hint seam,
   multi-path `peer_path`, WebRTC signaling kinds + STUN in `ProtocolIds`.
7. **Leaf link, serial first.** `SyncStream` over USB-CDC + the courier
   loop on the Tauri host; doubles as the M4 micro-probe transport. Gated
   behind M3 (windowed store) for a *real* leaf, but the host-side link
   code and a dev-board echo need nothing from the ladder.
8. **BLE leaf link** (btleplug central ↔ NimBLE peripheral): after 7, same
   courier, new framing. **M4-adjacent by definition.**
9. **Parked, unchanged:** Agregore `pubsub://`/`hyper://` backends (their
   seam slots exist, `mod.rs:133-145`), WebTransport, browser device APIs.

The single highest-leverage first step is 1→2→3 as one arc: it attacks the
only latency problem users feel today, reuses a deployed server for its
original purpose, adds zero new infrastructure, and leaves every layer
above `src/net` untouched.

---

## 9. Risks

1. **The unstable-API bet (Option A).** `unstable-custom-transports` can
   reshape under any iroh release. Contained by the exact pin
   (`Cargo.toml:153`) and by the spike-first ordering; the standing exit
   is Option B on the walkie-owned seam. Watch upstream: if iroh grows a
   first-party browser WebRTC transport, ours retires into it (§6).
2. **wasm `Send` shimming is unproven in anger.** The channel-pump pattern
   is precedented in this repo (`rendezvous.rs:473-605`) but nobody has
   run iroh's custom-transport plumbing on wasm; that is why step 1 exists
   and is allowed to fail loudly.
3. **ICE failure residue.** STUN-only WebRTC strands the hardest NAT
   pairs on the relay forever. That is the *designed* outcome (§6), but
   the UI must keep reporting it honestly (`PeerPath::Relayed`), or users
   will file "direct mode is broken" against working fallback.
4. **Signaling channel abuse.** `walkie-rtc` messages are attacker-
   costless on a known topic hash, like hellos before them; the same caps
   apply (`MAX_RENDEZVOUS_PEERS`, `rendezvous.rs:48-53`) plus
   offer-rate limiting per remote id, and Option A's QUIC handshake bounds
   the damage to failed dials. Signed signaling payloads close the
   Option-B privacy gap.
5. **Battery/socket cost of many datachannels on mobile browsers.**
   Bounded by gossip's active view rather than room size (§4.2), but
   unmeasured; the step-3 acceptance run should record it (the
   perf-suite's "track, don't gate" posture,
   `performance-benchmark-suite.md` §7.1).
6. **Two transports' worth of diagnostics.** Path flapping
   (relay↔webrtc) is new observable behavior; without surfacing path
   changes as `Diagnostic`/UI events, debugging reports will conflate
   transport churn with sync bugs — the same observability lesson the
   dial-failure postmortem already taught
   (`peer-discovery-design.md` §1.4).
7. **Scope creep toward the composite.** The cross-backend
   `MultiTransport` (§5) has no user until a second live backend exists.
   Building it "while we're in there" before Option B or an Agregore mode
   ships would be the exact n=1-abstraction trap the crate plan warns
   about (`tutti-crate-architecture.md` §6.1); park it in the tutti-net
   API sketch only.
