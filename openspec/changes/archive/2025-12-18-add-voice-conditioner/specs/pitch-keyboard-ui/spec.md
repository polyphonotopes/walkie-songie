## ADDED Requirements

### Requirement: Pitch Confidence Indicator
The system SHALL display a circling dot indicator on top of the keyboard that shows the detected pitch position and confidence in real-time during voice input.

#### Scenario: Dot follows detected pitch
- **WHEN** voice input detects a pitch
- **THEN** a dot appears at the corresponding angular position on the circular keyboard (overlaid on top)

#### Scenario: Dot shows confidence via appearance
- **WHEN** pitch is detected with varying confidence levels
- **THEN** the dot's visual properties reflect confidence:
  - High confidence: solid, bright, larger
  - Medium confidence: semi-transparent, medium size
  - Low confidence: faint, smaller, possibly jittery

#### Scenario: Dot shows pitch precision via position
- **WHEN** pitch is detected between two pitch classes (e.g., 30 cents sharp of C)
- **THEN** the dot appears at the interpolated angular position (not snapped to keys)

#### Scenario: Dot disappears when gate closed
- **WHEN** the voice conditioner gate is closed (no voice detected)
- **THEN** the dot fades out or disappears

#### Scenario: Dot distinct from key states
- **WHEN** the dot overlays a lit or pressed key
- **THEN** the dot remains visually distinct (different layer, color, or glow effect)

### Requirement: Continuous Pitch Tracking Display
The system SHALL provide smooth, continuous visual feedback of pitch position rather than discrete jumps.

#### Scenario: Smooth dot movement
- **WHEN** the user slides between pitches while singing
- **THEN** the dot animates smoothly around the circle following the pitch

#### Scenario: Stable when holding pitch
- **WHEN** the user holds a steady pitch
- **THEN** the dot remains stable (minimal jitter) with slight natural variation

#### Scenario: Fast response to pitch changes
- **WHEN** the user jumps to a new pitch
- **THEN** the dot moves to the new position within ~50ms (perceptually instant)
