## ADDED Requirements

### Requirement: Per-Peer Pitch Class Set Structure
The system SHALL maintain pitch class sets in a yrs YMap where each peer has an isolated set of active pitch classes.

#### Scenario: Document structure
- **GIVEN** a room with connected peers
- **THEN** the yrs document contains a YMap "pitch_sets" with structure: `{ peer_id: YArray<pitch_class_index> }`

#### Scenario: Pitch class representation
- **GIVEN** a tuning with N pitch classes (e.g., 12 for 12-TET)
- **THEN** pitch class indices are integers in range [0, N-1]

#### Scenario: Peer isolation
- **WHEN** a peer modifies pitch classes
- **THEN** only their own entry in the YMap is modified (peers cannot edit each other's sets)

#### Scenario: New peer joins
- **WHEN** a peer joins a room
- **THEN** their entry is created in the YMap with an empty YArray

#### Scenario: Peer identity
- **GIVEN** a peer's matchbox PeerId
- **THEN** that ID is used as the key in the pitch_sets YMap

### Requirement: Local Pitch Class Set Operations
The system SHALL provide add, remove, and toggle operations for the local peer's pitch class set.

#### Scenario: Add pitch class via voice (single-pitch mode)
- **WHEN** the user commits a pitch class via voice input
- **THEN** the pitch class replaces any existing pitch in the local peer's YArray (one pitch at a time for voice)

#### Scenario: Toggle existing pitch via voice
- **WHEN** the user commits the same pitch class already in their set via voice
- **THEN** it is removed (toggle off behavior)

#### Scenario: Remove pitch class via UI
- **WHEN** the user toggles off an active pitch class via button
- **THEN** the pitch class index is removed from the local peer's YArray

#### Scenario: Future multi-pitch interfaces
- **GIVEN** future input methods (keyboard, text input)
- **THEN** the YArray may hold multiple pitch classes per peer (not limited to voice's single-pitch behavior)

### Requirement: Pitch Class Toggle UI
The system SHALL display toggleable buttons for each active pitch class.

#### Scenario: Display active pitch classes
- **WHEN** pitch classes are in the local active set
- **THEN** they appear as toggle buttons showing their note name

#### Scenario: Toggle off via button
- **WHEN** the user clicks an active pitch class button
- **THEN** that pitch class is removed from the local set

### Requirement: CRDT State Synchronization
The system SHALL synchronize pitch class state to room peers using yrs over the existing matchbox reliable channel.

#### Scenario: Broadcast on local change
- **WHEN** the local pitch class set changes (add or remove)
- **THEN** the yrs document update is broadcast to all connected peers

#### Scenario: Merge incoming updates
- **WHEN** a yrs update is received from a peer
- **THEN** it is merged into the local document using yrs sync protocol

#### Scenario: Concurrent operations
- **WHEN** two peers concurrently add or remove pitch classes
- **THEN** both operations are preserved (CRDT semantics, no lost updates)

### Requirement: Room Combination Method
The system SHALL support configurable methods for combining peer pitch class sets into a room result.

#### Scenario: Combination method storage
- **GIVEN** a room's yrs document
- **THEN** it contains a "combination_method" field specifying how to combine peer sets

#### Scenario: Union method (default)
- **WHEN** combination_method is "union"
- **THEN** room result = all pitch classes active by any peer

#### Scenario: Intersection method
- **WHEN** combination_method is "intersection"
- **THEN** room result = only pitch classes active by all peers

#### Scenario: Change combination method
- **WHEN** any peer changes the combination_method
- **THEN** the change syncs to all peers and room result is recomputed

#### Scenario: Extensible methods
- **GIVEN** future combination methods (e.g., transformations, weighted voting)
- **THEN** the combination_method field supports string identifiers for extensibility

### Requirement: Room Pitch Class Result
The system SHALL compute and display the combined pitch class result based on the room's combination method.

#### Scenario: Compute room result
- **WHEN** peers have active pitch classes and a combination method is set
- **THEN** the room result is computed by applying the method to all peer sets

#### Scenario: Overlapping pitch classes
- **WHEN** multiple peers contribute to the same pitch class in the result
- **THEN** attribution shows which peer(s) contributed

#### Scenario: Attribute pitch classes to peers
- **WHEN** displaying the room result
- **THEN** each pitch class shows which peer(s) have it active (e.g., color-coded, list of names)

#### Scenario: Real-time result updates
- **WHEN** any peer adds or removes a pitch class
- **THEN** the room result is recomputed and UI updated immediately

#### Scenario: Peer disconnect
- **WHEN** a peer disconnects from the room
- **THEN** their entry is removed from pitch_sets YMap and result is recomputed

#### Scenario: Peer reconnect
- **WHEN** a previously disconnected peer rejoins
- **THEN** their pitch_sets entry is re-synced (may be empty or restored depending on session persistence)

### Requirement: Per-Room SCL Tuning CRDT
The system SHALL store the room's SCL tuning content as a yrs YText, allowing collaborative editing.

#### Scenario: Initialize room tuning
- **WHEN** a new room is created
- **THEN** the SCL YText is empty (defaults to 12-TET)

#### Scenario: Edit tuning
- **WHEN** any peer edits the SCL content
- **THEN** the change is synced to all peers via yrs

#### Scenario: Concurrent tuning edits
- **WHEN** two peers edit the SCL content simultaneously
- **THEN** changes are merged using yrs text CRDT semantics

#### Scenario: Tuning change propagation
- **WHEN** the SCL content changes
- **THEN** all peers re-parse and update their local tuning for pitch quantization
