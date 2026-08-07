//! Runtime-independent client contract.
//!
//! Tauri IPC and the optional Agregore/Peersky loopback adapter carry these
//! exact types. No Tauri, HTTP, websocket, or browser type appears here.

use serde::{Deserialize, Serialize};

use crate::room::ops::{AuthorId, OpId};
use crate::tuning::{TunedDegree, TunedPeriodicPitch, TuningDefinition, TuningId};

pub const CLIENT_PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub protocol_version: u16,
    pub native_iroh: bool,
    pub mdns: bool,
    pub relay: bool,
    pub native_midi: bool,
    pub durable_storage: bool,
}

impl Capabilities {
    pub const fn tauri_desktop() -> Self {
        Self {
            protocol_version: CLIENT_PROTOCOL_VERSION,
            native_iroh: true,
            mdns: true,
            relay: true,
            native_midi: true,
            durable_storage: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientCommand {
    EnterRoom {
        room_name: String,
    },
    JoinTicket {
        ticket: String,
    },
    LeaveRoom,
    SetTuning {
        definition: TuningDefinition,
    },
    AddDegree {
        pitch: TunedDegree,
    },
    RemoveDegree {
        pitch: TunedDegree,
    },
    PutPiece {
        emoji: String,
        pitch: TunedPeriodicPitch,
    },
    MovePiece {
        piece: OpId,
        pitch: TunedPeriodicPitch,
    },
    RemovePiece {
        piece: OpId,
    },
    SetRoomConfig {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pieces_locked: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        available_emojis: Option<String>,
    },
    SetVoicePreview {
        session: u64,
        pitch: Option<TunedPeriodicPitch>,
    },
    ListMidiPorts,
    SelectMidiInput {
        port_id: Option<String>,
    },
    SelectMidiOutput {
        port_id: Option<String>,
    },
    PanicMidi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerPath {
    Connecting,
    Direct,
    Relayed,
    Disconnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoverySource {
    Ticket,
    Mdns,
    Gossip,
    AddressLookup,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerSnapshot {
    pub author: AuthorId,
    pub endpoint_id: String,
    pub path: PeerPath,
    pub discovery: DiscoverySource,
    pub round_trip_ms: Option<u32>,
    pub synchronized: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PieceSnapshot {
    pub id: OpId,
    pub owner: AuthorId,
    pub emoji: String,
    pub pitch: TunedPeriodicPitch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceSnapshot {
    pub author: AuthorId,
    pub session: u64,
    pub sequence: u64,
    pub pitch: Option<TunedPeriodicPitch>,
    pub expires_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MidiPortSnapshot {
    pub id: String,
    pub name: String,
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSnapshot {
    pub capabilities: Capabilities,
    pub room_name: Option<String>,
    pub room_topic: Option<String>,
    pub room_ticket: Option<String>,
    pub tuning: Option<TuningDefinition>,
    pub tuning_id: Option<TuningId>,
    pub active_degrees: Vec<TunedDegree>,
    pub pieces: Vec<PieceSnapshot>,
    pub pieces_locked: bool,
    pub available_emojis: Option<String>,
    pub voices: Vec<VoiceSnapshot>,
    pub peers: Vec<PeerSnapshot>,
    pub midi_inputs: Vec<MidiPortSnapshot>,
    pub midi_outputs: Vec<MidiPortSnapshot>,
}

impl AppSnapshot {
    pub fn empty(capabilities: Capabilities) -> Self {
        Self {
            capabilities,
            room_name: None,
            room_topic: None,
            room_ticket: None,
            tuning: None,
            tuning_id: None,
            active_degrees: Vec::new(),
            pieces: Vec::new(),
            pieces_locked: false,
            available_emojis: None,
            voices: Vec::new(),
            peers: Vec::new(),
            midi_inputs: Vec::new(),
            midi_outputs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AppEvent {
    Snapshot {
        snapshot: AppSnapshot,
    },
    RoomChanged {
        room_name: Option<String>,
        room_topic: Option<String>,
        ticket: Option<String>,
    },
    TuningChanged {
        definition: TuningDefinition,
    },
    DegreeAdded {
        pitch: TunedDegree,
        authors: Vec<AuthorId>,
    },
    DegreeRemoved {
        pitch: TunedDegree,
    },
    PieceUpserted {
        piece: PieceSnapshot,
    },
    PieceRemoved {
        piece: OpId,
    },
    RoomConfigChanged {
        pieces_locked: bool,
        available_emojis: Option<String>,
    },
    VoiceUpdated {
        voice: VoiceSnapshot,
    },
    VoiceExpired {
        author: AuthorId,
        session: u64,
    },
    PeerUpdated {
        peer: PeerSnapshot,
    },
    PeerRemoved {
        author: AuthorId,
    },
    MidiPortsChanged {
        inputs: Vec<MidiPortSnapshot>,
        outputs: Vec<MidiPortSnapshot>,
    },
    Diagnostic {
        code: String,
        message: String,
    },
}

/// Sequence is assigned by the native runtime immediately before fan-out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppEventEnvelope {
    pub sequence: u64,
    pub event: AppEvent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppErrorCode {
    InvalidCommand,
    InvalidRoom,
    InvalidTicket,
    InvalidTuning,
    UnknownTuning,
    UnsupportedCapability,
    NetworkUnavailable,
    MidiUnavailable,
    Persistence,
    ResourceLimit,
    ShuttingDown,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppError {
    pub code: AppErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl AppError {
    pub fn new(code: AppErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            detail: None,
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandAck {
    pub accepted_sequence: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tuning;

    #[test]
    fn command_codec_is_stably_tagged_and_round_trips() {
        let tuning = Tuning::twelve_tet();
        let command = ClientCommand::AddDegree {
            pitch: TunedDegree::new(&tuning, 9).unwrap(),
        };
        let json = serde_json::to_string(&command).unwrap();
        assert!(json.contains(r#""type":"add_degree""#));
        assert_eq!(
            serde_json::from_str::<ClientCommand>(&json).unwrap(),
            command
        );
    }

    #[test]
    fn snapshot_event_round_trips() {
        let envelope = AppEventEnvelope {
            sequence: 7,
            event: AppEvent::Snapshot {
                snapshot: AppSnapshot::empty(Capabilities::tauri_desktop()),
            },
        };
        let bytes = serde_json::to_vec(&envelope).unwrap();
        assert_eq!(
            serde_json::from_slice::<AppEventEnvelope>(&bytes).unwrap(),
            envelope
        );
    }
}
