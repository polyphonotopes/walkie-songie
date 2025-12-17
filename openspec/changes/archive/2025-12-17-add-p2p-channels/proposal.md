# Change: Add P2P Channel Infrastructure

## Why
Walkie-songie needs a foundation for real-time music collaboration. Musicians need to join named "rooms" and exchange musical events with minimal latency. This change adds the signalling layer that enables peer discovery and WebRTC connections.

## What Changes
- Add iroh-gossip based signaller implementing matchbox's `SignallerBuilder` trait
- Peer discovery via gossip presence broadcasts
- WebRTC offer/answer exchange via iroh direct messages
- Direct peer-to-peer connections on same LAN (~1ms latency)
- Relay fallback via iroh's public relay infrastructure

## Implementation
- `src/net/signaller.rs` - IrohSignallerBuilder + background task
- `src/net/direct_message.rs` - Direct message protocol handler
- Cargo.toml - iroh, iroh-gossip, matchbox_socket dependencies

## Impact
- Affected specs: `p2p-channels` (new capability)
- Affected code: New `src/net/` module
- This is foundational infrastructure - higher-level channel abstractions build on this
