## ADDED Requirements

### Requirement: Circular Keyboard Display
The system SHALL display pitch classes using the all-around-keyboard web component in a circular layout.

#### Scenario: Render keyboard matching tuning
- **WHEN** the app loads with a tuning of N pitch classes
- **THEN** the keyboard displays N keys arranged in a circle

#### Scenario: Dynamic tuning changes
- **WHEN** the user changes to a microtonal tuning (e.g., 19-TET)
- **THEN** the keyboard re-renders with the new number of keys

### Requirement: Keyboard Interaction
The system SHALL allow users to toggle pitch classes by clicking keyboard keys.

#### Scenario: Click to toggle pitch on
- **WHEN** a user clicks an inactive key
- **THEN** that pitch class is added to the local peer's set

#### Scenario: Click to toggle pitch off
- **WHEN** a user clicks an active key
- **THEN** that pitch class is removed from the local peer's set

### Requirement: Active Pitch Visualization
The system SHALL visually highlight active pitch classes on the keyboard.

#### Scenario: Show local active pitches
- **WHEN** the local peer has pitch classes in their set
- **THEN** those keys are visually pressed/highlighted on the keyboard

#### Scenario: Voice commit highlights key
- **WHEN** a pitch is committed via voice input
- **THEN** the corresponding key is highlighted before being toggled
