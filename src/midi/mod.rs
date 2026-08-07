//! Platform-independent MIDI routing and native port ownership.
//!
//! The source ledger is intentionally independent of `midir`: its output is a
//! deterministic sequence of MIDI 1.0 messages which can be checked with a
//! fake sink. Native device access is a thin feature-gated adapter.

mod input;
mod ledger;

#[cfg(all(feature = "native-midi", not(target_arch = "wasm32")))]
mod native;

pub use input::{HeldInputAction, MidiInputTracker, PhysicalMidiKey, midi_note_frequency_hz};
pub use ledger::{
    DEFAULT_VELOCITY, MidiLedger, MidiMessage, MidiOutputConfig, MidiRouteError, MidiSource,
    MidiVoice,
};

#[cfg(all(feature = "native-midi", not(target_arch = "wasm32")))]
pub use native::{
    MidiDeviceDirection, MidiInputEvent, NativeMidiError, NativeMidiService, NativePort,
};
