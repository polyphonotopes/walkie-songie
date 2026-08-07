use async_channel::{Receiver, Sender};
use midir::{Ignore, MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::MidiMessage;

const INPUT_PREFIX: &str = "input:";
const OUTPUT_PREFIX: &str = "output:";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MidiDeviceDirection {
    Input,
    Output,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativePort {
    pub id: String,
    pub name: String,
    pub direction: MidiDeviceDirection,
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiInputEvent {
    pub timestamp_micros: u64,
    pub port_id: String,
    pub channel: u8,
    pub note: u8,
    pub velocity: u8,
    pub is_note_on: bool,
}

#[derive(Debug, Error)]
pub enum NativeMidiError {
    #[error("could not initialize native MIDI {direction}: {detail}")]
    Initialize {
        direction: &'static str,
        detail: String,
    },
    #[error("native MIDI port {0:?} is unavailable")]
    PortUnavailable(String),
    #[error("could not inspect native MIDI port: {0}")]
    PortInfo(String),
    #[error("could not connect native MIDI {direction}: {detail}")]
    Connect {
        direction: &'static str,
        detail: String,
    },
    #[error("could not send native MIDI message: {0}")]
    Send(String),
}

/// Backend-owned native MIDI connections.
///
/// The service survives webview reloads. Re-enumeration uses midir's opaque,
/// backend-provided stable port IDs, so list ordering changes do not silently
/// retarget a selected connection.
pub struct NativeMidiService {
    selected_input: Option<String>,
    selected_output: Option<String>,
    input_connection: Option<MidiInputConnection<()>>,
    output_connection: Option<MidiOutputConnection>,
    input_sender: Sender<MidiInputEvent>,
    input_receiver: Receiver<MidiInputEvent>,
}

impl std::fmt::Debug for NativeMidiService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeMidiService")
            .field("selected_input", &self.selected_input)
            .field("selected_output", &self.selected_output)
            .field("input_connected", &self.input_connection.is_some())
            .field("output_connected", &self.output_connection.is_some())
            .finish()
    }
}

impl NativeMidiService {
    pub fn new() -> Self {
        let (input_sender, input_receiver) = async_channel::bounded(1024);
        Self {
            selected_input: None,
            selected_output: None,
            input_connection: None,
            output_connection: None,
            input_sender,
            input_receiver,
        }
    }

    pub fn input_events(&self) -> Receiver<MidiInputEvent> {
        self.input_receiver.clone()
    }

    pub fn selected_input(&self) -> Option<&str> {
        self.selected_input.as_deref()
    }

    pub fn selected_output(&self) -> Option<&str> {
        self.selected_output.as_deref()
    }

    pub fn list_ports(&self) -> Result<Vec<NativePort>, NativeMidiError> {
        let input = MidiInput::new("walkie-songie-list-input").map_err(|error| {
            NativeMidiError::Initialize {
                direction: "input",
                detail: error.to_string(),
            }
        })?;
        let output = MidiOutput::new("walkie-songie-list-output").map_err(|error| {
            NativeMidiError::Initialize {
                direction: "output",
                detail: error.to_string(),
            }
        })?;
        let mut ports = Vec::new();
        for port in input.ports() {
            let id = encode_port_id(MidiDeviceDirection::Input, &port.id());
            ports.push(NativePort {
                selected: self.selected_input.as_deref() == Some(id.as_str()),
                id,
                name: input
                    .port_name(&port)
                    .map_err(|error| NativeMidiError::PortInfo(error.to_string()))?,
                direction: MidiDeviceDirection::Input,
            });
        }
        for port in output.ports() {
            let id = encode_port_id(MidiDeviceDirection::Output, &port.id());
            ports.push(NativePort {
                selected: self.selected_output.as_deref() == Some(id.as_str()),
                id,
                name: output
                    .port_name(&port)
                    .map_err(|error| NativeMidiError::PortInfo(error.to_string()))?,
                direction: MidiDeviceDirection::Output,
            });
        }
        ports.sort_by(|left, right| {
            (left.direction as u8, &left.name, &left.id).cmp(&(
                right.direction as u8,
                &right.name,
                &right.id,
            ))
        });
        Ok(ports)
    }

    /// Refresh port state and release a selected connection if its opaque port
    /// ID has disappeared. Returns whether input/output selection was lost.
    pub fn refresh(&mut self) -> Result<(Vec<NativePort>, bool, bool), NativeMidiError> {
        let ports = self.list_ports()?;
        let input_lost = self
            .selected_input
            .as_ref()
            .is_some_and(|selected| !ports.iter().any(|port| &port.id == selected));
        let output_lost = self
            .selected_output
            .as_ref()
            .is_some_and(|selected| !ports.iter().any(|port| &port.id == selected));
        if input_lost {
            self.input_connection = None;
            self.selected_input = None;
        }
        if output_lost {
            self.panic_output_best_effort();
            self.output_connection = None;
            self.selected_output = None;
        }
        let ports = if input_lost || output_lost {
            self.list_ports()?
        } else {
            ports
        };
        Ok((ports, input_lost, output_lost))
    }

    pub fn select_input(&mut self, port_id: Option<&str>) -> Result<(), NativeMidiError> {
        self.input_connection = None;
        self.selected_input = None;
        let Some(port_id) = port_id else {
            return Ok(());
        };
        let raw_id = decode_port_id(MidiDeviceDirection::Input, port_id)?;
        let mut input =
            MidiInput::new("walkie-songie-input").map_err(|error| NativeMidiError::Initialize {
                direction: "input",
                detail: error.to_string(),
            })?;
        input.ignore(Ignore::None);
        let port = input
            .find_port_by_id(raw_id)
            .ok_or_else(|| NativeMidiError::PortUnavailable(port_id.to_owned()))?;
        let sender = self.input_sender.clone();
        let event_port_id = port_id.to_owned();
        let connection = input
            .connect(
                &port,
                "walkie-songie-input",
                move |timestamp_micros, bytes, ()| {
                    let Ok(message) = wmidi::MidiMessage::try_from(bytes) else {
                        return;
                    };
                    let event = match message {
                        wmidi::MidiMessage::NoteOn(channel, note, velocity) => {
                            Some(MidiInputEvent {
                                timestamp_micros,
                                port_id: event_port_id.clone(),
                                channel: channel.index(),
                                note: note.into(),
                                velocity: velocity.into(),
                                is_note_on: true,
                            })
                        }
                        // wmidi normalizes velocity-zero note-on to NoteOff.
                        wmidi::MidiMessage::NoteOff(channel, note, velocity) => {
                            Some(MidiInputEvent {
                                timestamp_micros,
                                port_id: event_port_id.clone(),
                                channel: channel.index(),
                                note: note.into(),
                                velocity: velocity.into(),
                                is_note_on: false,
                            })
                        }
                        _ => None,
                    };
                    if let Some(event) = event {
                        let _ = sender.try_send(event);
                    }
                },
                (),
            )
            .map_err(|error| NativeMidiError::Connect {
                direction: "input",
                detail: error.to_string(),
            })?;
        self.input_connection = Some(connection);
        self.selected_input = Some(port_id.to_owned());
        Ok(())
    }

    pub fn select_output(&mut self, port_id: Option<&str>) -> Result<(), NativeMidiError> {
        self.panic_output_best_effort();
        self.output_connection = None;
        self.selected_output = None;
        let Some(port_id) = port_id else {
            return Ok(());
        };
        let raw_id = decode_port_id(MidiDeviceDirection::Output, port_id)?;
        let output = MidiOutput::new("walkie-songie-output").map_err(|error| {
            NativeMidiError::Initialize {
                direction: "output",
                detail: error.to_string(),
            }
        })?;
        let port = output
            .find_port_by_id(raw_id)
            .ok_or_else(|| NativeMidiError::PortUnavailable(port_id.to_owned()))?;
        let connection = output
            .connect(&port, "walkie-songie-output")
            .map_err(|error| NativeMidiError::Connect {
                direction: "output",
                detail: error.to_string(),
            })?;
        self.output_connection = Some(connection);
        self.selected_output = Some(port_id.to_owned());
        Ok(())
    }

    pub fn send_messages(
        &mut self,
        messages: impl IntoIterator<Item = MidiMessage>,
    ) -> Result<(), NativeMidiError> {
        let Some(connection) = self.output_connection.as_mut() else {
            return Ok(());
        };
        for message in messages {
            connection
                .send(&message.to_bytes())
                .map_err(|error| NativeMidiError::Send(error.to_string()))?;
        }
        Ok(())
    }

    pub fn panic_output(&mut self) -> Result<(), NativeMidiError> {
        let Some(connection) = self.output_connection.as_mut() else {
            return Ok(());
        };
        for channel in 0..16 {
            connection
                .send(
                    &MidiMessage::ControlChange {
                        channel,
                        controller: 121,
                        value: 0,
                    }
                    .to_bytes(),
                )
                .map_err(|error| NativeMidiError::Send(error.to_string()))?;
            connection
                .send(
                    &MidiMessage::ControlChange {
                        channel,
                        controller: 123,
                        value: 0,
                    }
                    .to_bytes(),
                )
                .map_err(|error| NativeMidiError::Send(error.to_string()))?;
        }
        Ok(())
    }

    fn panic_output_best_effort(&mut self) {
        let _ = self.panic_output();
    }
}

impl Default for NativeMidiService {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for NativeMidiService {
    fn drop(&mut self) {
        self.panic_output_best_effort();
    }
}

fn encode_port_id(direction: MidiDeviceDirection, raw_id: &str) -> String {
    match direction {
        MidiDeviceDirection::Input => format!("{INPUT_PREFIX}{raw_id}"),
        MidiDeviceDirection::Output => format!("{OUTPUT_PREFIX}{raw_id}"),
    }
}

fn decode_port_id(direction: MidiDeviceDirection, port_id: &str) -> Result<&str, NativeMidiError> {
    let prefix = match direction {
        MidiDeviceDirection::Input => INPUT_PREFIX,
        MidiDeviceDirection::Output => OUTPUT_PREFIX,
    };
    port_id
        .strip_prefix(prefix)
        .filter(|raw| !raw.is_empty())
        .ok_or_else(|| NativeMidiError::PortUnavailable(port_id.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_prefix_prevents_cross_selecting_ports() {
        let input = encode_port_id(MidiDeviceDirection::Input, "opaque");
        assert_eq!(
            decode_port_id(MidiDeviceDirection::Input, &input).unwrap(),
            "opaque"
        );
        assert!(decode_port_id(MidiDeviceDirection::Output, &input).is_err());
    }
}
