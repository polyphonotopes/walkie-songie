# Change: Add a nice-plug Tutti bridge peer

## Why

Musicians need one host that can join Walkie-Songie rooms over Iroh while also
connecting nearby Tutti ESP32 instruments without moving Iroh, QUIC, or desktop
network policy onto the audio thread or making direct Iroh-over-BLE a product
prerequisite.

The previous plugin proposal targeted a retired nih-plug/Yrs stack. The live
application now uses Room-v5 HHHS replicas and Iroh, and the maintained local
plugin examples use nice-plug. This change replaces that obsolete integration
plan rather than adding another plugin architecture beside it.

## What Changes

- Add a runtime-independent `BridgeCore` which owns one Walkie Room-v5 peer,
  Iroh carrier supervision, and zero or more local Tutti links.
- Build the desktop host with `nice-plug`: CLAP plugin first, with an optional
  standalone executable using the same plugin and bridge core.
- Keep every network, BLE, persistence, and HHHS operation off the audio
  callback. The callback exchanges bounded realtime intents and MIDI events
  with the bridge through non-blocking queues.
- Add a desktop BLE host adapter for the shared `tutti-ble` GATT service. The
  adapter uses application-authenticated, boot-bound sessions without requiring
  OS pairing or bonding.
- Multiplex a compact realtime musical lane and byte-exact HHHS repair lane over
  one BLE connection. Realtime effects remain provisional until the ordinary
  Room-v5 music Replica confirms or corrects them.
- Let one connected ESP32 act as a BLE gateway while its existing ESP-NOW mesh
  carries state to sibling boards. Direct Iroh-over-BLE remains an independent
  experiment, not a dependency of this change.
- Keep the bridge core suitable for a later smartphone shell; this change does
  not choose or implement the mobile UI toolkit.

## Impact

- Affected specs: `plugin-peer` (replaced delta), new `tutti-bridge`
- Affected code: `Cargo.toml`, `xtask`, new bridge/plugin modules, native
  carrier composition, MIDI queues, and focused integration tests
- External prerequisite: a reviewed/tagged Tutti release containing the shared
  BLE framing/session crate and the same music protocol generation used by the
  ESP32 firmware
- Dependencies: `nice-plug`, optional `nice-plug-egui`, optional
  `nice-plug/standalone`, and a desktop BLE backend selected behind a host trait
- No change to HHHS canonical identity, causal semantics, admission rules, or
  repair messages
