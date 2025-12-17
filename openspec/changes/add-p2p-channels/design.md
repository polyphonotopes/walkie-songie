# Design: P2P Channel Infrastructure

## Context

Walkie-songie needs real-time P2P communication for musical event collaboration. Requirements:
- Named channels (join by string name)
- Pub/sub messaging (broadcast to all peers in channel)
- Works in both native Rust and wasm32/browser
- Direct browser-to-browser P2P (low latency)
- No custom backend server required

## Goals / Non-Goals

**Goals:**
- Get messages between peers with minimal latency
- Direct P2P in browsers (not relayed for data)
- No signalling server to maintain
- Simple API: join(name), publish(bytes), subscribe() -> Stream

**Non-Goals:**
- Message persistence or history (for now)
- Offline sync / catching up (add later with p2panda)
- Complex peer discovery beyond channel membership

## Decision

**Use iroh-gossip for signalling + matchbox for WebRTC data channels**

This pattern is now **officially supported** in matchbox via the [custom_signaller example](https://github.com/johanhelsing/matchbox/tree/main/examples/custom_signaller).

### Architecture

```
┌─────────────────────────────────────────────────────────┐
│                    Your App                              │
│         channel.join("jam-123")                          │
│         channel.publish(midi_event)                      │
│         channel.subscribe() -> Stream<Event>             │
├─────────────────────────────────────────────────────────┤
│                  Channel Trait                           │
├─────────────────────────────────────────────────────────┤
│                                                          │
│   ┌─────────────────┐      ┌─────────────────────────┐  │
│   │  iroh-gossip    │      │    matchbox_socket      │  │
│   │                 │      │                         │  │
│   │  • Signalling   │ ───▶ │  • WebRTC data channels │  │
│   │  • Peer discovery│      │  • Direct P2P           │  │
│   │  • Room topics  │      │  • Browser ↔ Browser    │  │
│   └────────┬────────┘      └─────────────────────────┘  │
│            │                           │                 │
│            ▼                           ▼                 │
│   ┌─────────────────┐      ┌─────────────────────────┐  │
│   │  iroh relay     │      │   Direct WebRTC         │  │
│   │  (public, free) │      │   (no relay needed)     │  │
│   └─────────────────┘      └─────────────────────────┘  │
│                                                          │
│   Control plane (light)         Data plane (fast)        │
└─────────────────────────────────────────────────────────┘
```

### How It Works

1. **iroh-gossip** handles signalling:
   - Peers join a "topic" (room/channel name hashed)
   - Exchange WebRTC offers/answers via gossip
   - Uses iroh's free public relay servers
   - No custom signalling server needed!

2. **matchbox** handles data:
   - Establishes direct WebRTC connections
   - Browser ↔ browser without relay
   - Low latency for real-time music

### Why This Combo?

| Concern | Solution |
|---------|----------|
| Signalling server | iroh-gossip + public relays (free) |
| Browser direct P2P | matchbox WebRTC |
| Latency | Data goes direct, not through relay |
| Complexity | Both libs handle their domain well |

## Implementation Notes

### Cargo.toml

```toml
[dependencies]
iroh = { version = "0.34", default-features = false }
iroh-gossip = "0.34"
matchbox_socket = "0.10"

[target.'cfg(target_arch = "wasm32")'.dependencies]
wasm-bindgen-futures = "0.4"
```

### Custom Signaller

Implement matchbox's `Signaller` trait using iroh-gossip:

```rust
// Conceptual - based on github.com/Sparganothis/Sparganothis-v2
struct IrohSignaller {
    gossip: GossipTopic,
}

impl Signaller for IrohSignaller {
    async fn new(attempts: Option<u16>, room_url: &str) -> Result<Self, SignalingError> {
        // Hash room name to topic ID
        let topic_id = TopicId::from_bytes(*blake3::hash(room_url.as_bytes()).as_bytes());
        // Join gossip topic
        let gossip = iroh_gossip.subscribe(topic_id, bootstrap_peers).await?;
        Ok(Self { gossip })
    }

    async fn send(&mut self, request: String) -> Result<(), SignalingError> {
        self.gossip.broadcast(request.as_bytes()).await?;
        Ok(())
    }

    async fn next_message(&mut self) -> Result<String, SignalingError> {
        let msg = self.gossip.next().await?;
        Ok(String::from_utf8(msg)?)
    }
}
```

### Reference Implementation

See working code at:
- https://github.com/Sparganothis/Sparganothis-v2/blob/main/protocol/src/global_matchmaker.rs
- Live demo: https://sparganothis.org/Sparganothis-v2/chat

### Channel API

```rust
pub trait Channel {
    async fn join(name: &str) -> Self;
    async fn publish(&self, data: &[u8]);
    fn subscribe(&self) -> impl Stream<Item = (PeerId, Vec<u8>)>;
    fn peers(&self) -> Vec<PeerId>;
    async fn leave(self);
}
```

## Risks / Trade-offs

| Risk | Mitigation |
|------|------------|
| iroh relay availability | Can self-host if needed; multiple public relays exist |
| matchbox Signaller trait is private | Issue #484 discusses making it public; can fork if needed |
| API instability (both pre-1.0) | Pin versions, wrap in Channel trait |

## Open Questions

- [ ] Is matchbox Signaller trait public now, or need fork?
- [ ] Bootstrap peers for iroh-gossip - use pre-shared keys or discovery?
- [ ] App-level encryption needed beyond WebRTC's DTLS?

## Evolution Path

```
Phase 1 (now):    iroh-gossip signalling + matchbox WebRTC
Phase 2 (later):  Add p2panda-sync for state sync / CRDTs
Phase 3 (later):  Add p2panda-net if we want full p2panda stack
```

## References

- **[Official matchbox custom_signaller example](https://github.com/johanhelsing/matchbox/tree/main/examples/custom_signaller)** - Complete iroh-gossip + matchbox WebRTC integration
- [online-breakout](https://github.com/yadokani389/online-breakout) - Working game using iroh+matchbox
- [matchbox issue #484](https://github.com/johanhelsing/matchbox/issues/484) - Original proposal
- [iroh wasm docs](https://iroh.computer/docs/wasm-browser-support)
