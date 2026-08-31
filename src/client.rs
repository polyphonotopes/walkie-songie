//! Runtime-independent client contract.
//!
//! Tauri IPC and the optional Agregore/Peersky loopback adapter carry these
//! exact types. No Tauri, HTTP, websocket, or browser type appears here.

use serde::{Deserialize, Serialize};

use crate::room::v5::{ActorId, PieceId};
use crate::tuning::{TunedDegree, TunedPeriodicPitch, TuningDefinition, TuningId};
use tutti_music::{SharedPitchSet, roundtable::RoundTableConfig};

pub const CLIENT_PROTOCOL_VERSION: u16 = 5;

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
    SetRoundTable {
        config: RoundTableConfig,
    },
    AddPitch {
        pitch: TunedPeriodicPitch,
    },
    RemovePitch {
        pitch: TunedPeriodicPitch,
    },
    /// Add an emoji piece. Its pitch class contributes to the room's derived
    /// sounding set while remaining distinct from manual pitch membership.
    PutPiece {
        emoji: String,
        pitch: TunedPeriodicPitch,
    },
    /// Move an emoji piece. The sounding contribution follows the piece's
    /// causal identity; no mirrored manual-pitch operation is authored.
    MovePiece {
        piece: PieceId,
        pitch: TunedPeriodicPitch,
    },
    RemovePiece {
        piece: PieceId,
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
    pub author: ActorId,
    pub endpoint_id: String,
    pub path: PeerPath,
    pub discovery: DiscoverySource,
    pub round_trip_ms: Option<u32>,
    pub synchronized: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PieceSnapshot {
    pub id: PieceId,
    pub owner: ActorId,
    pub emoji: String,
    pub pitch: TunedPeriodicPitch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceSnapshot {
    pub author: ActorId,
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

/// A signed, transient MIDI event received from a room peer.
///
/// This deliberately is not part of [`AppSnapshot`]: held notes are session
/// state, not durable HHHS musical history. The source/session/sequence tuple
/// lets every UI adapter apply the same replay and stuck-note boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RealtimeMidiSnapshot {
    pub source: ActorId,
    pub session: u64,
    pub sequence: u64,
    pub voice_id: i32,
    pub channel: u8,
    pub note: u8,
    pub kind: RealtimeMidiKind,
    pub value: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RealtimeMidiKind {
    NoteOn,
    NoteOff,
    Choke,
    PolyPressure,
    PitchBend,
    ChannelPressure,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSnapshot {
    pub capabilities: Capabilities,
    pub local_actor: Option<ActorId>,
    pub room_name: Option<String>,
    pub room_topic: Option<String>,
    pub room_ticket: Option<String>,
    pub tuning: Option<TuningDefinition>,
    pub tuning_id: Option<TuningId>,
    pub round_table: RoundTableConfig,
    pub shared_pitches: SharedPitchSet,
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
            local_actor: None,
            room_name: None,
            room_topic: None,
            room_ticket: None,
            tuning: None,
            tuning_id: None,
            round_table: RoundTableConfig::default(),
            shared_pitches: SharedPitchSet::default(),
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
        snapshot: Box<AppSnapshot>,
    },
    RoomChanged {
        room_name: Option<String>,
        room_topic: Option<String>,
        ticket: Option<String>,
    },
    TuningChanged {
        definition: TuningDefinition,
    },
    RoundTableChanged {
        config: RoundTableConfig,
    },
    PitchSetChanged {
        shared: SharedPitchSet,
    },
    PieceUpserted {
        piece: PieceSnapshot,
    },
    PieceRemoved {
        piece: PieceId,
    },
    RoomConfigChanged {
        pieces_locked: bool,
        available_emojis: Option<String>,
    },
    VoiceUpdated {
        voice: VoiceSnapshot,
    },
    VoiceExpired {
        author: ActorId,
        session: u64,
    },
    PeerUpdated {
        peer: PeerSnapshot,
    },
    PeerRemoved {
        author: ActorId,
    },
    MidiPortsChanged {
        inputs: Vec<MidiPortSnapshot>,
        outputs: Vec<MidiPortSnapshot>,
    },
    RealtimeMidi {
        midi: RealtimeMidiSnapshot,
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
                snapshot: Box::new(AppSnapshot::empty(Capabilities::tauri_desktop())),
            },
        };
        let bytes = serde_json::to_vec(&envelope).unwrap();
        assert_eq!(
            serde_json::from_slice::<AppEventEnvelope>(&bytes).unwrap(),
            envelope
        );
    }

    #[test]
    fn transient_midi_event_round_trips_without_entering_snapshot() {
        let envelope = AppEventEnvelope {
            sequence: 8,
            event: AppEvent::RealtimeMidi {
                midi: RealtimeMidiSnapshot {
                    source: ActorId([7; 32]),
                    session: 11,
                    sequence: 12,
                    voice_id: -1,
                    channel: 2,
                    note: 60,
                    kind: RealtimeMidiKind::NoteOn,
                    value: 48_000,
                },
            },
        };
        let bytes = serde_json::to_vec(&envelope).unwrap();
        assert_eq!(
            serde_json::from_slice::<AppEventEnvelope>(&bytes).unwrap(),
            envelope
        );
    }

    #[test]
    fn round_table_command_round_trips() {
        let command = ClientCommand::SetRoundTable {
            config: RoundTableConfig::default(),
        };
        let bytes = serde_json::to_vec(&command).unwrap();
        assert_eq!(
            serde_json::from_slice::<ClientCommand>(&bytes).unwrap(),
            command
        );
    }
}
