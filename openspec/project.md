# Project Context

## Purpose
Walkie-songie is a P2P collaborative music application for creating real-time "chat rooms" of musical data (MIDI events, notes, chords, scales). It enables musicians to jam together remotely by sharing musical events in real-time.

### Core Modes
1. **Text input mode**: Type a chord, note, or musical scale and toggle it on/off
2. **Voice input mode**: Sing a tone into your phone/device mic and toggle that detected tone on/off

### Goals
- Low-latency P2P music collaboration
- Works in both native Rust contexts and web browsers
- Simple, intuitive interface for toggling musical events
- Decentralized architecture (no central server required for data relay)

## Tech Stack
- **Language**: Rust (2024 edition)
- **Build tooling**: trunk (wasm web app bundler)
- **Browser target**: WebAssembly (wasm32)
- **P2P networking**: TBD - need simple pub/sub on named channels. Options to evaluate:
  - iroh
  - p2panda
  - Plain WebSockets
  - WebTransport
  - Requirements: stupid simple peering that just works, named channel pub/sub model
- **Related packages**:
  - `vibe-grammars` - separate package for musical notation parsing

## Project Conventions

### Code Style
- Standard Rust formatting via `rustfmt`
- Use Rust streams extensively for event handling
- Prefer explicit error handling over panics
- Keep the library portion decoupled from the web app

### Build Tooling
- Use `trunk` (pure Rust) for wasm builds
- If JS dependencies are unavoidable, use `pnpm` (never npm - security concerns)
- Prefer Rust-based tooling over Node ecosystem where possible

### Architecture Patterns
- **Library + App split**: Core functionality as a reusable library, web app as a thin client
- **Streams-based**: Heavy use of Rust async streams for musical event flow
- **Dual-target**: Must compile for both native and wasm32 targets
- **P2P-first**: Prefer decentralized communication, signalling server only for peer discovery

### Testing Strategy
- Unit tests for core library logic
- Integration tests for P2P connectivity and event flow
- Use `cargo test` as the primary test runner
- Test both native and wasm targets where applicable

### Git Workflow
- Main branch for stable code
- Feature branches for new work
- Imperative commit messages (Fix, Add, Implement, Update, Remove)

## Domain Context
- **Musical events**: MIDI-like messages (note on/off, control change, etc.)
- **Chord/scale notation**: Parsed by the separate `vibe-grammars` package
- **Pitch detection**: Voice input requires pitch detection from microphone audio
- **Real-time constraints**: Musical collaboration requires low latency (<100ms ideal)

## Important Constraints
- Must work in web browsers (wasm compatibility required)
- Must work on mobile devices (phone mic input for voice mode)
- WebTransport only if widely available on target devices
- No central server for relaying musical data (P2P only)
- Signalling server acceptable for peer discovery (can use public servers)

## External Dependencies
- **iroh / iroh-gossip**: Signalling and peer discovery (uses public relays)
- **matchbox**: WebRTC direct P2P data channels
- **vibe-grammars**: Musical notation parsing (separate crate)
- **Web APIs**: WebAudio for mic input, potentially WebMIDI

## Parking Lot

Ideas for future consideration, not yet ready to become change proposals:

- **p2panda-sync**: Add sync protocols for state convergence / CRDTs (transport-agnostic, works over matchbox)
- **p2panda-net**: Full p2panda stack if we outgrow iroh-gossip+matchbox
- **CRDT state sync**: Shared pitch class sets (union of all peers' active notes)
- **Offline sync**: Catch up on musical events missed while disconnected
- **Piano keyboard UI**: Integrate a web component for visual piano keyboard (will require pnpm for JS dep)
- **Shared pitch class set mode**: Alternative to per-peer sets where all peers edit a single shared set directly (simpler model, different collaboration dynamic)
