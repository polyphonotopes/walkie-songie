## ADDED Requirements

### Requirement: Always In A Room
The system SHALL ensure users are always in a room, with no "roomless" state.

#### Scenario: First visit auto-generates room
- **WHEN** a user visits the app with no room in the URL
- **THEN** a random room name is generated and they join it automatically

#### Scenario: Room name always visible
- **WHEN** the app is running
- **THEN** the current room name is displayed in the header/corner

#### Scenario: Room persists in URL
- **WHEN** a user is in a room
- **THEN** the URL reflects the room name (e.g., `?room=sunny-garden-melody`)

### Requirement: Room Menu Overlay
The system SHALL provide an overlay UI for sharing and switching rooms.

#### Scenario: Open overlay via room name
- **WHEN** the user clicks the room name or QR button
- **THEN** an overlay appears showing room QR code, name, and controls

#### Scenario: Dismiss overlay
- **WHEN** the user clicks outside the overlay or presses Escape
- **THEN** the overlay closes and returns to the main view

#### Scenario: Shuffle to new room
- **WHEN** the user clicks the shuffle button in the overlay
- **THEN** a new random room name is generated and they join it

#### Scenario: Join specific room
- **WHEN** the user types a room name and submits
- **THEN** they leave the current room and join the specified room

### Requirement: Wholesome Room Codes
The system SHALL generate room names using a curated wholesome word list.

#### Scenario: Word format
- **WHEN** a room name is generated
- **THEN** it follows adjective-noun-noun format (e.g., `sunny-garden-melody`)

#### Scenario: Wholesome vocabulary
- **WHEN** room names are generated
- **THEN** only positive, family-friendly words are used

#### Scenario: Deterministic joining
- **WHEN** two users enter the same room name
- **THEN** they join the same P2P channel (name hashes to topic ID)

### Requirement: QR Code Sharing
The system SHALL generate scannable QR codes for room sharing.

#### Scenario: QR encodes room URL
- **WHEN** the overlay is open
- **THEN** it displays a QR code encoding the full room URL

#### Scenario: Scan to join
- **WHEN** another user scans the QR code
- **THEN** they are directed to the app and join that room
