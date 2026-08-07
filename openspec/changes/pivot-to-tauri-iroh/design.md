## Context

walkie-songie currently has a Rust/WASM dominator UI, browser WebAudio/Web MIDI
integration, legacy yrs state, and partially completed p2panda/HHHS/Iroh rewrite
work. The completed RoomStore and L0 suites already establish deterministic
signed-state convergence. The missing boundary is a supported runtime that can
open native UDP sockets and own long-lived network/device resources.

Current first-party facts used by this design:

- Iroh 1.0 provides QUIC multipath and NAT traversal; Iroh 1.0.3 is the latest
  maintenance release and improves path validation under real network changes.
- `iroh-mdns-address-lookup 0.4` is the current Rust mDNS implementation and can
  both resolve known endpoint IDs and stream locally discovered endpoints.
- Tauri 2 directly supports a Trunk-built Rust frontend. Its ordered channels
  are intended for high-throughput streaming; generic events are not.
- Scala SCL integers are ratios, the declared note count is exact, the final
  pitch defines the repeating period, and keyboard mapping is a separate concern.

## Goals / Non-Goals

### Goals

- Ship Linux, macOS, and Windows desktop peers from one Tauri 2 workspace.
- Use real native Iroh paths: direct UDP whenever viable, relay fallback when
  required, and mDNS discovery without Internet access.
- Preserve signed authorship, deterministic HHHS reads, and convergence.
- Make tuning, pitch, voice lifecycle, and MIDI output musically explicit and
  independently testable.
- Keep the existing UI and browser audio-analysis investment.
- Keep one narrow, capability-negotiated client contract that can later serve
  a shared Agregore/Peersky WebExtension through a native local bridge.
- Use current stable, reproducibly pinned tooling.

### Non-Goals

- Standalone browser-to-browser Iroh, libp2p, or a custom Iroh WebRTC
  transport. Agregore/Peersky adapters require the optional native bridge in
  this change.
- Tauri mobile packaging in the first milestone.
- Replacing p2panda-core, HHHS, SwiftF0, the voice conditioner, dominator, or
  the all-around-keyboard component.
- Audio synthesis, DAW plugin migration, MIDI clock, or a general-purpose
  rendezvous service.
- Automatic conversion of historical pitches between unrelated tunings.

## Decisions

### 1. Tauri is the runtime boundary, not a UI rewrite

Add `src-tauri/` as a workspace member. Trunk still builds the dominator/WASM
frontend into `dist/`, which Tauri loads with `withGlobalTauri` enabled. The
root crate remains the reusable domain/data library. Native features compile
Iroh and device I/O only into the Tauri backend; frontend features compile DOM,
WebAudio, and pitch detection only into wasm.

The backend owns one managed `AppRuntime` containing identity, endpoint,
RoomStore, room task, presence table, persistence journal, and MIDI engine.
Async Tauri commands submit typed user actions. A registered ordered
`tauri::ipc::Channel<AppEvent>` streams snapshots and deltas to the frontend.
UI-only animation data stays in the frontend.

### 2. Native Iroh is direct-first with observable fallback

Use one stable Iroh `Endpoint` per application identity. Configure the
self-hosted Iroh relay as the production home relay and allow the N0 preset as
an explicit development fallback. Iroh decides and migrates paths; the
application does not implement its own hole-punch protocol.

For each active peer, sample `Endpoint::remote_info` and classify active
`TransportAddr::Ip` paths as direct and active relay addresses as relayed. Show
that state in the UI and retain it in diagnostics so "P2P" is measurable.

A room derives an mDNS service name from a truncated, encoded room-topic hash,
not from the human room name. `MdnsAddressLookup::subscribe` adds discovered
room peers and removes expired LAN presence. Typing the same room name is
therefore sufficient on one LAN, even with no Internet. WAN joining uses a
shareable ticket containing topic, endpoint ID, and addressing information;
after one bootstrap connection, gossip discovers room membership.

### 3. Gossip is live delivery; H6 is repair

Keep the two-protocol shape from the interrupted design:

- iroh-gossip broadcasts bounded, verbatim signed durable operations and
  bounded signed presence frames.
- `walkie/rbsr/1` runs HHHS `SyncSession` reconciliation over a bidirectional
  Iroh stream.

Every durable inbound operation is topic-bound, signature-verified, schema-
validated, and domain-validated before RoomStore ingestion. Presence frames
have author, session, monotonic sequence, expiry, tuning ID, and optional
periodic pitch. They are not inserted into the durable DAG.

### 4. Durable intent and live voice are different data

Manual keyboard/MIDI input and voice release create or remove durable,
per-author pitch contributions. Their shared projection is a union with source
attribution; one author cannot remove another author's contribution.

Live voice detection is presence. It is visible and can drive MIDI while fresh,
but expires after a short lease and is cleared immediately on normal release.
This prevents a lost `ClearVoice` or crashed process from creating a permanent
musical note. The durable op schema drops `SetVoice`/`ClearVoice`; old v2 records
are not imported as live presence.

### 5. Pitches are scoped to an exact tuning context

Replace unvalidated `pc`, `of`, and loosely related absolute integers with:

- `TuningId`: BLAKE3 of a versioned canonical tuning context.
- `ScaleDegree`: a validated degree within that tuning.
- `PeriodicPitch`: signed period number plus scale degree.

The tuning context contains validated SCL steps, its explicit period/equave,
and a deterministic reference/keyboard mapping (optional KBM input; documented
default when absent). It supports up to a documented resource limit and uses a
wide core index rather than `u8`.

Quantization compares the target frequency against candidates on both sides of
the period boundary and returns the exact chosen `PeriodicPitch`, center
frequency, and un-clamped signed deviation. Conventional MIDI note names and
major-scale solfège are shown only for a compatible 12-TET mapping; other
tunings show their defined degree labels/frequencies.

When the resolved room tuning changes, the active projection includes only
contributions with the winning `TuningId`. Prior operations remain in history
but produce note-offs and are not silently reinterpreted. A future explicit
migration operation may translate them.

### 6. Native MIDI preserves identity and pitch

The Tauri backend uses current stable native MIDI crates (`midir` for ports and
`wmidi` for messages). A source ledger keys every sounding note by logical
source (author + durable op, author + voice session, or local input key), so
duplicate pitches and multiple voices do not steal or prematurely stop one
another.

12-TET uses exact note-on/off messages. Non-12-TET output uses a per-note
channel allocator with pitch bend (MPE-compatible MIDI 1.0) and resets bends
when channels are released. If a destination cannot support the configured
mode, approximation is opt-in and visibly reported; it is never silent.
Incoming MIDI notes are converted to frequency and quantized through the
current tuning, not reduced modulo the tuning length.

### 7. Stable-version policy

Check in `rust-toolchain.toml` for stable Rust 1.97.1 with rustfmt, clippy, and
the wasm target; use Rust 2024 edition. Pin the implementation baseline to the
stable versions recorded in the proposal and use the lockfile in CI and release
builds. Do not adopt beta Trunk or prerelease networking crates merely because
they are newer.

Dependency updates after this proposal require:

1. current first-party release/changelog review,
2. native and wasm compile checks,
3. the full domain/convergence suite, and
4. direct, relay, and mDNS transport smoke tests.

### 8. Agregore and Peersky use one optional local adapter

Do not couple the domain API to Tauri macros. Define serializable
`ClientCommand`, `AppEvent`, snapshot, error, and capability types in the core
crate, then adapt those types to Tauri IPC.

After Tauri is working, expose the same contract from an optional headless
native bridge on loopback only. One Manifest V3 extension package can run in
both Agregore and Peersky and connect to that bridge. The bridge uses a
per-launch unguessable token, validates Origin/extension identity where the
browser provides it, applies the same command/resource limits as Tauri, and
never exposes the Ed25519 secret key.

This route is preferred because both browsers currently load WebExtensions,
while adding a new first-class native URL protocol requires browser-specific
main-process changes. A feasibility spike may compare first-party Iroh Node
bindings inside an Electron main-process plugin, but that becomes an
implementation only if both browsers expose or accept a stable extension point.
The bridge advertises capabilities, so an adapter can clearly report when
mDNS, native MIDI, or persistence is unavailable instead of emulating them
incorrectly.

## Risks / Trade-offs

- **Desktop-first delays browser reach.** Real Iroh networking lands in Tauri
  first. A later Agregore/Peersky adapter still requires a native bridge; the
  unbridged PWA remains a non-peer demo.
- **Tauri IPC can add latency.** Keep audio analysis and visual interpolation in
  WASM, coalesce preview updates, and use ordered channels only for meaningful
  state changes.
- **mDNS can be blocked by OS/firewall policy.** Surface discovery health and
  keep explicit tickets as a deterministic fallback.
- **Hole punching cannot succeed through every NAT.** Require direct upgrade in
  a punchable-NAT fixture and reliable relay fallback everywhere else; never
  market a relayed path as direct.
- **The op schema breaks existing development state.** Export if needed, then
  reset; do not infer a `TuningId` from `(pc, of)`.
- **MPE channel capacity is finite.** Define deterministic voice stealing,
  expose exhaustion, and balance all released/stolen notes.
- **The current worktree is partially migrated.** Preserve completed RoomStore,
  identity, HHHS, and tests; remove only code explicitly superseded by this
  design.
- **A loopback bridge adds a local attack surface.** Bind only to loopback, use
  per-launch authentication, validate origins when possible, minimize
  capabilities, rate-limit commands, and never send secret key material.

## Migration Plan

1. Record the current dirty-worktree baseline and keep the 73 passing native/L0
   tests green.
2. Mark the overlapping transport portions of older pending changes as
   superseded by this change; do not reset their completed source work.
3. Land and test the corrected tuning/pitch types before changing the UI.
4. Introduce the Tauri shell and IPC with an in-process fake backend.
5. Add native Iroh, mDNS, relay/ticket bootstrap, gossip, and H6 sync.
6. Rewire the UI, voice presence, persistence, and native MIDI.
7. Remove browser networking, yrs, libp2p, wasm Iroh, and obsolete relay code
   only after the Tauri path passes end-to-end tests.
8. Reset incompatible development journals and ship desktop packages.
9. Run the shared Agregore/Peersky extension + loopback bridge spike without
   delaying the desktop release.

Rollback before release is to keep the last green desktop build and restore the
previous UI-only PWA artifact; v2 and v3 peers are intentionally wire-isolated.

## Open Questions

- Whether Tauri mobile packaging should be the next milestone or remain a
  separate proposal.
- Whether the first production release should permit the N0 relay fallback or
  require `relay.wondering.xyz` exclusively.
- Whether full KBM editing belongs in the first UI or only file import plus a
  documented default mapping.
- Whether Agregore and Peersky want to upstream a stable native protocol/plugin
  hook after the shared bridge proves the client contract.
