//! Walkie's MIDI surface: the `tutti_midi` bridge crate fixed to walkie's own
//! source-key type, plus native port ownership.
//!
//! The source-balanced ledger and the input tracker live in `tutti_midi` (tutti
//! Phase II, roadmap §A.5); this module re-exports them and supplies the one
//! thing that is walkie's: [`MidiSource`], the app's answer to "what is one
//! independent reason a note sounds". Native device access stays here as a thin
//! feature-gated adapter.

#[cfg(all(feature = "native-midi", not(target_arch = "wasm32")))]
mod native;

pub use tutti_midi::{
    DEFAULT_VELOCITY, HeldInputAction, MidiInputTracker, MidiMessage, MidiOutputConfig,
    MidiRouteError, MidiVoice, PhysicalMidiKey, midi_note_frequency_hz,
};

#[cfg(all(feature = "native-midi", not(target_arch = "wasm32")))]
pub use native::{
    MidiDeviceDirection, MidiInputEvent, NativeMidiError, NativeMidiService, NativePort,
};

use crate::room::v5::{ActorId, PieceId};
use crate::tuning::TunedDegree;

/// One independent reason a MIDI note is sounding in Walkie's projection.
/// The durable shared set has one source per member, independent of authorship;
/// pieces, voice previews, and locally held keys remain separate sources.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MidiSource {
    /// One member of the durable shared pitch-class set.
    SharedDegree { pitch: TunedDegree },
    /// One explicitly octave-bearing member of the durable shared pitch set.
    SharedPitch {
        pitch: crate::tuning::TunedPeriodicPitch,
    },
    /// An emoji piece, keyed by its creating op.
    Piece { id: PieceId },
    /// A peer's live voice preview (presence-leased, never durable).
    Voice { author: ActorId, session: u64 },
    /// A locally held physical key.
    LocalInput {
        port_id: String,
        channel: u8,
        note: u8,
    },
}

/// Walkie's ledger spelling — the generic ledger fixed at [`MidiSource`].
pub type MidiLedger = tutti_midi::MidiLedger<MidiSource>;
