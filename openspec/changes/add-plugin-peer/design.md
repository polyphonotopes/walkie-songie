# Design: nih-plug Plugin Peer

## Context

Audio plugins run in a realtime context with strict latency requirements. The audio thread must never block on I/O, locks, or allocations. Meanwhile, P2P networking requires async I/O, DNS resolution, and potentially long-running operations. These constraints require careful separation of concerns.

## Goals / Non-Goals

**Goals:**
- Plugin runs in any VST3/CLAP host without audio dropouts
- Mobile users can join by scanning a QR code
- Channel address persists between DAW sessions
- Reuse existing P2P infrastructure from `net::` module
- MIDI input: broadcast local MIDI notes to the room
- MIDI output: receive remote peer notes as MIDI events
- Multiple plugin instances in same DAW session work independently

**Non-Goals:**
- Audio synthesis in the plugin (out of scope)
- Voice input in the plugin (phones handle that)
- Complex preset management
- Standalone mode (deferred)

## Decisions

### Threading Model: Dedicated Network Thread

**Decision:** Spawn a dedicated OS thread (not async task on audio thread) for all networking operations.

**Rationale:**
- Audio plugins must never block the audio thread
- Even async I/O can cause priority inversion with the OS scheduler
- A separate thread with its own tokio runtime isolates all network latency
- Communication via lock-free channels (`crossbeam-channel` or `std::sync::mpsc`)

**Alternative considered:** Running async networking on the audio thread with non-blocking polls.
- Rejected: Too risky - any accidental `.await` or lock contention causes audio dropouts.

### GUI Framework: egui via nih_plug_egui

**Decision:** Use `nih_plug_egui` for the plugin editor.

**Rationale:**
- Immediate-mode GUI fits plugin development well
- Good QR code rendering support via egui's texture API
- Simplest integration with nih-plug
- Compact UI requirements (QR + few buttons) don't need a complex framework

**Alternatives considered:**
- `nih_plug_vizia`: More structured, but heavier for our simple needs
- `nih_plug_iced`: Retained-mode doesn't fit as naturally for simple UIs

### Channel Address Format

**Decision:** Channel address is the iroh node ID (base32 encoded) of a bootstrap peer or a human-readable word-based channel name.

**Format options:**
1. Word-based: `fuzzy-piano-midnight` (uses existing `words.rs`)
2. Iroh node ID: `abacd1234...` (long but guaranteed unique)

**QR Code content:** URL format `walkie-songie://channel/<address>` for mobile app deep linking.

### State Persistence

**Decision:** Use nih-plug's `#[persist = "channel_address"]` attribute on a `String` field.

**Rationale:**
- Native nih-plug feature, zero additional code
- Automatically saved/restored with plugin state
- Works across all hosts (VST3, CLAP)

```rust
#[derive(Params)]
struct PluginParams {
    #[persist = "channel_address"]
    channel_address: Arc<Mutex<String>>,
}
```

### Feature Gating

**Decision:** All plugin code behind `#[cfg(feature = "plugin")]` and separate binary target.

**Cargo.toml structure:**
```toml
[features]
default = []
plugin = ["dep:nih_plug", "dep:nih_plug_egui", "dep:qrcode"]

[[bin]]
name = "walkie-songie-plugin"
required-features = ["plugin"]
```

## Risks / Trade-offs

| Risk | Mitigation |
|------|------------|
| Thread synchronization complexity | Use simple message passing, avoid shared mutable state |
| QR code too small to scan | Make editor resizable, use error correction level M or higher |
| Plugin binary size with tokio runtime | Accept ~2MB overhead; networking requires it |
| egui rendering performance | Minimal UI, only redraw on state change |

## Migration Plan

Not applicable - new feature, no existing users to migrate.

## Resolved Questions

1. **MIDI input/output?** Yes - plugin broadcasts MIDI input to room and outputs remote peer notes as MIDI.
2. **Standalone mode?** Deferred for now.
3. **Multiple instances in same DAW?** Supported - each instance gets its own peer ID and can join different channels. Local discovery (iroh-gossip) and WebRTC handle instance-to-instance communication naturally.
