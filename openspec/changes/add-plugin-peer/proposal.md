# Change: Add nih-plug VST3/CLAP Plugin Peer

## Why

DAW users want to participate in walkie-songie sessions directly from their music production environment. A VST3/CLAP plugin would let them connect to the same channels as mobile/web users, enabling collaborative music creation between phones (voice input) and DAWs (MIDI/instrument input).

## What Changes

- Add new `plugin` feature flag in Cargo.toml for conditional compilation
- Implement nih-plug Plugin trait with basic audio passthrough
- Create dedicated networking thread separate from audio thread (critical for realtime safety)
- Build minimal plugin UI with:
  - QR code display for mobile users to scan and join
  - Shuffle button to generate new random channel
  - Text input for custom channel address
- Persist channel address in plugin state via nih-plug's `#[persist]` system
- Reuse existing `net::` signalling infrastructure for P2P connectivity

## Impact

- Affected specs: None existing (new capability)
- New spec: `plugin-peer`
- Affected code:
  - `Cargo.toml` (new feature flag + nih-plug dependencies)
  - New `src/plugin/` module tree
- Dependencies added: `nih_plug`, `nih_plug_egui` (or similar), `qrcode` crate
