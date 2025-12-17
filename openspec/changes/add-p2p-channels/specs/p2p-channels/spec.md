# P2P Channels

## ADDED Requirements

### Requirement: Named Channel Join
Users SHALL be able to join a channel by providing a channel name string. The channel name serves as the unique identifier for peer discovery.

#### Scenario: Join channel by name
- **WHEN** a user calls `join("jam-session-123")`
- **THEN** they are connected to all other peers in that channel

#### Scenario: Channel name isolation
- **WHEN** user A joins "room-a" and user B joins "room-b"
- **THEN** they do not receive each other's messages

### Requirement: Publish Messages
Users SHALL be able to publish arbitrary byte messages to their joined channel. All connected peers receive the message.

#### Scenario: Broadcast to peers
- **WHEN** a user publishes a message to a channel
- **THEN** all other peers in that channel receive the message

#### Scenario: No echo
- **WHEN** a user publishes a message
- **THEN** they do not receive their own message back

### Requirement: Subscribe to Messages
Users SHALL receive messages from other peers in the channel via an async stream.

#### Scenario: Receive as stream
- **WHEN** peers publish messages to a joined channel
- **THEN** the user receives them as items in an async Stream

#### Scenario: Message ordering
- **WHEN** a single peer sends messages A then B
- **THEN** they are received in order A then B (per-sender ordering)

### Requirement: Leave Channel
Users SHALL be able to leave a channel, stopping message flow and releasing resources.

#### Scenario: Clean disconnect
- **WHEN** a user leaves a channel
- **THEN** they stop receiving messages and other peers are notified

### Requirement: Cross-Platform Support
The channel implementation SHALL work in both native Rust and wasm32 browser environments.

#### Scenario: Native Rust usage
- **WHEN** compiled for native target
- **THEN** channels function correctly

#### Scenario: Browser usage
- **WHEN** compiled for wasm32 and run in browser
- **THEN** channels function correctly via trunk/wasm-bindgen
