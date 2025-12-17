# Change: Add P2P Channel Infrastructure

## Why
Walkie-songie needs a foundation for real-time music collaboration. Musicians need to join named "rooms" and exchange musical events with minimal latency. The P2P layer should be dead simple: join a channel by name, publish events, receive events from peers.

## What Changes
- Add core P2P channel abstraction (named pub/sub channels)
- Evaluate and select transport layer (iroh, p2panda, WebSocket, or WebTransport)
- Support both native Rust and wasm/browser targets
- Automatic peer discovery within channels

## Impact
- Affected specs: `p2p-channels` (new capability)
- Affected code: New `src/channels/` module, Cargo.toml dependencies
- This is foundational infrastructure - all other features depend on it
