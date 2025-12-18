## ADDED Requirements

### Requirement: Dynamic Noise Gate
The system SHALL implement a voice conditioner that combines adaptive noise estimation, voice activity detection (VAD), automatic gain control (AGC), and hysteresis gating to isolate singing voice in noisy environments with musical feedback.

#### Scenario: Adapt to ambient noise
- **WHEN** the microphone detects sustained background noise (music, bar ambience)
- **THEN** the noise floor estimate updates via exponential moving average with ~1s time constant

#### Scenario: Voice activity detection
- **WHEN** audio energy exceeds the noise floor AND shows voice-like characteristics (zero-crossing rate, pitch confidence)
- **THEN** the VAD flags the frame as containing voice

#### Scenario: Gate opens for voice
- **WHEN** VAD detects voice AND signal exceeds open threshold (noise_floor + 9dB)
- **THEN** the gate opens and audio passes to pitch detection

#### Scenario: Gate closes with hysteresis
- **WHEN** signal drops below close threshold (noise_floor + 3dB) for sustained duration (~50ms)
- **THEN** the gate closes, preventing pitch detection on ambient sounds

#### Scenario: Gate rejects music feedback
- **WHEN** loud music plays through speakers and feeds back into mic
- **THEN** the VAD distinguishes it from voice (different zero-crossing pattern, no clear pitch confidence) and gate remains closed

#### Scenario: Automatic gain control
- **WHEN** the gate is open and voice is detected
- **THEN** the AGC normalizes the voice signal to a consistent level (-12dBFS target) for stable pitch detection

#### Scenario: Quiet singing captured
- **WHEN** a user sings quietly but clearly above the adaptive noise floor
- **THEN** the AGC boosts the signal and pitch detection succeeds

#### Scenario: Threshold decay
- **WHEN** ambient noise decreases over time
- **THEN** the noise floor estimate slowly decays to match, lowering the gate thresholds

### Requirement: Reference Level Calibration
The system SHALL calibrate a "reference loudness" when the user begins singing, using this as a trust anchor for confidence scoring.

#### Scenario: Capture reference level on voice start
- **WHEN** voice input starts and the first confident pitch is detected (clear voice, high confidence)
- **THEN** the system captures that RMS level as the "reference level" for this session

#### Scenario: Boost confidence for signals near reference
- **WHEN** subsequent audio has RMS within ~3dB of the reference level
- **THEN** the confidence score receives a boost (signal likely from intended close-mic singing)

#### Scenario: Reduce confidence for quiet signals
- **WHEN** subsequent audio has RMS more than 12dB below the reference level
- **THEN** the confidence score is attenuated (signal may be ambient bleed, not intentional singing)

#### Scenario: Reference level adapts upward
- **WHEN** a louder signal is detected with high pitch confidence
- **THEN** the reference level slowly adapts upward (user moved closer to mic)

#### Scenario: Reference level does not adapt downward quickly
- **WHEN** signal level drops
- **THEN** the reference level decays very slowly (prevents ambient noise from lowering the bar)

#### Scenario: Reference resets on new session
- **WHEN** voice input stops and restarts
- **THEN** the reference level is cleared and recalibrated from the new first confident detection

### Requirement: Voice Conditioner Configuration
The system SHALL allow optional configuration of voice conditioner parameters for different environments.

#### Scenario: Default configuration works in noisy bar
- **WHEN** the app starts with default settings
- **THEN** the voice conditioner uses conservative defaults suitable for high-noise environments

#### Scenario: Sensitivity adjustment
- **WHEN** the user adjusts voice sensitivity (if exposed in UI)
- **THEN** the gate open/close thresholds shift relative to noise floor

#### Scenario: Bypass conditioner
- **WHEN** debugging or in quiet environment
- **THEN** the conditioner can be bypassed to pass raw audio to pitch detector
