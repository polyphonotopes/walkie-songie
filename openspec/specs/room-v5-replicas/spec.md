# Room v5 Replicas Specification

## Purpose

Define Walkie-Songie's durable collaboration boundary: two independent,
capability-native HHHS replicas whose storage, admission, repair, and
materialization semantics stay independent of the application-owned carrier
and presentation layers.

## Requirements

### Requirement: Independent capability-native replicas

The system SHALL represent a Room v5 as independent music and extension HHHS
Replicas with disjoint namespaces, causal histories, capability roots, storage,
repair protocols, and materialization checkpoints.

#### Scenario: One lane advances

- **WHEN** a valid music command is admitted while the extension lane is idle
- **THEN** only the music Replica history, checkpoint, and view advance
- **THEN** no extension frontier or entry becomes a predecessor of that command

### Requirement: Proof-bound command admission

Every durable command MUST be authorized by an explicitly presented, causally
live capability path whose receiver, exact command-derived area, and `Invoke`
right match the command payload.

#### Scenario: Connected peer lacks authority

- **WHEN** a connected peer advertises and negotiates the command's lane but
  presents no matching live grant
- **THEN** the Replica refuses the command as unauthorized
- **THEN** connectivity, transport identity, and protocol support do not affect
  the authorization result

### Requirement: Capability-native lifecycle

Top-level grants, attenuating delegations, revocations, and renunciations SHALL
be canonical lane history and SHALL take effect according to their causal
positions without role-list or membership lookup.

#### Scenario: Concurrent barrier revocation

- **WHEN** a barrier revocation is concurrent with an action using its target
  grant
- **THEN** the capability evaluator applies the defined concurrent barrier rule
- **THEN** actions causally predating the revocation remain historical facts

### Requirement: Storage before publication

Replica admission MUST atomically persist canonical history and any associated
local evidence, secrets, and checkpoints before publishing growth to room or UI
subscribers.

#### Scenario: Atomic storage commit fails

- **WHEN** a storage adapter refuses or fails the admission transaction
- **THEN** no part of the command becomes visible in canonical history,
  materialized views, secrets, checkpoints, or subscriptions

### Requirement: Rebuildable materialization

Music and extension views SHALL be rebuildable from immutable canonical
snapshots, and a persisted checkpoint MUST be accepted only when its state and
history anchors validate.

#### Scenario: Checkpoint does not match history

- **WHEN** a stored projection checkpoint is corrupt or anchored to a different
  history root
- **THEN** the host discards or refuses it and rebuilds from canonical history
- **THEN** canonical history is never changed to fit a projection

### Requirement: Application-owned carrier

HHHS SHALL expose Replica repair through a transport-neutral host and frame
driver. Walkie SHALL own endpoints, discovery, meshes, relays, WebRTC, IPC,
protocol negotiation, peer lifecycle, and execution placement.

#### Scenario: Carrier is replaced

- **WHEN** a room uses loopback, Iroh QUIC, WebRTC, a pipe, or IPC as its ordered
  frame carrier
- **THEN** it drives the same lane Replica repair contract without changing
  command, capability, history, or materialization semantics

### Requirement: Protocol support is not authority

Tickets and rendezvous hellos SHALL describe supported lane protocols using a
non-empty `ProtocolSupport` value. This value MUST be used only for connection
attempts and synchronization reporting.

#### Scenario: Music-only peer joins

- **WHEN** a bare music peer advertises only music support and presents a live
  music capability
- **THEN** the full Walkie peer repairs and collaborates with it on the music
  Replica without requiring the extension protocol
- **THEN** the bare peer receives no extension entry, proof, or frame

### Requirement: Hard functional generation boundary

Room v5 MUST preserve the user-visible music and extension behaviours required
by Room v4, but MUST NOT require v4 signed bytes, p2panda headers, operation
identities, source-log APIs, journals, or compatibility fallback.

#### Scenario: Room v4 artifact reaches Room v5

- **WHEN** a v4 ticket, command frame, journal, repair ALPN, or presence message
  reaches a Room v5 host
- **THEN** the host refuses it before either Replica mutates

### Requirement: Cross-runtime release evidence

Automated release gates MUST exercise native and browser restart, capability
admission and revocation, dropped-broadcast repair, offline rejoin, composed
view convergence, carrier substitution, and bare music isolation.

#### Scenario: Browser and native peers rejoin

- **WHEN** browser and native peers author on both sides of a partition and then
  repair both supported lanes
- **THEN** their per-lane history roots and materialized views converge
- **THEN** their composed Room views agree and no lane crosses its carrier or
  authority boundary
