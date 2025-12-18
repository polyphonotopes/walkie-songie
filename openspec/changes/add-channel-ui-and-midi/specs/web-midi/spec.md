## ADDED Requirements

### Requirement: MIDI Output for Room Pitches
The system SHALL output room pitches as MIDI note messages to connected MIDI devices.

#### Scenario: Toggle set outputs on channel 1
- **WHEN** a pitch is toggled on/off in the shared toggle set (manual keyboard presses)
- **THEN** a MIDI note-on/off message is sent on channel 1

#### Scenario: Voice pitches output on channel 2
- **WHEN** a peer's voice pitch is detected
- **THEN** a MIDI note-on message is sent on channel 2 with the nearest MIDI note number (preserving octave)

#### Scenario: Voice note-off on silence
- **WHEN** a peer stops singing (gate closes)
- **THEN** a MIDI note-off is sent on channel 2 for their previous pitch

#### Scenario: Multi-peer voice pitches
- **WHEN** multiple peers are singing simultaneously
- **THEN** each voice pitch is sent as a separate note on channel 2 (polyphonic)

### Requirement: MIDI Input from Controllers
The system SHALL receive MIDI input from connected controllers and route it to the shared toggle set.

#### Scenario: Controller note-on toggles pitch
- **WHEN** a MIDI note-on is received from an external controller
- **THEN** the corresponding pitch class is added to the shared toggle set

#### Scenario: Controller note-off toggles pitch
- **WHEN** a MIDI note-off is received from an external controller
- **THEN** the corresponding pitch class is removed from the shared toggle set

#### Scenario: Connect to all inputs
- **WHEN** Web MIDI access is granted
- **THEN** the system listens to all available MIDI input devices

### Requirement: MIDI Device Management
The system SHALL handle MIDI device connection and disconnection gracefully.

#### Scenario: Request MIDI access
- **WHEN** the app starts
- **THEN** it requests Web MIDI API access from the browser

#### Scenario: No MIDI fallback
- **WHEN** Web MIDI is not supported or access is denied
- **THEN** the app functions normally without MIDI (graceful degradation)

#### Scenario: Output to all devices
- **WHEN** MIDI messages are sent
- **THEN** they are sent to all connected MIDI output devices

### Requirement: Graceful Note Transitions
The system SHALL cleanly transition MIDI notes when switching rooms or devices to avoid stuck notes.

#### Scenario: Leaving a room
- **WHEN** the user leaves a room (switches to another room)
- **THEN** note-off messages are sent for all currently active notes on both channels

#### Scenario: Entering a room
- **WHEN** the user joins a room with existing active notes
- **THEN** note-on messages are sent for the current toggle set and voice pitches

#### Scenario: MIDI output device changes
- **WHEN** a new MIDI output device is connected
- **THEN** the current room state is sent as note-ons to the new device

#### Scenario: All notes off on disconnect
- **WHEN** leaving a room or closing the app
- **THEN** an "all notes off" (CC 123) is sent to ensure no stuck notes
