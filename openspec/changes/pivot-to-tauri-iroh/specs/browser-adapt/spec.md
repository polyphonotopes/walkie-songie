## ADDED Requirements

### Requirement: Shared Browser Adapter Contract
The system SHALL define Tauri-independent typed client commands, application events, snapshots, errors, and capability negotiation that can be carried by either Tauri IPC or an optional browser adapter.

#### Scenario: Same command on two adapters
- **WHEN** the Tauri frontend and a bridged browser frontend submit the same valid client command against equivalent state
- **THEN** the native runtime applies the same validation and produces equivalent domain events

#### Scenario: Unsupported capability
- **WHEN** an adapter does not provide a requested native capability
- **THEN** it reports that capability as unavailable and returns a structured error rather than emulating incorrect behavior

### Requirement: Authenticated Loopback Bridge
The optional browser bridge SHALL expose the native Iroh runtime only on loopback and require per-launch authentication without exposing participant secret keys.

#### Scenario: Authenticated extension connects
- **WHEN** an allowed Agregore or Peersky extension presents the current bridge token
- **THEN** it can register for a snapshot and ordered application events subject to the same command and resource limits as Tauri

#### Scenario: Remote host attempts connection
- **WHEN** a non-loopback network address attempts to reach the bridge
- **THEN** no bridge listener is reachable on that interface

#### Scenario: Missing or stale token
- **WHEN** a client omits the token or presents a token from an earlier bridge launch
- **THEN** the bridge rejects the connection before exposing state

#### Scenario: Adapter requests identity material
- **WHEN** a browser adapter requests the participant's secret signing seed
- **THEN** the bridge rejects the request because secret key export is not part of the adapter contract

### Requirement: Shared Agregore and Peersky Extension
The project SHALL prefer one minimal Manifest V3 extension package for current Agregore and Peersky over maintaining browser-specific application forks.

#### Scenario: Load extension in Agregore
- **WHEN** the extension is installed in a supported Agregore release and the native bridge is running
- **THEN** the walkie-songie frontend can connect, receive capabilities, join a room, and exchange musical state through native Iroh

#### Scenario: Load extension in Peersky
- **WHEN** the same extension package is installed in a supported Peersky release and the native bridge is running
- **THEN** the walkie-songie frontend can connect, receive capabilities, join a room, and exchange musical state through native Iroh

#### Scenario: Bridge is absent
- **WHEN** the extension loads without a native bridge
- **THEN** it remains a non-peer UI, explains that native Iroh is unavailable, and does not claim direct or relayed connectivity

### Requirement: Adapter Is Not the Critical Path
The Tauri desktop peer SHALL be releasable without completing or shipping the optional Agregore and Peersky adapter.

#### Scenario: Adapter spike fails
- **WHEN** either browser lacks a stable required extension capability
- **THEN** the Tauri release continues and the adapter remains experimental without changing the room wire protocol
