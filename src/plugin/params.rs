use std::sync::{Arc, Mutex};

use nice_plug::prelude::*;

use crate::bridge::MidiInputMode;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Enum)]
pub enum MidiInputPolicy {
    #[id = "toggle-set"]
    #[name = "Toggle set"]
    ToggleSet,
    #[id = "gate-set"]
    #[name = "Gate set"]
    GateSet,
    #[id = "perform"]
    Perform,
}

impl From<MidiInputPolicy> for MidiInputMode {
    fn from(value: MidiInputPolicy) -> Self {
        match value {
            MidiInputPolicy::ToggleSet => Self::ToggleSet,
            MidiInputPolicy::GateSet => Self::GateSet,
            MidiInputPolicy::Perform => Self::Perform,
        }
    }
}

#[derive(Params)]
pub struct TuttiBridgeParams {
    #[id = "midi_input_policy"]
    pub midi_input_policy: EnumParam<MidiInputPolicy>,
    #[id = "passthru"]
    pub midi_thru: BoolParam,
    #[id = "share_midi"]
    pub share_midi: BoolParam,
    #[id = "receive_midi"]
    pub receive_midi: BoolParam,
    #[persist = "room_name"]
    pub room_name: Arc<Mutex<String>>,
    #[persist = "trusted_board_identities"]
    pub trusted_boards: Arc<Mutex<Vec<[u8; 32]>>>,
    #[persist = "bridge_identity_seed"]
    pub bridge_identity_seed: Arc<Mutex<[u8; 32]>>,
}

impl Default for TuttiBridgeParams {
    fn default() -> Self {
        Self {
            midi_input_policy: EnumParam::new("Input mode", MidiInputPolicy::ToggleSet),
            // In the default pitch-set editing mode, downstream arpeggiators
            // must hear only reconciled room membership edges. Passing the raw
            // key release through would prematurely turn off a persistent set
            // member and can also duplicate the confirmed NoteOn.
            midi_thru: BoolParam::new("MIDI thru", false),
            share_midi: BoolParam::new("Share MIDI", true),
            receive_midi: BoolParam::new("Receive MIDI", true),
            room_name: Arc::new(Mutex::new(String::new())),
            trusted_boards: Arc::new(Mutex::new(Vec::new())),
            bridge_identity_seed: Arc::new(Mutex::new(rand::random())),
        }
    }
}
