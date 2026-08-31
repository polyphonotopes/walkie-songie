# Tasks: Add a nice-plug Tutti bridge peer

## 1. Shared protocol prerequisite

- [x] 1.1 Review frozen Tutti BLE hello, fragment, lane, and handshake vectors.
- [x] 1.2 Align Walkie and ESP on one tagged Tutti music protocol generation and
  HHHS dependency line; refuse mismatches rather than translating.
- [ ] 1.3 Add carrier-equivalence fixtures proving Iroh and BLE repair produce
  identical canonical records and history roots.

## 2. Bridge core

- [x] 2.1 Add runtime-independent bridge commands, events, status snapshots,
  and explicit bounded queue policies.
- [ ] 2.2 Own native Iroh/Room-v5 and local Tutti link supervision on a dedicated
  background runtime.
- [ ] 2.3 Route compact realtime messages separately from durable HHHS repair
  and correlate provisional intents with confirmation/correction.
- [ ] 2.4 Add in-memory link tests for reconnect, duplicate delivery,
  partition/rejoin, saturation, refusal, and cancellation.
- [ ] 2.5 Add one shared causal pitch/pitch-class set which any authorized peer
  may edit, including observed cross-peer removal and concurrent add-wins tests.
- [ ] 2.6 Adapt host MIDI, board web/MIDI inputs, and browser controls to edit
  that shared set and add state-derived offs-before-ons output reconciliation.

## 3. nice-plug host

- [x] 3.1 Replace the obsolete nih-plug xtask/dependency plan with nice-plug
  using the maintained Polyphonotopes patterns.
- [x] 3.2 Implement a CLAP MIDI-effect shell over bounded non-blocking queues.
- [x] 3.3 Add a compact editor for room, peer, board, trust, link, and repair
  status without exposing transport state as musical authority.
- [x] 3.4 Persist room selection and trusted board identities outside the
  realtime callback.
- [x] 3.5 Add the optional nice-plug standalone executable using the same plugin
  and bridge core.

## 4. Desktop BLE adapter

- [x] 4.1 Define the platform BLE host trait and in-memory conformance adapter.
- [ ] 4.2 Select and feature-gate a desktop backend after lifecycle and resource
  probes on supported operating systems.
- [ ] 4.3 Implement scan, connect, GATT subscribe/write, reconnect, TOFU, and
  bounded lane routing.
- [x] 4.4 Expose permissions and connection failures as observable bridge
  events without blocking audio.

## 5. ESP32 gateway probe

- [x] 5.1 Add a feature-gated GATT-server probe sharing the existing Bluedroid
  driver with the BLE-MIDI central; preserve the default path and flash the
  probe only after explicit approval.
- [ ] 5.2 Measure dual-role idle/active/repair heap, PSRAM, task stacks, flash,
  connection latency, and MIDI coexistence.
- [ ] 5.3 Connect the authenticated realtime and HHHS lanes to the existing leaf
  session/Replica seams.
- [ ] 5.4 Verify one BLE gateway fans durable and realtime state to sibling
  boards through existing ESP-NOW behavior.

## 6. Acceptance

- [ ] 6.1 Prove the plugin audio callback performs no I/O, waits, locks, heap
  allocation, HHHS operations, or cryptography.
- [ ] 6.2 Load and save the CLAP plugin in at least one production host; verify
  multiple instances and clean shutdown.
- [ ] 6.3 Verify optional standalone startup, MIDI routing, reconnect, and state
  persistence.
- [ ] 6.4 Run two-board partition/edit/rejoin convergence with identical roots
  and no history growth from high-rate controls.
- [ ] 6.5 Record p50/p95 realtime feedback and separate durable confirmation
  latency, plus ten-minute memory watermarks.
