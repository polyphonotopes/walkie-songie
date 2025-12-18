# Tasks

## Channel UI

- [ ] Create wholesome word list module (`src/words.rs`)
  - [ ] Curate ~200 wholesome adjectives
  - [ ] Curate ~300 wholesome nouns
  - [ ] Implement `generate_room_name()` -> "adjective-noun-noun"
  - [ ] Add unit tests for name generation

- [ ] Implement room name -> topic ID hashing
  - [ ] Hash room name string to derive iroh-gossip topic
  - [ ] Ensure deterministic mapping (same name = same topic)

- [ ] Add QR code generation
  - [ ] Add `qrcode` crate dependency
  - [ ] Create SVG rendering function for room URL
  - [ ] Test QR code is scannable

- [ ] Create room menu overlay component
  - [ ] Add overlay container with backdrop
  - [ ] Display current room QR code (SVG)
  - [ ] Display current room name
  - [ ] Add shuffle button for new random room
  - [ ] Add text input for joining by name
  - [ ] Add click-outside-to-dismiss / Escape to close
  - [ ] Style overlay to match app theme

- [ ] Add room name to header
  - [ ] Display current room name in header/corner
  - [ ] Make room name clickable to open overlay
  - [ ] Add QR icon next to name

- [ ] Implement room switching
  - [ ] On new room name, leave current gossip topic
  - [ ] Join new gossip topic derived from name
  - [ ] Update URL query param (`?room=name`)

- [ ] Implement "always in a room"
  - [ ] Read room from URL on page load
  - [ ] If no room in URL, auto-generate random room name
  - [ ] Immediately join the room (no roomless state)

## Web MIDI

- [ ] Create MIDI module (`src/web/midi.rs`)
  - [ ] Define MidiMessage enum (NoteOn, NoteOff, PitchBend)
  - [ ] Create async_channel for MIDI input messages
  - [ ] Create MidiState resource (connected, device names)

- [ ] Implement MIDI input (from controllers)
  - [ ] Request MIDI access via `navigator.requestMIDIAccess()`
  - [ ] Iterate and connect to all MIDI inputs
  - [ ] Set up `onmidimessage` callbacks
  - [ ] Parse note on/off messages (0x90, 0x80)
  - [ ] Send parsed messages to async_channel
  - [ ] Process input channel in app update loop

- [ ] Route MIDI input to toggle set
  - [ ] Convert MIDI note number to pitch class
  - [ ] Call toggle_pitch() on room state for note-on
  - [ ] Handle note-off (remove from toggle set)

- [ ] Implement MIDI output
  - [ ] Get MIDI outputs from MidiAccess
  - [ ] Create output sending function
  - [ ] Send to all connected outputs

- [ ] Output toggle set changes on channel 1
  - [ ] Subscribe to room state changes
  - [ ] On pitch added: send note-on (ch 1, note, velocity 100)
  - [ ] On pitch removed: send note-off (ch 1, note)

- [ ] Output voice pitches on channel 2
  - [ ] Subscribe to voice pitch changes (all peers)
  - [ ] Calculate MIDI note from Hz (nearest semitone, preserving octave)
  - [ ] Send note-on (ch 2)
  - [ ] Send note-off when voice stops

- [ ] Handle multi-peer voice pitches
  - [ ] Track active voice notes per peer
  - [ ] Send note-off for previous pitch when peer changes pitch
  - [ ] Handle peer disconnect (note-off for their pitch)

- [ ] Handle graceful note transitions
  - [ ] Track currently active MIDI notes (both channels)
  - [ ] On room leave: send note-off for all active notes
  - [ ] On room join: send note-on for current state
  - [ ] On new output device: sync current state to it
  - [ ] On app close/disconnect: send CC 123 (all notes off)

## Integration & Polish

- [ ] Test MIDI with external DAW (e.g., Ableton, Logic)
- [ ] Test QR code scanning on mobile
- [ ] Test room joining across devices
- [ ] Test room switching doesn't leave stuck notes
- [ ] Add MIDI status indicator in UI (optional)
