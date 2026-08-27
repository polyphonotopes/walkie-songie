# Browser Replica Worker Specification

## Purpose

Define the browser execution boundary that keeps authoritative HHHS admission,
durability, repair stepping, and materialization off the window thread while
keeping presentation and carrier effects explicit.

## Requirements

### Requirement: Authoritative browser data plane runs off the window thread

The browser SHALL place both durable Room-v5 Replica lanes, their IndexedDB
logs, capability admission, and materialization in one dedicated worker while
the window retains presentation and carrier objects.

#### Scenario: Local durable command

- **WHEN** the window submits a valid room command
- **THEN** the worker persists and admits it before emitting its authoritative
  projection revision and public outbound record
- **AND** the window thread performs no HHHS admission or IndexedDB log write

### Requirement: Worker placement preserves repair

The worker boundary SHALL carry bounded repair frames without moving Iroh or
WebRTC ownership into HHHS or the worker.

#### Scenario: Offline peers reconnect

- **WHEN** two browser peers make independent durable edits while partitioned
- **AND** their window-owned carrier reconnects
- **THEN** frame-stepped worker repair converges both lane histories and views

### Requirement: Projection continuity is explicit

Projection subscriptions SHALL begin with a snapshot and continue through
exact revisions; subscriber lag or worker restart SHALL produce an explicit
reset snapshot.

#### Scenario: Worker restarts

- **WHEN** a worker generation terminates after durable commits
- **THEN** a replacement worker recovers the same Room-v5 history from
  IndexedDB
- **AND** the window rejects stale-generation events and applies a reset
