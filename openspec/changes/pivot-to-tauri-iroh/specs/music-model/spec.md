## ADDED Requirements

### Requirement: Validated Scala Tuning Context
The system SHALL parse Scala scale data according to the SCL format, retain its explicit repeating period, apply an optional keyboard mapping or documented default mapping, and derive a deterministic versioned TuningId from the canonical result.

#### Scenario: Integer ratio
- **WHEN** an SCL pitch line contains `5` with no decimal point or slash
- **THEN** it is interpreted as ratio `5/1`, not as five cents

#### Scenario: Declared note count mismatch
- **WHEN** the number of valid pitch lines differs from the declared note count
- **THEN** parsing fails with a structured error and no TuningId is produced

#### Scenario: Non-octave period
- **WHEN** the last pitch value defines a valid period other than 1200 cents
- **THEN** quantization and periodic pitch arithmetic use that period rather than assuming an octave

#### Scenario: Equivalent canonical input
- **WHEN** peers parse semantically equivalent supported tuning input
- **THEN** they derive the same TuningId and golden canonical bytes

#### Scenario: Invalid or excessive tuning
- **WHEN** a tuning contains invalid ratios, non-finite values, non-increasing steps, or exceeds configured bounds
- **THEN** it is rejected before network or UI state changes

### Requirement: Tuning-Scoped Pitch Types
Every durable or live pitch SHALL carry a validated scale degree or periodic pitch bound to an exact TuningId.

#### Scenario: Construct valid pitch
- **WHEN** a degree and period position are valid for a known tuning
- **THEN** the system constructs a typed pitch whose degree agrees with its periodic position

#### Scenario: Degree outside tuning
- **WHEN** an operation names a degree outside the referenced tuning
- **THEN** the operation is rejected before RoomStore ingestion

#### Scenario: Different equal-cardinality tunings
- **WHEN** two 12-note tunings have different canonical steps or mappings
- **THEN** they have different TuningIds and their pitches are never treated as the same musical pitch merely because both contain 12 degrees

### Requirement: Correct Periodic Quantization
The system SHALL quantize positive finite frequencies to the globally nearest candidate around the current period boundary and return the selected periodic pitch, exact center frequency, and signed cents deviation.

#### Scenario: Frequency below the reference boundary
- **WHEN** a frequency lies just below the reference degree and is nearest to that degree in the previous or current wrapped period
- **THEN** the returned period index and center frequency identify the actually selected candidate

#### Scenario: Frequency above the final degree
- **WHEN** a frequency is nearest to the root in the next period
- **THEN** the returned periodic pitch advances the period and does not report the lower root center

#### Scenario: Unequal scale spacing
- **WHEN** adjacent scale intervals are not equal
- **THEN** nearest-degree selection uses their real cent positions and does not clamp deviation to a hard-coded 50 cents

#### Scenario: Invalid frequency
- **WHEN** the input frequency is zero, negative, NaN, or infinite
- **THEN** quantization returns a structured error rather than inventing a pitch

### Requirement: Tuning Change Safety
The active musical projection SHALL include only pitch contributions bound to the resolved current TuningId.

#### Scenario: Room tuning changes
- **WHEN** a new tuning wins the room register
- **THEN** old-tuning contributions remain in signed history but leave active output with balanced note-offs and are not reinterpreted under the new tuning

#### Scenario: Concurrent tuning writes converge
- **WHEN** peers resolve concurrent tuning changes
- **THEN** they select the same TuningId and project the same active contributions

### Requirement: Per-Author Durable Pitch Intent
The system SHALL maintain independent durable pitch contributions per author and expose their attributed union without allowing one author to retract another author's contribution.

#### Scenario: Two authors add the same degree
- **WHEN** two authors add the same tuning-scoped degree
- **THEN** the shared union contains one degree attributed to both authors

#### Scenario: One author retracts
- **WHEN** one of those authors retracts their own observed contribution
- **THEN** the degree remains active and attributed to the other author

#### Scenario: Voice release toggles durable intent
- **WHEN** a stable voice capture is released on a degree already held by the local author
- **THEN** the local contribution is retracted; otherwise it is added

### Requirement: Ephemeral Live Voice Presence
Live voice previews SHALL be signed, session-scoped, sequenced, and leased presence rather than durable room operations.

#### Scenario: Singing preview
- **WHEN** a confident pitch is detected during an active capture
- **THEN** peers may display and output the newest fresh periodic pitch for that author/session

#### Scenario: Normal release
- **WHEN** the user releases voice capture
- **THEN** live presence clears immediately and any durable toggle is processed separately

#### Scenario: Peer crashes
- **WHEN** a peer disappears without sending a clear frame
- **THEN** its live voice expires after the lease and produces balanced output removal

#### Scenario: Reordered preview frames
- **WHEN** an older presence sequence arrives after a newer one
- **THEN** the older frame cannot restore a stale pitch

### Requirement: Tuning-Truthful Musical Labels
The system SHALL show conventional MIDI note names, major-scale modes, and solfège only when the active tuning and mapping support those interpretations.

#### Scenario: Standard 12-TET mapping
- **WHEN** the room uses compatible 12-TET with the documented MIDI mapping
- **THEN** the UI may show conventional note names and 12-TET major-scale interpretations

#### Scenario: Arbitrary microtonal tuning
- **WHEN** the room uses a non-compatible tuning
- **THEN** the UI shows tuning-defined degree labels, periodic position, or frequency and does not present 12-TET solfège as fact
