# P2P Channels

## ADDED Requirements

### Requirement: Iroh-Based Signaller
The system SHALL provide a custom matchbox signaller using iroh-gossip for peer discovery and direct messages for WebRTC offer/answer exchange.

#### Scenario: Create signaller
- **WHEN** `create_signaller()` is called
- **THEN** an iroh endpoint is created with gossip and direct message protocols

#### Scenario: Peer discovery via gossip
- **WHEN** two peers join the same gossip topic
- **THEN** they discover each other's matchbox peer IDs via presence broadcasts

#### Scenario: WebRTC signalling via direct messages
- **WHEN** matchbox needs to exchange WebRTC offers/answers
- **THEN** the signaller sends them via iroh direct QUIC streams

### Requirement: Matchbox WebRTC Integration
The signaller SHALL implement matchbox's `SignallerBuilder` trait, enabling use with `WebRtcSocket::builder().signaller_builder()`.

#### Scenario: Build socket with custom signaller
- **WHEN** a WebRtcSocket is built with the iroh signaller
- **THEN** peer connections are established via iroh discovery + WebRTC data channels

#### Scenario: Direct LAN connections
- **WHEN** two peers are on the same local network
- **THEN** they establish direct peer-to-peer WebRTC connections (not relayed)

### Requirement: Bootstrap Peer Connection
Users SHALL be able to connect to existing peers by providing an iroh node ID as a bootstrap.

#### Scenario: Join via bootstrap
- **WHEN** a user provides another peer's iroh node ID
- **THEN** they connect to the gossip topic via that peer and discover all other peers

#### Scenario: No bootstrap (first peer)
- **WHEN** a user starts without a bootstrap peer
- **THEN** they create a new gossip topic and wait for others to join

### Requirement: Peer Lifecycle Events
The signaller SHALL emit peer events for connection state changes.

#### Scenario: New peer notification
- **WHEN** a new peer joins the channel
- **THEN** existing peers receive a `NewPeer` event

#### Scenario: Peer timeout
- **WHEN** a peer stops sending presence broadcasts for 15 seconds
- **THEN** other peers receive a `PeerLeft` event
