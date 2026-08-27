# Change: Pivot to Tauri-native Iroh and correct musical semantics

## Why

The now-archived `rewrite-p2panda-hhhs-stack` work proved the signed p2panda
operation log, HHHS read model, shared Ed25519 identity, and deterministic
anti-entropy model, but its browser transport direction cannot provide the
native UDP paths needed for Iroh's real NAT traversal. Wrapping the existing
Rust/WASM UI in Tauri lets the application keep its current interface while a
native Rust backend owns Iroh QUIC, hole punching, relay fallback, and mDNS LAN
discovery.

The same review found domain errors that network convergence tests cannot
detect: wrapped quantization can report the wrong octave and center frequency;
the SCL parser does not follow Scala's integer-ratio and note-count rules;
pitch operations identify a tuning only by its note count; durable voice state
can survive a crashed peer; and the current MIDI path collapses polyphonic
voices and silently maps microtonal pitches to incorrect 12-TET notes. The
transport pivot is the right boundary at which to make these semantics explicit
and testable.

## What Changes

- **BREAKING (supported client):** make a Tauri 2 desktop application the
  primary network-capable client. Keep the existing dominator/Trunk Rust-WASM
  UI as the Tauri frontend, but move networking, persistence, and MIDI device
  I/O into the native backend.
- **BREAKING (browser transport):** stop making raw-Iroh browser networking,
  libp2p, or a custom WebRTC transport part of the critical path. Agregore and
  Peersky become post-Tauri adapter targets: a shared WebExtension may use an
  authenticated local bridge backed by the same native Iroh runtime. A hosted
  PWA without that bridge remains a UI/demo build, not a room peer.
- Run stable native `iroh 1.0.3` with `iroh-gossip 0.101`, a configurable
  self-hosted Iroh relay (`relay.wondering.xyz`), direct-path upgrade/hole
  punching, and `iroh-mdns-address-lookup 0.4` for room-scoped LAN discovery.
- Preserve the completed p2panda-core + HHHS signed operation log, RoomStore,
  H6 reconciliation, shared author/endpoint identity, and L0 convergence work.
- Add a Tauri command/channel boundary: commands carry user intent into the
  backend; ordered channels carry snapshots, deltas, peer/path status, and
  errors back to the UI.
- Define a transport-neutral client adapter contract while building the Tauri
  boundary. After the desktop path is green, spike one shared Manifest V3
  extension plus loopback bridge for current Agregore and Peersky before
  considering browser-specific native protocol patches.
- **BREAKING (operation schema):** replace `(pc, of)` pitch fields with validated
  tuning-scoped pitch types. A deterministic `TuningId` binds every pitch to
  the exact parsed scale and reference mapping under which it was authored.
- Correct Scala parsing and periodic quantization, including integer ratios,
  exact pitch-line counts, non-octave periods, wraparound octave/equave
  selection, and optional keyboard mapping.
- Separate durable, user-latched pitch contributions from ephemeral live voice
  previews. Voice previews expire on release, timeout, disconnect, or crash;
  release can toggle a durable per-author pitch contribution.
- Replace Web MIDI as the desktop authority with native MIDI I/O. Track notes
  by source/peer, preserve polyphony and note-off balance, and use explicit
  microtonal output rather than silently folding arbitrary tunings into 12-TET.
- Pin a reproducible stable toolchain and current stable dependencies. As
  verified on 2026-07-30: Rust 1.97.1, Tauri 2.11.5, Tauri CLI 2.11.4,
  tauri-build 2.6.3, Trunk 0.21.14 (not the 0.22 beta), Iroh 1.0.3,
  iroh-gossip 0.101.0, and iroh-mdns-address-lookup 0.4.0.

## Impact

- **Supersedes:** the raw browser-Iroh, WebRTC, and wasm-Iroh portions of
  archived `rewrite-p2panda-hhhs-stack` plan. Agregore and Peersky are reprioritized behind
  the Tauri milestone and use the common adapter seam if the spike succeeds.
  Completed data-layer and test work remains the implementation foundation.
- **Consolidates:** the desktop-relevant portions of `add-voice-input-mode` and
  `add-channel-ui-and-midi`. `add-plugin-peer` remains a separate future
  client; the Tauri app lands first.
- **Affected specs:** adds `desktop-run`, `peer-connect`, `music-model`,
  `midi-route`, and `browser-adapt`. Existing `voice-input` and
  `pitch-keyboard-ui` behavior is preserved where it does not conflict with the
  new tuning and preview rules.
- **Affected code:**
  - new `src-tauri/` Tauri application and IPC boundary
  - `src/net/**` native Iroh endpoint, mDNS, gossip, sync, tickets, diagnostics
  - `src/room/{ops,store}.rs` operation schema v3 and tuning-scoped reads
  - `src/tuning/**` Scala/KBM parsing and periodic quantization
  - `src/web/**` RoomStore/IPC rewire; no transport ownership
  - new native MIDI module; Web MIDI becomes optional demo fallback
  - optional `adapter/` loopback bridge and shared WebExtension after Tauri
  - `Cargo.toml`, `Cargo.lock`, `Trunk.toml`, checked-in Rust toolchain, CI
- **Migration:** development-only v2 room journals are not wire-compatible with
  v3 and will be discarded or explicitly exported before cutover. No silent
  reinterpretation is allowed.

## Approval

This is a breaking architecture and musical-data-model change. Per
`openspec/AGENTS.md`, implementation must not start until this proposal is
reviewed and approved.
