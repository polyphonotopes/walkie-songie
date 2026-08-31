# plugin-peer Specification

## Purpose

A nice-plug CLAP and optional standalone host for the shared Walkie/Tutti
bridge core.

## ADDED Requirements

### Requirement: nice-plug host
The plugin peer SHALL use nice-plug and SHALL export a CLAP plugin. An optional
standalone executable SHALL instantiate the same plugin and bridge core.

#### Scenario: CLAP host loads the peer
- **WHEN** a CLAP host instantiates the plugin
- **THEN** it receives the same bridge behavior and persistent settings as the
  standalone shell
- **AND** no obsolete nih-plug or Yrs networking path is constructed

#### Scenario: Standalone feature is disabled
- **WHEN** the plugin is built without the standalone feature
- **THEN** standalone audio/MIDI backend dependencies are not included

### Requirement: Realtime-safe ownership
The plugin SHALL perform networking, BLE, HHHS, persistence, cryptography, and
UI work outside the audio callback.

#### Scenario: Audio processing
- **WHEN** the host invokes the audio callback
- **THEN** the callback performs no blocking wait, network or BLE I/O, mutex
  acquisition, heap allocation, HHHS operation, or cryptographic operation
- **AND** it communicates through bounded non-blocking queues

#### Scenario: Queue saturation
- **WHEN** an ephemeral input or output queue is full
- **THEN** the configured coalescing or drop policy is applied without blocking
- **AND** saturation becomes observable outside the audio callback

### Requirement: MIDI bridge
The plugin SHALL translate local MIDI input into bounded bridge intents and
remote projected musical changes into sample-positioned MIDI output.

#### Scenario: Local note input
- **WHEN** the plugin receives a MIDI note event
- **THEN** it enqueues the corresponding compact realtime intent immediately
- **AND** any durable musical edit is confirmed separately by the Room-v5
  replica

#### Scenario: Remote confirmation or correction
- **WHEN** a projected durable revision confirms or contradicts provisional
  realtime feedback
- **THEN** the plugin emits the minimal MIDI difference needed to match the
  durable projection

### Requirement: Persistent bridge selection
The plugin SHALL persist room selection and trusted board identities without
persisting ephemeral session keys or live connection state.

#### Scenario: DAW project reload
- **WHEN** a saved project restores the plugin
- **THEN** the plugin restores its room and trust configuration
- **AND** creates fresh Iroh and BLE sessions

### Requirement: Independent carrier lifecycle
The native Room-v5 carrier and local BLE-board carrier SHALL retain independent
connection and failure lifecycles while sharing the bridge core.

#### Scenario: Room is joined and left with a board connected
- **GIVEN** the BLE board has reached authenticated Ready
- **WHEN** the plugin joins and then leaves a native Room-v5 session
- **THEN** the room transitions through its own Ready and Offline states
- **AND** the board remains authenticated Ready throughout

#### Scenario: One carrier cannot initialize
- **WHEN** native Iroh or the platform BLE adapter fails during startup
- **THEN** that carrier publishes its own Failed state and diagnostic
- **AND** the other carrier still starts and remains usable

### Requirement: Multiple instances
Multiple plugin instances SHALL not duplicate heavyweight service ownership or
share musical routing accidentally.

#### Scenario: Two instances use different rooms
- **WHEN** two instances in one process select different rooms
- **THEN** their MIDI queues and room projections remain isolated
- **AND** any process-level network or BLE service sharing preserves that
  isolation
