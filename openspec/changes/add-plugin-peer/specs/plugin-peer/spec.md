# plugin-peer Specification

## Purpose

VST3/CLAP audio plugin that connects to walkie-songie P2P channels, enabling DAW users to collaborate with mobile and web users.

## ADDED Requirements

### Requirement: Plugin Feature Gate
The plugin code SHALL be conditionally compiled behind the `plugin` feature flag.

#### Scenario: Feature disabled by default
- **WHEN** building without `--features plugin`
- **THEN** no plugin-related code is compiled
- **AND** no nih-plug dependencies are included

#### Scenario: Feature enabled
- **WHEN** building with `--features plugin`
- **THEN** the plugin binary target is available
- **AND** VST3 and CLAP exports are generated

### Requirement: Realtime-Safe Networking
The plugin SHALL perform all networking operations on a dedicated background thread, never on the audio thread.

#### Scenario: Audio thread isolation
- **WHEN** the plugin processes audio
- **THEN** no network I/O, DNS lookups, or blocking operations occur on the audio thread

#### Scenario: Network thread communication
- **WHEN** the UI or audio thread needs to send/receive network data
- **THEN** communication occurs via lock-free message channels

#### Scenario: Network thread lifecycle
- **WHEN** the plugin is instantiated
- **THEN** a background networking thread is spawned
- **AND** when the plugin is dropped, the thread shuts down gracefully

### Requirement: QR Code Channel Display
The plugin editor SHALL display a QR code that mobile users can scan to join the same channel.

#### Scenario: QR code generation
- **WHEN** the plugin editor opens
- **THEN** a QR code is displayed containing the current channel address

#### Scenario: QR code content format
- **WHEN** a QR code is generated
- **THEN** it contains a URL in the format `walkie-songie://channel/<address>`

#### Scenario: QR code updates on channel change
- **WHEN** the channel address changes
- **THEN** the QR code regenerates to reflect the new address

### Requirement: Channel Shuffle
The plugin editor SHALL provide a button to generate a new random channel address.

#### Scenario: Shuffle button press
- **WHEN** the user clicks the shuffle button
- **THEN** a new random channel address is generated
- **AND** the plugin reconnects to the new channel
- **AND** the QR code updates

### Requirement: Custom Channel Address
The plugin editor SHALL allow users to enter a custom channel address.

#### Scenario: Enter custom address
- **WHEN** the user types a channel address in the text input
- **AND** confirms the input
- **THEN** the plugin connects to the specified channel
- **AND** the QR code updates to the new address

#### Scenario: Invalid address handling
- **WHEN** the user enters an invalid channel address
- **THEN** the plugin displays an error message
- **AND** does not disconnect from the current channel

### Requirement: Channel Address Persistence
The plugin SHALL persist the current channel address in its saved state.

#### Scenario: Save state
- **WHEN** the DAW saves the plugin state (project save, preset save)
- **THEN** the current channel address is included in the saved state

#### Scenario: Restore state
- **WHEN** the DAW loads a saved plugin state
- **THEN** the plugin restores the saved channel address
- **AND** reconnects to that channel

### Requirement: MIDI Input to Room
The plugin SHALL broadcast incoming MIDI note events to the connected room.

#### Scenario: MIDI note on
- **WHEN** the plugin receives a MIDI note-on event
- **THEN** the corresponding pitch class is toggled on in the local peer's pitch set
- **AND** the change is broadcast to connected peers

#### Scenario: MIDI note off
- **WHEN** the plugin receives a MIDI note-off event
- **THEN** the corresponding pitch class is toggled off in the local peer's pitch set
- **AND** the change is broadcast to connected peers

### Requirement: Room to MIDI Output
The plugin SHALL output MIDI note events when remote peers change their pitch sets.

#### Scenario: Remote peer note on
- **WHEN** a remote peer adds a pitch class to their set
- **THEN** the plugin outputs a MIDI note-on event for that pitch

#### Scenario: Remote peer note off
- **WHEN** a remote peer removes a pitch class from their set
- **THEN** the plugin outputs a MIDI note-off event for that pitch

### Requirement: Multiple Instance Support
Multiple plugin instances in the same DAW session SHALL operate independently.

#### Scenario: Independent peer identities
- **WHEN** multiple plugin instances are loaded
- **THEN** each instance receives a unique peer ID

#### Scenario: Independent channels
- **WHEN** multiple plugin instances are configured with different channel addresses
- **THEN** each instance connects to its own channel independently

#### Scenario: Same channel collaboration
- **WHEN** multiple plugin instances join the same channel
- **THEN** they discover each other via local P2P and can exchange pitch data
