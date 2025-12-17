## ADDED Requirements

### Requirement: Microphone Pitch Detection
The system SHALL detect the fundamental pitch from microphone audio using dual algorithms (BCF for fast feedback, pYIN for accurate commit) compiled to wasm and running in an AudioWorklet.

#### Scenario: Start pitch detection
- **WHEN** the user grants microphone permission and starts detection
- **THEN** the system captures audio via AudioWorklet and runs both BCF and pYIN in parallel

#### Scenario: Fast feedback via BCF
- **WHEN** audio is being captured
- **THEN** BCF provides low-latency pitch estimates (~15ms) for immediate UI feedback

#### Scenario: Accurate pitch via pYIN
- **WHEN** audio is being captured
- **THEN** pYIN provides smoothed, accurate pitch estimates (~50ms) for committing

#### Scenario: Confidence filtering
- **WHEN** the detected pitch has low confidence (background noise, no clear tone)
- **THEN** the system outputs no pitch (None) rather than a false detection

#### Scenario: Microphone permission denied
- **WHEN** the user denies microphone permission
- **THEN** the system displays an error explaining that mic access is required

### Requirement: Dynamic Noise Gate
The system SHALL implement a dynamic noise gate that adjusts its threshold based on ambient noise levels, enabling use in environments with speaker feedback.

#### Scenario: Adapt to ambient noise
- **WHEN** the microphone detects sustained background noise
- **THEN** the gate threshold rises to noise floor plus a margin (e.g., +6dB)

#### Scenario: Gate closed
- **WHEN** input signal is below the dynamic threshold
- **THEN** no pitch is reported (gate closed)

#### Scenario: Gate open
- **WHEN** input signal exceeds the dynamic threshold
- **THEN** pitch detection runs and reports detected pitch

#### Scenario: Threshold decay
- **WHEN** ambient noise decreases
- **THEN** the gate threshold slowly decays to match new noise floor

### Requirement: SCL Tuning Support
The system SHALL support Scala (.scl) tuning files for defining pitch classes, with 12-TET as the default.

#### Scenario: Load 12-TET default
- **WHEN** no custom tuning is set for the room
- **THEN** the system uses 12-tone equal temperament (12 pitch classes per octave)

#### Scenario: Parse SCL content
- **WHEN** the room has SCL content in its tuning CRDT
- **THEN** the system parses it and uses those pitch classes for quantization

#### Scenario: Invalid SCL content
- **WHEN** the SCL content is invalid or unparseable
- **THEN** the system displays a parse error and falls back to 12-TET

### Requirement: Hz to Pitch Class Quantization
The system SHALL quantize detected Hz values to the nearest pitch class in the current tuning, reporting both the pitch class and cents deviation.

#### Scenario: Quantize to nearest pitch class
- **WHEN** a pitch is detected at a given Hz
- **THEN** the system returns the pitch class index and cents deviation (-50 to +50)

#### Scenario: Quantize with octave normalization
- **WHEN** the same pitch class is sung in different octaves
- **THEN** they map to the same pitch class index (octave-equivalent)

### Requirement: Press-Hold-Release Pitch Capture
The system SHALL provide a press-hold-release UI pattern for capturing pitches.

#### Scenario: Hold to detect
- **WHEN** the user presses and holds the capture button
- **THEN** pitch detection starts and real-time BCF feedback is displayed

#### Scenario: Release to commit
- **WHEN** the user releases the capture button
- **THEN** the pYIN-detected pitch class is added to local active set (BCF fallback if pYIN unavailable)

#### Scenario: Cancel capture
- **WHEN** the user releases with no stable pitch detected
- **THEN** no pitch class is added (capture cancelled)

### Requirement: Real-Time Pitch Feedback
The system SHALL display real-time feedback during pitch capture showing note name, cents deviation, and a visual closeness indicator.

#### Scenario: Display during capture
- **WHEN** the user is holding the capture button and singing
- **THEN** the UI shows note name (e.g., "A4"), cents deviation, and a tuning indicator

#### Scenario: Closeness indicator
- **WHEN** a pitch is detected
- **THEN** the UI shows how close the pitch is to the target (e.g., needle gauge, color gradient)

#### Scenario: No pitch detected
- **WHEN** no clear pitch is detected during capture (gate closed or low confidence)
- **THEN** the UI indicates "No pitch detected" or similar
