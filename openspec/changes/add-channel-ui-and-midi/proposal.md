# Proposal: Add Channel UI and Web MIDI

> **Status (2026-07-30): desktop implementation superseded by
> `pivot-to-tauri-iroh`.** Preserve the room and performance UX goals, but route
> networking and MIDI through the Tauri backend. Browser MIDI remains optional
> adapter behavior rather than the primary desktop path.

## Summary
Add a room/channel management UI for sharing and joining collaborative sessions, plus Web MIDI I/O for integrating with external instruments and DAWs.

## Motivation
Currently there's no user-facing way to create or join channels - the P2P infrastructure exists but has no UI. Musicians need an easy way to:
1. Create a room and share it (QR code, short wholesome word code)
2. Join existing rooms by scanning QR or typing the code
3. Send room pitches to external synths/DAWs via MIDI
4. Receive MIDI input from controllers

## Scope

### Channel UI
- **Always in a room**: First visit auto-generates a random room, no "roomless" state
- **Room name always visible**: Displayed in header, clickable to open menu
- **Room menu overlay**: Shows QR code, room name, shuffle button (new random room), text input to join specific room
- Short codes use a wholesome word list (e.g., `sunny-garden-melody`)
- No privileged host - anyone can create or join any room

### Web MIDI I/O
Two distinct note streams mapped to MIDI channels:
- **MIDI Channel 1 - Shared Toggle Set**: Collaborative CRDT where anyone can toggle notes on/off. Manual keyboard presses go here.
- **MIDI Channel 2 - Voice Pitches**: Single-writer per peer, combined but no one can silence another's voice. Carries actual Hz (for pitch bend), displays as pitch class on keyboard.

MIDI input from controllers feeds into the shared toggle set (channel 1).

## Out of Scope
- Per-peer MIDI channel routing (future)
- MIDI clock/sync
- MPE support
- Room persistence/history
