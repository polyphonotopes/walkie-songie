# Design: nice-plug Tutti bridge peer

## Context

Walkie-Songie already owns native Iroh endpoints and Room-v5 HHHS replicas.
Tutti ESP32 firmware already owns an authenticated session handshake, compact
round-table messages, HHHS repair over ESP-NOW, and a BLE-MIDI central. The
missing path is an application-owned BLE GATT link from a phone or desktop host
to one gateway board.

Audio hosts impose a stricter boundary than ordinary desktop applications: the
process callback cannot perform network I/O, block, allocate, wait on a mutex,
or run HHHS admission/materialization. The bridge must therefore be an owned
background service rather than plugin state polled from the audio callback.

## Goals / Non-Goals

### Goals

- One shared bridge core for CLAP and standalone hosts.
- Native Iroh on the host and bounded authenticated BLE GATT on the ESP edge.
- No OS pairing ceremony in the default jam flow.
- The same canonical Tutti music records and history roots on browser, host,
  and ESP replicas.
- Compact low-latency performance feedback followed by durable HHHS
  confirmation or correction.
- Explicit bounds on queues, BLE messages, repair frames, retries, and retained
  peers.

### Non-Goals

- Running Iroh over BLE on the ESP32.
- Translating canonical records between protocol generations.
- A second causal model or embedded-only HHHS wire format.
- Performing networking, BLE, cryptography, HHHS work, or heap allocation in
  the audio callback.
- Shipping a smartphone UI in this change.

## Decisions

### One bridge core, several shells

`BridgeCore` owns the room and local-device state machine. `nice-plug`,
standalone, and future mobile shells send commands to it and subscribe to
bounded snapshots/events. Shells do not construct Iroh endpoints, BLE sessions,
or HHHS repair drivers themselves.

The nice-plug implementation follows the maintained Polyphonotopes examples:
`nice-plug` 0.2.x, `nice-plug-egui` when an editor is enabled, a dedicated
background runtime, `nice_export_clap!`, and
`nice_export_standalone::<Plugin>()` behind the `standalone` feature. VST3 can
be exported from the same implementation but is not a release gate for the
first bridge slice.

### Two carrier legs, no translation

```text
Walkie/HHHS peer <- hhhs-iroh -> BridgeCore replica
                                      |
                                hhhs-sync frames
                                      |
                              tutti-ble repair lane
                                      |
                               gateway ESP replica
                                      |
                                   ESP-NOW
```

The bridge terminates an Iroh connection and a BLE connection, but not the
canonical data model. It stores and serves the same Replica records on both
legs. The music protocol generation, namespace, authority profile, and
canonical encodings must match before the BLE repair lane opens.

### BLE link ownership

The platform adapter owns scanning, permissions, connection lifecycle, GATT
callbacks, and characteristic writes/notifications. The shared `tutti-ble`
crate owns UUIDs, boot hello bytes, fragmentation, authenticated lane framing,
and the HHHS `FrameStream` adapter.

The provisional desktop implementation is `btleplug` 0.12 behind the
`desktop-ble` feature. It owns a private one-worker Tokio runtime and exposes
only bounded non-blocking commands/events to `BridgeCore`. Linux additionally
requires the D-Bus development library for BlueZ. This selection remains
provisional until connect/disconnect, permission, sleep/wake, and multi-instance
lifecycle probes pass on Linux, macOS, and Windows.

The GATT service exposes:

- host-to-ESP write/write-without-response characteristic;
- ESP-to-host notify characteristic;
- read/notify information characteristic for bootstrap and status.

The link pump is the only owner of the session codec and reassembler. It routes
authenticated payloads into bounded per-lane queues:

- lane 0: link control and liveness;
- lane 1: compact realtime musical/session messages;
- lane 2: byte-exact `hhhs-sync` frames.

### Trust and Bluetooth pairing

Default jam mode uses application-level trust-on-first-use. A fresh boot hello
contains the persistent peer identity, boot nonce, limits, and capabilities;
the existing Tutti signed ephemeral handshake binds both endpoint identities
and boot nonces. The host shows the first-seen board identity once and retains
the decision. Subsequent frames use directional keyed authentication and replay
windows.

This default provides peer authentication and integrity but not
confidentiality. The bridge must not transmit secrets over an unpaired link.
A later locked-room mode may add application encryption or request BLE link
encryption without changing HHHS records.

### Realtime and durable lanes

The realtime lane carries short-lived note, pitch, tempo, gesture, roster, and
round-table messages. It never becomes durable merely because it was received.
An intent which changes durable musical meaning names or produces the ordinary
Tutti music command that later confirms it. A projected Room-v5 revision wins
over provisional feedback when they disagree.

### Pitch-set chatroom model

The room owns one shared add-wins observed-remove pitch set. Any authorized
peer may add or remove any tuning-scoped pitch class or absolute periodic
pitch; pitches are not owned by the actor which added them. A removal clears
every matching add in its causal past, regardless of author. A truly concurrent
add survives, and a later removal which observes it clears it.

Local producers such as an ESP web UI, its BLE keyboard, a plugin host, and a
browser keyboard are adapters editing that same room state. They may turn one
another's pitches on or off. Peer departure and reidentification therefore do
not leave an unreachable owner contribution behind, and no presence timeout is
used to mutate durable musical meaning.

MIDI, AMY, browser, and ESP renderers keep an endpoint shadow and reconcile it
against the latest materialized shared set. Every transition emits retractions before
additions; reconnect uses current state rather than replaying missed edges.
High-rate previews remain session messages, while shared-set adds and removes
are ordinary canonical music commands.

### Intent, canonical state, and effect ownership

The I/O boundary is deliberately three-stage rather than an event loop:

```text
input adapter -> proposed intent -> canonical commit/materialization -> effect projector
```

An input adapter may keep a small optimistic shadow so a toggle or gate gesture
can be interpreted while a command is in flight, but that shadow never drives an
effect. Only a confirmed materialization changes the durable pitch-set output.
Canonical commands are idempotent add/remove intents; `toggle` is an interaction
mode resolved at the adapter and is not a network operation.

Each effect endpoint owns a generation-tagged applied-state shadow. It reconciles
that shadow to the newest desired level, with retractions before additions, and
coalesces intermediate desired states rather than accumulating an unbounded edge
log. A room change, carrier loss, output disable, process reset, or endpoint
replacement first retracts everything owned by the old generation. Late results
from an old generation are observable but cannot mutate the current shadow.

Host-generated membership output carries a stable origin and a bounded one-shot
fingerprint for DAWs which strip note IDs. Routed output is therefore not
reinterpreted as a new authoring gesture. Deliberate host input is still allowed,
but it enters through the selected toggle/gate/perform adapter exactly once.

The ephemeral perform lane has separate voice ownership: note-on allocates a
session/voice-scoped effect and note-off, choke, disconnect, or lease expiry
retracts that exact voice. It may provide low-latency preview, but it never
silently edits the durable shared set and canonical confirmation is never inferred
from hearing the preview.

The HHHS repair lane runs with embedded-specific frame and session budgets.
BLE fragmentation is below HHHS framing and therefore cannot affect entry
identity or roots.

### Audio-thread boundary

The process callback may only:

- drain a bounded non-blocking queue of already-decoded MIDI/output events;
- enqueue bounded fixed-size input intents without waiting;
- update audio/MIDI state held exclusively by the callback.

Queue saturation is observable and uses an explicit coalescing/drop policy for
ephemeral events. Durable commands are never reported committed until the
background replica confirms them.

## Risks / Trade-offs

- The ESP currently operates as a BLE-MIDI central. Adding a GATT peripheral
  requires a measured Bluedroid dual-role probe before enabling both roles in
  the preserved firmware image.
- A native desktop BLE library may behave differently across Linux, macOS, and
  Windows. The platform trait and an in-memory conformance adapter precede the
  concrete backend.
- The shared Tutti/HHHS generations in the current local ESP experiment and
  released Walkie dependency are not yet identical. The bridge must refuse the
  mismatch; it must not translate histories.
- No-pairing jam mode exposes frame contents to nearby observers even though
  injection and replay are rejected. The UI must describe that accurately.
- Plugin hosts may instantiate several copies. A process-level bridge registry
  may share heavyweight network/BLE services, but each plugin instance keeps
  independent room routing and bounded audio queues.

## Migration Plan

1. Land and tag the shared Tutti BLE/session wire crate with frozen vectors.
2. Add an in-memory `BridgeCore` and carrier-equivalence test in Walkie.
3. Port the plugin shell to nice-plug and prove audio-thread isolation without
   enabling hardware BLE.
4. Add a desktop BLE adapter and test against a host-side simulated GATT peer.
5. Add the ESP GATT-server/dual-role probe and record flash/RAM/latency impact.
6. Enable one physical gateway board, then partition/rejoin and realtime/durable
   confirmation tests.
7. Export the optional nice-plug standalone binary.

Rollback removes the BLE adapter and plugin features while preserving ordinary
Room-v5/Iroh and ESP-NOW operation; no canonical history migration is needed.

## Open Questions

- Does the provisional `btleplug` backend pass the required
  Linux/macOS/Windows lifecycle tests with acceptable binary and runtime cost?
- Should locked-room confidentiality use an application AEAD lane or opt into
  platform BLE link encryption first?
- Does the ESP32 dual-role Bluedroid configuration retain enough internal heap
  while AMY, Wi-Fi, HTTP, ESP-NOW, and HHHS repair are active?
