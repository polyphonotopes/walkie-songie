## 1. Core Library Abstractions
- [ ] 1.1 Define `PitchDetector` trait (async stream of pitch events)
- [ ] 1.2 Define `RoomState` trait (CRDT ops: add/remove pitch, get union, edit SCL)
- [ ] 1.3 Define `Transport` trait (send/recv messages, peer events)
- [ ] 1.4 Ensure core lib compiles for both wasm32 and native targets

## 2. Tuning System
- [ ] 2.1 Define pitch class and tuning types (PitchClass, Tuning, Scale)
- [ ] 2.2 Implement 12-TET default tuning with note names
- [ ] 2.3 Implement SCL file parser (minimal, spec-compliant)
- [ ] 2.4 Add Hz-to-pitch-class quantization returning (index, cents_deviation)
- [ ] 2.5 Unit tests for tuning math, SCL parsing, cents calculation

## 3. yrs CRDT Integration
- [ ] 3.1 Add yrs dependency (native), ywasm for web target
- [ ] 3.2 Define room document schema (YText for SCL, YMap for pitch sets)
- [ ] 3.3 Implement `RoomState` trait using yrs
- [ ] 3.4 Implement yrs sync protocol over matchbox reliable channel
- [ ] 3.5 Handle peer join/leave (sync initial state, cleanup on disconnect)
- [ ] 3.6 Unit tests for CRDT operations and sync

## 4. Pitch Detection (wasm AudioWorklet)
- [ ] 4.1 Add `pitch` (BCF) and `pyin-rs` crate dependencies, verify wasm compilation
- [ ] 4.2 Create AudioWorklet processor in Rust (wasm-pack build)
- [ ] 4.3 Implement dual-algorithm runner (BCF on small buffer, pYIN on larger buffer)
- [ ] 4.4 Implement dynamic noise gate (RMS envelope, adaptive threshold)
- [ ] 4.5 Wire both pitch outputs to main thread via message port
- [ ] 4.6 Implement `PitchDetector` trait for web (wraps AudioWorklet)
- [ ] 4.7 Test pitch detection accuracy with known frequencies
- [ ] 4.8 Measure wasm bundle size, optimize if >200KB

## 5. dominator Web App Setup
- [ ] 5.1 Add dominator + futures-signals dependencies
- [ ] 5.2 Set up wasm-pack build for web app
- [ ] 5.3 Create basic app shell with dominator
- [ ] 5.4 Wire up core library (RoomState, Transport) to reactive signals

## 6. Voice Input UI (dominator)
- [ ] 6.1 Create press-hold-release button component (touch + mouse events)
- [ ] 6.2 Wire button to start/stop pitch detection
- [ ] 6.3 Display real-time pitch feedback (note name + Hz) via Mutable signal
- [ ] 6.4 Add closeness indicator (cents deviation gauge/color)
- [ ] 6.5 On release, commit detected pitch class to RoomState
- [ ] 6.6 Handle "no pitch detected" state gracefully

## 7. Pitch State UI (dominator)
- [ ] 7.1 Display local active pitch classes as toggle buttons
- [ ] 7.2 Implement toggle on/off via button click
- [ ] 7.3 Display room union with peer attribution
- [ ] 7.4 React to yrs document changes (signal updates on CRDT change)

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
