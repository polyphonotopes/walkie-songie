# Tasks — Tauri-native Iroh and musical correctness

Implementation begins only after proposal approval. Preserve the existing dirty
worktree and complete tasks in order.

## 0. Reconcile the interrupted work

- [x] 0.1 Record the current diff and the 73-test baseline; identify completed
  RoomStore, identity, HHHS, and L0 files that must be retained.
- [x] 0.2 Mark the overlapping browser transport portions of
  the archived `rewrite-p2panda-hhhs-stack`, `add-voice-input-mode`, and
  `add-channel-ui-and-midi` as superseded by this change.
- [x] 0.3 Remove no legacy code until its Tauri replacement passes the relevant
  acceptance tests.

## 1. Reproducible current toolchain

- [x] 1.1 Add stable Rust 1.97.1 toolchain components/targets and move workspace
  packages to Rust 2024 edition.
- [x] 1.2 Pin Tauri 2.11.5, Tauri CLI 2.11.4, tauri-build 2.6.3, stable Trunk
  0.21.14, Iroh 1.0.3, iroh-gossip 0.101.0, and
  iroh-mdns-address-lookup 0.4.0; regenerate and audit the lockfile.
- [x] 1.3 Define explicit `core`, `web-ui`, `native-net`, `native-midi`,
  `client-adapter`, `tauri-app`, `plugin`, and `test-support` feature
  boundaries.
- [x] 1.4 Remove wasm Iroh/clang configuration and all libp2p resolution edges.

## 2. Correct tuning and pitch domain

- [x] 2.1 Replace the current SCL parser with a spec-conformant parser: exact
  count, integer ratios, suffix tolerance, finite positive ratios, explicit
  period, resource bounds, and structured errors.
- [x] 2.2 Add optional KBM parsing plus a documented deterministic default
  mapping; define versioned canonical bytes and `TuningId`.
- [x] 2.3 Introduce validated `ScaleDegree` and `PeriodicPitch` types with
  checked constructors and serialization.
- [x] 2.4 Rewrite quantization around the actual period and adjacent-period
  candidates; return exact center frequency and un-clamped deviation.
- [ ] 2.5 Add official Scala fixtures, boundary/wrap tests, non-octave tests,
  large-scale bounds, round-trip properties, and independent frequency oracles.
- [x] 2.6 Gate 12-TET note names, mode, and solfège logic to compatible mappings;
  provide tuning-aware degree labels elsewhere.

## 3. Operation schema v3 and musical projection

- [x] 3.1 Replace `(pc, of)` and raw pitch fields in durable ops with
  `TuningId` + validated degree/periodic pitch; remove durable
  `SetVoice`/`ClearVoice`.
- [x] 3.2 Reject invalid tuning IDs, degrees, periodic pitches, oversized
  strings, and incoherent piece moves before RoomStore ingestion.
- [x] 3.3 Project only the winning tuning's contributions and emit balanced
  removals on tuning change without deleting signed history.
- [x] 3.4 Preserve per-author add-wins semantics and owner-gated pieces; extend
  the independent oracle and mutation tests to all new invariants.
- [x] 3.5 Define signed, sequenced, leased voice-presence frames outside the
  durable DAG; expire them on release, timeout, disconnect, and session change.
- [ ] 3.6 Bump wire/storage generations and add golden native/wasm vectors.

## 4. Native Iroh runtime

- [x] 4.1 Upgrade the shared Ed25519 identity module to persistent Tauri
  app-data storage with restrictive file permissions and atomic creation.
- [x] 4.2 Build one Iroh endpoint with the production custom RelayMap and
  explicit N0 development fallback.
- [x] 4.3 Add room-scoped `MdnsAddressLookup`, discovery/expiry handling, and an
  offline same-LAN join flow.
- [x] 4.4 Implement versioned room tickets, bootstrap connection, peer
  membership, and reconnect behavior.
- [x] 4.5 Wire iroh-gossip live operations/presence and `walkie/rbsr/1` H6 repair
  with verification and resource budgets.
- [x] 4.6 Expose per-peer direct/relay path status from Iroh remote information,
  plus discovery, relay, RTT, sync, and protocol diagnostics.
- [x] 4.7 Persist verbatim signed operations and metadata in a crash-safe Tauri
  app-data journal; recover deterministically.

## 5. Tauri application and UI bridge

- [x] 5.1 Scaffold `src-tauri/` as a workspace member and configure Trunk
  `beforeDevCommand`/`beforeBuildCommand`, `frontendDist`, and
  `withGlobalTauri`.
- [x] 5.2 Add managed `AppRuntime`, typed async commands, one ordered
  `Channel<AppEvent>`, cancellation, shutdown, and structured errors.
- [x] 5.3 Rewire local UI actions to backend commands and RoomStore/path/presence
  updates to channel events; remove browser transport ownership.
- [x] 5.4 Keep WebAudio/SwiftF0 processing local to the frontend, coalesce live
  preview frames, and implement press/hold/release durable toggle semantics.
- [x] 5.5 Rewire room/ticket UI and show mDNS discovery, direct/relay state,
  expiry, reconnect, and actionable failures.
- [x] 5.6 Add minimal Tauri capabilities/CSP, app icons/metadata, clean shutdown,
  and Linux/macOS/Windows packaging configuration.

## 6. Correct native MIDI

- [x] 6.1 Add native port discovery/hot-plug via midir 0.11 and typed messages
  via wmidi 4.0.11 behind the backend boundary.
- [x] 6.2 Implement a source-keyed sounding-note ledger with reference counts so
  peers, previews, pieces, and toggles remain polyphonic and balanced.
- [x] 6.3 Preserve absolute 12-TET pitch; convert MIDI input to frequency before
  tuning quantization.
- [ ] 6.4 Add MPE-compatible per-note channel/pitch-bend allocation for
  non-12-TET output, bend reset, deterministic exhaustion behavior, and an
  explicit opt-in approximation mode.
- [x] 6.5 Send balanced note-offs/all-notes-off on source removal, voice expiry,
  tuning/room/device change, disconnect, panic boundary, and application exit.
- [ ] 6.6 Add a fake MIDI backend and property tests for duplicate notes,
  same-note multi-peer voices, device churn, channel exhaustion, and stuck-note
  prevention.

## 7. Verification and cutover

- [ ] 7.1 Keep all existing native/L0 tests green and add wasm domain golden
  vectors.
- [x] 7.2 Add two native endpoints on one LAN: discover only the matching room
  via mDNS and connect directly with Internet/relay disabled.
- [ ] 7.3 Add a punchable two-NAT fixture: bootstrap through a relay, assert the
  active path upgrades to direct IP, exchange ops, reconcile, and converge.
- [ ] 7.4 Add a non-punchable fixture: remain relayed, exchange ops, reconcile,
  and converge without claiming a direct path.
- [ ] 7.5 Test packet loss, duplication, reorder, late join, app restart,
  mDNS expiry, relay interruption, network migration, and stale voice expiry.
- [ ] 7.6 Build Tauri debug/release packages in Linux, macOS, and Windows CI and
  run the stable Trunk wasm build.
- [ ] 7.7 Perform a two-device jam acceptance test covering LAN mDNS, WAN ticket,
  visible path state, tuning change, live voice expiry, MIDI polyphony, and
  clean shutdown.
- [ ] 7.8 Only after all checks pass, remove yrs, browser transport code,
  standalone libp2p relay, obsolete wasm artifacts/configuration, and update all
  checkboxes.

## 8. Optional Agregore and Peersky adapter (post-Tauri)

- [x] 8.1 Keep Tauri-independent `ClientCommand`, `AppEvent`, snapshot, error,
  and capability types in the core crate and test codec compatibility.
- [ ] 8.2 Implement an opt-in headless bridge bound only to loopback with a
  per-launch token, origin checks where available, command limits, and no
  private-key export.
- [ ] 8.3 Build one Manifest V3 extension package that connects the existing
  frontend to the bridge and declares only the required loopback permissions.
- [ ] 8.4 Smoke-test the same extension in current Agregore and current Peersky,
  including reconnect, capability negotiation, mic access, and clean bridge
  shutdown.
- [ ] 8.5 Compare the bridge with first-party Iroh Node bindings inside each
  Electron main process; pursue browser-specific native protocol patches only
  if both projects expose or accept a maintainable hook.
