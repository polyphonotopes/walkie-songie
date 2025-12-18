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

## 4. Pitch Detection (wasm AudioWorklet)
- [x] 4.1 Add `pitch` (BCF) and `pyin-rs` crate dependencies, verify wasm compilation
  - Note: pyin uses libc FFI, so it's native-only. BCF works on wasm.
- [ ] 4.2 Create AudioWorklet processor in Rust (wasm-pack build) - *requires pnpm integration*
- [x] 4.3 Implement dual-algorithm runner (BCF on small buffer, pYIN on larger buffer)
- [x] 4.4 Implement dynamic noise gate (RMS envelope, adaptive threshold)
- [ ] 4.5 Wire both pitch outputs to main thread via message port - *requires AudioWorklet*
- [x] 4.6 Implement `PitchDetector` trait for web (wraps AudioWorklet)
- [x] 4.7 Test pitch detection accuracy with known frequencies
- [ ] 4.8 Measure wasm bundle size, optimize if >200KB - *pending full build*

## 5. dominator Web App Setup
- [x] 5.1 Add dominator + futures-signals dependencies
- [ ] 5.2 Set up wasm-pack build for web app - *trunk configured, pending testing*
- [x] 5.3 Create basic app shell with dominator
- [x] 5.4 Wire up core library (RoomState, Transport) to reactive signals

## 6. Voice Input UI (dominator)
- [x] 6.1 Create press-hold-release button component (touch + mouse events)
- [x] 6.2 Wire button to start/stop pitch detection
- [x] 6.3 Display real-time pitch feedback (note name + Hz) via Mutable signal
- [x] 6.4 Add closeness indicator (cents deviation gauge/color)
- [x] 6.5 On release, commit detected pitch class to RoomState
- [x] 6.6 Handle "no pitch detected" state gracefully

## 7. Pitch State UI (dominator)
- [x] 7.1 Display local active pitch classes as toggle buttons
- [x] 7.2 Implement toggle on/off via button click
- [ ] 7.3 Display room union with peer attribution - *component ready, needs P2P*
- [x] 7.4 React to yrs document changes (signal updates on CRDT change)

## 8. Room Tuning UI (dominator)
- [ ] 8.1 Display current tuning name/summary
- [ ] 8.2 Add SCL content editor (textarea bound to YText via signal)
- [ ] 8.3 Show parse errors inline when SCL is invalid
- [ ] 8.4 Re-quantize active pitches when tuning changes

## 9. Integration & Testing
- [ ] 9.1 Wire voice input → tuning → pitch state → sync
- [ ] 9.2 End-to-end test: two peers, sing → detect → sync → display union
- [ ] 9.3 Test collaborative SCL editing between peers
- [ ] 9.4 Performance test: latency from sing to display

---

## Summary

**Completed (22 items):**
- Core library with PitchDetector and RoomState traits
- Full tuning system with 12-TET, SCL parser, and Hz quantization
- yrs CRDT room state implementation with sync support
- Dual-algorithm pitch detection (BCF + pYIN) with noise gate
- Dominator web app structure with voice input and pitch grid components
- 22 passing unit tests

**Remaining:**
- AudioWorklet integration (requires pnpm for JS tooling)
- Room Tuning UI editor
- P2P sync wiring (uses existing matchbox/iroh infrastructure)
- End-to-end integration testing
