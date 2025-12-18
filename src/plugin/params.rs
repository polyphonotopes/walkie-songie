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
}

impl Default for WalkieSongieParams {
    fn default() -> Self {
        Self {
            channel_address: Mutex::new(generate_room_name()),
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
