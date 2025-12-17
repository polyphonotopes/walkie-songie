## Context
This is the first musical feature for walkie-songie. It establishes patterns for:
- Audio input handling in the browser (wasm + Web Audio)
- Tuning/scale representation with collaborative editing
- Shared musical state across peers via CRDTs

The feature spans audio capture, pitch detection, tuning quantization, local state management, and P2P synchronization. The environment is a room where phone mic input may pick up audio playing from speakers (feedback scenario).

## Goals / Non-Goals
- Goals:
  - Real-time pitch detection with visual feedback (note name + closeness indicator)
  - Support arbitrary tunings via SCL files, editable per-room as text CRDT
  - Fast CRDT sync for pitch class sets using established library
  - Dynamic noise gating that adapts to ambient levels
  - Works in modern browsers (Chrome, Firefox, Safari)

- Non-Goals:
  - Polyphonic detection (single voice only)
  - MIDI output (future feature)
  - Offline-first sync (online peers only for now)
  - Complex frontend framework (keep UI minimal with Rust HTML/JS helpers)

## Decisions

### Architecture: Library + Thin Client
- **Core library** (`walkie-songie`): Platform-agnostic peer/channel/state abstraction
  - P2P connection management (existing iroh+matchbox signaller)
  - Room state CRDT (yrs document with pitch sets + SCL tuning)
  - Pitch detection traits and implementations
  - No UI dependencies - usable from Leptos, Bevy, or CLI
- **dominator web app**: Thin prototype client consuming the core library
  - `dominator` for zero-cost DOM bindings + `futures-signals` for reactive state
  - Web-specific: AudioWorklet integration, mic permissions UI
  - Press-hold-release button, pitch feedback display
  - Minimal footprint (~20KB), signal-based reactivity fits audio streams well
  - Build tooling: `trunk` (pure Rust) preferred; if JS deps needed, use `pnpm` (never npm)
- **Future Bevy app**: Alternative client using same core library
  - Native audio input via cpal/rodio
  - Bevy UI for pitch display and controls

Key trait abstractions:
- `PitchDetector` - async stream of pitch events
- `RoomState` - CRDT operations, sync protocol
- `Transport` - message send/receive (matchbox impl, could swap later)

### Pitch Detection: Dual Algorithm (BCF + pYIN)
- Use two pitch detection algorithms in parallel for best UX:
  - **BCF** (`pitch` crate) - fast, low-latency (~15ms), for immediate visual feedback
  - **pYIN** (`pyin-rs` crate) - accurate, smoothed (~50ms), for final committed pitch
- Run in AudioWorklet for low-latency processing
- Hybrid strategy:
  - Display BCF result immediately (snappy closeness indicator)
  - pYIN runs in parallel on larger buffer
  - On button release, commit the pYIN result (more reliable, fewer octave errors)
  - If pYIN hasn't settled yet, use BCF as fallback
- Wasm size estimate: BCF adds ~5KB, pYIN adds ~50-80KB (FFT + ndarray deps)
- AudioWorklet integration: Follow wasm-pack + AudioWorklet pattern

### Dynamic Noise Gate
- Continuously track ambient noise floor from mic input
- Gate threshold = noise floor + configurable margin (e.g., +6dB)
- Only report pitch when signal exceeds threshold
- Allows use in rooms with speakers playing back audio
- Implementation: RMS envelope follower with slow attack (1-2s), fast release

### Tuning System: Per-Room SCL as Text CRDT
- SCL file content stored as a text CRDT (one per room)
- All peers in room share the same tuning
- Any peer can edit the tuning collaboratively
- Default to 12-TET when room has no custom SCL
- Store pitch classes as indices into the current scale

### Pitch Feedback UI
- Display note name (e.g., "A4") with closeness indicator
- Closeness: cents deviation from nearest pitch class (-50 to +50 cents)
- Visual: needle/gauge or color gradient (green=in tune, red=sharp/flat)
- Also show Hz value for reference

### State Sync: yrs (Y-CRDT)
- Use `yrs` for all CRDT needs (mature, fast, good wasm support via `ywasm`)
- Room document schema:
  - `tuning: YText` - SCL file content
  - `pitch_sets: YMap<peer_id, YArray<pitch_class_index>>` - per-peer active sets
  - `combination_method: String` - how to combine sets (union, intersection, etc.)
- Sync over existing matchbox reliable channel
- Alternatives considered:
  - automerge - also good, but yrs has larger ecosystem (yjs compatibility)
  - Custom ORSet - unnecessary when yrs exists
- Rationale: yrs is battle-tested, handles text+sets, has dedicated wasm bindings

### Message Transport
- Reuse existing matchbox reliable channel for CRDT sync messages
- Use yrs sync protocol (awareness + document updates)
- Binary format (yrs native encoding) for efficiency

## Risks / Trade-offs
- Browser mic permissions may be denied → Show clear error, guide user
- Pitch detection accuracy varies by device → Dynamic gating helps, consider calibration
- AudioWorklet complexity → Use established patterns from pitch-detection-app
- yrs adds dependency (~100KB wasm) → Worth it for correctness and text CRDT

## Migration Plan
- No migration needed (greenfield feature)
- Existing p2p-channels spec unchanged
- New capabilities are additive

## Open Questions
- Which pitch crate performs best in practice? → Prototype and compare
- Should room SCL be editable by all peers or just room creator? → Start with all peers

## Evolution Path
- **Phase 1 (this change)**: Voice input → single pitch per peer at a time; per-peer sets combined via room method
- **Phase 2**: Additional input interfaces (keyboard, text) allowing multiple pitches per peer
- **Phase 3 (potential)**: Shared pitch class set mode where peers edit a single shared set directly (no per-peer isolation + combination)
