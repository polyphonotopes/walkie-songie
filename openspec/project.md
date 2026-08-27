# Project Context

## Purpose

Walkie-Songie is a peer-to-peer collaborative music application for creating
rooms of musical state and low-latency performance events. Musicians can play
together in browsers or native hosts while durable history remains repairable
after disconnects, restarts, and missed live delivery.

### Core modes

1. **Text input:** type a chord, note, or scale and toggle it on or off.
2. **Voice input:** sing into a microphone and use detected tones as musical
   input.

### Goals

- Responsive local musical interaction and low-latency peer feedback.
- Durable, convergent collaboration in native Rust and web browsers.
- Simple musical controls whose UI is a projection of authoritative room state.
- Decentralized operation with explicit discovery and relay infrastructure.

## Current stack

- **Language:** Rust 2024.
- **Browser build:** WebAssembly bundled by Trunk.
- **Durable state:** independent capability-native HHHS Room-v5 replicas for
  music and Walkie extension data.
- **Networking:** Walkie-owned Iroh endpoints, rendezvous, gossip, and repair
  streams. Browser peers can use a direct WebRTC custom transport.
- **Browser placement:** authoritative replica work and IndexedDB persistence
  run in a dedicated worker. The window owns UI, audio, MIDI, and carrier
  objects.
- **Music parsing:** `vibe-grammars`.
- **Keyboard projection:** `all-around-keyboard`.

## Architecture boundaries

- HHHS owns causal history, admission, capabilities, durable transactions,
  materialization, and transport-neutral repair semantics.
- Walkie owns room composition, endpoints, discovery, relays, WebRTC, IPC,
  protocol negotiation, peer lifecycle, scheduling, and presentation.
- Music and extension data use separate replica namespaces and histories.
- Protocol support, tickets, peer identity, and connectivity do not grant
  authority. Commands require an explicitly presented live capability path.
- Browser UI, audio, MIDI, and keyboard state are projections or effects of the
  room data plane; they are not durability or synchronization dependencies.
- Compact realtime session messages may eventually precede durable HHHS
  confirmation, but that provisional lane does not yet exist.

## Project conventions

### Code and dependencies

- Format Rust with `rustfmt` and prefer explicit errors over panics.
- Keep protocol/data-plane code usable without the web application.
- Use `pnpm` for required JavaScript tooling; do not use npm lifecycle scripts.
- Pin HHHS and Tutti to immutable release tags.

### Testing

- Unit tests cover codecs, authority, materialization, storage, and projections.
- Integration tests cover carrier substitution, partition/rejoin, bare-peer
  isolation, and native room behavior.
- Production-browser acceptance covers worker placement, IndexedDB recovery,
  two-peer synchronization, reconnect, UI projection, and latency.
- `scripts/check-release-candidate.sh` is the opt-in comprehensive release gate.
  Shared CI remains manual.

### Performance

- The keyboard must remain a cheap projection, never a serial dependency of
  admission or dissemination.
- The production-browser gate currently budgets p95 latency at 15 ms for local
  projection, 30 ms for local visibility, 75 ms for peer projection, and
  100 ms for peer visibility. Faster session-lane feedback remains a goal.
- High-rate musical controls need an explicitly negotiated realtime session
  protocol rather than one full durable transaction per control sample.

## Important constraints

- Browser and mobile use must remain first-class.
- Relay and rendezvous infrastructure may help peers meet, but it must not own
  room state or authority.
- Local and remote effects must remain observable and fallible.
- Room-v4 and p2panda source-log artifacts are refused compatibility fixtures,
  not live architecture or future direction.

## Parking lot

Ideas require a reviewed change proposal before implementation:

- Compact realtime session messages negotiated into MIDI-style affordances.
- Session capabilities and symmetric authentication for bounded realtime intent.
- IPC, pipes, embedded links, and hybrid carriers over the same repair boundary.
- Shared-set collaboration modes distinct from the current per-actor semantics.
- Richer solfège and graph projections.
