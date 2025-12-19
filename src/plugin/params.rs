//! Plugin parameters with channel persistence.

use std::sync::Mutex;

use nih_plug::prelude::*;

use crate::words::generate_room_name;

/// Parameters for the walkie-songie plugin.
#[derive(Params)]
pub struct WalkieSongieParams {
    /// The current channel address (persisted with plugin state).
    #[persist = "channel_address"]
    pub channel_address: Mutex<String>,

    // === MIDI Output Enable/Disable ===

    /// Enable unified pitch class output (toggles + pieces + voice as pitch classes).
    #[id = "pitch_classes_enabled"]
    pub pitch_classes_enabled: BoolParam,

    /// Enable voice melody output (absolute pitches from voice detection).
    #[id = "voice_enabled"]
    pub voice_enabled: BoolParam,

    /// Enable pieces output (absolute pitches from emoji pieces).
    #[id = "pieces_enabled"]
    pub pieces_enabled: BoolParam,

    // === MIDI Channel Routing (1-16 displayed, 0-15 internal) ===

    /// MIDI channel for pitch class output (1-16).
    #[id = "pitch_classes_channel"]
    pub pitch_classes_channel: IntParam,

    /// MIDI channel for voice output (1-16).
    #[id = "voice_channel"]
    pub voice_channel: IntParam,

    /// MIDI channel for pieces output (1-16).
    #[id = "pieces_channel"]
    pub pieces_channel: IntParam,
}

impl Default for WalkieSongieParams {
    fn default() -> Self {
        Self {
            channel_address: Mutex::new(generate_room_name()),

            pitch_classes_enabled: BoolParam::new("Pitch Classes", true),
            voice_enabled: BoolParam::new("Voice", true),
            pieces_enabled: BoolParam::new("Pieces", true),

            // Channels 1-16 (displayed), internally 0-15
            pitch_classes_channel: IntParam::new(
                "PC Channel",
                1,
                IntRange::Linear { min: 1, max: 16 },
            ),
            voice_channel: IntParam::new(
                "Voice Channel",
                2,
                IntRange::Linear { min: 1, max: 16 },
            ),
            pieces_channel: IntParam::new(
                "Pieces Channel",
                3,
                IntRange::Linear { min: 1, max: 16 },
            ),
        }
    }
}

impl WalkieSongieParams {
    /// Get the current channel address.
    pub fn get_channel(&self) -> String {
        self.channel_address.lock().unwrap().clone()
    }

    /// Set a new channel address.
    pub fn set_channel(&self, channel: String) {
        *self.channel_address.lock().unwrap() = channel;
    }
}
