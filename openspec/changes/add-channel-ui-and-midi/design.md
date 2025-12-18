# Design: Channel UI and Web MIDI

## Architectural Decisions

### AD-1: Wholesome Word List for Room Codes
**Decision**: Use a curated wholesome word list (adjective-noun-noun pattern) for room codes.
**Rationale**: Short codes are easier to share verbally in a noisy bar. Wholesome words avoid awkward/offensive combinations. Three words gives ~10^9 combinations with modest word lists.
**Example**: `sunny-garden-melody`, `happy-river-song`, `gentle-forest-drum`

### AD-2: Room Names as Gossip Topic Seeds
**Decision**: Room name string is hashed to derive the iroh-gossip topic ID.
**Rationale**: Deterministic mapping means anyone typing the same room name joins the same channel. No central registry needed.
**Trade-off**: Possible collisions with short names, but wholesome word combos are long enough.

### AD-3: Two-Channel MIDI Model
**Decision**: Map the two note streams to MIDI channels 1 and 2.
- Channel 1: Shared toggle set (collaborative, pitch classes)
- Channel 2: Voice pitches (per-peer, combined read-only, full MIDI notes with octave)

**Rationale**: Separating streams lets DAWs route them to different instruments. Toggle set is "keys", voice is "lead". Voice preserves octave information for melodic context.

### AD-4: MIDI Input Routes to Toggle Set
**Decision**: External MIDI controller input feeds into the shared toggle set (channel 1), not voice.
**Rationale**: Voice channel is for sung pitches only. Controllers are like clicking keys.

### AD-5: Always In A Room
**Decision**: Users are always in a room. First visit auto-generates a random room name.
**Rationale**: No "create vs join" decision needed. Simpler mental model. Room name always visible so you know where you are. Easy to share or switch.

### AD-6: Overlay for Room Menu
**Decision**: Room menu appears as an overlay (not a route change), dismissible by clicking outside or pressing escape.
**Rationale**: Quick access without losing current state. Common pattern for settings panels.

## Component Interactions

```
┌─────────────────────────────────────────────────────┐
│                    UI Layer                          │
│  ┌─────────┐  ┌──────────────┐  ┌────────────────┐  │
│  │Keyboard │  │ Room Overlay │  │ Voice Button   │  │
│  │ clicks  │  │ (QR, name)   │  │ (sing input)   │  │
│  └────┬────┘  └──────┬───────┘  └───────┬────────┘  │
│       │              │                   │           │
└───────┼──────────────┼───────────────────┼───────────┘
        │              │                   │
        ▼              ▼                   ▼
┌───────────────┐ ┌──────────┐  ┌─────────────────────┐
│ Toggle Set    │ │ Channel  │  │ Voice Pitch Stream  │
│ (CRDT shared) │ │ Manager  │  │ (single-writer/peer)│
│ MIDI Ch 1     │ │          │  │ MIDI Ch 2 + bend    │
└───────┬───────┘ └──────────┘  └──────────┬──────────┘
        │                                   │
        └──────────────┬───────────────────┘
                       ▼
              ┌────────────────┐
              │  Web MIDI API  │
              │  (Output)      │
              └────────────────┘
```

## QR Code Generation
Use a pure-Rust QR code library (qrcode-generator or qrcode) compiled to WASM. Render as SVG in the overlay.

## Word List Source
Embed a small (~500 word) curated list of wholesome adjectives and nouns. Generate at runtime: `adjective-noun-noun` from random selection.

## Web MIDI Implementation Pattern
Reference: `../polyphonotopes-2025/musical-graphs-app/src/midi_input.rs`

**Input (from controllers):**
- Use `navigator.requestMIDIAccess()` to get `MidiAccess`
- Iterate `midi_access.inputs()` to connect to all available inputs
- Set `onmidimessage` callback on each `MidiInput`
- Parse status byte: 0x90 = note on, 0x80 = note off, 0xB0 = CC
- Use `async_channel` to send messages from JS callbacks to Rust event loop

**Output (to synths/DAWs):**
- Iterate `midi_access.outputs()` to get available outputs
- Use `MidiOutput.send(data)` to send MIDI messages
- Data format: `[status | channel, note, velocity]`

**Channel Mapping:**
- Channel 1 (0x00): Shared toggle set (pitch classes, no octave)
- Channel 2 (0x01): Voice pitches (full MIDI notes with octave)
