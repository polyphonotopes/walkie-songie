# tutti-bridge Specification

## Purpose

A host-owned bridge between Walkie Iroh peers and nearby Tutti ESP32 replicas
using authenticated BLE GATT.

## ADDED Requirements

### Requirement: Two carrier legs
The bridge SHALL synchronize its Room-v5 music replica with remote peers over
Iroh and with a gateway ESP32 over the shared Tutti BLE HHHS repair lane.

#### Scenario: Entry crosses the bridge
- **WHEN** a canonical music record is admitted from either carrier leg
- **THEN** ordinary HHHS repair can serve that same byte-exact record to the
  other leg
- **AND** no transport-specific record identity or causal model is introduced

#### Scenario: Protocol mismatch
- **WHEN** the bridge and ESP disagree on music generation, namespace,
  authority profile, or HHHS repair strategy
- **THEN** the bridge refuses the repair lane
- **AND** does not translate or downgrade the history

### Requirement: No-pairing authenticated BLE
The default jam flow SHALL connect through the Tutti GATT service without
requiring OS pairing or bonding and SHALL authenticate the peer at the
application layer.

#### Scenario: First trusted connection
- **WHEN** a user selects a previously unseen Tutti board
- **THEN** the bridge presents its persistent identity for one-tap TOFU
  acceptance
- **AND** binds the signed handshake to both fresh boot nonces

#### Scenario: Reconnect after reboot
- **WHEN** a trusted board reconnects with the same persistent identity and a
  new boot nonce
- **THEN** the bridge establishes fresh directional session keys
- **AND** rejects frames replayed from the earlier boot

### Requirement: Bounded multiplexed BLE link
One BLE connection SHALL carry control, realtime, and HHHS repair lanes with
bounded fragmentation, reassembly, authentication, replay protection, queues,
and timeouts.

#### Scenario: HHHS frame exceeds one characteristic value
- **WHEN** a valid HHHS frame is larger than the negotiated GATT value size
- **THEN** the BLE link fragments and reassembles it below HHHS framing
- **AND** the reconstructed byte sequence is identical

#### Scenario: Peer exceeds a bound
- **WHEN** a fragment, message, queue, retry count, or session exceeds its
  configured bound
- **THEN** the link refuses or closes that work without unbounded allocation
- **AND** preserves already admitted HHHS history

### Requirement: Realtime intent with durable confirmation
The bridge SHALL keep high-rate musical controls out of durable HHHS history
while allowing durable musical meaning to confirm or correct provisional
feedback.

#### Scenario: High-rate performance gesture
- **WHEN** a peer sends a note, pitch, tempo, or round-table pulse
- **THEN** the bridge routes it through the compact authenticated realtime lane
- **AND** does not admit the transient message into HHHS history

#### Scenario: Durable musical edit
- **WHEN** a realtime intent represents a durable pattern or configuration edit
- **THEN** the corresponding canonical Tutti music command is admitted through
  the ordinary Room-v5 path
- **AND** its projection confirms or corrects the provisional effect

### Requirement: Gateway fan-out
The host SHALL require at most one BLE connection for an ESP-NOW Tutti mesh.

#### Scenario: Several boards are present
- **WHEN** the connected gateway discovers sibling Tutti boards over ESP-NOW
- **THEN** durable repair and supported realtime state can reach those siblings
  without a separate phone or plugin BLE connection to each board

#### Scenario: Gateway unavailable
- **WHEN** the selected gateway disconnects
- **THEN** the bridge reports the loss and may connect another trusted roster
  member
- **AND** the room remains repairable after reconnection

### Requirement: Shared causal pitch set
The room SHALL expose one add-wins observed-remove pitch and pitch-class set
which any authorized peer may edit. Membership SHALL not be owned by the peer
which added it.

#### Scenario: Cross-peer note off
- **GIVEN** one peer added a pitch and another peer has observed that add
- **WHEN** the second peer removes the pitch
- **THEN** every repaired materialization excludes it regardless of its author

#### Scenario: Concurrent edit
- **WHEN** a pitch add and remove are genuinely concurrent
- **THEN** the add survives under add-wins semantics
- **AND** any later authorized remove which observes that add clears it

#### Scenario: ESP input adapters
- **WHEN** the ESP web UI or BLE keyboard toggles a pitch
- **THEN** it edits the same room-owned set
- **AND** either adapter can retract a pitch added by the other or by a remote peer

#### Scenario: Peer departure
- **WHEN** an adding peer leaves or later rejoins under a different identity
- **THEN** another authorized peer can still remove its pitches
- **AND** no presence lease silently changes durable pitch membership

#### Scenario: Downstream reconciliation
- **WHEN** the derived room set changes or an output reconnects
- **THEN** each MIDI, browser, or embedded output reconciles from its endpoint
  shadow to current state with note-offs before note-ons
- **AND** missed realtime edges cannot leave a stuck downstream note

#### Scenario: Pending input is not canonical output
- **WHEN** a local toggle or gate intent is queued but has not yet appeared in a
  confirmed room materialization
- **THEN** it may affect interpretation of the next local gesture
- **AND** it does not emit a durable-membership note edge to any effect endpoint

#### Scenario: Effect generation is replaced
- **WHEN** a room, carrier, plugin output, or embedded renderer is disabled or
  replaced while notes are active
- **THEN** the old generation retracts every note it owns before being discarded
- **AND** a late acknowledgement or event from that generation cannot reactivate it

#### Scenario: Routed host output returns as input
- **WHEN** a DAW routes a membership note emitted by the plugin back to its MIDI input
- **THEN** stable origin metadata or the bounded fallback fingerprint suppresses
  that echo exactly once
- **AND** the echo does not author another shared-set command

#### Scenario: Ephemeral performance voice ends
- **WHEN** a transient performance voice receives note-off, choke, disconnect, or
  lease expiry
- **THEN** the exact session/voice-owned effect is retracted
- **AND** neither its note-on nor its retraction is mistaken for durable pitch-set
  membership
