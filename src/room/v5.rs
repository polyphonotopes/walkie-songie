//! Room v5: two capability-native HHHS replicas composed by the application.
//!
//! The music and extension lanes share no causal frontier, storage transaction,
//! or repair session. HHHS owns admission, evidence, persistence,
//! materialization, and the repair host. Walkie owns room tasks, carriers,
//! discovery, protocol negotiation, and the composed view.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use hhhs::{DagRead, DagSnapshot, Digest, Encoder, Entry, EntryHash, Position, ReachIndex};
use hhhs_cap::{
    Area, AuthorizationDecision, AuthorizationRequest, CapabilityOp, CapabilitySnapshot, Grant,
    Revoke, Right, Rights, decode_op as decode_capability, encode_op as encode_capability,
};
use hhhs_proof::{
    Ed25519Verifier, MAX_PRESENTED_GRANTS, PresentationContext, PresentationEnvelope, SigningKey,
    VerifierRegistry,
};
use hhhs_replica::{
    AdmissionOutcome, AdmissionPolicy, AdmissionRequest, AdmittedAuthority, AsyncTransactionSink,
    DurableReplicaRepairHost, PreparedAdmission, Replica, ReplicaError, ReplicaRecord,
    ReplicaRepairHost,
};
use hhhs_store::{
    Materializer, MemoryStorage, ProjectionCheckpoint, ProjectionKey, ReplicaStorage, SecretKey,
    SecretValue, StorageTransaction,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
pub use tutti_music::MusicOp;
use tutti_music::{Envelope, TunedDegree, TuningDefinition};

use crate::tuning::{MAX_SCALE_DEGREES, TunedPeriodicPitch};

const MAX_ABS_PERIOD: i32 = 1_000_000;

pub const ROOM_PROTOCOL_GENERATION: u32 = 5;
pub const MUSIC_REPAIR_ALPN: &[u8] = tutti_music_hhhs::REPAIR_ALPN;
pub const EXTENSION_REPAIR_ALPN: &[u8] = b"walkie/extension/hhhs-replica/5";
pub const LANE_STRATEGY_VERSION: u32 = 1;
pub const MUSIC_STRATEGY_NAME: &str = tutti_music_hhhs::STRATEGY_NAME;
pub const EXTENSION_STRATEGY_NAME: &str = "walkie-extension-hhhs-entry";

const ROOM_OBJECT_DOMAIN: &[u8] = b"walkie room object v5";
const OPEN_ROOM_AUTHORITY_DOMAIN: &[u8] = b"walkie open room authority v5";
const LANE_NAMESPACE_DOMAIN: &[u8] = b"walkie room lane namespace v5";
const EXTENSION_COMMAND_DOMAIN: &[u8] = b"walkie extension command v5\0";
const PRESENCE_DOMAIN: &[u8] = b"walkie capability presence v5\0";
const PRESENCE_CLAIMS_DOMAIN: &[u8] = b"walkie capability presence claims v5\0";
const MAX_COMMAND_BYTES: usize = 1024 * 1024;
const MAX_PRESENCE_BYTES: usize = 32 * 1024;
const MAX_EMOJI_BYTES: usize = 64;
const MAX_EMOJI_PALETTE_BYTES: usize = 4096;

/// A Room v5 causal lane. The numeric tag is stable protocol identity.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum RoomLane {
    Music = 0x01,
    Extension = 0x02,
}

impl RoomLane {
    pub const fn tag(self) -> u8 {
        self as u8
    }

    pub const fn repair_alpn(self) -> &'static [u8] {
        match self {
            Self::Music => MUSIC_REPAIR_ALPN,
            Self::Extension => EXTENSION_REPAIR_ALPN,
        }
    }

    pub const fn strategy_name(self) -> &'static str {
        match self {
            Self::Music => MUSIC_STRATEGY_NAME,
            Self::Extension => EXTENSION_STRATEGY_NAME,
        }
    }
}

/// Connectivity metadata only. This value never enters capability evaluation.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ProtocolSupport(u8);

impl ProtocolSupport {
    pub const MUSIC: Self = Self(RoomLane::Music as u8);
    pub const EXTENSION: Self = Self(RoomLane::Extension as u8);
    pub const WALKIE: Self = Self(Self::MUSIC.0 | Self::EXTENSION.0);

    pub const fn from_bits(bits: u8) -> Option<Self> {
        if bits == 0 || bits & !Self::WALKIE.0 != 0 {
            None
        } else {
            Some(Self(bits))
        }
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn supports(self, lane: RoomLane) -> bool {
        self.0 & lane.tag() != 0
    }
}

/// Stable proof receiver shared with independent tutti-music Replicas.
pub use tutti_music_hhhs::ActorId;

/// Extension object identity. A piece is the entry which created it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct PieceId(pub [u8; 32]);

impl PieceId {
    pub fn from_entry(entry: EntryHash) -> Self {
        Self(*entry.as_bytes())
    }

    pub fn entry(self) -> EntryHash {
        EntryHash(Digest(self.0))
    }

    pub fn to_hex(self) -> String {
        hex_bytes(&self.0)
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(
        String::with_capacity(bytes.len() * 2),
        |mut encoded, byte| {
            let _ = write!(encoded, "{byte:02x}");
            encoded
        },
    )
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RoomIdentity {
    pub object: Digest,
    pub music: Digest,
    pub extension: Digest,
}

impl RoomIdentity {
    pub fn from_name(room_name: &str) -> Self {
        let normalized = room_name.to_ascii_lowercase();
        let mut object_encoder = Encoder::new();
        object_encoder.bytes(ROOM_OBJECT_DOMAIN).str(&normalized);
        Self::from_object(object_encoder.digest_finish())
    }

    /// Reconstruct lane namespaces from the object carried by a Room-v5
    /// invitation or ticket. The human room phrase is not protocol identity.
    pub fn from_object(object: Digest) -> Self {
        let namespace = |lane: RoomLane| {
            let mut encoder = Encoder::new();
            encoder
                .bytes(LANE_NAMESPACE_DOMAIN)
                .digest(&object)
                .u32(u32::from(lane.tag()));
            encoder.digest_finish()
        };
        Self {
            object,
            music: namespace(RoomLane::Music),
            extension: namespace(RoomLane::Extension),
        }
    }

    pub const fn namespace(&self, lane: RoomLane) -> Digest {
        match lane {
            RoomLane::Music => self.music,
            RoomLane::Extension => self.extension,
        }
    }
}

/// Derive the bearer authority for a human-named open room.
///
/// This deliberately preserves the original three-word-code experience: the
/// normalized room phrase is sufficient to join and author in an open jam.
/// It is not a private-room secret; anyone who knows or guesses the phrase has
/// the same authority. Private or delegated rooms use receiver-bound tickets
/// instead.
pub fn open_room_authority(room_name: &str) -> SigningKey {
    let mut encoder = Encoder::new();
    encoder
        .bytes(OPEN_ROOM_AUTHORITY_DOMAIN)
        .str(&room_name.to_ascii_lowercase());
    SigningKey::from_bytes(encoder.digest_finish().as_bytes())
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ExtensionCommand {
    PutPiece {
        emoji: String,
        pitch: TunedPeriodicPitch,
    },
    MovePiece {
        piece: PieceId,
        pitch: TunedPeriodicPitch,
    },
    RemovePiece {
        piece: PieceId,
    },
    UnremovePiece {
        remove: PieceId,
    },
    SetConfig {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pieces_locked: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        available_emojis: Option<String>,
    },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RoomCommand {
    Music(MusicOp),
    Extension(ExtensionCommand),
}

/// Verified, non-canonical live state. Presence is deliberately not an HHHS
/// entry: losing it has no effect on the room history and a later heartbeat
/// replaces it. The proof still binds it to this room, actor, causal position,
/// and an explicitly presented live music capability.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RoomPresence {
    pub actor: ActorId,
    pub session: u64,
    pub sequence: u64,
    pub pitch: Option<TunedPeriodicPitch>,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
struct PresenceClaims {
    generation: u32,
    room: [u8; 32],
    actor: ActorId,
    session: u64,
    sequence: u64,
    pitch: Option<TunedPeriodicPitch>,
    at: Vec<[u8; 32]>,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
struct PresenceEnvelope {
    claims: PresenceClaims,
    proof: Vec<u8>,
}

#[derive(Clone, PartialEq, Eq, Debug, Error)]
pub enum PresenceError {
    #[error("presence message is too large: {0} bytes")]
    TooLarge(usize),
    #[error("presence message belongs to another protocol domain")]
    WrongDomain,
    #[error("presence message is malformed")]
    Malformed,
    #[error("presence encoding is not canonical")]
    NonCanonical,
    #[error("presence message names another room or generation")]
    WrongRoom,
    #[error("presence proof is invalid")]
    InvalidProof,
    #[error("presence capability is not live at the signed position")]
    Unauthorized,
}

fn presence_claims_bytes(claims: &PresenceClaims) -> Result<Vec<u8>, PresenceError> {
    let json = serde_json::to_vec(claims).map_err(|_| PresenceError::Malformed)?;
    let mut bytes = Vec::with_capacity(PRESENCE_CLAIMS_DOMAIN.len() + json.len());
    bytes.extend_from_slice(PRESENCE_CLAIMS_DOMAIN);
    bytes.extend_from_slice(&json);
    Ok(bytes)
}

fn encode_presence_envelope(envelope: &PresenceEnvelope) -> Result<Vec<u8>, PresenceError> {
    let json = serde_json::to_vec(envelope).map_err(|_| PresenceError::Malformed)?;
    let mut bytes = Vec::with_capacity(PRESENCE_DOMAIN.len() + json.len());
    bytes.extend_from_slice(PRESENCE_DOMAIN);
    bytes.extend_from_slice(&json);
    if bytes.len() > MAX_PRESENCE_BYTES {
        return Err(PresenceError::TooLarge(bytes.len()));
    }
    Ok(bytes)
}

fn decode_presence_envelope(bytes: &[u8]) -> Result<PresenceEnvelope, PresenceError> {
    if bytes.len() > MAX_PRESENCE_BYTES {
        return Err(PresenceError::TooLarge(bytes.len()));
    }
    let json = bytes
        .strip_prefix(PRESENCE_DOMAIN)
        .ok_or(PresenceError::WrongDomain)?;
    let envelope: PresenceEnvelope =
        serde_json::from_slice(json).map_err(|_| PresenceError::Malformed)?;
    if encode_presence_envelope(&envelope)? != bytes {
        return Err(PresenceError::NonCanonical);
    }
    Ok(envelope)
}

impl RoomCommand {
    pub const fn lane(&self) -> RoomLane {
        match self {
            Self::Music(_) => RoomLane::Music,
            Self::Extension(_) => RoomLane::Extension,
        }
    }
}

impl From<MusicOp> for RoomCommand {
    fn from(command: MusicOp) -> Self {
        Self::Music(command)
    }
}

impl From<ExtensionCommand> for RoomCommand {
    fn from(command: ExtensionCommand) -> Self {
        Self::Extension(command)
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
struct CommandEnvelope<T> {
    generation: u32,
    namespace: [u8; 32],
    actor: ActorId,
    presented: Vec<[u8; 32]>,
    command: T,
}

#[derive(Clone, PartialEq, Eq, Debug, Error)]
pub enum CommandCodecError {
    #[error("command is too large: {0} bytes")]
    TooLarge(usize),
    #[error("command belongs to another protocol domain")]
    WrongDomain,
    #[error("command JSON is malformed")]
    Malformed,
    #[error("command encoding is not canonical")]
    NonCanonical,
    #[error("unsupported command generation {0}")]
    UnsupportedGeneration(u32),
}

fn command_domain(lane: RoomLane) -> &'static [u8] {
    match lane {
        RoomLane::Music => tutti_music_hhhs::COMMAND_DOMAIN,
        RoomLane::Extension => EXTENSION_COMMAND_DOMAIN,
    }
}

fn encode_envelope<T: Serialize>(
    lane: RoomLane,
    envelope: &CommandEnvelope<T>,
) -> Result<Vec<u8>, CommandCodecError> {
    let json = serde_json::to_vec(envelope).map_err(|_| CommandCodecError::Malformed)?;
    let mut bytes = Vec::with_capacity(command_domain(lane).len() + json.len());
    bytes.extend_from_slice(command_domain(lane));
    bytes.extend_from_slice(&json);
    if bytes.len() > MAX_COMMAND_BYTES {
        return Err(CommandCodecError::TooLarge(bytes.len()));
    }
    Ok(bytes)
}

fn decode_envelope<T: Serialize + DeserializeOwned>(
    lane: RoomLane,
    bytes: &[u8],
) -> Result<CommandEnvelope<T>, CommandCodecError> {
    if bytes.len() > MAX_COMMAND_BYTES {
        return Err(CommandCodecError::TooLarge(bytes.len()));
    }
    let json = bytes
        .strip_prefix(command_domain(lane))
        .ok_or(CommandCodecError::WrongDomain)?;
    let envelope: CommandEnvelope<T> =
        serde_json::from_slice(json).map_err(|_| CommandCodecError::Malformed)?;
    if envelope.generation != ROOM_PROTOCOL_GENERATION {
        return Err(CommandCodecError::UnsupportedGeneration(
            envelope.generation,
        ));
    }
    if encode_envelope(lane, &envelope)? != bytes {
        return Err(CommandCodecError::NonCanonical);
    }
    Ok(envelope)
}

pub fn encode_music_command(
    namespace: Digest,
    actor: ActorId,
    presented: &[EntryHash],
    command: MusicOp,
) -> Result<Vec<u8>, CommandCodecError> {
    tutti_music_hhhs::encode_command(namespace, actor, presented, command).map_err(|error| {
        match error {
            tutti_music_hhhs::CommandCodecError::TooLarge(bytes) => {
                CommandCodecError::TooLarge(bytes)
            }
            tutti_music_hhhs::CommandCodecError::WrongDomain => CommandCodecError::WrongDomain,
            tutti_music_hhhs::CommandCodecError::Malformed => CommandCodecError::Malformed,
            tutti_music_hhhs::CommandCodecError::NonCanonical => CommandCodecError::NonCanonical,
            tutti_music_hhhs::CommandCodecError::UnsupportedGeneration(generation) => {
                CommandCodecError::UnsupportedGeneration(generation)
            }
        }
    })
}

pub fn encode_extension_command(
    namespace: Digest,
    actor: ActorId,
    presented: &[EntryHash],
    command: ExtensionCommand,
) -> Result<Vec<u8>, CommandCodecError> {
    encode_envelope(
        RoomLane::Extension,
        &CommandEnvelope {
            generation: ROOM_PROTOCOL_GENERATION,
            namespace: *namespace.as_bytes(),
            actor,
            presented: presented.iter().map(|grant| *grant.as_bytes()).collect(),
            command,
        },
    )
}

pub fn music_notes_area(namespace: Digest) -> Area {
    tutti_music_hhhs::notes_area(namespace)
}

pub fn music_tuning_area(namespace: Digest) -> Area {
    tutti_music_hhhs::tuning_area(namespace)
}

pub fn extension_pieces_area(namespace: Digest) -> Area {
    Area::new(namespace, [b"extension".to_vec(), b"pieces".to_vec()]).expect("bounded command area")
}

pub fn extension_config_area(namespace: Digest) -> Area {
    Area::new(namespace, [b"extension".to_vec(), b"config".to_vec()]).expect("bounded command area")
}

fn music_command_area(namespace: Digest, command: &MusicOp) -> Area {
    tutti_music_hhhs::command_area(namespace, command)
}

fn extension_command_area(namespace: Digest, command: &ExtensionCommand) -> Area {
    match command {
        ExtensionCommand::PutPiece { .. }
        | ExtensionCommand::MovePiece { .. }
        | ExtensionCommand::RemovePiece { .. }
        | ExtensionCommand::UnremovePiece { .. } => extension_pieces_area(namespace),
        ExtensionCommand::SetConfig { .. } => extension_config_area(namespace),
    }
}

fn presented_ids<T>(envelope: &CommandEnvelope<T>) -> Result<Vec<EntryHash>, String> {
    if envelope.presented.is_empty() || envelope.presented.len() > MAX_PRESENTED_GRANTS {
        return Err("command presents an invalid number of capability grants".into());
    }
    let ids: Vec<_> = envelope
        .presented
        .iter()
        .map(|bytes| EntryHash(Digest(*bytes)))
        .collect();
    let unique: BTreeSet<_> = ids.iter().copied().collect();
    if unique.len() != ids.len() {
        return Err("command repeats a presented capability grant".into());
    }
    Ok(ids)
}

#[derive(Clone)]
pub struct RoomAdmissionPolicy {
    lane: RoomLane,
    namespace: Digest,
}

impl RoomAdmissionPolicy {
    pub const fn new(lane: RoomLane, namespace: Digest) -> Self {
        Self { lane, namespace }
    }

    fn validate_capability(
        &self,
        op: &CapabilityOp,
        entry: &Entry,
        history: &DagSnapshot,
        authority: &AdmittedAuthority,
    ) -> Result<(), String> {
        match (op, authority) {
            (CapabilityOp::Grant(grant), AdmittedAuthority::TrustedRoot) => {
                if grant.parent.is_some()
                    || grant.issuer != grant.receiver
                    || grant.area != Area::root(self.namespace)
                    || grant.rights != Rights::ALL
                    || grant.receiver.as_bytes().len() != 32
                {
                    return Err("invalid Room v5 top-level capability grant".into());
                }
                Ok(())
            }
            (CapabilityOp::Grant(grant), AdmittedAuthority::Presented { presentation, .. }) => {
                let parent = grant
                    .parent
                    .ok_or_else(|| "delegation is missing its parent".to_owned())?;
                if grant.issuer != *presentation.receiver() {
                    return Err("delegation issuer does not equal proof receiver".into());
                }
                if !presentation.presented().contains(&parent) {
                    return Err("delegation did not explicitly present its parent".into());
                }
                if presentation.context().area != grant.area
                    || presentation.context().right != Right::Invoke
                {
                    return Err("delegation proof is bound to another area or right".into());
                }
                if grant.receiver.as_bytes().len() != 32 {
                    return Err("Room v5 receivers must be Ed25519 public keys".into());
                }
                Ok(())
            }
            (CapabilityOp::Revoke(revoke), AdmittedAuthority::Presented { presentation, .. }) => {
                if revoke.revoker != *presentation.receiver() {
                    return Err("revoker does not equal proof receiver".into());
                }
                let target_entry = history
                    .entry(&revoke.target)
                    .ok_or_else(|| "revocation target is missing".to_owned())?;
                let CapabilityOp::Grant(target) = decode_capability(&target_entry.payload)
                    .map_err(|_| "revocation target is not a capability grant".to_owned())?
                else {
                    return Err("revocation target is not a grant".into());
                };
                if revoke.revoker != target.issuer && revoke.revoker != target.receiver {
                    return Err("revoker is neither grant issuer nor receiver".into());
                }
                if presentation.context().area != target.area
                    || presentation.context().right != Right::Invoke
                {
                    return Err("revocation proof is bound to another area or right".into());
                }
                if !ReachIndex::new(history).is_ancestor(&revoke.target, &entry.hash()) {
                    return Err("revocation target is not in the causal past".into());
                }
                Ok(())
            }
            _ => Err("capability operation used the wrong authority path".into()),
        }
    }

    fn validate_presented_command<T>(
        &self,
        envelope: &CommandEnvelope<T>,
        expected_area: Area,
        authority: &AdmittedAuthority,
    ) -> Result<(), String> {
        if envelope.namespace != *self.namespace.as_bytes() {
            return Err("command namespace does not match this Replica".into());
        }
        let AdmittedAuthority::Presented { presentation, .. } = authority else {
            return Err("Room v5 commands require a verified capability presentation".into());
        };
        if presentation.receiver().as_bytes() != envelope.actor.0 {
            return Err("command actor does not equal proof receiver".into());
        }
        if presentation.presented() != presented_ids(envelope)? {
            return Err("command grant list does not equal the proof statement".into());
        }
        if presentation.context().area != expected_area
            || presentation.context().right != Right::Invoke
        {
            return Err("command proof is bound to another area or right".into());
        }
        Ok(())
    }

    fn validate_extension(
        &self,
        command: &ExtensionCommand,
        entry: &Entry,
        history: &DagSnapshot,
    ) -> Result<(), String> {
        let validate_pitch = |pitch: TunedPeriodicPitch| {
            if usize::from(pitch.degree().degree.index()) >= MAX_SCALE_DEGREES
                || pitch.pitch.period().unsigned_abs() > MAX_ABS_PERIOD as u32
            {
                Err("piece pitch is outside protocol bounds".to_owned())
            } else {
                Ok(())
            }
        };
        match command {
            ExtensionCommand::PutPiece { emoji, pitch } => {
                if emoji.is_empty() || emoji.len() > MAX_EMOJI_BYTES {
                    return Err("piece emoji is outside protocol bounds".into());
                }
                validate_pitch(*pitch)?;
            }
            ExtensionCommand::MovePiece { piece, pitch } => {
                validate_pitch(*pitch)?;
                require_extension_target(history, piece.entry(), |command| {
                    matches!(command, ExtensionCommand::PutPiece { .. })
                })?;
                if extension_lock_at(history, &entry.header.prevs) {
                    return Err("pieces are locked at this command's causal position".into());
                }
            }
            ExtensionCommand::RemovePiece { piece } => {
                require_extension_target(history, piece.entry(), |command| {
                    matches!(command, ExtensionCommand::PutPiece { .. })
                })?;
                if extension_lock_at(history, &entry.header.prevs) {
                    return Err("pieces are locked at this command's causal position".into());
                }
            }
            ExtensionCommand::UnremovePiece { remove } => {
                require_extension_target(history, remove.entry(), |command| {
                    matches!(command, ExtensionCommand::RemovePiece { .. })
                })?;
                if extension_lock_at(history, &entry.header.prevs) {
                    return Err("pieces are locked at this command's causal position".into());
                }
            }
            ExtensionCommand::SetConfig {
                available_emojis: Some(emojis),
                ..
            } if emojis.len() > MAX_EMOJI_PALETTE_BYTES => {
                return Err("emoji palette is outside protocol bounds".into());
            }
            ExtensionCommand::SetConfig { .. } => {}
        }
        Ok(())
    }
}

impl AdmissionPolicy for RoomAdmissionPolicy {
    fn validate(
        &self,
        entry: &Entry,
        history: &DagSnapshot,
        authority: &AdmittedAuthority,
    ) -> Result<(), String> {
        if let Ok(capability) = decode_capability(&entry.payload) {
            if self.lane == RoomLane::Music {
                return AdmissionPolicy::validate(
                    &tutti_music_hhhs::MusicAdmissionPolicy::new(self.namespace),
                    entry,
                    history,
                    authority,
                );
            }
            return self.validate_capability(&capability, entry, history, authority);
        }
        match self.lane {
            RoomLane::Music => AdmissionPolicy::validate(
                &tutti_music_hhhs::MusicAdmissionPolicy::new(self.namespace),
                entry,
                history,
                authority,
            ),
            RoomLane::Extension => {
                let envelope =
                    decode_envelope::<ExtensionCommand>(RoomLane::Extension, &entry.payload)
                        .map_err(|error| error.to_string())?;
                self.validate_presented_command(
                    &envelope,
                    extension_command_area(self.namespace, &envelope.command),
                    authority,
                )?;
                self.validate_extension(&envelope.command, entry, history)
            }
        }
    }
}

fn require_extension_target(
    history: &DagSnapshot,
    target: EntryHash,
    expected: impl FnOnce(&ExtensionCommand) -> bool,
) -> Result<(), String> {
    let entry = history
        .entry(&target)
        .ok_or_else(|| "extension target is missing".to_owned())?;
    let envelope = decode_envelope::<ExtensionCommand>(RoomLane::Extension, &entry.payload)
        .map_err(|_| "extension target has another payload type".to_owned())?;
    if !expected(&envelope.command) {
        return Err("extension target has the wrong command type".into());
    }
    Ok(())
}

fn extension_lock_at(history: &DagSnapshot, at: &Position) -> bool {
    let reach = ReachIndex::new(history);
    let visible = reach.observed_at(at);
    let writes: Vec<_> = history
        .entries_topo()
        .into_iter()
        .filter(|entry| visible.contains(&entry.hash()))
        .filter_map(|entry| {
            let envelope =
                decode_envelope::<ExtensionCommand>(RoomLane::Extension, &entry.payload).ok()?;
            let ExtensionCommand::SetConfig {
                pieces_locked: Some(locked),
                ..
            } = envelope.command
            else {
                return None;
            };
            Some((entry.hash(), locked))
        })
        .collect();
    resolve_register(&reach, writes).unwrap_or(false)
}

fn resolve_register<T>(reach: &ReachIndex, values: Vec<(EntryHash, T)>) -> Option<T> {
    let ids: Vec<_> = values.iter().map(|(id, _)| *id).collect();
    let winner = ids
        .iter()
        .filter(|candidate| !ids.iter().any(|other| reach.is_ancestor(candidate, other)))
        .max()
        .copied()?;
    values
        .into_iter()
        .find_map(|(id, value)| (id == winner).then_some(value))
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct MusicView {
    pub live: BTreeSet<TunedDegree>,
    pub holders: BTreeMap<TunedDegree, BTreeSet<ActorId>>,
    pub envelopes: BTreeMap<TunedDegree, Envelope>,
    pub tuning: TuningDefinition,
}

impl Default for MusicView {
    fn default() -> Self {
        Self {
            live: BTreeSet::new(),
            holders: BTreeMap::new(),
            envelopes: BTreeMap::new(),
            tuning: TuningDefinition::twelve_tet(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Piece {
    pub owner: ActorId,
    pub emoji: String,
    pub pitch: TunedPeriodicPitch,
}

#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct ExtensionView {
    pub pieces: BTreeMap<PieceId, Piece>,
    pub pieces_locked: bool,
    pub available_emojis: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RoomView {
    pub music: MusicView,
    pub pieces: BTreeMap<PieceId, Piece>,
    pub pieces_locked: bool,
    pub available_emojis: Option<String>,
}

/// UI-facing consequences between two materialized snapshots. These are
/// derived acceleration data: callers may always discard them and compare or
/// rebuild full views again.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct RoomViewDelta {
    pub pitches_added: BTreeSet<TunedDegree>,
    pub pitches_retracted: BTreeSet<TunedDegree>,
    pub pieces_upserted: BTreeMap<PieceId, Piece>,
    pub pieces_retracted: BTreeSet<PieceId>,
    pub tuning_changed: bool,
    pub pieces_locked_changed: bool,
    pub available_emojis_changed: bool,
}

impl RoomView {
    pub fn changes_since(&self, previous: &Self) -> RoomViewDelta {
        let pitches_added = self
            .music
            .live
            .difference(&previous.music.live)
            .copied()
            .collect();
        let pitches_retracted = previous
            .music
            .live
            .difference(&self.music.live)
            .copied()
            .collect();
        let pieces_upserted = self
            .pieces
            .iter()
            .filter(|(id, piece)| previous.pieces.get(id) != Some(*piece))
            .map(|(id, piece)| (*id, piece.clone()))
            .collect();
        let pieces_retracted = previous
            .pieces
            .keys()
            .filter(|id| !self.pieces.contains_key(id))
            .copied()
            .collect();
        RoomViewDelta {
            pitches_added,
            pitches_retracted,
            pieces_upserted,
            pieces_retracted,
            tuning_changed: self.music.tuning != previous.music.tuning,
            pieces_locked_changed: self.pieces_locked != previous.pieces_locked,
            available_emojis_changed: self.available_emojis != previous.available_emojis,
        }
    }
}

fn command_is_currently_authorized<T>(
    capabilities: &CapabilitySnapshot,
    history: &DagSnapshot,
    entry: EntryHash,
    envelope: &CommandEnvelope<T>,
    area: Area,
) -> bool {
    let Ok(presented) = presented_ids(envelope) else {
        return false;
    };
    matches!(
        capabilities.authorize(&AuthorizationRequest {
            receiver: envelope.actor.receiver(),
            area,
            right: Right::Invoke,
            presented,
            at: Position::of([entry]),
            from: history.frontier(),
        }),
        AuthorizationDecision::Allowed(_)
    )
}

pub fn materialize_music(history: &DagSnapshot, roots: &[EntryHash]) -> MusicView {
    let view = tutti_music_hhhs::materialize(history, roots);
    MusicView {
        live: view.live,
        holders: view.holders,
        envelopes: view.envelopes,
        tuning: view.tuning,
    }
}

fn extension_commands(
    history: &DagSnapshot,
    roots: &[EntryHash],
) -> Vec<(EntryHash, ActorId, ExtensionCommand)> {
    let capabilities = CapabilitySnapshot::capture(history, roots.iter().copied());
    history
        .entries_topo()
        .into_iter()
        .filter_map(|entry| {
            let id = entry.hash();
            let envelope =
                decode_envelope::<ExtensionCommand>(RoomLane::Extension, &entry.payload).ok()?;
            if !command_is_currently_authorized(
                &capabilities,
                history,
                id,
                &envelope,
                extension_command_area(Digest(envelope.namespace), &envelope.command),
            ) {
                return None;
            }
            Some((id, envelope.actor, envelope.command))
        })
        .collect()
}

pub fn materialize_extension(history: &DagSnapshot, roots: &[EntryHash]) -> ExtensionView {
    let commands = extension_commands(history, roots);
    let reach = ReachIndex::new(history);
    let pieces_locked = resolve_register(
        &reach,
        commands
            .iter()
            .filter_map(|(id, _, command)| match command {
                ExtensionCommand::SetConfig {
                    pieces_locked: Some(value),
                    ..
                } => Some((*id, *value)),
                _ => None,
            })
            .collect(),
    )
    .unwrap_or(false);
    let available_emojis = resolve_register(
        &reach,
        commands
            .iter()
            .filter_map(|(id, _, command)| match command {
                ExtensionCommand::SetConfig {
                    available_emojis: Some(value),
                    ..
                } => Some((*id, value.clone())),
                _ => None,
            })
            .collect(),
    );

    let unremoves: Vec<_> = commands
        .iter()
        .filter_map(|(id, _, command)| match command {
            ExtensionCommand::UnremovePiece { remove } => Some((*id, remove.entry())),
            _ => None,
        })
        .collect();
    let effective_removes: Vec<_> = commands
        .iter()
        .filter_map(|(id, _, command)| match command {
            ExtensionCommand::RemovePiece { piece } => Some((*id, piece.entry())),
            _ => None,
        })
        .filter(|(remove, _)| {
            !unremoves
                .iter()
                .any(|(unremove, target)| target == remove && reach.is_ancestor(remove, unremove))
        })
        .collect();

    let mut pieces = BTreeMap::new();
    for (put, owner, emoji, put_pitch) in commands.iter().filter_map(|(id, actor, command)| {
        let ExtensionCommand::PutPiece { emoji, pitch } = command else {
            return None;
        };
        Some((*id, *actor, emoji.clone(), *pitch))
    }) {
        let mut assertions = vec![(put, put_pitch)];
        assertions.extend(
            commands
                .iter()
                .filter_map(|(id, _, command)| match command {
                    ExtensionCommand::MovePiece { piece, pitch }
                        if piece.entry() == put && pitch.tuning_id == put_pitch.tuning_id =>
                    {
                        Some((*id, *pitch))
                    }
                    _ => None,
                }),
        );
        assertions.retain(|(assertion, _)| {
            !effective_removes
                .iter()
                .any(|(remove, target)| *target == put && reach.is_ancestor(assertion, remove))
        });
        let Some(pitch) = resolve_register(&reach, assertions) else {
            continue;
        };
        pieces.insert(
            PieceId::from_entry(put),
            Piece {
                owner,
                emoji,
                pitch,
            },
        );
    }

    ExtensionView {
        pieces,
        pieces_locked,
        available_emojis,
    }
}

#[derive(Clone, Copy)]
struct MusicMaterializer {
    root: EntryHash,
}

#[derive(Serialize, Deserialize)]
struct MusicCheckpointState {
    live: Vec<TunedDegree>,
    holders: Vec<(TunedDegree, Vec<ActorId>)>,
    envelopes: Vec<(TunedDegree, Envelope)>,
    tuning: TuningDefinition,
}

impl From<MusicView> for MusicCheckpointState {
    fn from(view: MusicView) -> Self {
        Self {
            live: view.live.into_iter().collect(),
            holders: view
                .holders
                .into_iter()
                .map(|(degree, actors)| (degree, actors.into_iter().collect()))
                .collect(),
            envelopes: view.envelopes.into_iter().collect(),
            tuning: view.tuning,
        }
    }
}

impl From<MusicCheckpointState> for MusicView {
    fn from(state: MusicCheckpointState) -> Self {
        Self {
            live: state.live.into_iter().collect(),
            holders: state
                .holders
                .into_iter()
                .map(|(degree, actors)| (degree, actors.into_iter().collect()))
                .collect(),
            envelopes: state.envelopes.into_iter().collect(),
            tuning: state.tuning,
        }
    }
}

impl Materializer for MusicMaterializer {
    type Error = serde_json::Error;

    fn key(&self) -> ProjectionKey {
        ProjectionKey::new("walkie/music", 5).expect("constant projection key")
    }

    fn project(
        &self,
        history: &DagSnapshot,
        _prior: Option<&ProjectionCheckpoint>,
    ) -> Result<Vec<u8>, Self::Error> {
        serde_json::to_vec(&MusicCheckpointState::from(materialize_music(
            history,
            &[self.root],
        )))
    }
}

#[derive(Clone, Copy)]
struct ExtensionMaterializer {
    root: EntryHash,
}

#[derive(Serialize, Deserialize)]
struct ExtensionCheckpointState {
    pieces: Vec<(PieceId, Piece)>,
    pieces_locked: bool,
    available_emojis: Option<String>,
}

impl From<ExtensionView> for ExtensionCheckpointState {
    fn from(view: ExtensionView) -> Self {
        Self {
            pieces: view.pieces.into_iter().collect(),
            pieces_locked: view.pieces_locked,
            available_emojis: view.available_emojis,
        }
    }
}

impl From<ExtensionCheckpointState> for ExtensionView {
    fn from(state: ExtensionCheckpointState) -> Self {
        Self {
            pieces: state.pieces.into_iter().collect(),
            pieces_locked: state.pieces_locked,
            available_emojis: state.available_emojis,
        }
    }
}

impl Materializer for ExtensionMaterializer {
    type Error = serde_json::Error;

    fn key(&self) -> ProjectionKey {
        ProjectionKey::new("walkie/extension", 5).expect("constant projection key")
    }

    fn project(
        &self,
        history: &DagSnapshot,
        _prior: Option<&ProjectionCheckpoint>,
    ) -> Result<Vec<u8>, Self::Error> {
        serde_json::to_vec(&ExtensionCheckpointState::from(materialize_extension(
            history,
            &[self.root],
        )))
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MemberCapabilities {
    pub music: Vec<EntryHash>,
    pub extension: Vec<EntryHash>,
}

impl MemberCapabilities {
    pub fn for_lane(&self, lane: RoomLane) -> &[EntryHash] {
        match lane {
            RoomLane::Music => &self.music,
            RoomLane::Extension => &self.extension,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RoomInvitation {
    pub room: RoomIdentity,
    pub owner: ActorId,
    pub member: ActorId,
    pub capabilities: MemberCapabilities,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CommandReceipt {
    pub lane: RoomLane,
    pub entry: EntryHash,
}

pub struct PreparedRoomCommand {
    lane: RoomLane,
    prepared: PreparedAdmission,
}

pub struct PreparedMemberGrant {
    lane: RoomLane,
    member: ActorId,
    prepared: PreparedAdmission,
}

/// An explicit capability attenuation request.
///
/// The issuer key remains a separate argument because it is signing authority,
/// while this value is safe to construct, inspect, and pass between layers.
#[derive(Debug, Clone)]
pub struct Delegation {
    pub lane: RoomLane,
    pub presented: Vec<EntryHash>,
    pub parent: EntryHash,
    pub receiver: ActorId,
    pub area: Area,
    pub rights: Rights,
}

impl PreparedMemberGrant {
    pub const fn lane(&self) -> RoomLane {
        self.lane
    }

    pub const fn member(&self) -> ActorId {
        self.member
    }

    pub fn transaction(&self) -> &StorageTransaction {
        self.prepared.transaction()
    }

    pub const fn entry(&self) -> EntryHash {
        self.prepared.entry()
    }

    /// Public, secret-free admission material for a low-latency carrier path.
    /// Receivers still run this through ordinary Replica admission; repair is
    /// the eventual-completeness fallback when causal closure is missing.
    pub fn replica_record(&self) -> ReplicaRecord {
        self.prepared.replica_record()
    }
}

impl PreparedRoomCommand {
    pub const fn lane(&self) -> RoomLane {
        self.lane
    }

    pub fn transaction(&self) -> &hhhs_store::StorageTransaction {
        self.prepared.transaction()
    }

    pub const fn entry(&self) -> EntryHash {
        self.prepared.entry()
    }

    /// Public, secret-free admission material for a low-latency carrier path.
    /// It contains no storage sequence, endpoint, route, or local secret.
    pub fn replica_record(&self) -> ReplicaRecord {
        self.prepared.replica_record()
    }
}

#[derive(Debug, Error)]
pub enum RoomError {
    #[error("HHHS replica rejected the operation: {0}")]
    Replica(#[from] ReplicaError),
    #[error("Room v5 command codec failed: {0}")]
    Codec(#[from] CommandCodecError),
    #[error("capability target {0:?} is missing or not a grant")]
    InvalidCapabilityTarget(EntryHash),
    #[error("materialization failed: {0}")]
    Materialization(String),
    #[error("materialized checkpoint could not be decoded: {0}")]
    Checkpoint(serde_json::Error),
    #[error("durable room recovery failed: {0}")]
    Recovery(String),
}

type LaneReplica<S> = Replica<S, RoomAdmissionPolicy>;

pub struct RoomReplicas<MS, ES>
where
    MS: ReplicaStorage + 'static,
    ES: ReplicaStorage + 'static,
{
    identity: RoomIdentity,
    owner: ActorId,
    music_root: EntryHash,
    extension_root: EntryHash,
    music: LaneReplica<MS>,
    extension: LaneReplica<ES>,
}

impl RoomReplicas<MemoryStorage, MemoryStorage> {
    pub fn memory(room_name: &str, owner: ActorId) -> Result<Self, RoomError> {
        Self::initialize(
            RoomIdentity::from_name(room_name),
            owner,
            MemoryStorage::new(),
            MemoryStorage::new(),
        )
    }

    /// Reconstruct deterministic roots, then strictly replay externally
    /// persisted per-lane transactions into the two independent stores.
    pub fn from_transaction_logs(
        identity: RoomIdentity,
        owner: ActorId,
        music_transactions: Vec<StorageTransaction>,
        extension_transactions: Vec<StorageTransaction>,
    ) -> Result<Self, RoomError> {
        let music_storage = MemoryStorage::new();
        let extension_storage = MemoryStorage::new();
        let room = Self::initialize(
            identity,
            owner,
            music_storage.clone(),
            extension_storage.clone(),
        )?;
        for transaction in music_transactions {
            music_storage
                .commit(transaction)
                .map_err(|error| RoomError::Recovery(error.to_string()))?;
        }
        for transaction in extension_transactions {
            extension_storage
                .commit(transaction)
                .map_err(|error| RoomError::Recovery(error.to_string()))?;
        }
        hhhs_sync::RepairHost::capture(&room.music_repair_host(), [0; 16])
            .map_err(|error| RoomError::Recovery(error.to_string()))?;
        hhhs_sync::RepairHost::capture(&room.extension_repair_host(), [0; 16])
            .map_err(|error| RoomError::Recovery(error.to_string()))?;
        Ok(room)
    }
}

impl<MS, ES> RoomReplicas<MS, ES>
where
    MS: ReplicaStorage + 'static,
    ES: ReplicaStorage + 'static,
{
    pub fn initialize(
        identity: RoomIdentity,
        owner: ActorId,
        music_storage: MS,
        extension_storage: ES,
    ) -> Result<Self, RoomError> {
        let (music, music_root) =
            initialize_lane(RoomLane::Music, identity.music, owner, music_storage)?;
        let (extension, extension_root) = initialize_lane(
            RoomLane::Extension,
            identity.extension,
            owner,
            extension_storage,
        )?;
        Ok(Self {
            identity,
            owner,
            music_root,
            extension_root,
            music,
            extension,
        })
    }

    pub fn identity(&self) -> &RoomIdentity {
        &self.identity
    }

    pub const fn owner(&self) -> ActorId {
        self.owner
    }

    pub fn owner_capabilities(&self) -> MemberCapabilities {
        MemberCapabilities {
            music: vec![self.music_root],
            extension: vec![self.extension_root],
        }
    }

    /// Discover receiver-bound grants already present and live in each lane.
    /// This is local convenience, not an authority oracle: authoring still
    /// presents the returned hashes and Replica admission verifies their full
    /// causal delegation paths.
    pub fn capabilities_for(&self, actor: ActorId) -> MemberCapabilities {
        MemberCapabilities {
            music: live_grants_for(&self.music.snapshot().history, self.music_root, actor),
            extension: live_grants_for(
                &self.extension.snapshot().history,
                self.extension_root,
                actor,
            ),
        }
    }

    /// Sign one bounded ephemeral presence update. The signed causal position
    /// makes revocation semantics explicit without turning heartbeats into
    /// durable history or giving gossip any authority of its own.
    pub fn sign_presence(
        &self,
        key: &SigningKey,
        capabilities: &MemberCapabilities,
        session: u64,
        sequence: u64,
        pitch: Option<TunedPeriodicPitch>,
    ) -> Result<Vec<u8>, PresenceError> {
        if capabilities.music.is_empty() || capabilities.music.len() > MAX_PRESENTED_GRANTS {
            return Err(PresenceError::Unauthorized);
        }
        let snapshot = self.music.snapshot();
        let at = snapshot.history.frontier();
        let actor = ActorId::from_signing_key(key);
        let authority = CapabilitySnapshot::capture(&snapshot.history, [self.music_root]);
        if !authority
            .authorize(&AuthorizationRequest {
                receiver: actor.receiver(),
                area: music_notes_area(self.identity.music),
                right: Right::Invoke,
                presented: capabilities.music.clone(),
                at: at.clone(),
                from: at.clone(),
            })
            .is_allowed()
        {
            return Err(PresenceError::Unauthorized);
        }
        let claims = PresenceClaims {
            generation: ROOM_PROTOCOL_GENERATION,
            room: *self.identity.object.as_bytes(),
            actor,
            session,
            sequence,
            pitch,
            at: at.0.iter().map(|entry| *entry.as_bytes()).collect(),
        };
        let action = Digest::of(&presence_claims_bytes(&claims)?);
        let context = PresentationContext::new(
            self.identity.music,
            action,
            at,
            music_notes_area(self.identity.music),
            Right::Invoke,
        )
        .map_err(|_| PresenceError::InvalidProof)?;
        let proof = Ed25519Verifier::present(key, capabilities.music.clone(), &context)
            .map_err(|_| PresenceError::InvalidProof)?
            .encode();
        encode_presence_envelope(&PresenceEnvelope { claims, proof })
    }

    /// Verify an ephemeral update against this Replica's current admitted
    /// capability history. Unknown causal heads fail closed; a later heartbeat
    /// can succeed after ordinary Replica repair catches the receiver up.
    pub fn verify_presence(&self, bytes: &[u8]) -> Result<RoomPresence, PresenceError> {
        let envelope = decode_presence_envelope(bytes)?;
        let claims = &envelope.claims;
        if claims.generation != ROOM_PROTOCOL_GENERATION
            || claims.room != *self.identity.object.as_bytes()
        {
            return Err(PresenceError::WrongRoom);
        }
        let at = Position::of(claims.at.iter().map(|bytes| EntryHash(Digest(*bytes))));
        let canonical_at: Vec<_> = at.0.iter().map(|entry| *entry.as_bytes()).collect();
        if canonical_at != claims.at {
            return Err(PresenceError::NonCanonical);
        }
        let action = Digest::of(&presence_claims_bytes(claims)?);
        let context = PresentationContext::new(
            self.identity.music,
            action,
            at,
            music_notes_area(self.identity.music),
            Right::Invoke,
        )
        .map_err(|_| PresenceError::InvalidProof)?;
        let proof = PresentationEnvelope::decode(&envelope.proof)
            .map_err(|_| PresenceError::InvalidProof)?;
        let mut verifiers = VerifierRegistry::new();
        verifiers
            .register(Arc::new(Ed25519Verifier))
            .map_err(|_| PresenceError::InvalidProof)?;
        let verified = verifiers
            .verify(&proof, &context)
            .map_err(|_| PresenceError::InvalidProof)?;
        if verified.receiver() != &claims.actor.receiver() {
            return Err(PresenceError::InvalidProof);
        }
        let history = self.music.snapshot().history;
        let capabilities = CapabilitySnapshot::capture(&history, [self.music_root]);
        if !capabilities
            .authorize(&verified.authorization_request(history.frontier()))
            .is_allowed()
        {
            return Err(PresenceError::Unauthorized);
        }
        Ok(RoomPresence {
            actor: claims.actor,
            session: claims.session,
            sequence: claims.sequence,
            pitch: claims.pitch,
        })
    }

    pub fn grant_member(
        &self,
        owner_key: &SigningKey,
        member: ActorId,
    ) -> Result<RoomInvitation, RoomError> {
        let music = delegate_member(
            &self.music,
            self.identity.music,
            self.music_root,
            owner_key,
            member,
        )?;
        let extension = delegate_member(
            &self.extension,
            self.identity.extension,
            self.extension_root,
            owner_key,
            member,
        )?;
        Ok(RoomInvitation {
            room: self.identity.clone(),
            owner: self.owner,
            member,
            capabilities: MemberCapabilities {
                music: vec![music],
                extension: vec![extension],
            },
        })
    }

    /// Prepare one receiver-bound member grant for an external asynchronous
    /// durability owner. Lanes remain independent; callers persist/finalize
    /// each grant separately and may safely retry a missing lane.
    pub fn prepare_member_grant(
        &self,
        lane: RoomLane,
        owner_key: &SigningKey,
        member: ActorId,
    ) -> Result<PreparedMemberGrant, RoomError> {
        let prepared = match lane {
            RoomLane::Music => prepare_member_grant_on(
                &self.music,
                self.identity.music,
                self.music_root,
                owner_key,
                member,
            )?,
            RoomLane::Extension => prepare_member_grant_on(
                &self.extension,
                self.identity.extension,
                self.extension_root,
                owner_key,
                member,
            )?,
        };
        Ok(PreparedMemberGrant {
            lane,
            member,
            prepared,
        })
    }

    pub fn commit_prepared_member_grant(
        &self,
        prepared: PreparedMemberGrant,
    ) -> Result<EntryHash, RoomError> {
        let outcome = match prepared.lane {
            RoomLane::Music => self.music.commit_prepared(prepared.prepared)?,
            RoomLane::Extension => self.extension.commit_prepared(prepared.prepared)?,
        };
        Ok(outcome.entry)
    }

    /// Delegate or attenuate one explicitly presented capability. Authority is
    /// never discovered from a member list: the caller names the exact parent,
    /// child receiver, area, and rights for causal evaluation.
    pub fn delegate(
        &self,
        issuer_key: &SigningKey,
        delegation: Delegation,
    ) -> Result<EntryHash, RoomError> {
        let Delegation {
            lane,
            presented,
            parent,
            receiver,
            area,
            rights,
        } = delegation;
        let grant = Grant {
            issuer: ActorId::from_signing_key(issuer_key).receiver(),
            receiver: receiver.receiver(),
            area: area.clone(),
            rights,
            parent: Some(parent),
        };
        let payload = encode_capability(&CapabilityOp::Grant(grant));
        let outcome = match lane {
            RoomLane::Music => {
                self.music
                    .author_ed25519(payload, area, Right::Invoke, presented, issuer_key)?
            }
            RoomLane::Extension => self.extension.author_ed25519(
                payload,
                area,
                Right::Invoke,
                presented,
                issuer_key,
            )?,
        };
        Ok(outcome.entry)
    }

    pub fn author(
        &self,
        key: &SigningKey,
        capabilities: &MemberCapabilities,
        command: RoomCommand,
    ) -> Result<CommandReceipt, RoomError> {
        self.author_with(key, capabilities, command, |_| {})
    }

    /// Author one typed command while attaching local-only storage state to the
    /// same admission transaction. The attachment cannot change command
    /// identity, capability context, or canonical history.
    pub fn author_with(
        &self,
        key: &SigningKey,
        capabilities: &MemberCapabilities,
        command: RoomCommand,
        attach: impl FnOnce(&mut AdmissionRequest),
    ) -> Result<CommandReceipt, RoomError> {
        let actor = ActorId::from_signing_key(key);
        let lane = command.lane();
        let namespace = self.identity.namespace(lane);
        let presented = capabilities.for_lane(lane).to_vec();
        let area = match &command {
            RoomCommand::Music(command) => music_command_area(namespace, command),
            RoomCommand::Extension(command) => extension_command_area(namespace, command),
        };
        let payload = match command {
            RoomCommand::Music(command) => {
                encode_music_command(namespace, actor, &presented, command)?
            }
            RoomCommand::Extension(command) => {
                encode_extension_command(namespace, actor, &presented, command)?
            }
        };
        let outcome = match lane {
            RoomLane::Music => self.music.author_ed25519_with(
                payload,
                area,
                Right::Invoke,
                presented,
                key,
                attach,
            )?,
            RoomLane::Extension => self.extension.author_ed25519_with(
                payload,
                area,
                Right::Invoke,
                presented,
                key,
                attach,
            )?,
        };
        Ok(CommandReceipt {
            lane,
            entry: outcome.entry,
        })
    }

    /// Validate and stage a typed command for an asynchronous durability
    /// adapter. The returned transaction can be awaited in IndexedDB before
    /// `commit_prepared` makes the command visible.
    pub fn prepare_author(
        &self,
        key: &SigningKey,
        capabilities: &MemberCapabilities,
        command: RoomCommand,
    ) -> Result<PreparedRoomCommand, RoomError> {
        let actor = ActorId::from_signing_key(key);
        let lane = command.lane();
        let namespace = self.identity.namespace(lane);
        let presented = capabilities.for_lane(lane).to_vec();
        let area = match &command {
            RoomCommand::Music(command) => music_command_area(namespace, command),
            RoomCommand::Extension(command) => extension_command_area(namespace, command),
        };
        let payload = match command {
            RoomCommand::Music(command) => {
                encode_music_command(namespace, actor, &presented, command)?
            }
            RoomCommand::Extension(command) => {
                encode_extension_command(namespace, actor, &presented, command)?
            }
        };
        let prepared = match lane {
            RoomLane::Music => {
                self.music
                    .prepare_ed25519(payload, area, Right::Invoke, presented, key)?
            }
            RoomLane::Extension => {
                self.extension
                    .prepare_ed25519(payload, area, Right::Invoke, presented, key)?
            }
        };
        Ok(PreparedRoomCommand { lane, prepared })
    }

    pub fn commit_prepared(
        &self,
        prepared: PreparedRoomCommand,
    ) -> Result<CommandReceipt, RoomError> {
        let outcome = match prepared.lane {
            RoomLane::Music => self.music.commit_prepared(prepared.prepared)?,
            RoomLane::Extension => self.extension.commit_prepared(prepared.prepared)?,
        };
        Ok(CommandReceipt {
            lane: prepared.lane,
            entry: outcome.entry,
        })
    }

    pub fn revoke(
        &self,
        lane: RoomLane,
        key: &SigningKey,
        capabilities: &[EntryHash],
        target: EntryHash,
        barrier: bool,
    ) -> Result<AdmissionOutcome, RoomError> {
        let replica = self.replica(lane);
        let target_entry = replica
            .snapshot()
            .history
            .entry(&target)
            .ok_or(RoomError::InvalidCapabilityTarget(target))?;
        let CapabilityOp::Grant(target_grant) = decode_capability(&target_entry.payload)
            .map_err(|_| RoomError::InvalidCapabilityTarget(target))?
        else {
            return Err(RoomError::InvalidCapabilityTarget(target));
        };
        let payload = encode_capability(&CapabilityOp::Revoke(Revoke {
            revoker: ActorId::from_signing_key(key).receiver(),
            target,
            barrier,
        }));
        match lane {
            RoomLane::Music => Ok(self.music.author_ed25519(
                payload,
                target_grant.area,
                Right::Invoke,
                capabilities.to_vec(),
                key,
            )?),
            RoomLane::Extension => Ok(self.extension.author_ed25519(
                payload,
                target_grant.area,
                Right::Invoke,
                capabilities.to_vec(),
                key,
            )?),
        }
    }

    pub fn view(&self) -> RoomView {
        let music = materialize_music(&self.music.snapshot().history, &[self.music_root]);
        let extension =
            materialize_extension(&self.extension.snapshot().history, &[self.extension_root]);
        let pieces = match music.tuning.validate("active Room v5 tuning") {
            Ok(active) => extension
                .pieces
                .iter()
                .filter(|(_, piece)| piece.pitch.validate(&active).is_ok())
                .map(|(id, piece)| (*id, piece.clone()))
                .collect(),
            Err(_) => BTreeMap::new(),
        };
        RoomView {
            music,
            pieces,
            pieces_locked: extension.pieces_locked,
            available_emojis: extension.available_emojis,
        }
    }

    pub fn materialize_checkpoints(
        &self,
    ) -> Result<(ProjectionCheckpoint, ProjectionCheckpoint), RoomError> {
        let music = self
            .music
            .materialize(&MusicMaterializer {
                root: self.music_root,
            })
            .map_err(|error| RoomError::Materialization(format!("{error:?}")))?;
        let extension = self
            .extension
            .materialize(&ExtensionMaterializer {
                root: self.extension_root,
            })
            .map_err(|error| RoomError::Materialization(format!("{error:?}")))?;
        Ok((music, extension))
    }

    pub fn decode_music_checkpoint(
        checkpoint: &ProjectionCheckpoint,
    ) -> Result<MusicView, RoomError> {
        serde_json::from_slice::<MusicCheckpointState>(checkpoint.bytes())
            .map(MusicView::from)
            .map_err(RoomError::Checkpoint)
    }

    pub fn decode_extension_checkpoint(
        checkpoint: &ProjectionCheckpoint,
    ) -> Result<ExtensionView, RoomError> {
        serde_json::from_slice::<ExtensionCheckpointState>(checkpoint.bytes())
            .map(ExtensionView::from)
            .map_err(RoomError::Checkpoint)
    }

    pub fn music_snapshot(&self) -> hhhs_store::StorageSnapshot {
        self.music.snapshot()
    }

    pub fn extension_snapshot(&self) -> hhhs_store::StorageSnapshot {
        self.extension.snapshot()
    }

    pub fn local_secret(
        &self,
        lane: RoomLane,
        key: &SecretKey,
    ) -> Result<Option<SecretValue>, RoomError> {
        match lane {
            RoomLane::Music => Ok(self.music.secret(key)?),
            RoomLane::Extension => Ok(self.extension.secret(key)?),
        }
    }

    pub fn music_repair_host(&self) -> ReplicaRepairHost<MS, RoomAdmissionPolicy> {
        ReplicaRepairHost::new(self.music.clone())
    }

    pub fn extension_repair_host(&self) -> ReplicaRepairHost<ES, RoomAdmissionPolicy> {
        ReplicaRepairHost::new(self.extension.clone())
    }

    pub fn music_durable_repair_host<D>(
        &self,
        durability: D,
    ) -> DurableReplicaRepairHost<MS, RoomAdmissionPolicy, D>
    where
        D: AsyncTransactionSink,
    {
        DurableReplicaRepairHost::new(self.music.clone(), durability)
    }

    pub fn extension_durable_repair_host<D>(
        &self,
        durability: D,
    ) -> DurableReplicaRepairHost<ES, RoomAdmissionPolicy, D>
    where
        D: AsyncTransactionSink,
    {
        DurableReplicaRepairHost::new(self.extension.clone(), durability)
    }

    fn replica(&self, lane: RoomLane) -> ReplicaRef<'_, MS, ES> {
        match lane {
            RoomLane::Music => ReplicaRef::Music(&self.music),
            RoomLane::Extension => ReplicaRef::Extension(&self.extension),
        }
    }
}

enum ReplicaRef<'a, MS, ES>
where
    MS: ReplicaStorage + 'static,
    ES: ReplicaStorage + 'static,
{
    Music(&'a LaneReplica<MS>),
    Extension(&'a LaneReplica<ES>),
}

impl<MS, ES> ReplicaRef<'_, MS, ES>
where
    MS: ReplicaStorage + 'static,
    ES: ReplicaStorage + 'static,
{
    fn snapshot(&self) -> hhhs_store::StorageSnapshot {
        match self {
            Self::Music(replica) => replica.snapshot(),
            Self::Extension(replica) => replica.snapshot(),
        }
    }
}

fn initialize_lane<S: ReplicaStorage + 'static>(
    lane: RoomLane,
    namespace: Digest,
    owner: ActorId,
    storage: S,
) -> Result<(LaneReplica<S>, EntryHash), RoomError> {
    let root = hhhs_cap::entry(
        &CapabilityOp::Grant(Grant {
            issuer: owner.receiver(),
            receiver: owner.receiver(),
            area: Area::root(namespace),
            rights: Rights::ALL,
            parent: None,
        }),
        Position::empty(),
    );
    let root_id = root.hash();
    let replica = Replica::builder(
        storage,
        RoomAdmissionPolicy::new(lane, namespace),
        namespace,
    )
    .ed25519_capabilities([root_id])?
    .build()?;
    if !replica.snapshot().history.contains(&root_id) {
        replica.admit(AdmissionRequest::trusted_root(root))?;
    }
    Ok((replica, root_id))
}

fn delegate_member<S: ReplicaStorage + 'static>(
    replica: &LaneReplica<S>,
    namespace: Digest,
    parent: EntryHash,
    issuer_key: &SigningKey,
    member: ActorId,
) -> Result<EntryHash, RoomError> {
    let grant = Grant {
        issuer: ActorId::from_signing_key(issuer_key).receiver(),
        receiver: member.receiver(),
        area: Area::root(namespace),
        rights: Rights::INVOKE,
        parent: Some(parent),
    };
    let outcome = replica.author_ed25519(
        encode_capability(&CapabilityOp::Grant(grant.clone())),
        grant.area,
        Right::Invoke,
        vec![parent],
        issuer_key,
    )?;
    Ok(outcome.entry)
}

fn prepare_member_grant_on<S: ReplicaStorage + 'static>(
    replica: &LaneReplica<S>,
    namespace: Digest,
    root: EntryHash,
    owner_key: &SigningKey,
    member: ActorId,
) -> Result<PreparedAdmission, RoomError> {
    let grant = Grant {
        issuer: ActorId::from_signing_key(owner_key).receiver(),
        receiver: member.receiver(),
        area: Area::root(namespace),
        rights: Rights::INVOKE,
        parent: Some(root),
    };
    Ok(replica.prepare_ed25519(
        encode_capability(&CapabilityOp::Grant(grant.clone())),
        grant.area,
        Right::Invoke,
        vec![root],
        owner_key,
    )?)
}

fn live_grants_for(history: &DagSnapshot, root: EntryHash, actor: ActorId) -> Vec<EntryHash> {
    let capabilities = CapabilitySnapshot::capture(history, [root]);
    let frontier = history.frontier();
    history
        .entries_topo()
        .into_iter()
        .filter_map(|entry| {
            let id = entry.hash();
            let CapabilityOp::Grant(grant) = decode_capability(&entry.payload).ok()? else {
                return None;
            };
            if grant.receiver != actor.receiver()
                || !grant.rights.contains(Right::Invoke)
                || !matches!(
                    capabilities.authorize(&AuthorizationRequest {
                        receiver: actor.receiver(),
                        area: grant.area,
                        right: Right::Invoke,
                        presented: vec![id],
                        at: frontier.clone(),
                        from: frontier.clone(),
                    }),
                    AuthorizationDecision::Allowed(_)
                )
            {
                return None;
            }
            Some(id)
        })
        .take(MAX_PRESENTED_GRANTS)
        .collect()
}

#[cfg(test)]
mod tests {
    use futures::executor::block_on;
    use hhhs_store::JournalStorage;
    use hhhs_sync::{EntrySource, RepairHost};

    use super::*;
    use crate::tuning::{TunedDegree, Tuning};

    fn key(byte: u8) -> SigningKey {
        SigningKey::from_bytes(&[byte; 32])
    }

    fn degree(index: u16) -> TunedDegree {
        TunedDegree::new(&Tuning::twelve_tet(), index).unwrap()
    }

    fn repair_lane<S1, S2>(
        source: ReplicaRepairHost<S1, RoomAdmissionPolicy>,
        mut target: ReplicaRepairHost<S2, RoomAdmissionPolicy>,
    ) where
        S1: ReplicaStorage + 'static,
        S2: ReplicaStorage + 'static,
    {
        let snapshot = source.capture([7; 16]).unwrap();
        let hashes = source.replica().snapshot().history.all_hashes();
        let mut included = BTreeSet::new();
        let mut delivered = Vec::new();
        for hash in hashes {
            delivered.extend(snapshot.bytes_with_closure(&hash, &mut included));
        }
        let report = block_on(target.apply(&delivered)).unwrap();
        assert!(report.refused.is_empty());
    }

    fn repair<AS, AE, BS, BE>(a: &RoomReplicas<AS, AE>, b: &RoomReplicas<BS, BE>)
    where
        AS: ReplicaStorage + 'static,
        AE: ReplicaStorage + 'static,
        BS: ReplicaStorage + 'static,
        BE: ReplicaStorage + 'static,
    {
        repair_lane(a.music_repair_host(), b.music_repair_host());
        repair_lane(a.extension_repair_host(), b.extension_repair_host());
    }

    #[test]
    fn command_codec_is_strict_and_lane_separated() {
        let identity = RoomIdentity::from_name("bright-river-song");
        assert_eq!(
            RoomIdentity::from_object(identity.object),
            identity,
            "a ticket needs only the Room-v5 object to reconstruct both lanes"
        );
        let actor = ActorId::from_signing_key(&key(1));
        let canonical = encode_music_command(
            identity.music,
            actor,
            &[EntryHash(Digest([1; 32]))],
            MusicOp::AddDegree { degree: degree(4) },
        )
        .unwrap();
        assert!(decode_envelope::<MusicOp>(RoomLane::Music, &canonical).is_ok());
        assert!(matches!(
            decode_envelope::<ExtensionCommand>(RoomLane::Extension, &canonical),
            Err(CommandCodecError::WrongDomain)
        ));
        let mut spaced = canonical.clone();
        spaced.insert(tutti_music_hhhs::COMMAND_DOMAIN.len(), b' ');
        assert!(matches!(
            decode_envelope::<MusicOp>(RoomLane::Music, &spaced),
            Err(CommandCodecError::NonCanonical)
        ));
    }

    #[test]
    fn open_room_phrase_is_a_normalized_bearer_authority() {
        let lower = open_room_authority("bright-river-song");
        let mixed = open_room_authority("Bright-River-Song");
        let other = open_room_authority("bright-river-dawn");

        assert_eq!(lower.verifying_key(), mixed.verifying_key());
        assert_ne!(lower.verifying_key(), other.verifying_key());
    }

    #[test]
    fn proof_receiver_is_bound_to_payload_actor() {
        let owner_key = key(1);
        let owner = ActorId::from_signing_key(&owner_key);
        let room = RoomReplicas::memory("bright-river-song", owner).unwrap();
        let payload = encode_music_command(
            room.identity.music,
            ActorId::from_signing_key(&key(2)),
            &[room.music_root],
            MusicOp::AddDegree { degree: degree(3) },
        )
        .unwrap();
        let error = room
            .music
            .author_ed25519(
                payload,
                music_notes_area(room.identity.music),
                Right::Invoke,
                vec![room.music_root],
                &owner_key,
            )
            .unwrap_err();
        assert!(matches!(error, ReplicaError::ApplicationRejected(_)));
    }

    #[test]
    fn ephemeral_presence_is_room_bound_capability_checked_and_strict() {
        let owner_key = key(1);
        let member_key = key(2);
        let owner = ActorId::from_signing_key(&owner_key);
        let member = ActorId::from_signing_key(&member_key);
        let owner_room = RoomReplicas::memory("bright-river-song", owner).unwrap();
        let member_room = RoomReplicas::memory("bright-river-song", owner).unwrap();
        let foreign_room = RoomReplicas::memory("quiet-forest-song", owner).unwrap();
        let invitation = owner_room.grant_member(&owner_key, member).unwrap();
        repair(&owner_room, &member_room);
        let pitch = TunedPeriodicPitch::new(&Tuning::twelve_tet(), 5, 0).unwrap();

        let wire = member_room
            .sign_presence(&member_key, &invitation.capabilities, 7, 11, Some(pitch))
            .unwrap();
        assert_eq!(
            owner_room.verify_presence(&wire).unwrap(),
            RoomPresence {
                actor: member,
                session: 7,
                sequence: 11,
                pitch: Some(pitch),
            }
        );
        assert!(matches!(
            foreign_room.verify_presence(&wire),
            Err(PresenceError::WrongRoom)
        ));
        let mut tampered = wire.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(owner_room.verify_presence(&tampered).is_err());

        owner_room
            .revoke(
                RoomLane::Music,
                &owner_key,
                &[owner_room.music_root],
                invitation.capabilities.music[0],
                true,
            )
            .unwrap();
        repair(&owner_room, &member_room);
        assert!(matches!(
            member_room.sign_presence(&member_key, &invitation.capabilities, 7, 12, None),
            Err(PresenceError::Unauthorized)
        ));
    }

    #[test]
    fn semantic_command_area_is_proof_bound() {
        let owner_key = key(1);
        let owner = ActorId::from_signing_key(&owner_key);
        let room = RoomReplicas::memory("bright-river-song", owner).unwrap();
        let payload = encode_music_command(
            room.identity.music,
            owner,
            &[room.music_root],
            MusicOp::AddDegree { degree: degree(3) },
        )
        .unwrap();
        let error = room
            .music
            .author_ed25519(
                payload,
                music_tuning_area(room.identity.music),
                Right::Invoke,
                vec![room.music_root],
                &owner_key,
            )
            .unwrap_err();
        assert!(matches!(error, ReplicaError::ApplicationRejected(_)));
    }

    #[test]
    fn member_can_attenuate_a_capability_without_a_role_lookup() {
        let owner_key = key(1);
        let member_key = key(2);
        let device_key = key(3);
        let owner = ActorId::from_signing_key(&owner_key);
        let member = ActorId::from_signing_key(&member_key);
        let device = ActorId::from_signing_key(&device_key);
        let owner_room = RoomReplicas::memory("bright-river-song", owner).unwrap();
        let member_room = RoomReplicas::memory("bright-river-song", owner).unwrap();
        let device_room = RoomReplicas::memory("bright-river-song", owner).unwrap();
        let invitation = owner_room.grant_member(&owner_key, member).unwrap();
        repair(&owner_room, &member_room);
        assert_eq!(
            member_room.capabilities_for(member),
            invitation.capabilities
        );

        let notes_grant = member_room
            .delegate(
                &member_key,
                Delegation {
                    lane: RoomLane::Music,
                    presented: invitation.capabilities.music.clone(),
                    parent: invitation.capabilities.music[0],
                    receiver: device,
                    area: music_notes_area(member_room.identity.music),
                    rights: Rights::INVOKE,
                },
            )
            .unwrap();
        repair(&member_room, &device_room);
        let device_capabilities = MemberCapabilities {
            music: vec![notes_grant],
            extension: Vec::new(),
        };
        device_room
            .author(
                &device_key,
                &device_capabilities,
                MusicOp::AddDegree { degree: degree(6) }.into(),
            )
            .unwrap();
        let tuning_error = device_room
            .author(
                &device_key,
                &device_capabilities,
                MusicOp::SetTuning {
                    definition: TuningDefinition::twelve_tet(),
                }
                .into(),
            )
            .unwrap_err();
        assert!(matches!(
            tuning_error,
            RoomError::Replica(ReplicaError::CapabilityDenied(_))
        ));
    }

    #[test]
    fn invitation_repair_and_offline_commands_converge() {
        let owner_key = key(1);
        let member_key = key(2);
        let owner = ActorId::from_signing_key(&owner_key);
        let member = ActorId::from_signing_key(&member_key);
        let a = RoomReplicas::memory("bright-river-song", owner).unwrap();
        let b = RoomReplicas::memory("bright-river-song", owner).unwrap();
        let invitation = a.grant_member(&owner_key, member).unwrap();
        repair(&a, &b);

        a.author(
            &owner_key,
            &a.owner_capabilities(),
            MusicOp::AddDegree { degree: degree(1) }.into(),
        )
        .unwrap();
        b.author(
            &member_key,
            &invitation.capabilities,
            MusicOp::AddDegree { degree: degree(7) }.into(),
        )
        .unwrap();
        repair(&a, &b);
        repair(&b, &a);

        assert_eq!(
            a.music_snapshot().history.all_hashes().len(),
            b.music_snapshot().history.all_hashes().len()
        );
        assert_eq!(a.view(), b.view());
        assert_eq!(a.extension_snapshot().history.all_hashes().len(), 2);
    }

    #[test]
    fn revocation_blocks_future_commands_without_erasing_history() {
        let owner_key = key(1);
        let member_key = key(2);
        let owner = ActorId::from_signing_key(&owner_key);
        let member = ActorId::from_signing_key(&member_key);
        let a = RoomReplicas::memory("bright-river-song", owner).unwrap();
        let b = RoomReplicas::memory("bright-river-song", owner).unwrap();
        let invitation = a.grant_member(&owner_key, member).unwrap();
        repair(&a, &b);
        b.author(
            &member_key,
            &invitation.capabilities,
            MusicOp::AddDegree { degree: degree(5) }.into(),
        )
        .unwrap();
        repair(&b, &a);

        a.revoke(
            RoomLane::Music,
            &owner_key,
            &[a.music_root],
            invitation.capabilities.music[0],
            true,
        )
        .unwrap();
        repair(&a, &b);
        let error = b
            .author(
                &member_key,
                &invitation.capabilities,
                MusicOp::AddDegree { degree: degree(9) }.into(),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            RoomError::Replica(ReplicaError::CapabilityDenied(_))
        ));
        assert!(b.view().music.live.contains(&degree(5)));
        assert!(!b.view().music.live.contains(&degree(9)));
    }

    #[test]
    fn concurrent_barrier_converges_history_and_retracts_the_view() {
        let owner_key = key(1);
        let member_key = key(2);
        let owner = ActorId::from_signing_key(&owner_key);
        let member = ActorId::from_signing_key(&member_key);
        let a = RoomReplicas::memory("bright-river-song", owner).unwrap();
        let b = RoomReplicas::memory("bright-river-song", owner).unwrap();
        let invitation = a.grant_member(&owner_key, member).unwrap();
        repair(&a, &b);

        // Neither side observes the other's next operation: the action and
        // barrier revocation have the same causal predecessor and are
        // concurrent.
        b.author(
            &member_key,
            &invitation.capabilities,
            MusicOp::AddDegree { degree: degree(10) }.into(),
        )
        .unwrap();
        a.revoke(
            RoomLane::Music,
            &owner_key,
            &[a.music_root],
            invitation.capabilities.music[0],
            true,
        )
        .unwrap();
        assert!(b.view().music.live.contains(&degree(10)));

        repair(&a, &b);
        repair(&b, &a);
        let a_hashes: BTreeSet<_> = a
            .music_snapshot()
            .history
            .all_hashes()
            .into_iter()
            .collect();
        let b_hashes: BTreeSet<_> = b
            .music_snapshot()
            .history
            .all_hashes()
            .into_iter()
            .collect();
        assert_eq!(a_hashes, b_hashes);
        assert!(!a.view().music.live.contains(&degree(10)));
        assert!(!b.view().music.live.contains(&degree(10)));
    }

    #[test]
    fn materialized_checkpoints_round_trip() {
        let owner_key = key(1);
        let owner = ActorId::from_signing_key(&owner_key);
        let room = RoomReplicas::memory("bright-river-song", owner).unwrap();
        room.author(
            &owner_key,
            &room.owner_capabilities(),
            MusicOp::AddDegree { degree: degree(4) }.into(),
        )
        .unwrap();
        let (music, extension) = room.materialize_checkpoints().unwrap();
        assert_eq!(
            RoomReplicas::<MemoryStorage, MemoryStorage>::decode_music_checkpoint(&music).unwrap(),
            room.view().music
        );
        assert_eq!(
            RoomReplicas::<MemoryStorage, MemoryStorage>::decode_extension_checkpoint(&extension)
                .unwrap(),
            materialize_extension(&room.extension_snapshot().history, &[room.extension_root])
        );
    }

    #[test]
    fn prepared_command_is_visible_only_after_durable_commit() {
        let owner_key = key(1);
        let owner = ActorId::from_signing_key(&owner_key);
        let room = RoomReplicas::memory("bright-river-song", owner).unwrap();
        let before = room.view();
        let prepared = room
            .prepare_author(
                &owner_key,
                &room.owner_capabilities(),
                MusicOp::AddDegree { degree: degree(4) }.into(),
            )
            .unwrap();

        assert_eq!(prepared.lane(), RoomLane::Music);
        assert_eq!(room.view(), before);
        let encoded = hhhs_store::encode_storage_transaction(prepared.transaction());
        let decoded = hhhs_store::decode_storage_transaction(&encoded).unwrap();
        assert_eq!(hhhs_store::encode_storage_transaction(&decoded), encoded);

        let receipt = room.commit_prepared(prepared).unwrap();
        assert_eq!(receipt.lane, RoomLane::Music);
        assert!(room.view().music.live.contains(&degree(4)));
    }

    #[test]
    fn prepared_command_refuses_a_stale_storage_sequence() {
        let owner_key = key(1);
        let owner = ActorId::from_signing_key(&owner_key);
        let room = RoomReplicas::memory("bright-river-song", owner).unwrap();
        let stale = room
            .prepare_author(
                &owner_key,
                &room.owner_capabilities(),
                MusicOp::AddDegree { degree: degree(4) }.into(),
            )
            .unwrap();
        room.author(
            &owner_key,
            &room.owner_capabilities(),
            MusicOp::AddDegree { degree: degree(5) }.into(),
        )
        .unwrap();

        assert!(matches!(
            room.commit_prepared(stale),
            Err(RoomError::Replica(ReplicaError::Storage(_)))
        ));
        assert!(!room.view().music.live.contains(&degree(4)));
        assert!(room.view().music.live.contains(&degree(5)));
    }

    #[test]
    fn external_transaction_logs_reconstruct_both_replica_roots_and_history() {
        let owner_key = key(1);
        let owner = ActorId::from_signing_key(&owner_key);
        let identity = RoomIdentity::from_name("bright-river-song");
        let room = RoomReplicas::memory("bright-river-song", owner).unwrap();
        let prepared = room
            .prepare_author(
                &owner_key,
                &room.owner_capabilities(),
                MusicOp::AddDegree { degree: degree(4) }.into(),
            )
            .unwrap();
        let music_log = vec![prepared.transaction().clone()];
        room.commit_prepared(prepared).unwrap();

        let recovered =
            RoomReplicas::from_transaction_logs(identity, owner, music_log, Vec::new()).unwrap();
        assert_eq!(recovered.view(), room.view());
        assert_eq!(recovered.owner_capabilities(), room.owner_capabilities());
        assert_eq!(recovered.extension_snapshot().history.all_hashes().len(), 1);
    }

    #[test]
    fn native_journals_reopen_history_secret_and_checkpoint() {
        let directory = tempfile::tempdir().unwrap();
        let music_path = directory.path().join("music.hhhs");
        let extension_path = directory.path().join("extension.hhhs");
        let owner_key = key(1);
        let owner = ActorId::from_signing_key(&owner_key);
        let identity = RoomIdentity::from_name("bright-river-song");
        let secret_key = SecretKey::new("walkie/test-key").unwrap();
        {
            let room = RoomReplicas::initialize(
                identity.clone(),
                owner,
                JournalStorage::open(&music_path).unwrap(),
                JournalStorage::open(&extension_path).unwrap(),
            )
            .unwrap();
            room.author_with(
                &owner_key,
                &room.owner_capabilities(),
                MusicOp::AddDegree { degree: degree(8) }.into(),
                |request| {
                    request.put_secret(
                        secret_key.clone(),
                        SecretValue::new(b"local material".to_vec()).unwrap(),
                    );
                },
            )
            .unwrap();
            room.materialize_checkpoints().unwrap();
        }

        let reopened = RoomReplicas::initialize(
            identity,
            owner,
            JournalStorage::open(&music_path).unwrap(),
            JournalStorage::open(&extension_path).unwrap(),
        )
        .unwrap();
        assert!(reopened.view().music.live.contains(&degree(8)));
        assert_eq!(
            reopened
                .local_secret(RoomLane::Music, &secret_key)
                .unwrap()
                .unwrap()
                .expose(),
            b"local material"
        );
        let (music, extension) = reopened.materialize_checkpoints().unwrap();
        assert_eq!(
            RoomReplicas::<JournalStorage, JournalStorage>::decode_music_checkpoint(&music)
                .unwrap(),
            reopened.view().music
        );
        assert_eq!(
            RoomReplicas::<JournalStorage, JournalStorage>::decode_extension_checkpoint(&extension)
                .unwrap(),
            materialize_extension(
                &reopened.extension_snapshot().history,
                &[reopened.extension_root]
            )
        );
    }

    #[test]
    fn composed_view_reports_additions_and_retractions() {
        let owner_key = key(1);
        let owner = ActorId::from_signing_key(&owner_key);
        let room = RoomReplicas::memory("bright-river-song", owner).unwrap();
        let before = room.view();
        room.author(
            &owner_key,
            &room.owner_capabilities(),
            MusicOp::AddDegree { degree: degree(4) }.into(),
        )
        .unwrap();
        let after_add = room.view();
        assert_eq!(
            after_add.changes_since(&before).pitches_added,
            BTreeSet::from([degree(4)])
        );
        room.author(
            &owner_key,
            &room.owner_capabilities(),
            MusicOp::RemoveDegree { degree: degree(4) }.into(),
        )
        .unwrap();
        let after_remove = room.view();
        assert_eq!(
            after_remove.changes_since(&after_add).pitches_retracted,
            BTreeSet::from([degree(4)])
        );
    }
}
