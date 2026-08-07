## ADDED Requirements

### Requirement: Native Desktop MIDI Ports
The Tauri backend SHALL enumerate, open, monitor, and release native MIDI input and output ports independently of Web MIDI support in the webview.

#### Scenario: Device hot-plug
- **WHEN** a MIDI device is connected or removed while the app runs
- **THEN** the backend updates the device list and preserves or cleanly releases affected routing state

#### Scenario: Frontend reload
- **WHEN** the webview reloads
- **THEN** native MIDI ports remain backend-owned and are not duplicated

### Requirement: Source-Balanced Polyphonic Output
The MIDI engine SHALL track sounding notes by logical source and balance every note-on with a corresponding note-off without collapsing different peers or sources.

#### Scenario: Two peers sing different notes
- **WHEN** two fresh voice presences contain different periodic pitches
- **THEN** both notes sound concurrently and updating one does not stop the other

#### Scenario: Two sources share one note
- **WHEN** two logical sources produce the same output note
- **THEN** releasing one source does not silence the remaining source

#### Scenario: Voice expires
- **WHEN** a voice presence lease expires
- **THEN** only that presence's allocated note is released

### Requirement: Tuning-Aware MIDI Input
Incoming MIDI note messages SHALL be converted through their defined 12-TET frequency and quantized against the current room tuning, with held-input identity tracked until release.

#### Scenario: Input in a 19-degree tuning
- **WHEN** a MIDI note-on arrives while a 19-degree tuning is active
- **THEN** its frequency is quantized through the tuning rather than reduced modulo 19

#### Scenario: Velocity-zero note-on
- **WHEN** a MIDI note-on has velocity zero
- **THEN** it is handled as a note-off for the matching held input

#### Scenario: Multiple held keys quantize to one degree
- **WHEN** multiple physical inputs quantize to the same degree
- **THEN** releasing one input does not retract the degree until the local source policy says no held input remains

### Requirement: Exact 12-TET Output
For a compatible 12-TET mapping, the MIDI engine SHALL preserve periodic pitch as the exact MIDI note number when it lies in the supported range.

#### Scenario: Voice A4
- **WHEN** live or durable state specifies standard-mapped A4
- **THEN** the engine emits MIDI note 69 rather than a middle-octave pitch-class substitute

#### Scenario: Pitch outside MIDI range
- **WHEN** a periodic pitch cannot be represented as MIDI note 0 through 127
- **THEN** the engine reports it as unrepresentable and does not silently clamp it to a different pitch

### Requirement: Explicit Microtonal Output
For non-12-TET tunings, the MIDI engine SHALL use configured per-note pitch output or explicitly report that exact output is unavailable; silent proportional folding into 12-TET is forbidden.

#### Scenario: MPE-compatible destination
- **WHEN** exact microtonal output is enabled for an MPE-compatible destination
- **THEN** the engine allocates a member channel, sends a base note plus calculated pitch bend, and resets the bend when the note is released

#### Scenario: Channel pool exhausted
- **WHEN** all configured member channels are occupied
- **THEN** the documented deterministic policy rejects or steals a voice with balanced cleanup and reports the event

#### Scenario: Non-microtonal destination
- **WHEN** the destination cannot represent the current pitch exactly
- **THEN** approximation remains disabled unless the user explicitly opts in and the UI marks the output as approximate

### Requirement: Stuck-Note Prevention
The MIDI engine SHALL release affected notes and reset controllers on room, tuning, device, presence, and application lifecycle boundaries.

#### Scenario: Change tuning
- **WHEN** the room changes to a different TuningId
- **THEN** all notes from the prior tuning receive balanced release/reset messages before new-tuning notes start

#### Scenario: Leave room or exit
- **WHEN** the user leaves a room or the application shuts down
- **THEN** the engine releases every tracked source and sends the configured all-notes-off/reset sequence to each active destination
