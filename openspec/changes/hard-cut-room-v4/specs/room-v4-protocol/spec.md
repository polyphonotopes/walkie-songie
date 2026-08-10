## ADDED Requirements

### Requirement: Independent causal lanes

The system SHALL represent a Room v4 as an exact `MusicLang` store and an
independent walkie extension store, each with its own author heads, frontier,
entry identities, repair namespace, courier namespace, and durable lane tag.

#### Scenario: Bare music operation enters a walkie room

- **WHEN** a valid topic-bound `MusicLang` operation from a bare peer is received
- **THEN** walkie stores the exact signed bytes in the music lane and derives the
  same operation and entry identities as the bare peer
- **THEN** no extension predecessor, entry, or frame enters that lane

### Requirement: Hard protocol-generation boundary

Every live native and browser room runtime MUST use the Room v4 topic, ticket,
discovery, presence, journal, repair, and courier identities and MUST NOT fall
back to a v3 room artifact or ALPN.

#### Scenario: A v3 peer or artifact reaches Room v4

- **WHEN** a Room v4 runtime receives a v3 ticket, journal, presence frame, room
  operation, rendezvous hello, or repair negotiation
- **THEN** it refuses the artifact before it can mutate either lane

### Requirement: Persistence before visibility

The native and browser runtimes MUST durably store an operation's exact
lane-tagged wire bytes before making that operation visible in memory or the UI.

#### Scenario: Durable append fails

- **WHEN** verification succeeds but the lane journal append fails
- **THEN** the operation remains absent from both lane stores and the composed
  room view
- **THEN** the runtime surfaces a persistence error

### Requirement: Lane-scoped anti-entropy

The system SHALL run repair and courier exchanges independently per lane, with
the authenticated ALPN as the authoritative lane and purpose discriminator.

#### Scenario: Full peers repair divergent rooms

- **WHEN** two full peers have divergent histories in both lanes
- **THEN** they negotiate separate music and extension repair connections
- **THEN** both lane identity sets, roots, and the composed room view converge
- **THEN** no repair or courier frame contains another lane's operation bytes or
  entry identities

### Requirement: Capability-aware synchronization

Tickets and rendezvous hellos SHALL advertise a non-empty valid lane set, and a
peer SHALL be reported synchronized only after every advertised or successfully
negotiated lane has completed without root mismatch or incompleteness.

#### Scenario: Music-only peer synchronizes

- **WHEN** a peer advertises only the music lane and completes music repair
- **THEN** the full walkie peer reports it synchronized without attempting to
  require the extension lane

### Requirement: Cross-runtime release evidence

Automated release gates MUST exercise browser/browser, browser/native, and bare
music interoperability, durable reload, corruption refusal, dropped-gossip
repair, convergence, and byte-level lane isolation.

#### Scenario: Browser reloads a repaired room

- **WHEN** a browser repairs both lanes, persists every accepted operation, and
  reloads from IndexedDB
- **THEN** both lanes recover with no pending operations and reproduce the same
  roots and composed view
