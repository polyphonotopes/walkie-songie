//! Runtime-independent bridge between DAW-safe MIDI queues and network links.
//!
//! The audio-facing half of this module is deliberately tiny: it only pushes
//! and pops fixed-size values through preallocated lock-free queues. Bluetooth,
//! Iroh, HHHS, cryptography, allocation, locks, and waiting belong to the
//! background [`BridgeTransport`] implementation.

mod ble;
#[cfg(feature = "desktop-ble")]
mod ble_btleplug;
mod ble_transport;
mod composite;
#[cfg(feature = "native-net")]
mod native_room;

pub use ble::{
    BleAddress, BleHost, BleHostError, BleHostEvent, BleScanResult, BleWriteMessage,
    BleWritePriority, InMemoryBleHost,
};
#[cfg(feature = "desktop-ble")]
pub use ble_btleplug::BtleplugHost;
pub use ble_transport::{BleLinkConfig, BleLinkTransport};
pub use composite::{CarrierLeg, CarrierLegKind, CompositeTransport};
#[cfg(feature = "native-net")]
pub use native_room::{NativeRoomConfig, NativeRoomTransport};

use std::{
    collections::BTreeMap,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crossbeam_queue::ArrayQueue;
use thiserror::Error;
use tutti_ble::{
    PROFILE_CAP_HHHS_REPAIR, PROFILE_CAP_MUSIC, PROFILE_CAP_REALTIME, PROFILE_CAP_WALKIE_EXTENSION,
    PeerHello,
};
use tutti_music::{
    SharedPitchSet, TunedDegree, TunedPeriodicPitch, Tuning,
    render::fractional_midi,
    roundtable::{RoundTableConfig, RoundTablePattern},
};
use tutti_roundtable::{ConfigState, Frame as RoundTableFrame};

use crate::room::v5::{LANE_STRATEGY_VERSION, ROOM_PROTOCOL_GENERATION};

pub const DEFAULT_UI_COMMAND_CAPACITY: usize = 64;
pub const DEFAULT_UI_EVENT_CAPACITY: usize = 128;
pub const DEFAULT_REALTIME_CAPACITY: usize = 512;
pub const DEFAULT_RELEASE_CAPACITY: usize = 128;
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(1);

/// Semantic profile negotiated before a bridge opens either durable lane.
///
/// `room_generation` is relevant only when both peers advertise the Walkie
/// extension lane. A Tutti leaf can support canonical music while leaving that
/// capability unset, so it does not need to pretend to implement Room-v5.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ProtocolProfile {
    pub music_generation: u32,
    pub music_vocabulary_generation: u32,
    pub hhhs_strategy_version: u32,
    pub hhhs_repair_generation: u32,
    pub room_generation: u32,
    pub capabilities: u32,
}

impl ProtocolProfile {
    pub const CAP_MUSIC: u32 = PROFILE_CAP_MUSIC;
    pub const CAP_HHHS_REPAIR: u32 = PROFILE_CAP_HHHS_REPAIR;
    pub const CAP_REALTIME: u32 = PROFILE_CAP_REALTIME;
    pub const CAP_WALKIE_EXTENSION: u32 = PROFILE_CAP_WALKIE_EXTENSION;

    pub const WALKIE: Self = Self {
        music_generation: tutti_music_hhhs::PROTOCOL_GENERATION,
        music_vocabulary_generation: tutti_music_hhhs::EMBEDDED_VOCABULARY_GENERATION,
        hhhs_strategy_version: LANE_STRATEGY_VERSION,
        hhhs_repair_generation: hhhs_sync::REPAIR_WIRE_GENERATION as u32,
        room_generation: ROOM_PROTOCOL_GENERATION,
        capabilities: Self::CAP_MUSIC
            | Self::CAP_HHHS_REPAIR
            | Self::CAP_REALTIME
            | Self::CAP_WALKIE_EXTENSION,
    };

    pub const TUTTI_LEAF: Self = Self {
        music_generation: tutti_music_hhhs::PROTOCOL_GENERATION,
        music_vocabulary_generation: tutti_music_hhhs::EMBEDDED_VOCABULARY_GENERATION,
        hhhs_strategy_version: tutti_music_hhhs::STRATEGY_VERSION,
        hhhs_repair_generation: hhhs_sync::REPAIR_WIRE_GENERATION as u32,
        room_generation: 0,
        capabilities: Self::CAP_MUSIC | Self::CAP_REALTIME,
    };

    pub fn check_compatible(self, remote: Self) -> Result<(), ProtocolMismatch> {
        if self.capabilities & Self::CAP_MUSIC != 0
            && remote.capabilities & Self::CAP_MUSIC != 0
            && self.music_generation != remote.music_generation
        {
            return Err(ProtocolMismatch::MusicGeneration {
                local: self.music_generation,
                remote: remote.music_generation,
            });
        }
        if self.hhhs_strategy_version != remote.hhhs_strategy_version {
            return Err(ProtocolMismatch::HhhsStrategy {
                local: self.hhhs_strategy_version,
                remote: remote.hhhs_strategy_version,
            });
        }
        if self.capabilities & Self::CAP_MUSIC != 0
            && remote.capabilities & Self::CAP_MUSIC != 0
            && self.music_vocabulary_generation != remote.music_vocabulary_generation
        {
            return Err(ProtocolMismatch::MusicVocabularyGeneration {
                local: self.music_vocabulary_generation,
                remote: remote.music_vocabulary_generation,
            });
        }
        if self.capabilities & Self::CAP_HHHS_REPAIR != 0
            && remote.capabilities & Self::CAP_HHHS_REPAIR != 0
            && self.hhhs_repair_generation != remote.hhhs_repair_generation
        {
            return Err(ProtocolMismatch::HhhsRepairGeneration {
                local: self.hhhs_repair_generation,
                remote: remote.hhhs_repair_generation,
            });
        }
        if self.capabilities & Self::CAP_WALKIE_EXTENSION != 0
            && remote.capabilities & Self::CAP_WALKIE_EXTENSION != 0
            && self.room_generation != remote.room_generation
        {
            return Err(ProtocolMismatch::RoomGeneration {
                local: self.room_generation,
                remote: remote.room_generation,
            });
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Error)]
pub enum ProtocolMismatch {
    #[error("music generation differs (local {local}, remote {remote})")]
    MusicGeneration { local: u32, remote: u32 },
    #[error("music vocabulary differs (local {local}, remote {remote})")]
    MusicVocabularyGeneration { local: u32, remote: u32 },
    #[error("HHHS lane strategy differs (local {local}, remote {remote})")]
    HhhsStrategy { local: u32, remote: u32 },
    #[error("HHHS repair wire generation differs (local {local}, remote {remote})")]
    HhhsRepairGeneration { local: u32, remote: u32 },
    #[error("Walkie room generation differs (local {local}, remote {remote})")]
    RoomGeneration { local: u32, remote: u32 },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum LinkState {
    Offline = 0,
    Discovering = 1,
    Connecting = 2,
    Authenticating = 3,
    Repairing = 4,
    Ready = 5,
    Refused = 6,
    Failed = 7,
}

impl LinkState {
    const fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Discovering,
            2 => Self::Connecting,
            3 => Self::Authenticating,
            4 => Self::Repairing,
            5 => Self::Ready,
            6 => Self::Refused,
            7 => Self::Failed,
            _ => Self::Offline,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BridgeStatus {
    pub revision: u64,
    pub room_link: LinkState,
    pub board_link: LinkState,
    pub room_peers: u32,
    pub trusted_boards: u32,
    /// MIDI events accepted from the DAW/standalone host.
    pub realtime_ingress_events: u64,
    /// MIDI events accepted from a room or board carrier for the host.
    pub realtime_egress_events: u64,
    pub realtime_ingress_dropped: u64,
    pub realtime_egress_dropped: u64,
    pub ui_events_dropped: u64,
}

struct AtomicBridgeStatus {
    revision: AtomicU64,
    room_link: AtomicU8,
    board_link: AtomicU8,
    room_peers: AtomicU32,
    trusted_boards: AtomicU32,
    realtime_ingress_events: AtomicU64,
    realtime_egress_events: AtomicU64,
    realtime_ingress_dropped: AtomicU64,
    realtime_egress_dropped: AtomicU64,
    ui_events_dropped: AtomicU64,
}

impl Default for AtomicBridgeStatus {
    fn default() -> Self {
        Self {
            revision: AtomicU64::new(0),
            room_link: AtomicU8::new(LinkState::Offline as u8),
            board_link: AtomicU8::new(LinkState::Offline as u8),
            room_peers: AtomicU32::new(0),
            trusted_boards: AtomicU32::new(0),
            realtime_ingress_events: AtomicU64::new(0),
            realtime_egress_events: AtomicU64::new(0),
            realtime_ingress_dropped: AtomicU64::new(0),
            realtime_egress_dropped: AtomicU64::new(0),
            ui_events_dropped: AtomicU64::new(0),
        }
    }
}

impl AtomicBridgeStatus {
    fn load(&self) -> BridgeStatus {
        BridgeStatus {
            revision: self.revision.load(Ordering::Acquire),
            room_link: LinkState::from_u8(self.room_link.load(Ordering::Acquire)),
            board_link: LinkState::from_u8(self.board_link.load(Ordering::Acquire)),
            room_peers: self.room_peers.load(Ordering::Acquire),
            trusted_boards: self.trusted_boards.load(Ordering::Acquire),
            realtime_ingress_events: self.realtime_ingress_events.load(Ordering::Relaxed),
            realtime_egress_events: self.realtime_egress_events.load(Ordering::Relaxed),
            realtime_ingress_dropped: self.realtime_ingress_dropped.load(Ordering::Relaxed),
            realtime_egress_dropped: self.realtime_egress_dropped.load(Ordering::Relaxed),
            ui_events_dropped: self.ui_events_dropped.load(Ordering::Relaxed),
        }
    }

    fn revise(&self) {
        self.revision.fetch_add(1, Ordering::Release);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum RealtimeMidiKind {
    NoteOn = 1,
    NoteOff = 2,
    Choke = 3,
    PolyPressure = 4,
    PitchBend = 5,
    ChannelPressure = 6,
}

/// Policy applied to MIDI note edges as they cross into the room bridge.
///
/// This is deliberately independent from the confirmed room-to-host
/// projection. Input gestures propose edits or performances; they never
/// directly mutate the set of notes currently held at the host output.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(u8)]
pub enum MidiInputMode {
    /// A fresh note-on toggles pitch-class membership. Note-off only rearms the
    /// key so keyboard auto-repeat cannot toggle repeatedly.
    #[default]
    ToggleSet = 0,
    /// Note-on adds pitch-class membership and the final matching note-off
    /// removes it.
    GateSet = 1,
    /// Note edges are transient performance traffic and do not edit HHHS.
    Perform = 2,
}

impl MidiInputMode {
    const fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::GateSet,
            2 => Self::Perform,
            _ => Self::ToggleSet,
        }
    }
}

/// Fixed-size message crossing the realtime/background boundary.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct RealtimeMidi {
    pub timing: u32,
    pub voice_id: i32,
    pub channel: u8,
    pub note: u8,
    pub kind: RealtimeMidiKind,
    pub value: f32,
}

impl RealtimeMidi {
    pub const NO_VOICE_ID: i32 = -1;
    // A stable, nonnegative CLAP note ID namespace for edges produced by the
    // confirmed room projection. Hosts that preserve note IDs can route these
    // events back without turning them into new room edits.
    const MEMBERSHIP_VOICE_BASE: i32 = 0x5455_0000;

    pub const fn membership_voice_id(note: u8) -> i32 {
        Self::MEMBERSHIP_VOICE_BASE | note as i32
    }

    pub const fn is_membership_projection(self) -> bool {
        self.voice_id & !0x7f == Self::MEMBERSHIP_VOICE_BASE
            && self.note as i32 == self.voice_id & 0x7f
            && matches!(
                self.kind,
                RealtimeMidiKind::NoteOn | RealtimeMidiKind::NoteOff
            )
    }

    pub const fn is_release(self) -> bool {
        matches!(
            self.kind,
            RealtimeMidiKind::NoteOff | RealtimeMidiKind::Choke
        )
    }
}

/// Exact authenticated BLE placement that owns one provisioning exchange.
/// A bundle prepared for an older board boot or symmetric session must never
/// be installed on a newer link.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BoardSessionBinding {
    pub identity: [u8; 32],
    pub boot_nonce: u64,
    pub session_id: u64,
}

#[derive(Clone, PartialEq, Debug)]
pub enum BridgeCommand {
    /// Apply the persisted native-room identity before opening a room. Plugin
    /// state is restored after construction, so the room worker must not
    /// permanently capture the constructor's temporary seed.
    ConfigureRoomIdentity {
        identity_seed: [u8; 32],
    },
    SelectRoom(String),
    LeaveRoom,
    StartBoardScan,
    ConnectBoard(BleAddress),
    TrustBoard([u8; 32]),
    ForgetBoard([u8; 32]),
    DisconnectBoard,
    /// Ask the active room to grant and export a receiver-bound music bundle
    /// for this exact authenticated board placement.
    PrepareBoardProvisioning(BoardSessionBinding),
    /// Send a room-prepared, preflighted bundle only if the board placement is
    /// still current. Readiness waits for the board's digest acknowledgement.
    SendBoardCapabilityBundle {
        binding: BoardSessionBinding,
        bundle: Vec<u8>,
    },
    /// Begin one ordinary HHHS music repair against the provisioned board.
    StartBoardRepair(BoardSessionBinding),
    /// Byte-exact HHHS frame emitted by the room-owned stepwise attempt.
    SendBoardRepairFrame {
        binding: BoardSessionBinding,
        frame: Vec<u8>,
    },
    /// Byte-exact HHHS frame received from the authenticated BLE lane.
    ObserveBoardRepairFrame {
        binding: BoardSessionBinding,
        frame: Vec<u8>,
    },
    /// The room-owned HHHS attempt is terminal; begin explicit BLE FIN/Ack.
    FinishBoardRepair(BoardSessionBinding),
    /// The BLE carrier proved both terminal prefixes and acknowledgements.
    ConfirmBoardRepairClose(BoardSessionBinding),
    /// The room-owned attempt additionally proved same-placement freshness
    /// after carrier close; only this transition may make the board Ready.
    CompleteBoardRepair(BoardSessionBinding),
    /// Abandon an in-flight attempt after link/session/room replacement.
    AbortBoardRepair(BoardSessionBinding),
    /// Publish room-wide round-table timing/timbre settings.
    PublishRoundTable(tutti_roundtable::Frame),
    /// Queue one explicit board-origin gesture as a bounded source envelope.
    /// Settings and pitch edits remain separate durable intents and may admit
    /// independently; `token` supplies honest applied/rejected lifecycle and
    /// only fences stale presentation while their canonical result is pending.
    PublishBoardEdit {
        token: u64,
        frame: tutti_roundtable::Frame,
        settings: Option<RoundTableConfig>,
        pitch_edits: Vec<(TunedPeriodicPitch, bool)>,
    },
    /// Apply the derived room pitch set/settings to the local board only.
    SendBoardRoundTable(tutti_roundtable::Frame),
    /// Edit the room-owned set from a MIDI/board pitch. The default bridge
    /// canonicalizes this to a pitch-class member; any authorized room peer may
    /// add or causally remove it. `token` correlates queue acceptance with an
    /// explicit terminal adapter outcome; only canonical projection confirms
    /// membership.
    SetSharedPitch {
        token: u64,
        pitch: TunedPeriodicPitch,
        active: bool,
    },
    ConfigureBle {
        identity_seed: [u8; 32],
        trusted_boards: Vec<[u8; 32]>,
    },
}

#[derive(Clone, PartialEq, Debug)]
pub enum BridgeEvent {
    Status(BridgeStatus),
    RoomSelected(String),
    BoardDiscovered(BleScanResult),
    TrustRequired(PeerHello),
    ProtocolRefused {
        local: ProtocolProfile,
        remote: ProtocolProfile,
        reason: ProtocolMismatch,
    },
    Diagnostic(String),
    RoundTable(tutti_roundtable::Frame),
    PitchSet(SharedPitchSet),
}

#[derive(Debug, Error)]
pub enum BridgeError {
    #[error("the bounded {queue} queue is full")]
    QueueFull { queue: &'static str },
    #[error("bridge transport is unavailable: {0}")]
    Unavailable(String),
    #[error("bridge transport failed: {0}")]
    Transport(String),
}

#[derive(Clone, Debug)]
pub struct BridgeConfig {
    pub ui_command_capacity: usize,
    pub ui_event_capacity: usize,
    pub realtime_capacity: usize,
    pub release_capacity: usize,
    pub poll_interval: Duration,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            ui_command_capacity: DEFAULT_UI_COMMAND_CAPACITY,
            ui_event_capacity: DEFAULT_UI_EVENT_CAPACITY,
            realtime_capacity: DEFAULT_REALTIME_CAPACITY,
            release_capacity: DEFAULT_RELEASE_CAPACITY,
            poll_interval: DEFAULT_POLL_INTERVAL,
        }
    }
}

impl BridgeConfig {
    fn validate(&self) {
        assert!(self.ui_command_capacity > 0, "UI command capacity is zero");
        assert!(self.ui_event_capacity > 0, "UI event capacity is zero");
        assert!(self.realtime_capacity > 0, "realtime capacity is zero");
        assert!(self.release_capacity > 0, "release capacity is zero");
        assert!(!self.poll_interval.is_zero(), "poll interval is zero");
    }
}

/// Transport-owned events delivered to the core from its background thread.
#[derive(Clone, PartialEq, Debug)]
pub enum TransportEvent {
    RoomLink(LinkState),
    RoomSelected(String),
    BoardLink(LinkState),
    RoomPeers(u32),
    TrustedBoards(u32),
    Midi(RealtimeMidi),
    BoardDiscovered(BleScanResult),
    TrustRequired(PeerHello),
    RemoteProfile(ProtocolProfile),
    /// The signed board session and tagged profile are established, but the
    /// board is not ready until room capability provisioning completes.
    BoardProvisioningRequired(BoardSessionBinding),
    /// Active room result for one exact board placement.
    BoardCapabilityBundlePrepared {
        binding: BoardSessionBinding,
        bundle: Vec<u8>,
    },
    BoardProvisioningFailed {
        binding: BoardSessionBinding,
        reason: String,
    },
    /// The board imported and proved possession for the exact bundle digest.
    BoardProvisioned(BoardSessionBinding),
    BoardRepairOutbound {
        binding: BoardSessionBinding,
        frame: Vec<u8>,
    },
    BoardRepairInbound {
        binding: BoardSessionBinding,
        frame: Vec<u8>,
    },
    BoardRepairTerminal(BoardSessionBinding),
    BoardRepairCarrierClosed(BoardSessionBinding),
    BoardRepairSynchronized(BoardSessionBinding),
    /// The room-owned stepwise attempt failed before symmetric close. The
    /// board leg must abandon the matching authenticated attempt and must not
    /// remain observably ready or repairing.
    BoardRepairFailed {
        binding: BoardSessionBinding,
        reason: String,
    },
    RoundTable(tutti_roundtable::Frame),
    /// Materialized shared set received from the Iroh room leg.
    RoomPitchSet(SharedPitchSet),
    /// Authenticated board round-table frame. Configuration frames are
    /// classified by the core as exact acknowledgements, stale echoes, or
    /// genuine board edits before anything is published into the room.
    BoardRoundTable(tutti_roundtable::Frame),
    /// Completion of the native room adapter's work for one queued pitch
    /// intent. For a mixed board gesture, `Applied` means every separate
    /// durable intent in the envelope admitted; it never supersedes canonical
    /// SharedPitchSet confirmation.
    PitchIntentOutcome {
        token: u64,
        outcome: PitchIntentOutcome,
    },
    /// One or more terminal outcomes could not be delivered (for example
    /// because the bounded adapter event queue was saturated). Only intents
    /// at or below `through_token` are abandoned, so a late reset cannot erase
    /// a newer pitch intent.
    PitchIntentReset {
        through_token: u64,
    },
    Diagnostic(String),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PitchIntentOutcome {
    /// All separate durable intents and the associated realtime publication
    /// completed. Canonical projection still supplies the membership truth.
    Applied,
    /// The envelope stopped. A prefix (or even all durable intents followed
    /// by a carrier failure) may already be canonical; callers must consume
    /// the room projection rather than infer rollback from this outcome.
    Rejected(String),
}

/// Background-only carrier boundary.
///
/// Implementations may allocate and perform I/O here. None of these methods is
/// called from a plugin process callback.
pub trait BridgeTransport: Send + 'static {
    fn start(&mut self) -> Result<(), BridgeError>;
    fn handle_command(&mut self, command: BridgeCommand) -> Result<(), BridgeError>;
    fn send_realtime(&mut self, event: RealtimeMidi) -> Result<(), BridgeError>;
    fn poll_event(&mut self) -> Option<TransportEvent>;
    fn shutdown(&mut self);
}

/// Safe default used until a platform BLE backend is selected.
#[derive(Default)]
pub struct UnavailableTransport;

impl BridgeTransport for UnavailableTransport {
    fn start(&mut self) -> Result<(), BridgeError> {
        Ok(())
    }

    fn handle_command(&mut self, _command: BridgeCommand) -> Result<(), BridgeError> {
        Err(BridgeError::Unavailable(
            "no native Iroh/BLE bridge adapter is configured".into(),
        ))
    }

    fn send_realtime(&mut self, _event: RealtimeMidi) -> Result<(), BridgeError> {
        Err(BridgeError::Unavailable(
            "no realtime carrier is connected".into(),
        ))
    }

    fn poll_event(&mut self) -> Option<TransportEvent> {
        None
    }

    fn shutdown(&mut self) {}
}

struct BridgeQueues {
    ui_commands: Arc<ArrayQueue<BridgeCommand>>,
    ui_events: Arc<ArrayQueue<BridgeEvent>>,
    realtime_in: Arc<ArrayQueue<RealtimeMidi>>,
    release_in: Arc<ArrayQueue<RealtimeMidi>>,
    realtime_out: Arc<ArrayQueue<RealtimeMidi>>,
    release_out: Arc<ArrayQueue<RealtimeMidi>>,
}

/// Background-owned state for the notes-chatroom projection.
///
/// Optimistic shadow of the one room-owned set plus endpoint reconciliation
/// state. Input sources are adapters editing this set, not owners of subsets.
struct BridgePitchState {
    board_confirmed: Option<RoundTableConfig>,
    board_target: Option<RoundTableConfig>,
    board_retry_at: Option<Instant>,
    /// Highest passive snapshot revision accepted in this authenticated board
    /// connection. Reset when the board link establishes a new session.
    board_snapshot_revision: Option<u64>,
    host_held: [u64; 2],
    /// Latest unconfirmed local intent per pitch class. This is consulted only
    /// when interpreting the next input gesture; it must never drive host
    /// output or board materialization.
    pending_classes: BTreeMap<TunedDegree, PendingPitchIntent>,
    next_intent_token: u64,
    /// Last confirmed HHHS materialization received from the room carrier.
    room: SharedPitchSet,
    room_config: RoundTableConfig,
    output_notes: [bool; 128],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct PendingPitchIntent {
    active: bool,
    intent_token: u64,
    source: PitchIntentSource,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PitchIntentSource {
    Host,
    Board,
}

struct BoardEditPlan {
    settings: Option<RoundTableConfig>,
    pitch_edits: Vec<(TunedPeriodicPitch, bool)>,
}

impl Default for BridgePitchState {
    fn default() -> Self {
        Self {
            board_confirmed: None,
            board_target: None,
            board_retry_at: None,
            board_snapshot_revision: None,
            host_held: [0; 2],
            pending_classes: BTreeMap::new(),
            next_intent_token: 1,
            room: SharedPitchSet::default(),
            room_config: RoundTableConfig::default(),
            output_notes: [false; 128],
        }
    }
}

impl BridgePitchState {
    fn set_host_held(&mut self, note: u8, held: bool) -> bool {
        let index = usize::from(note);
        let mask = 1_u64 << (index % 64);
        let word = &mut self.host_held[index / 64];
        let was_held = *word & mask != 0;
        if held {
            *word |= mask;
        } else {
            *word &= !mask;
        }
        was_held
    }

    fn pitch_for_midi(note: u8) -> Result<TunedPeriodicPitch, BridgeError> {
        let tuning = Tuning::twelve_tet();
        let pitch = tuning.periodic_pitch_for_midi(note).ok_or_else(|| {
            BridgeError::Transport(format!("MIDI note {note} is outside the active tuning"))
        })?;
        Ok(TunedPeriodicPitch {
            tuning_id: tuning.id(),
            pitch,
        })
    }

    fn shared_degree(pitch: TunedPeriodicPitch) -> TunedDegree {
        TunedDegree {
            tuning_id: pitch.tuning_id,
            degree: pitch.pitch.degree(),
        }
    }

    fn contains_optimistic_pitch_class(&self, pitch: TunedPeriodicPitch) -> bool {
        let degree = Self::shared_degree(pitch);
        self.pending_classes
            .get(&degree)
            .map(|intent| intent.active)
            .unwrap_or_else(|| self.confirmed_contains_pitch_class(degree))
    }

    fn confirmed_contains_pitch_class(&self, degree: TunedDegree) -> bool {
        self.room.pitch_classes.contains(&degree)
            || self.room.pitches.iter().any(|candidate| {
                candidate.tuning_id == degree.tuning_id && candidate.pitch.degree() == degree.degree
            })
    }

    fn allocate_intent_token(&mut self) -> Result<u64, BridgeError> {
        let token = self.next_intent_token;
        self.next_intent_token = token.checked_add(1).ok_or_else(|| {
            BridgeError::Transport("pitch intent correlation space exhausted".into())
        })?;
        Ok(token)
    }

    fn track_pitch_intent(
        &mut self,
        token: u64,
        source: PitchIntentSource,
        pitch_edits: &[(TunedPeriodicPitch, bool)],
    ) {
        for (pitch, active) in pitch_edits {
            self.pending_classes.insert(
                Self::shared_degree(*pitch),
                PendingPitchIntent {
                    active: *active,
                    intent_token: token,
                    source,
                },
            );
        }
    }

    fn reject_pitch_intent(&mut self, token: u64) {
        self.pending_classes
            .retain(|_, intent| intent.intent_token != token);
    }

    fn abandon_pitch_intents(&mut self) {
        self.pending_classes.clear();
    }

    fn abandon_board_pitch_intents(&mut self) {
        self.pending_classes
            .retain(|_, intent| intent.source != PitchIntentSource::Board);
    }

    fn abandon_pitch_intents_through(&mut self, through_token: u64) {
        self.pending_classes
            .retain(|_, intent| intent.intent_token > through_token);
    }

    fn has_pending_pitch_intent(&self) -> bool {
        !self.pending_classes.is_empty()
    }

    fn settings_only(mut config: RoundTableConfig) -> RoundTableConfig {
        config.pattern = RoundTablePattern::default().cleared();
        config
    }

    fn plan_board_edit(&self, config: RoundTableConfig) -> Result<BoardEditPlan, BridgeError> {
        let previous = self.room_pattern();
        let settings = Self::settings_only(config);
        let current_settings = Self::settings_only(self.room_config);
        let settings = (settings != current_settings).then_some(settings);
        let mut pitch_edits = Vec::new();
        for pitch_class in 0_u8..12 {
            let note = 48 + pitch_class;
            let before = previous.contains(note);
            let after = config.pattern.contains(note);
            if before != after {
                pitch_edits.push((Self::pitch_for_midi(note)?, after));
            }
        }
        Ok(BoardEditPlan {
            settings,
            pitch_edits,
        })
    }

    /// Install an authoritative materialization and retire only those pending
    /// intents it actually confirms. A stale snapshot therefore cannot erase
    /// a newer local intent.
    fn apply_confirmed_room(&mut self, room: SharedPitchSet) -> bool {
        let changed = self.room != room;
        self.room = room;
        let confirmed = &self.room;
        self.pending_classes.retain(|degree, intent| {
            let present = confirmed.pitch_classes.contains(degree)
                || confirmed.pitches.iter().any(|candidate| {
                    candidate.tuning_id == degree.tuning_id
                        && candidate.pitch.degree() == degree.degree
                });
            present != intent.active
        });
        changed
    }

    fn has_held_pitch_class(&self, pitch: TunedPeriodicPitch) -> bool {
        let target = pitch.pitch.degree();
        (0_u8..=127).any(|note| {
            let index = usize::from(note);
            let held = self.host_held[index / 64] & (1_u64 << (index % 64)) != 0;
            held && Self::pitch_for_midi(note)
                .map(|candidate| candidate.pitch.degree() == target)
                .unwrap_or(false)
        })
    }

    fn room_midi_notes(&self) -> [bool; 128] {
        let tuning = Tuning::twelve_tet();
        let mut notes = [false; 128];
        for degree in &self.room.pitch_classes {
            if degree.tuning_id == tuning.id() {
                let note = 48_u16.saturating_add(degree.degree.index());
                if let Ok(note) = u8::try_from(note)
                    && note <= 127
                {
                    notes[usize::from(note)] = true;
                }
            }
        }
        for pitch in &self.room.pitches {
            if pitch.tuning_id == tuning.id() {
                let note = fractional_midi(&tuning, pitch.pitch).round();
                if (0.0..=127.0).contains(&note) {
                    notes[note as usize] = true;
                }
            }
        }
        notes
    }

    fn board_config(&self) -> RoundTableConfig {
        let notes = self.room_midi_notes();
        let mut pattern = RoundTablePattern::default().cleared();
        for (note, active) in notes.into_iter().enumerate() {
            if active && let Ok(next) = pattern.toggled(note as u8) {
                pattern = next;
            }
        }
        let mut config = self.room_config;
        config.pattern = pattern;
        config
    }

    fn room_pattern(&self) -> RoundTablePattern {
        self.board_config().pattern
    }

    fn stage_board_target(&mut self) {
        // A local/board intent is not a second pitch-set authority, but it is
        // a continuity fence: do not repaint the board from an older
        // canonical projection while the room adapter is still admitting the
        // source-atomic edit. `apply_confirmed_room` retires these entries only
        // when the singular HHHS projection confirms their requested levels.
        if self.has_pending_pitch_intent() {
            self.board_target = None;
            self.board_retry_at = None;
            return;
        }
        let target = self.board_config();
        if self.board_confirmed == Some(target) && self.board_target.is_none() {
            return;
        }
        if self.board_target != Some(target) {
            self.board_target = Some(target);
        }
        self.board_retry_at = Some(Instant::now());
    }

    fn board_target_due(&mut self) -> Option<RoundTableFrame> {
        let target = self.board_target?;
        if self
            .board_retry_at
            .is_some_and(|deadline| Instant::now() < deadline)
        {
            return None;
        }
        self.board_retry_at = Some(Instant::now() + Duration::from_millis(250));
        Some(RoundTableFrame::Config(ConfigState { config: target }))
    }

    fn observe_board_config(&mut self, config: RoundTableConfig) -> BoardConfigObservation {
        // `Config` is an explicit board input intent. Passive applications and
        // acknowledgements use `ConfigSnapshot`, so they never enter here.
        if self.board_target.is_none() && self.board_confirmed == Some(config) {
            BoardConfigObservation::Confirmed
        } else {
            self.board_confirmed = Some(config);
            self.board_target = None;
            self.board_retry_at = None;
            BoardConfigObservation::BoardEdit
        }
    }

    fn observe_board_snapshot(
        &mut self,
        revision: u64,
        config: RoundTableConfig,
    ) -> BoardConfigObservation {
        if self
            .board_snapshot_revision
            .is_some_and(|seen| revision <= seen)
        {
            return BoardConfigObservation::StaleEcho;
        }
        self.board_snapshot_revision = Some(revision);
        if self.board_target == Some(config) {
            self.board_confirmed = Some(config);
            self.board_target = None;
            self.board_retry_at = None;
            BoardConfigObservation::Confirmed
        } else if self.board_target.is_some() {
            self.board_retry_at = Some(Instant::now());
            BoardConfigObservation::StaleEcho
        } else if self.board_confirmed == Some(config) {
            BoardConfigObservation::Confirmed
        } else {
            // A passive level is never an edit. Record what the board actually
            // materialized, then reassert the canonical room target. Genuine
            // board input arrives separately as an explicit `Config` intent.
            self.board_confirmed = Some(config);
            self.stage_board_target();
            BoardConfigObservation::StaleEcho
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BoardConfigObservation {
    Confirmed,
    StaleEcho,
    BoardEdit,
}

impl BridgeQueues {
    fn new(config: &BridgeConfig) -> Self {
        Self {
            ui_commands: Arc::new(ArrayQueue::new(config.ui_command_capacity)),
            ui_events: Arc::new(ArrayQueue::new(config.ui_event_capacity)),
            realtime_in: Arc::new(ArrayQueue::new(config.realtime_capacity)),
            release_in: Arc::new(ArrayQueue::new(config.release_capacity)),
            realtime_out: Arc::new(ArrayQueue::new(config.realtime_capacity)),
            release_out: Arc::new(ArrayQueue::new(config.release_capacity)),
        }
    }
}

/// Non-realtime command/event handle for editors and application shells.
#[derive(Clone)]
pub struct BridgeHandle {
    commands: Arc<ArrayQueue<BridgeCommand>>,
    events: Arc<ArrayQueue<BridgeEvent>>,
    status: Arc<AtomicBridgeStatus>,
    stopping: Arc<AtomicBool>,
}

impl BridgeHandle {
    pub fn try_command(&self, command: BridgeCommand) -> Result<(), BridgeError> {
        self.commands
            .push(command)
            .map_err(|_| BridgeError::QueueFull {
                queue: "UI command",
            })
    }

    pub fn try_event(&self) -> Option<BridgeEvent> {
        self.events.pop()
    }

    pub fn status(&self) -> BridgeStatus {
        self.status.load()
    }

    pub fn request_shutdown(&self) {
        self.stopping.store(true, Ordering::Release);
    }
}

/// Audio-callback endpoint. Every operation is bounded and non-blocking.
#[derive(Clone)]
pub struct AudioBridgePort {
    realtime_in: Arc<ArrayQueue<RealtimeMidi>>,
    release_in: Arc<ArrayQueue<RealtimeMidi>>,
    realtime_out: Arc<ArrayQueue<RealtimeMidi>>,
    release_out: Arc<ArrayQueue<RealtimeMidi>>,
    input_mode: Arc<AtomicU8>,
    output_enabled: Arc<AtomicBool>,
    status: Arc<AtomicBridgeStatus>,
}

impl AudioBridgePort {
    pub fn try_send(&self, event: RealtimeMidi) -> Result<(), RealtimeMidi> {
        let result = if event.is_release() {
            self.release_in.push(event)
        } else {
            self.realtime_in.push(event)
        };
        if result.is_err() {
            self.status
                .realtime_ingress_dropped
                .fetch_add(1, Ordering::Relaxed);
        } else {
            self.status
                .realtime_ingress_events
                .fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    /// Release/choke messages are always drained before ordinary updates.
    pub fn try_recv(&self) -> Option<RealtimeMidi> {
        self.release_out.pop().or_else(|| self.realtime_out.pop())
    }

    pub fn status(&self) -> BridgeStatus {
        self.status.load()
    }

    /// Update the input policy with a single realtime-safe atomic store.
    pub fn set_input_mode(&self, mode: MidiInputMode) {
        self.input_mode.store(mode as u8, Ordering::Release);
    }

    pub fn input_mode(&self) -> MidiInputMode {
        MidiInputMode::from_u8(self.input_mode.load(Ordering::Acquire))
    }

    /// Enable or retract the host's state-derived MIDI projection with one
    /// realtime-safe atomic store. Disabling is level-triggered: the
    /// background reconciler emits releases for every active membership, and
    /// re-enabling reconstructs the current authoritative set rather than
    /// replaying stale edges.
    pub fn set_output_enabled(&self, enabled: bool) {
        self.output_enabled.store(enabled, Ordering::Release);
    }

    pub fn output_enabled(&self) -> bool {
        self.output_enabled.load(Ordering::Acquire)
    }
}

/// Owns the background worker. Dropping it requests cancellation and joins the
/// worker outside the plugin process callback.
pub struct BridgeRuntime {
    handle: BridgeHandle,
    audio: AudioBridgePort,
    worker: Option<JoinHandle<()>>,
}

impl fmt::Debug for BridgeRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BridgeRuntime")
            .field("status", &self.handle.status())
            .finish_non_exhaustive()
    }
}

impl BridgeRuntime {
    pub fn spawn(config: BridgeConfig) -> std::io::Result<Self> {
        Self::spawn_with_transport(config, UnavailableTransport)
    }

    pub fn spawn_with_transport<T>(config: BridgeConfig, transport: T) -> std::io::Result<Self>
    where
        T: BridgeTransport,
    {
        config.validate();
        let queues = BridgeQueues::new(&config);
        let status = Arc::new(AtomicBridgeStatus::default());
        let stopping = Arc::new(AtomicBool::new(false));
        let input_mode = Arc::new(AtomicU8::new(MidiInputMode::default() as u8));
        let output_enabled = Arc::new(AtomicBool::new(true));
        let handle = BridgeHandle {
            commands: queues.ui_commands.clone(),
            events: queues.ui_events.clone(),
            status: status.clone(),
            stopping: stopping.clone(),
        };
        let audio = AudioBridgePort {
            realtime_in: queues.realtime_in.clone(),
            release_in: queues.release_in.clone(),
            realtime_out: queues.realtime_out.clone(),
            release_out: queues.release_out.clone(),
            input_mode: input_mode.clone(),
            output_enabled: output_enabled.clone(),
            status: status.clone(),
        };
        let worker = thread::Builder::new()
            .name("tutti-walkie-bridge".into())
            .spawn(move || {
                run_bridge(
                    config,
                    queues,
                    input_mode,
                    output_enabled,
                    status,
                    stopping,
                    transport,
                )
            })?;
        Ok(Self {
            handle,
            audio,
            worker: Some(worker),
        })
    }

    /// Construct an observable, non-panicking runtime when the operating
    /// system cannot create a bridge worker. The editor remains usable and can
    /// report the exact startup failure; the audio callback still sees valid,
    /// empty bounded queues.
    pub fn unavailable(config: BridgeConfig, error: impl Into<String>) -> Self {
        config.validate();
        let queues = BridgeQueues::new(&config);
        let status = Arc::new(AtomicBridgeStatus::default());
        status
            .room_link
            .store(LinkState::Failed as u8, Ordering::Release);
        status
            .board_link
            .store(LinkState::Failed as u8, Ordering::Release);
        status.revise();
        let stopping = Arc::new(AtomicBool::new(false));
        let input_mode = Arc::new(AtomicU8::new(MidiInputMode::default() as u8));
        let output_enabled = Arc::new(AtomicBool::new(true));
        let handle = BridgeHandle {
            commands: queues.ui_commands.clone(),
            events: queues.ui_events.clone(),
            status: status.clone(),
            stopping,
        };
        let audio = AudioBridgePort {
            realtime_in: queues.realtime_in,
            release_in: queues.release_in,
            realtime_out: queues.realtime_out,
            release_out: queues.release_out,
            input_mode,
            output_enabled,
            status: status.clone(),
        };
        publish_event(
            &handle.events,
            &status,
            BridgeEvent::Diagnostic(error.into()),
        );
        Self {
            handle,
            audio,
            worker: None,
        }
    }

    pub fn handle(&self) -> BridgeHandle {
        self.handle.clone()
    }

    pub fn audio_port(&self) -> AudioBridgePort {
        self.audio.clone()
    }

    pub fn shutdown(&mut self) {
        self.handle.request_shutdown();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for BridgeRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn apply_shared_pitch_intent<T: BridgeTransport>(
    pitches: &mut BridgePitchState,
    transport: &mut T,
    pitch: TunedPeriodicPitch,
    active: bool,
) -> Result<(), BridgeError> {
    let token = pitches.allocate_intent_token()?;
    transport.handle_command(BridgeCommand::SetSharedPitch {
        token,
        pitch,
        active,
    })?;
    pitches.track_pitch_intent(token, PitchIntentSource::Host, &[(pitch, active)]);
    Ok(())
}

fn handle_host_note_release<T: BridgeTransport>(
    pitches: &mut BridgePitchState,
    transport: &mut T,
    mode: MidiInputMode,
    note: u8,
) -> Result<(), BridgeError> {
    let was_held = pitches.set_host_held(note, false);
    if mode != MidiInputMode::GateSet || !was_held {
        return Ok(());
    }

    let pitch = BridgePitchState::pitch_for_midi(note)?;
    // Gate mode aggregates octaves locally. Releasing C4 must not remove the
    // shared C member while C5 is still physically held by this input.
    if !pitches.has_held_pitch_class(pitch) && pitches.contains_optimistic_pitch_class(pitch) {
        apply_shared_pitch_intent(pitches, transport, pitch, false)?;
    }
    Ok(())
}

fn handle_host_note_on<T: BridgeTransport>(
    pitches: &mut BridgePitchState,
    transport: &mut T,
    mode: MidiInputMode,
    note: u8,
) -> Result<(), BridgeError> {
    if pitches.set_host_held(note, true) {
        return Ok(());
    }

    let pitch = BridgePitchState::pitch_for_midi(note)?;
    let present = pitches.contains_optimistic_pitch_class(pitch);
    match mode {
        MidiInputMode::ToggleSet => apply_shared_pitch_intent(pitches, transport, pitch, !present),
        MidiInputMode::GateSet if !present => {
            apply_shared_pitch_intent(pitches, transport, pitch, true)
        }
        MidiInputMode::GateSet | MidiInputMode::Perform => Ok(()),
    }
}

fn should_forward_room_realtime(mode: MidiInputMode, event: RealtimeMidi) -> bool {
    mode == MidiInputMode::Perform || event.is_release()
}

fn run_bridge<T: BridgeTransport>(
    config: BridgeConfig,
    queues: BridgeQueues,
    input_mode: Arc<AtomicU8>,
    output_enabled: Arc<AtomicBool>,
    status: Arc<AtomicBridgeStatus>,
    stopping: Arc<AtomicBool>,
    mut transport: T,
) {
    let mut pitches = BridgePitchState::default();
    if let Err(error) = transport.start() {
        status
            .room_link
            .store(LinkState::Failed as u8, Ordering::Release);
        status
            .board_link
            .store(LinkState::Failed as u8, Ordering::Release);
        publish_event(
            &queues.ui_events,
            &status,
            BridgeEvent::Diagnostic(error.to_string()),
        );
    }

    while !stopping.load(Ordering::Acquire) {
        let mut progressed = false;

        while let Some(command) = queues.ui_commands.pop() {
            progressed = true;
            if let Err(error) = transport.handle_command(command) {
                publish_event(
                    &queues.ui_events,
                    &status,
                    BridgeEvent::Diagnostic(error.to_string()),
                );
            }
        }

        while let Some(event) = queues.release_in.pop() {
            progressed = true;
            let mode = MidiInputMode::from_u8(input_mode.load(Ordering::Acquire));
            let result = if mode == MidiInputMode::Perform {
                transport.send_realtime(event)
            } else if matches!(
                event.kind,
                RealtimeMidiKind::NoteOff | RealtimeMidiKind::Choke
            ) {
                handle_host_note_release(&mut pitches, &mut transport, mode, event.note)
            } else {
                transport.send_realtime(event)
            };
            if let Err(error) = result {
                publish_event(
                    &queues.ui_events,
                    &status,
                    BridgeEvent::Diagnostic(error.to_string()),
                );
            }
        }
        while let Some(event) = queues.realtime_in.pop() {
            progressed = true;
            let mode = MidiInputMode::from_u8(input_mode.load(Ordering::Acquire));
            if event.kind == RealtimeMidiKind::NoteOn {
                if mode == MidiInputMode::Perform {
                    if let Err(error) = transport.send_realtime(event) {
                        publish_event(
                            &queues.ui_events,
                            &status,
                            BridgeEvent::Diagnostic(error.to_string()),
                        );
                    }
                    continue;
                }
                if event.value <= 0.0 {
                    if let Err(error) =
                        handle_host_note_release(&mut pitches, &mut transport, mode, event.note)
                    {
                        publish_event(
                            &queues.ui_events,
                            &status,
                            BridgeEvent::Diagnostic(error.to_string()),
                        );
                    }
                    continue;
                }
                if let Err(error) =
                    handle_host_note_on(&mut pitches, &mut transport, mode, event.note)
                {
                    publish_event(
                        &queues.ui_events,
                        &status,
                        BridgeEvent::Diagnostic(error.to_string()),
                    );
                }
                continue;
            }
            if let Err(error) = transport.send_realtime(event) {
                publish_event(
                    &queues.ui_events,
                    &status,
                    BridgeEvent::Diagnostic(error.to_string()),
                );
            }
        }

        for _ in 0..64 {
            let Some(event) = transport.poll_event() else {
                break;
            };
            progressed = true;
            match event {
                TransportEvent::RoomLink(LinkState::Ready) => {
                    handle_transport_event(
                        TransportEvent::RoomLink(LinkState::Ready),
                        &queues,
                        &status,
                    );
                }
                TransportEvent::RoomLink(link) => {
                    pitches.abandon_pitch_intents();
                    pitches.board_target = None;
                    pitches.board_retry_at = None;
                    pitches.stage_board_target();
                    handle_transport_event(TransportEvent::RoomLink(link), &queues, &status);
                }
                TransportEvent::RoomSelected(room) => {
                    pitches.abandon_pitch_intents();
                    pitches.board_target = None;
                    pitches.board_retry_at = None;
                    pitches.stage_board_target();
                    handle_transport_event(TransportEvent::RoomSelected(room), &queues, &status);
                }
                TransportEvent::BoardLink(LinkState::Ready) => {
                    handle_transport_event(
                        TransportEvent::BoardLink(LinkState::Ready),
                        &queues,
                        &status,
                    );
                    pitches.board_confirmed = None;
                    pitches.board_snapshot_revision = None;
                    pitches.stage_board_target();
                }
                TransportEvent::BoardLink(link) => {
                    pitches.abandon_board_pitch_intents();
                    pitches.board_target = None;
                    pitches.board_retry_at = None;
                    pitches.board_confirmed = None;
                    pitches.board_snapshot_revision = None;
                    handle_transport_event(TransportEvent::BoardLink(link), &queues, &status);
                }
                TransportEvent::BoardRoundTable(frame) => {
                    let board_config = match frame {
                        RoundTableFrame::Run(run) => {
                            Some((run.config, pitches.observe_board_config(run.config)))
                        }
                        RoundTableFrame::Config(config) => {
                            Some((config.config, pitches.observe_board_config(config.config)))
                        }
                        RoundTableFrame::ConfigSnapshot(snapshot) => Some((
                            snapshot.config,
                            pitches.observe_board_snapshot(snapshot.revision, snapshot.config),
                        )),
                        RoundTableFrame::Pulse(_) => None,
                    };
                    if let Some((config, observation)) = board_config
                        && observation == BoardConfigObservation::BoardEdit
                    {
                        let mut failed = None;
                        match pitches.plan_board_edit(config) {
                            Ok(plan) => {
                                let source_frame = match frame {
                                    RoundTableFrame::Run(_) | RoundTableFrame::Config(_) => frame,
                                    // Passive snapshots can never reach the
                                    // BoardEdit branch, but preserve that invariant
                                    // explicitly if the classifier evolves.
                                    RoundTableFrame::ConfigSnapshot(_) => {
                                        failed = Some(BridgeError::Transport(
                                            "passive board snapshot cannot author a room edit"
                                                .into(),
                                        ));
                                        frame
                                    }
                                    RoundTableFrame::Pulse(_) => unreachable!(
                                        "a pulse has no board configuration to classify"
                                    ),
                                };
                                if failed.is_none() {
                                    let token = pitches.allocate_intent_token();
                                    match token.and_then(|token| {
                                        transport
                                            .handle_command(BridgeCommand::PublishBoardEdit {
                                                token,
                                                frame: source_frame,
                                                settings: plan.settings,
                                                pitch_edits: plan.pitch_edits.clone(),
                                            })
                                            .map(|()| token)
                                    }) {
                                        Ok(token) => {
                                            pitches.track_pitch_intent(
                                                token,
                                                PitchIntentSource::Board,
                                                &plan.pitch_edits,
                                            );
                                        }
                                        Err(error) => failed = Some(error),
                                    }
                                }
                            }
                            Err(error) => failed = Some(error),
                        }
                        if let Some(error) = failed {
                            publish_event(
                                &queues.ui_events,
                                &status,
                                BridgeEvent::Diagnostic(error.to_string()),
                            );
                        }
                    } else if matches!(frame, RoundTableFrame::Run(_) | RoundTableFrame::Pulse(_))
                        && let Err(error) =
                            transport.handle_command(BridgeCommand::PublishRoundTable(frame))
                    {
                        publish_event(
                            &queues.ui_events,
                            &status,
                            BridgeEvent::Diagnostic(error.to_string()),
                        );
                    }
                }
                TransportEvent::PitchIntentOutcome { token, outcome } => {
                    if let PitchIntentOutcome::Rejected(error) = outcome {
                        pitches.reject_pitch_intent(token);
                        pitches.stage_board_target();
                        publish_event(
                            &queues.ui_events,
                            &status,
                            BridgeEvent::Diagnostic(format!(
                                "pitch intent {token} was rejected: {error}"
                            )),
                        );
                    }
                }
                TransportEvent::PitchIntentReset { through_token } => {
                    pitches.abandon_pitch_intents_through(through_token);
                    pitches.stage_board_target();
                    publish_event(
                        &queues.ui_events,
                        &status,
                        BridgeEvent::Diagnostic(format!(
                            "pitch intent outcomes through {through_token} were abandoned"
                        )),
                    );
                }
                TransportEvent::RoomPitchSet(shared) => {
                    if pitches.apply_confirmed_room(shared.clone()) {
                        publish_event(&queues.ui_events, &status, BridgeEvent::PitchSet(shared));
                        pitches.stage_board_target();
                    }
                }
                TransportEvent::RoundTable(frame) => {
                    match frame {
                        RoundTableFrame::Run(run) => pitches.room_config = run.config,
                        RoundTableFrame::Config(config) => {
                            pitches.room_config = config.config;
                        }
                        RoundTableFrame::ConfigSnapshot(_) => {}
                        RoundTableFrame::Pulse(_) => {}
                    }
                    pitches.stage_board_target();
                    handle_transport_event(TransportEvent::RoundTable(frame), &queues, &status);
                }
                // In set-editing modes the host endpoint is a level-triggered
                // projection of durable membership. Mixing best-effort
                // performance edges into that same endpoint can leave notes
                // stuck after a lost NoteOff and makes the DAW disagree with
                // the visible set. Realtime MIDI is a distinct contract and is
                // forwarded only in Perform mode. Releases remain admissible
                // while leaving Perform mode so an already-forwarded note can
                // still be balanced.
                TransportEvent::Midi(event)
                    if !should_forward_room_realtime(
                        MidiInputMode::from_u8(input_mode.load(Ordering::Acquire)),
                        event,
                    ) => {}
                event => handle_transport_event(event, &queues, &status),
            }
        }

        if let Some(frame) = pitches.board_target_due() {
            progressed = true;
            if let Err(error) = transport.handle_command(BridgeCommand::SendBoardRoundTable(frame))
            {
                publish_event(
                    &queues.ui_events,
                    &status,
                    BridgeEvent::Diagnostic(error.to_string()),
                );
            }
        }

        reconcile_host_output(
            &mut pitches,
            &queues,
            &status,
            output_enabled.load(Ordering::Acquire),
        );

        if !progressed {
            thread::park_timeout(config.poll_interval);
        }
    }

    transport.shutdown();
    status
        .room_link
        .store(LinkState::Offline as u8, Ordering::Release);
    status
        .board_link
        .store(LinkState::Offline as u8, Ordering::Release);
    status.revise();
}

/// Reconcile the host MIDI endpoint from its shadow to the materialized room
/// set. Releases always enter the priority queue before additions. A saturated
/// queue leaves the shadow unchanged so the next worker pass retries state,
/// rather than pretending a dropped edge reached the downstream arpeggiator.
fn reconcile_host_output(
    pitches: &mut BridgePitchState,
    queues: &BridgeQueues,
    status: &AtomicBridgeStatus,
    output_enabled: bool,
) {
    let target = if output_enabled {
        pitches.room_midi_notes()
    } else {
        [false; 128]
    };
    for note in 0_u8..=127 {
        let index = usize::from(note);
        if pitches.output_notes[index] && !target[index] {
            let event = RealtimeMidi {
                timing: 0,
                voice_id: RealtimeMidi::membership_voice_id(note),
                channel: 0,
                note,
                kind: RealtimeMidiKind::NoteOff,
                value: 0.0,
            };
            if queues.release_out.push(event).is_err() {
                status
                    .realtime_egress_dropped
                    .fetch_add(1, Ordering::Relaxed);
                return;
            }
            pitches.output_notes[index] = false;
            status
                .realtime_egress_events
                .fetch_add(1, Ordering::Relaxed);
        }
    }
    for note in 0_u8..=127 {
        let index = usize::from(note);
        if !pitches.output_notes[index] && target[index] {
            let event = RealtimeMidi {
                timing: 0,
                voice_id: RealtimeMidi::membership_voice_id(note),
                channel: 0,
                note,
                kind: RealtimeMidiKind::NoteOn,
                value: 0.8,
            };
            if queues.realtime_out.push(event).is_err() {
                status
                    .realtime_egress_dropped
                    .fetch_add(1, Ordering::Relaxed);
                return;
            }
            pitches.output_notes[index] = true;
            status
                .realtime_egress_events
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn handle_transport_event(
    event: TransportEvent,
    queues: &BridgeQueues,
    status: &AtomicBridgeStatus,
) {
    match event {
        TransportEvent::RoomLink(link) => {
            status.room_link.store(link as u8, Ordering::Release);
            status.revise();
            publish_event(
                &queues.ui_events,
                status,
                BridgeEvent::Status(status.load()),
            );
        }
        TransportEvent::RoomSelected(room) => {
            publish_event(&queues.ui_events, status, BridgeEvent::RoomSelected(room));
        }
        TransportEvent::BoardLink(link) => {
            status.board_link.store(link as u8, Ordering::Release);
            status.revise();
            publish_event(
                &queues.ui_events,
                status,
                BridgeEvent::Status(status.load()),
            );
        }
        TransportEvent::RoomPeers(peers) => {
            status.room_peers.store(peers, Ordering::Release);
            status.revise();
            publish_event(
                &queues.ui_events,
                status,
                BridgeEvent::Status(status.load()),
            );
        }
        TransportEvent::TrustedBoards(boards) => {
            status.trusted_boards.store(boards, Ordering::Release);
            status.revise();
            publish_event(
                &queues.ui_events,
                status,
                BridgeEvent::Status(status.load()),
            );
        }
        TransportEvent::Midi(event) => {
            let result = if event.is_release() {
                queues.release_out.push(event)
            } else {
                queues.realtime_out.push(event)
            };
            if result.is_err() {
                status
                    .realtime_egress_dropped
                    .fetch_add(1, Ordering::Relaxed);
            } else {
                status
                    .realtime_egress_events
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        TransportEvent::BoardDiscovered(board) => publish_event(
            &queues.ui_events,
            status,
            BridgeEvent::BoardDiscovered(board),
        ),
        TransportEvent::TrustRequired(hello) => {
            publish_event(&queues.ui_events, status, BridgeEvent::TrustRequired(hello))
        }
        TransportEvent::RemoteProfile(remote) => {
            if let Err(reason) = ProtocolProfile::WALKIE.check_compatible(remote) {
                status
                    .board_link
                    .store(LinkState::Refused as u8, Ordering::Release);
                status.revise();
                publish_event(
                    &queues.ui_events,
                    status,
                    BridgeEvent::ProtocolRefused {
                        local: ProtocolProfile::WALKIE,
                        remote,
                        reason,
                    },
                );
            }
        }
        TransportEvent::RoundTable(frame) => {
            publish_event(&queues.ui_events, status, BridgeEvent::RoundTable(frame));
        }
        TransportEvent::RoomPitchSet(composed) => {
            publish_event(&queues.ui_events, status, BridgeEvent::PitchSet(composed));
        }
        TransportEvent::BoardRoundTable(_) => {}
        TransportEvent::BoardProvisioningRequired(_)
        | TransportEvent::BoardCapabilityBundlePrepared { .. }
        | TransportEvent::BoardProvisioningFailed { .. }
        | TransportEvent::BoardProvisioned(_)
        | TransportEvent::BoardRepairOutbound { .. }
        | TransportEvent::BoardRepairInbound { .. }
        | TransportEvent::BoardRepairTerminal(_)
        | TransportEvent::BoardRepairCarrierClosed(_)
        | TransportEvent::BoardRepairSynchronized(_) => {}
        TransportEvent::BoardRepairFailed { reason, .. } => {
            publish_event(
                &queues.ui_events,
                status,
                BridgeEvent::Diagnostic(format!("board HHHS repair failed: {reason}")),
            );
        }
        TransportEvent::PitchIntentOutcome { .. } | TransportEvent::PitchIntentReset { .. } => {}
        TransportEvent::Diagnostic(message) => {
            publish_event(&queues.ui_events, status, BridgeEvent::Diagnostic(message));
        }
    }
}

fn publish_event(queue: &ArrayQueue<BridgeEvent>, status: &AtomicBridgeStatus, event: BridgeEvent) {
    if let Err(event) = queue.push(event) {
        status.ui_events_dropped.fetch_add(1, Ordering::Relaxed);
        let _ = queue.pop();
        let _ = queue.push(event);
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex, time::Instant};

    use super::*;

    struct EchoTransport {
        events: Arc<Mutex<VecDeque<TransportEvent>>>,
    }

    impl BridgeTransport for EchoTransport {
        fn start(&mut self) -> Result<(), BridgeError> {
            self.events
                .lock()
                .unwrap()
                .push_back(TransportEvent::BoardLink(LinkState::Ready));
            Ok(())
        }

        fn handle_command(&mut self, command: BridgeCommand) -> Result<(), BridgeError> {
            let event = match command {
                BridgeCommand::SelectRoom(room) => {
                    Some(TransportEvent::Diagnostic(format!("room:{room}")))
                }
                BridgeCommand::SetSharedPitch {
                    token,
                    pitch,
                    active,
                } => {
                    let mut shared = SharedPitchSet::default();
                    if active {
                        shared
                            .pitch_classes
                            .insert(BridgePitchState::shared_degree(pitch));
                    }
                    self.events
                        .lock()
                        .unwrap()
                        .push_back(TransportEvent::RoomPitchSet(shared));
                    Some(TransportEvent::PitchIntentOutcome {
                        token,
                        outcome: PitchIntentOutcome::Applied,
                    })
                }
                _ => None,
            };
            if let Some(event) = event {
                self.events.lock().unwrap().push_back(event);
            }
            Ok(())
        }

        fn send_realtime(&mut self, event: RealtimeMidi) -> Result<(), BridgeError> {
            self.events
                .lock()
                .unwrap()
                .push_back(TransportEvent::Midi(event));
            Ok(())
        }

        fn poll_event(&mut self) -> Option<TransportEvent> {
            self.events.lock().unwrap().pop_front()
        }

        fn shutdown(&mut self) {}
    }

    fn note(kind: RealtimeMidiKind, note: u8) -> RealtimeMidi {
        RealtimeMidi {
            timing: 0,
            voice_id: RealtimeMidi::NO_VOICE_ID,
            channel: 0,
            note,
            kind,
            value: 1.0,
        }
    }

    fn wait_for<T>(mut probe: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(value) = probe() {
                return value;
            }
            assert!(Instant::now() < deadline, "background bridge timed out");
            thread::yield_now();
        }
    }

    #[test]
    fn walkie_and_tutti_profiles_share_music_without_claiming_extensions() {
        assert_eq!(
            ProtocolProfile::WALKIE.check_compatible(ProtocolProfile::TUTTI_LEAF),
            Ok(())
        );
        let incompatible = ProtocolProfile {
            music_generation: ProtocolProfile::TUTTI_LEAF.music_generation + 1,
            ..ProtocolProfile::TUTTI_LEAF
        };
        assert!(matches!(
            ProtocolProfile::WALKIE.check_compatible(incompatible),
            Err(ProtocolMismatch::MusicGeneration { .. })
        ));
        let incompatible_repair = ProtocolProfile {
            hhhs_repair_generation: ProtocolProfile::WALKIE.hhhs_repair_generation + 1,
            ..ProtocolProfile::WALKIE
        };
        assert!(matches!(
            ProtocolProfile::WALKIE.check_compatible(incompatible_repair),
            Err(ProtocolMismatch::HhhsRepairGeneration { .. })
        ));
    }

    #[test]
    fn host_midi_toggles_the_shared_set_and_reconciles_balanced_edges() {
        let events = Arc::new(Mutex::new(VecDeque::new()));
        let mut runtime = BridgeRuntime::spawn_with_transport(
            BridgeConfig::default(),
            EchoTransport {
                events: events.clone(),
            },
        )
        .unwrap();
        let audio = runtime.audio_port();
        audio.try_send(note(RealtimeMidiKind::NoteOn, 60)).unwrap();
        let first = wait_for(|| audio.try_recv());
        assert_eq!(first.kind, RealtimeMidiKind::NoteOn);
        assert_eq!(first.note, 48);

        // Release only rearms the keyboard toggle. The next strike removes the
        // room-owned pitch class even from a different octave, and the
        // reconciler emits the matching NoteOff.
        audio.try_send(note(RealtimeMidiKind::NoteOff, 60)).unwrap();
        thread::sleep(Duration::from_millis(20));
        audio.try_send(note(RealtimeMidiKind::NoteOn, 72)).unwrap();
        let second = wait_for(|| audio.try_recv());
        assert_eq!(second.kind, RealtimeMidiKind::NoteOff);
        assert_eq!(second.note, 48);
        let status = audio.status();
        assert_eq!(status.realtime_ingress_events, 3);
        assert_eq!(status.realtime_egress_events, 2);
        runtime.shutdown();
    }

    #[test]
    fn pending_intent_cannot_drive_confirmed_host_projection() {
        let mut pitches = BridgePitchState::default();
        let pitch = BridgePitchState::pitch_for_midi(60).unwrap();
        pitches.track_pitch_intent(1, PitchIntentSource::Host, &[(pitch, true)]);
        assert!(pitches.contains_optimistic_pitch_class(pitch));
        assert!(!pitches.room_midi_notes().into_iter().any(|active| active));

        let mut confirmed = SharedPitchSet::default();
        confirmed
            .pitch_classes
            .insert(BridgePitchState::shared_degree(pitch));
        assert!(pitches.apply_confirmed_room(confirmed));
        assert!(pitches.room_midi_notes()[48]);
        assert!(pitches.pending_classes.is_empty());
    }

    #[test]
    fn board_edit_fences_old_target_until_canonical_pitch_revision() {
        let mut pitches = BridgePitchState::default();
        let c = BridgePitchState::pitch_for_midi(60).unwrap();
        let e = BridgePitchState::pitch_for_midi(64).unwrap();
        let mut initial = SharedPitchSet::default();
        initial
            .pitch_classes
            .insert(BridgePitchState::shared_degree(c));
        assert!(pitches.apply_confirmed_room(initial.clone()));
        pitches.stage_board_target();
        let initial_target = pitches.board_target.take().expect("initial target");
        pitches.board_confirmed = Some(initial_target);
        pitches.board_retry_at = None;

        let mut board_edit = initial_target;
        board_edit.pattern = board_edit.pattern.toggled(64).unwrap();
        assert_eq!(
            pitches.observe_board_config(board_edit),
            BoardConfigObservation::BoardEdit
        );
        pitches.track_pitch_intent(9, PitchIntentSource::Board, &[(e, true)]);

        // Reproduce the old interleaving: SetRoundTable became visible while
        // the room's singular SharedPitchSet was still the prior revision.
        pitches.room_config.pulse_ms = pitches.room_config.pulse_ms.saturating_add(1);
        pitches.stage_board_target();
        assert_eq!(
            pitches.board_target, None,
            "an intermediate settings projection must not reassert the old pitch set"
        );
        assert!(
            !pitches.room_midi_notes()[52],
            "pending board intent must not become a second shared-set authority"
        );

        initial
            .pitch_classes
            .insert(BridgePitchState::shared_degree(e));
        assert!(pitches.apply_confirmed_room(initial));
        assert!(pitches.pending_classes.is_empty());
        pitches.stage_board_target();
        let confirmed = pitches.board_target.expect("confirmed target is restaged");
        assert!(confirmed.pattern.contains(52));
    }

    #[test]
    fn pattern_only_board_edit_plans_only_shared_pitch_operations() {
        let pitches = BridgePitchState::default();
        let mut config = pitches.room_config;
        config.pattern = RoundTablePattern::default().cleared().toggled(52).unwrap();

        let plan = pitches.plan_board_edit(config).unwrap();

        assert_eq!(plan.settings, None);
        assert_eq!(plan.pitch_edits.len(), 1);
        assert_eq!(plan.pitch_edits[0].1, true);
        assert_eq!(plan.pitch_edits[0].0.pitch.degree().index(), 4);
    }

    #[test]
    fn rejected_or_stale_board_outcome_cannot_clear_a_newer_intent() {
        let mut pitches = BridgePitchState::default();
        let e = BridgePitchState::pitch_for_midi(64).unwrap();
        pitches.track_pitch_intent(10, PitchIntentSource::Board, &[(e, true)]);
        pitches.track_pitch_intent(11, PitchIntentSource::Host, &[(e, false)]);

        pitches.reject_pitch_intent(10);
        assert!(pitches.has_pending_pitch_intent());
        assert!(!pitches.contains_optimistic_pitch_class(e));

        pitches.reject_pitch_intent(11);
        assert!(!pitches.has_pending_pitch_intent());
    }

    #[test]
    fn board_reset_preserves_host_intents_and_room_reset_abandons_all() {
        let mut pitches = BridgePitchState::default();
        let c = BridgePitchState::pitch_for_midi(60).unwrap();
        let e = BridgePitchState::pitch_for_midi(64).unwrap();
        pitches.track_pitch_intent(11, PitchIntentSource::Host, &[(c, true)]);
        pitches.track_pitch_intent(12, PitchIntentSource::Board, &[(e, true)]);

        pitches.abandon_board_pitch_intents();

        assert!(pitches.has_pending_pitch_intent());
        assert!(pitches.contains_optimistic_pitch_class(c));
        assert!(!pitches.contains_optimistic_pitch_class(e));

        pitches.abandon_pitch_intents();
        assert!(!pitches.has_pending_pitch_intent());
    }

    #[test]
    fn stale_queue_reset_cannot_clear_a_newer_board_intent() {
        let mut pitches = BridgePitchState::default();
        let c = BridgePitchState::pitch_for_midi(60).unwrap();
        let e = BridgePitchState::pitch_for_midi(64).unwrap();
        pitches.track_pitch_intent(20, PitchIntentSource::Board, &[(c, true)]);
        pitches.track_pitch_intent(21, PitchIntentSource::Host, &[(e, true)]);

        pitches.abandon_pitch_intents_through(20);

        assert!(!pitches.contains_optimistic_pitch_class(c));
        assert!(pitches.contains_optimistic_pitch_class(e));
        assert!(pitches.has_pending_pitch_intent());
    }

    #[test]
    fn stale_board_echo_cannot_overwrite_a_newer_room_target() {
        let mut pitches = BridgePitchState::default();
        pitches.stage_board_target();
        let target = pitches.board_target.expect("room target is staged");
        let mut stale = target;
        stale.pattern = stale.pattern.toggled(60).unwrap();

        assert_eq!(
            pitches.observe_board_snapshot(1, stale),
            BoardConfigObservation::StaleEcho
        );
        assert_eq!(pitches.board_target, Some(target));
        assert_ne!(pitches.board_confirmed, Some(stale));

        assert_eq!(
            pitches.observe_board_snapshot(2, target),
            BoardConfigObservation::Confirmed
        );
        assert_eq!(pitches.board_confirmed, Some(target));
        assert_eq!(pitches.board_target, None);
    }

    #[test]
    fn unsolicited_board_level_is_classified_as_an_edit() {
        let mut pitches = BridgePitchState::default();
        let config = RoundTableConfig::default();
        assert_eq!(
            pitches.observe_board_config(config),
            BoardConfigObservation::BoardEdit
        );
        assert_eq!(pitches.board_confirmed, Some(config));
    }

    #[test]
    fn passive_board_snapshots_are_revision_fenced_after_target_confirmation() {
        let mut pitches = BridgePitchState::default();
        pitches.stage_board_target();
        let empty = pitches.board_target.expect("empty room target is staged");
        let mut added = empty;
        added.pattern = added.pattern.toggled(52).unwrap();

        assert_eq!(
            pitches.observe_board_snapshot(7, added),
            BoardConfigObservation::StaleEcho
        );
        assert_eq!(
            pitches.observe_board_snapshot(8, empty),
            BoardConfigObservation::Confirmed
        );
        assert_eq!(pitches.board_target, None);

        // A delayed older snapshot cannot become a new room edit merely
        // because the exact target has already been acknowledged.
        assert_eq!(
            pitches.observe_board_snapshot(7, added),
            BoardConfigObservation::StaleEcho
        );
        assert_eq!(pitches.board_confirmed, Some(empty));

        // A genuinely newer passive materialization is corrected toward the
        // room; only an explicit Config intent can edit the room.
        assert_eq!(
            pitches.observe_board_snapshot(9, added),
            BoardConfigObservation::StaleEcho
        );
        assert_eq!(pitches.board_target, Some(empty));
    }

    #[test]
    fn gate_mode_balances_confirmed_membership_edges() {
        let events = Arc::new(Mutex::new(VecDeque::new()));
        let mut runtime = BridgeRuntime::spawn_with_transport(
            BridgeConfig::default(),
            EchoTransport {
                events: events.clone(),
            },
        )
        .unwrap();
        let audio = runtime.audio_port();
        audio.set_input_mode(MidiInputMode::GateSet);

        audio.try_send(note(RealtimeMidiKind::NoteOn, 60)).unwrap();
        let on = wait_for(|| audio.try_recv());
        assert_eq!((on.kind, on.note), (RealtimeMidiKind::NoteOn, 48));

        audio.try_send(note(RealtimeMidiKind::NoteOff, 60)).unwrap();
        let off = wait_for(|| audio.try_recv());
        assert_eq!((off.kind, off.note), (RealtimeMidiKind::NoteOff, 48));
        runtime.shutdown();
    }

    #[test]
    fn disabling_host_projection_retracts_and_reenable_reconstructs_current_set() {
        let events = Arc::new(Mutex::new(VecDeque::new()));
        let mut runtime = BridgeRuntime::spawn_with_transport(
            BridgeConfig::default(),
            EchoTransport {
                events: events.clone(),
            },
        )
        .unwrap();
        let audio = runtime.audio_port();

        audio.try_send(note(RealtimeMidiKind::NoteOn, 60)).unwrap();
        let initial = wait_for(|| audio.try_recv());
        assert_eq!((initial.kind, initial.note), (RealtimeMidiKind::NoteOn, 48));

        audio.set_output_enabled(false);
        let release = wait_for(|| audio.try_recv());
        assert_eq!(
            (release.kind, release.note),
            (RealtimeMidiKind::NoteOff, 48)
        );

        audio.set_output_enabled(true);
        let restored = wait_for(|| audio.try_recv());
        assert_eq!(
            (restored.kind, restored.note),
            (RealtimeMidiKind::NoteOn, 48)
        );
        runtime.shutdown();
    }

    #[test]
    fn membership_projection_has_a_stable_origin_voice() {
        for note in [0, 48, 127] {
            let projected = RealtimeMidi {
                timing: 0,
                voice_id: RealtimeMidi::membership_voice_id(note),
                channel: 0,
                note,
                kind: RealtimeMidiKind::NoteOn,
                value: 0.8,
            };
            assert!(projected.is_membership_projection());
        }
    }

    #[test]
    fn host_output_is_the_exact_delta_of_the_confirmed_shared_set() {
        let queues = BridgeQueues::new(&BridgeConfig::default());
        let status = AtomicBridgeStatus::default();
        let tuning = Tuning::twelve_tet();
        let degree = |index| TunedDegree::new(&tuning, index).unwrap();
        let mut pitches = BridgePitchState::default();

        let mut first = SharedPitchSet::default();
        first.pitch_classes.extend([degree(0), degree(4)]);
        assert!(pitches.apply_confirmed_room(first));
        reconcile_host_output(&mut pitches, &queues, &status, true);
        assert_eq!(queues.realtime_out.pop().unwrap().note, 48);
        assert_eq!(queues.realtime_out.pop().unwrap().note, 52);
        assert!(queues.realtime_out.is_empty());
        assert!(queues.release_out.is_empty());

        let mut second = SharedPitchSet::default();
        second.pitch_classes.extend([degree(4), degree(7)]);
        assert!(pitches.apply_confirmed_room(second));
        reconcile_host_output(&mut pitches, &queues, &status, true);
        let removed = queues.release_out.pop().unwrap();
        let added = queues.realtime_out.pop().unwrap();
        assert_eq!(
            (removed.kind, removed.note),
            (RealtimeMidiKind::NoteOff, 48)
        );
        assert_eq!((added.kind, added.note), (RealtimeMidiKind::NoteOn, 55));
        assert!(queues.realtime_out.is_empty());
        assert!(queues.release_out.is_empty());

        // Reconciliation is level-triggered: an unchanged authoritative set
        // never produces a duplicate edge.
        reconcile_host_output(&mut pitches, &queues, &status, true);
        assert!(queues.realtime_out.is_empty());
        assert!(queues.release_out.is_empty());
    }

    #[test]
    fn set_modes_do_not_mix_transient_note_ons_into_membership_output() {
        let on = note(RealtimeMidiKind::NoteOn, 64);
        let off = note(RealtimeMidiKind::NoteOff, 64);
        for mode in [MidiInputMode::ToggleSet, MidiInputMode::GateSet] {
            assert!(!should_forward_room_realtime(mode, on));
            assert!(should_forward_room_realtime(mode, off));
        }
        assert!(should_forward_room_realtime(MidiInputMode::Perform, on));
        assert!(should_forward_room_realtime(MidiInputMode::Perform, off));
    }

    #[test]
    fn saturation_is_bounded_and_observable() {
        let config = BridgeConfig {
            realtime_capacity: 1,
            release_capacity: 1,
            poll_interval: Duration::from_millis(25),
            ..BridgeConfig::default()
        };
        let runtime = BridgeRuntime::spawn(config).unwrap();
        let audio = runtime.audio_port();
        let mut observed_full = false;
        for key in 0..=127 {
            observed_full |= audio.try_send(note(RealtimeMidiKind::NoteOn, key)).is_err();
        }
        assert!(observed_full);
        assert!(audio.status().realtime_ingress_dropped > 0);
    }

    #[test]
    fn unavailable_runtime_reports_startup_failure_without_a_worker_panic() {
        let mut runtime = BridgeRuntime::unavailable(
            BridgeConfig::default(),
            "operating system refused the bridge thread",
        );
        let status = runtime.handle().status();
        assert_eq!(status.room_link, LinkState::Failed);
        assert_eq!(status.board_link, LinkState::Failed);
        assert!(matches!(
            runtime.handle().try_event(),
            Some(BridgeEvent::Diagnostic(message))
                if message == "operating system refused the bridge thread"
        ));
        runtime.shutdown();
    }
}
