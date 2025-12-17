# Tasks: Add P2P Channel Infrastructure

## 1. Research & Decision
- [x] 1.1 Evaluate iroh for wasm compatibility and pub/sub model
- [x] 1.2 Evaluate p2panda for wasm compatibility and pub/sub model
- [x] 1.3 Evaluate matchbox (WebRTC) for direct browser P2P
- [x] 1.4 Research iroh+matchbox combo (issue #484, Sparganothis)
- [x] 1.5 Document decision in design.md with rationale
- **Decision: iroh-gossip for signalling + matchbox for WebRTC data**

## 2. Core Dependencies
- [ ] 2.1 Add iroh + iroh-gossip to Cargo.toml
- [ ] 2.2 Add matchbox_socket to Cargo.toml
- [ ] 2.3 Configure for dual-target (native + wasm32)

## 3. Iroh Signaller Implementation
- [ ] 3.1 Study Sparganothis implementation as reference
- [ ] 3.2 Implement IrohSignaller struct wrapping iroh-gossip
- [ ] 3.3 Handle topic ID generation from room names
- [ ] 3.4 Handle bootstrap peer discovery (pre-shared keys approach)

## 4. Channel Abstraction
- [ ] 4.1 Create `Channel` trait defining pub/sub interface
- [ ] 4.2 Implement channel using IrohSignaller + matchbox_socket
- [ ] 4.3 Add channel event types (peer joined, peer left, message)
- [ ] 4.4 Implement broadcast (send to all peers)

## 5. Cross-Platform Support
- [ ] 5.1 Verify native target compiles and works
- [ ] 5.2 Verify wasm32 target compiles
- [ ] 5.3 Add trunk configuration for web builds
- [ ] 5.4 Test in browser environment

## 6. Testing
- [ ] 6.1 Unit tests for Channel trait
- [ ] 6.2 Integration test: two native peers
- [ ] 6.3 Integration test: browser to browser
- [ ] 6.4 Test reconnection behavior
