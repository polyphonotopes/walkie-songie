# Tasks: Add nih-plug Plugin Peer

## 1. Feature Flag & Dependencies
- [x] 1.1 Add `plugin` feature to Cargo.toml
- [x] 1.2 Add nih-plug dependency (feature-gated)
- [x] 1.3 Add nih_plug_egui for GUI (feature-gated)
- [x] 1.4 Add qrcode crate for QR generation
- [x] 1.5 Configure cargo-nih-plug bundling in xtask or workspace

## 2. Plugin Core
- [x] 2.1 Create `src/plugin/mod.rs` module (feature-gated)
- [x] 2.2 Implement `Plugin` trait with audio passthrough
- [x] 2.3 Define `Params` struct with `#[persist = "channel"]` for address
- [x] 2.4 Export VST3 and CLAP via `nih_export_vst3!` / `nih_export_clap!`

## 3. Networking Thread
- [x] 3.1 Create background networking task spawned on plugin init
- [x] 3.2 Use channel (mpsc) for communication between UI/audio and network thread
- [x] 3.3 Integrate with iroh-gossip for P2P networking
- [x] 3.4 Handle peer connection lifecycle without blocking audio thread
- [x] 3.5 Graceful shutdown on plugin deactivate/drop

## 4. Plugin UI
- [x] 4.1 Create egui-based editor implementing `Editor` trait
- [x] 4.2 Generate and display QR code from channel address
- [x] 4.3 Add "Shuffle Channel" button with random channel generation
- [x] 4.4 Add text input for custom channel address entry
- [x] 4.5 Display connection status (connected peers count)
- [x] 4.6 Style UI to be compact and DAW-friendly

## 5. State Synchronization & MIDI
- [x] 5.1 Connect plugin to RoomState for pitch/note sharing (YrsRoomState refactored with YMap)
- [x] 5.2 Add voice state per-peer (pitch number + pitch class) to CRDT
- [x] 5.3 Bridge incoming MIDI note events to room state (broadcast to peers)
- [x] 5.4 Convert remote peer pitch changes to outgoing MIDI note events
- [x] 5.5 Plugin bundled as VST3 and CLAP
- [x] 5.6 MIDI channel routing:
  - Channel 1 (input): Pitch class selection → room PCS contribution
  - Channel 1 (output): Room PCS (union of all peers' pitch classes, notes 60-71)
  - Channel 2 (input): Voice pitch → broadcast voice state
  - Channel 2 (output): All active voice pitches from peers

## 6. Testing & Validation
- [ ] 6.1 Test plugin loads in DAW (Reaper, Bitwig, etc.)
- [ ] 6.2 Test QR code scans correctly on mobile
- [ ] 6.3 Test P2P connection between plugin and web app
- [ ] 6.4 Test channel persistence across plugin reload
- [ ] 6.5 Verify audio thread never blocks on network operations
