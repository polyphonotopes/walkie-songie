## ADDED Requirements

### Requirement: Native Iroh Connectivity
The desktop peer SHALL use stable native Iroh QUIC with one persistent endpoint identity, attempt direct UDP paths including NAT hole punching, and retain relay fallback when a direct path is unavailable.

#### Scenario: Punchable NATs
- **GIVEN** two peers behind distinct NATs that permit coordinated UDP hole punching
- **WHEN** they bootstrap through an Iroh relay
- **THEN** they establish an active direct IP path and continue the room protocol over that path

#### Scenario: Non-punchable network
- **GIVEN** a peer network that blocks direct UDP traversal
- **WHEN** peers join the same room
- **THEN** they remain connected through the configured relay and the UI reports the path as relayed

#### Scenario: Network path changes
- **WHEN** a connected device changes interface or address
- **THEN** Iroh revalidates available paths and the room continues or reconnects without changing participant identity

### Requirement: Room-Scoped mDNS Discovery
The desktop peer SHALL advertise and discover Iroh endpoint addressing on the local network using an mDNS service name derived from the room topic.

#### Scenario: Offline LAN join
- **GIVEN** two devices on the same LAN with Internet and relay access disabled
- **WHEN** both enter the same room name
- **THEN** they discover one another through mDNS, establish a direct Iroh connection, and converge room state

#### Scenario: Different room on same LAN
- **WHEN** nearby peers advertise different room topics
- **THEN** their mDNS records do not cause them to join or disclose the human names of each other's rooms

#### Scenario: LAN peer expires
- **WHEN** an mDNS record expires
- **THEN** the peer loses LAN-presence status without deleting valid durable signed history

### Requirement: Explicit WAN Bootstrap
The system SHALL encode a versioned room ticket containing the room topic, bootstrap endpoint identity, and usable addressing information for non-LAN joining.

#### Scenario: Join shared ticket
- **WHEN** a user opens or scans a valid ticket
- **THEN** the peer validates its version and topic, connects to the bootstrap endpoint, joins live gossip, and starts anti-entropy repair

#### Scenario: Ticket has stale direct addresses
- **WHEN** a ticket's direct addresses are stale but its relay or address lookup information remains usable
- **THEN** Iroh resolves a working path without changing room identity

#### Scenario: Invalid ticket
- **WHEN** a ticket is malformed, unsupported, or exceeds its resource limits
- **THEN** the application rejects it without starting a network task

### Requirement: Verified Live and Repair Protocols
The system SHALL use bounded Iroh gossip for low-latency delivery and the HHHS SyncSession protocol for deterministic repair of durable operations.

#### Scenario: Live signed operation
- **WHEN** a peer receives a gossiped durable operation
- **THEN** it verifies signature, topic, schema, domain invariants, and size before ingestion and rebroadcast

#### Scenario: Lost gossip message
- **WHEN** a durable operation is lost during live gossip
- **THEN** a later HHHS reconciliation session transfers the verbatim signed bytes and both peers converge

#### Scenario: Invalid remote operation
- **WHEN** a remote operation fails cryptographic or domain validation
- **THEN** it is quarantined or rejected and cannot affect RoomStore, the UI, or MIDI output

### Requirement: Observable Connection Path
The system SHALL expose per-peer discovery source, active direct/relay path classification, reconnect state, and synchronization health to the frontend and diagnostics.

#### Scenario: Relay upgrades to direct
- **WHEN** Iroh activates an IP path after an initial relay connection
- **THEN** the displayed peer status changes to direct without claiming a new peer joined

#### Scenario: Direct path falls back
- **WHEN** an active direct path becomes unusable and relay remains available
- **THEN** the displayed status changes to relayed while musical state delivery continues
