## ADDED Requirements

### Requirement: Tauri Desktop Peer
The system SHALL provide a Tauri 2 desktop application for Linux, macOS, and Windows that packages the existing Trunk-built Rust frontend and runs native peer services in its Rust backend.

#### Scenario: Launch packaged application
- **WHEN** a user launches a supported desktop package
- **THEN** the application loads the existing musical interface and starts the native backend without requiring an external browser or sidecar

#### Scenario: Build from a clean checkout
- **WHEN** CI builds with the checked-in stable toolchain and lockfile
- **THEN** Trunk produces the frontend and Tauri produces the platform package using pinned stable dependencies

### Requirement: Native Service Ownership
The Tauri backend SHALL exclusively own the Iroh endpoint, durable room store, peer presence, identity, and native MIDI resources.

#### Scenario: Frontend reload
- **WHEN** the webview reloads while the application process remains alive
- **THEN** native peer state remains owned by the backend and the frontend can request a fresh snapshot without creating a second endpoint

#### Scenario: Application shutdown
- **WHEN** the application exits normally
- **THEN** the backend cancels network tasks, flushes durable state, releases MIDI ports, balances sounding notes, and closes the Iroh endpoint

### Requirement: Typed Ordered IPC
The frontend SHALL submit typed user intents through Tauri commands and receive ordered state snapshots and deltas through a Tauri channel.

#### Scenario: Concurrent peer updates
- **WHEN** multiple peer and musical updates arrive while the UI is active
- **THEN** the frontend observes them in backend emission order and can deterministically reach the backend snapshot

#### Scenario: Frontend reconnects to IPC
- **WHEN** the frontend registers a new channel after reload
- **THEN** the backend sends a complete snapshot before subsequent deltas

#### Scenario: Backend rejects an action
- **WHEN** a command contains invalid room, tuning, or musical data
- **THEN** the command returns a structured error and emits no partial state mutation

### Requirement: Desktop Persistence
The system SHALL persist the participant identity and verbatim signed room operations in the platform application-data directory with crash-safe updates.

#### Scenario: Restart application
- **WHEN** the application restarts after a normal exit or interrupted write
- **THEN** it restores the same endpoint/author identity and the last complete valid operation journal prefix

#### Scenario: Incompatible journal generation
- **WHEN** the application encounters a journal from an unsupported operation generation
- **THEN** it refuses silent import and offers an explicit reset or export path
