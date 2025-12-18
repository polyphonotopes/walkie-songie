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

