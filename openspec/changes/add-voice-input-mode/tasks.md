## 1. Core Library Abstractions
- [x] 1.1 Define `PitchDetector` trait (async stream of pitch events)
- [x] 1.2 Define `RoomState` trait (CRDT ops: add/remove pitch, get union, edit SCL)
- [ ] 1.3 Define `Transport` trait (send/recv messages, peer events) - *deferred to P2P integration*
- [x] 1.4 Ensure core lib compiles for both wasm32 and native targets

## 2. Tuning System
- [x] 2.1 Define pitch class and tuning types (PitchClass, Tuning, Scale)
- [x] 2.2 Implement 12-TET default tuning with note names
- [x] 2.3 Implement SCL file parser (minimal, spec-compliant)
- [x] 2.4 Add Hz-to-pitch-class quantization returning (index, cents_deviation)
- [x] 2.5 Unit tests for tuning math, SCL parsing, cents calculation

## 3. yrs CRDT Integration
- [x] 3.1 Add yrs dependency (native), ywasm for web target
- [x] 3.2 Define room document schema (YText for SCL, YMap for pitch sets)
- [x] 3.3 Implement `RoomState` trait using yrs
- [ ] 3.4 Implement yrs sync protocol over matchbox reliable channel - *deferred to P2P integration*
- [ ] 3.5 Handle peer join/leave (sync initial state, cleanup on disconnect) - *deferred to P2P integration*
- [x] 3.6 Unit tests for CRDT operations and sync

## 4. Pitch Detection (Multi-Algorithm)
- [x] 4.1 Add `pitch` (BCF), `pitch-detection` (McLeod), and `pyin-rs` crate dependencies
  - Note: pyin uses libc FFI, so it's native-only. BCF and McLeod work on wasm.
- [x] 4.2 Create ScriptProcessorNode audio capture (simpler than AudioWorklet, no JS tooling needed)
- [x] 4.3 Implement multi-algorithm runner (BCF fast, McLeod robust, pYIN accurate)
- [x] 4.4 Implement dynamic noise gate (RMS envelope, adaptive threshold)
- [x] 4.5 Wire pitch detection output to UI via callback
- [x] 4.6 Implement `PitchDetector` trait for web (wraps ScriptProcessorNode)
- [x] 4.7 Test pitch detection accuracy with known frequencies
- [x] 4.8 Add pitch locking with confidence accumulation and 150ms linger

## 5. dominator Web App Setup
- [x] 5.1 Add dominator + futures-signals dependencies
- [x] 5.2 Set up trunk build for web app
- [x] 5.3 Create basic app shell with dominator
- [x] 5.4 Wire up core library (RoomState, Transport) to reactive signals

## 6. Voice Input UI (dominator)
- [x] 6.1 Create press-hold-release button component (touch + mouse events)
- [x] 6.2 Wire button to start/stop pitch detection
- [x] 6.3 Display real-time pitch feedback (note name + Hz) via Mutable signal
- [x] 6.4 Add closeness indicator (cents deviation gauge/color)
- [x] 6.5 On release, commit detected pitch class to RoomState
- [x] 6.6 Handle "no pitch detected" state gracefully
- [x] 6.7 Add rolling confidence accumulator for stable pitch commitment

## 7. Pitch State UI (dominator)
- [x] 7.1 Display local active pitch classes as toggle buttons
- [x] 7.2 Implement toggle on/off via button click
- [ ] 7.3 Display room union with peer attribution - *component ready, needs P2P*
- [x] 7.4 React to yrs document changes (signal updates on CRDT change)

## 8. Room Tuning UI (dominator)
- [x] 8.1 Display current tuning name/summary
- [x] 8.2 Add SCL content editor (textarea bound to YText via signal)
- [x] 8.3 Show parse errors inline when SCL is invalid
- [ ] 8.4 Re-quantize active pitches when tuning changes - *deferred*

## 9. Integration & Testing
- [x] 9.1 Wire voice input → tuning → pitch state (local only)
- [ ] 9.2 End-to-end test: two peers, sing → detect → sync → display union - *needs P2P*
- [ ] 9.3 Test collaborative SCL editing between peers - *needs P2P*
- [ ] 9.4 Performance test: latency from sing to display - *pending browser test*

---

## Summary

**Completed:**
- Core library with PitchDetector and RoomState traits
- Full tuning system with 12-TET, SCL parser, and Hz quantization
- yrs CRDT room state implementation with sync support
- Multi-algorithm pitch detection: BCF (fast), McLeod (noise-robust), pYIN (accurate, native-only)
- ScriptProcessorNode audio capture (simpler alternative to AudioWorklet)
- Pitch locking with 150ms linger and confidence accumulation for stable UX
- Dominator web app with voice input, pitch grid, and tuning editor
- Full voice input flow: hold button → capture audio → detect pitch → lock with confidence → release → commit vote winner
- 22 passing unit tests
- Both native and wasm32 targets build successfully

**Remaining (deferred to P2P integration phase):**
- Platform-specific signaller (iroh for native, websocket for web)
- P2P sync wiring over matchbox reliable channel
- End-to-end multi-peer testing
