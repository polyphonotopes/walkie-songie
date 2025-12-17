# Tasks: Add P2P Channel Infrastructure

## 1. Research & Decision
- [x] 1.1 Evaluate iroh for wasm compatibility and pub/sub model
- [x] 1.2 Evaluate p2panda for wasm compatibility and pub/sub model
- [x] 1.3 Evaluate matchbox (WebRTC) for direct browser P2P
- [x] 1.4 Research iroh+matchbox combo (issue #484, yadokani389/online-breakout)
- [x] 1.5 Document decision in design.md with rationale
- **Decision: iroh-gossip for signalling + matchbox for WebRTC data**

## 2. Core Dependencies
- [x] 2.1 Add iroh + iroh-gossip to Cargo.toml
- [x] 2.2 Add matchbox_socket to Cargo.toml
- [x] 2.3 Configure for dual-target (native + wasm32)

## 3. Iroh Signaller Implementation
- [x] 3.1 Study matchbox custom_signaller example + yadokani389/online-breakout
- [x] 3.2 Implement IrohSignallerBuilder implementing SignallerBuilder trait
- [x] 3.3 Implement direct message protocol for WebRTC offer/answer exchange
- [x] 3.4 Implement gossip presence for peer discovery + ID mapping
- [x] 3.5 Test two native peers connecting (direct LAN connection verified)

## 4. Manual Verification
- [x] 4.1 Native target compiles and runs
- [x] 4.2 Two peers discover each other via gossip (manual test)
- [x] 4.3 WebRTC connection established via signaller (manual test)
- [x] 4.4 Direct LAN connection verified (~1ms latency, manual test)
