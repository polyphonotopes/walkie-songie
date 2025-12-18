# pitch-keyboard-ui Specification

## Purpose
TBD - created by archiving change upgrade-pitch-keyboard. Update Purpose after archive.
## Requirements
### Requirement: Circular Keyboard Display
The system SHALL display pitch classes using the all-around-keyboard web component in a circular layout.

#### Scenario: Render keyboard matching tuning
- **WHEN** the app loads with a tuning of N pitch classes
- **THEN** the keyboard displays N keys arranged in a circle

#### Scenario: Dynamic tuning changes
- **WHEN** the user changes to a microtonal tuning (e.g., 19-TET)
- **THEN** the keyboard re-renders with the new number of keys

#### Scenario: Raised keys adapt to tuning
- **WHEN** the tuning changes
- **THEN** the raised-notes pattern updates using a heuristic (e.g., 12-TET uses standard piano, other scales derive from interval sizes or use flat/pie mode)

### Requirement: Keyboard Interaction
The system SHALL allow users to toggle pitch classes by clicking keyboard keys.

#### Scenario: Click to toggle pitch on
- **WHEN** a user clicks an inactive key
- **THEN** that pitch class is added to the local peer's set

#### Scenario: Click to toggle pitch off
- **WHEN** a user clicks an active key
- **THEN** that pitch class is removed from the local peer's set

### Requirement: Dual Visual Feedback
The system SHALL use two distinct visual states: "lit" for detected pitches and "pressed" for active pitch classes.

#### Scenario: Show detected pitch in real-time
- **WHEN** the voice detector identifies a pitch
- **THEN** the corresponding key is lit (highlight state) as real-time feedback

#### Scenario: Show active pitch classes
- **WHEN** the local peer has pitch classes in their set
- **THEN** those keys are pressed (active state)

#### Scenario: Detected pitch dims when voice stops
- **WHEN** voice input stops or pitch changes
- **THEN** the previously lit key dims and the new pitch lights up

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

