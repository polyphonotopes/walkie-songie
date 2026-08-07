//! The signed operation format — walkie-songie's durable, gossiped op log (v3).
//!
//! This replaces the `yrs` (Yjs) state-CRDT with a **signed, per-author append-only
//! operation log** built on `p2panda-core`. Every musical contribution — a pitch
//! toggle, an emoji piece, a tuning, or room-config change — becomes a
//! [`WalkieOp`] wrapped in a [`VersionedOp`] envelope, CBOR-encoded as a p2panda
//! `Body`, and signed by the author's Ed25519 key via `Header::sign`. Verification
//! runs `validate_operation`, so who contributed which note is cryptographically
//! established.
//!
//! These signed ops are the source of truth; the HHHS causal-DAG mirror
//! (`room::store`) lifts them into a read model. Design mirrors `potluck-ops::signed`.
//!
//! ## v3 alphabet
//! The op alphabet is shaped for HHHS-native materialization:
//! - **Degrees** are a content-keyed add-wins set keyed by [`TunedDegree`]. A
//!   `RemoveDegree` supersedes only the adds in its causal past; a concurrent add
//!   survives.
//! - **Pieces** are graph-shaped and owner-gated: identity is the *op id* of the
//!   `PutPiece` that created them, so two peers creating "the same" piece
//!   concurrently are simply two pieces. `MovePiece`/`RemovePiece`/`UnremovePiece`
//!   reference that [`OpId`]; only the owner's ops take effect.
//! - **Tuning/config** are room-wide registers resolved by causal maxima.
//! - **Voice preview** is deliberately absent. It is signed, sequenced, leased
//!   presence and never enters durable history.
//!
//! **Evolution discipline:** append variants to [`WalkieOp`], never reorder them, add
//! fields only as `#[serde(default)]`, and bump [`OP_SCHEMA_VERSION`] when the payload
//! shape changes.
//!
//! **wasm timestamps:** `ts_micros` must be author-supplied. On wasm pass
//! `js_sys::Date::now() as u64 * 1000`; never call p2panda's `Timestamp::now()`
//! (it uses `SystemTime::now()`, which panics on `wasm32`).

use p2panda_core::cbor::{DecodeError, decode_cbor, encode_cbor};
use p2panda_core::{Body, Hash, Header, Operation, OperationError, validate_operation};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::tuning::{MAX_SCALE_DEGREES, TunedDegree, TunedPeriodicPitch, TuningDefinition};

/// Re-exported so the rest of the crate names the key types through `room::ops` and
/// never takes a direct `p2panda-core` dependency. `SigningKey::from_bytes` is
/// infallible — any 32 bytes are a valid Ed25519 seed.
pub use p2panda_core::{SigningKey, VerifyingKey};

/// The current op-payload schema version.
pub const OP_SCHEMA_VERSION: u16 = 3;
/// Largest legal signed payload.
///
/// This is the ROOT of the size ladder, not a free parameter. Everything that
/// has to carry one op — the gossip message cap
/// ([`MAX_GOSSIP_MESSAGE_BYTES`](crate::net::native::MAX_GOSSIP_MESSAGE_BYTES)),
/// the journal record cap, and above all the anti-entropy frame cap
/// ([`MAX_SYNC_FRAME_BYTES`](crate::net::sync::MAX_SYNC_FRAME_BYTES)) — is
/// derived from [`MAX_SIGNED_OP_WIRE_BYTES`] below and asserted at compile time.
///
/// It was 2 MiB, which is larger than the 1 MiB sync frame cap was: an op that
/// verified, gossiped and lifted could then never be re-served by anti-entropy,
/// because `bytes_with_closure` must include the requested hash whatever the
/// budget says. One such op permanently poisoned sync for the whole room — every
/// session that requested it died with `FrameTooLarge`, forever, and peers that
/// missed the gossip stayed silently divergent.
pub const MAX_SIGNED_PAYLOAD_BYTES: usize = 1024 * 1024;
pub const MAX_OBSERVED_OPS: usize = 4096;
pub const MAX_TOPIC_BYTES: usize = 256;
pub const MAX_EMOJI_BYTES: usize = 256;
pub const MAX_EMOJI_PALETTE_BYTES: usize = 16 * 1024;
pub const MAX_ABS_PERIOD: i32 = 1_000_000;
const SIGNED_OP_WIRE_MAGIC: &[u8] = b"walkie.signed-op/3\0";
pub const MAX_SIGNED_HEADER_BYTES: usize = 64 * 1024;

/// Largest possible [`SignedOp::to_wire_bytes`] output for a legal op — the unit
/// every carrying layer must be able to move whole. Anti-entropy has no way to
/// split one op across frames, so a transport cap below this is a permanent
/// convergence failure, not a slow path.
pub const MAX_SIGNED_OP_WIRE_BYTES: usize =
    SIGNED_OP_WIRE_MAGIC.len() + 8 + MAX_SIGNED_HEADER_BYTES + MAX_SIGNED_PAYLOAD_BYTES;

/// A 32-byte author identity — the Ed25519 verifying-key bytes. Doubles as the peer's
/// stable id across the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AuthorId(pub [u8; 32]);

impl AuthorId {
    pub fn to_hex(&self) -> String {
        hex32(&self.0)
    }
}

/// A p2panda operation id — `blake3(header bytes)` of a signed op. Used as the stable,
/// cross-peer identity for a piece (the id of the `PutPiece` that created it) and as
/// the causal-horizon references an op carries in [`VersionedOp::observed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OpId(pub [u8; 32]);

impl OpId {
    pub fn to_hex(&self) -> String {
        hex32(&self.0)
    }
}

fn hex32(bytes: &[u8; 32]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(&mut s, "{b:02x}");
        s
    })
}

/// The domain operation (v3). Materialization semantics live in `room::store`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WalkieOp {
    /// Add one tuning-scoped degree to the shared content-keyed set.
    AddDegree { pitch: TunedDegree },
    /// Retract this author's observed adds of one tuning-scoped degree.
    RemoveDegree { pitch: TunedDegree },
    /// Create an emoji piece. Its identity is THIS op's [`OpId`]; `MovePiece`/
    /// `RemovePiece` reference that id.
    PutPiece {
        emoji: String,
        pitch: TunedPeriodicPitch,
    },
    /// Move the piece created by `piece` to a new periodic pitch (owner-gated).
    MovePiece {
        piece: OpId,
        pitch: TunedPeriodicPitch,
    },
    /// Remove the piece created by `piece` (owner-gated).
    RemovePiece { piece: OpId },
    /// Undo a `RemovePiece` (a remove-of-remove); resurrects the piece.
    UnremovePiece { remove: OpId },
    /// Canonical room-wide tuning definition (register; causal-maxima resolved).
    SetTuning { definition: TuningDefinition },
    /// Room-wide configuration (register). Fields optional so one op carries one change.
    SetConfig {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pieces_locked: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        available_emojis: Option<String>,
    },
}

impl WalkieOp {
    fn validate_wire(&self) -> Result<(), String> {
        let validate_degree = |pitch: TunedDegree| {
            if usize::from(pitch.degree.index()) >= MAX_SCALE_DEGREES {
                Err(format!(
                    "degree {} exceeds the supported bound",
                    pitch.degree.index()
                ))
            } else {
                Ok(())
            }
        };
        let validate_periodic = |pitch: TunedPeriodicPitch| {
            validate_degree(pitch.degree())?;
            if pitch.pitch.period().unsigned_abs() > MAX_ABS_PERIOD as u32 {
                return Err(format!(
                    "period {} exceeds the supported bound",
                    pitch.pitch.period()
                ));
            }
            Ok(())
        };

        match self {
            Self::AddDegree { pitch } | Self::RemoveDegree { pitch } => validate_degree(*pitch),
            Self::PutPiece { emoji, pitch } => {
                if emoji.is_empty() || emoji.len() > MAX_EMOJI_BYTES {
                    return Err(format!(
                        "piece emoji must contain 1..={MAX_EMOJI_BYTES} UTF-8 bytes"
                    ));
                }
                validate_periodic(*pitch)
            }
            Self::MovePiece { pitch, .. } => validate_periodic(*pitch),
            Self::SetTuning { definition } => definition
                .validate("signed room tuning")
                .map(|_| ())
                .map_err(|error| error.to_string()),
            Self::SetConfig {
                available_emojis: Some(emojis),
                ..
            } if emojis.len() > MAX_EMOJI_PALETTE_BYTES => Err(format!(
                "emoji palette exceeds {MAX_EMOJI_PALETTE_BYTES} UTF-8 bytes"
            )),
            _ => Ok(()),
        }
    }
}

/// The signed-op envelope: the exact struct CBOR-encoded into the p2panda `Body`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionedOp {
    pub version: u16,
    /// Author-stamped time in microseconds since the epoch (display/tiebreak-of-last-
    /// resort only; ordering is causal, never wall-clock).
    pub ts_micros: u64,
    /// The room topic this op is bound to, preventing replay into another room.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// The op ids this author had already accepted when signing — its causal horizon
    /// beyond its own log. The HHHS mirror lifts these into an
    /// entry's predecessors, which is what makes cross-author causality (add-wins
    /// supersession, register recency) expressible at all. Stamped from the store
    /// frontier on every commit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observed: Vec<[u8; 32]>,
    pub op: WalkieOp,
}

impl VersionedOp {
    pub fn current(op: WalkieOp, ts_micros: u64) -> Self {
        Self {
            version: OP_SCHEMA_VERSION,
            ts_micros,
            topic: None,
            observed: Vec::new(),
            op,
        }
    }

    pub fn current_for_topic(op: WalkieOp, ts_micros: u64, topic: &str) -> Self {
        Self {
            version: OP_SCHEMA_VERSION,
            ts_micros,
            topic: Some(topic.to_string()),
            observed: Vec::new(),
            op,
        }
    }

    /// Attach the causal horizon (op ids this author has observed).
    pub fn observing(mut self, observed: impl IntoIterator<Item = [u8; 32]>) -> Self {
        self.observed = observed.into_iter().collect();
        self
    }

    /// Whether this build can apply the op as-is (peer isn't ahead on schema).
    pub fn is_supported(&self) -> bool {
        self.version == OP_SCHEMA_VERSION
    }
}

/// The head of an author's op log: the seq_num the *next* op must carry and the hash
/// of the current head op (its backlink).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogHead {
    pub next_seq: u64,
    pub backlink: Option<[u8; 32]>,
}

impl LogHead {
    pub fn genesis() -> Self {
        Self {
            next_seq: 0,
            backlink: None,
        }
    }
}

impl Default for LogHead {
    fn default() -> Self {
        Self::genesis()
    }
}

/// A signed op on the wire: the exact bytes the author signed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedOp {
    /// CBOR of the signed `Header<()>`.
    pub header: Vec<u8>,
    /// CBOR of the [`VersionedOp`] (the p2panda `Body` bytes).
    pub payload: Vec<u8>,
}

impl SignedOp {
    /// Stable length-delimited gossip/persistence frame containing the verbatim
    /// header and payload bytes. Verification still happens after decoding.
    pub fn to_wire_bytes(&self) -> Result<Vec<u8>, SignedOpWireError> {
        validate_wire_lengths(self.header.len(), self.payload.len())?;
        let mut output = Vec::with_capacity(
            SIGNED_OP_WIRE_MAGIC.len() + 8 + self.header.len() + self.payload.len(),
        );
        output.extend_from_slice(SIGNED_OP_WIRE_MAGIC);
        output.extend_from_slice(&(self.header.len() as u32).to_le_bytes());
        output.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        output.extend_from_slice(&self.header);
        output.extend_from_slice(&self.payload);
        Ok(output)
    }

    pub fn from_wire_bytes(bytes: &[u8]) -> Result<Self, SignedOpWireError> {
        let prefix = SIGNED_OP_WIRE_MAGIC.len();
        if bytes.len() < prefix + 8 || &bytes[..prefix] != SIGNED_OP_WIRE_MAGIC {
            return Err(SignedOpWireError::InvalidMagic);
        }
        let header_len =
            u32::from_le_bytes(bytes[prefix..prefix + 4].try_into().expect("fixed slice")) as usize;
        let payload_len = u32::from_le_bytes(
            bytes[prefix + 4..prefix + 8]
                .try_into()
                .expect("fixed slice"),
        ) as usize;
        validate_wire_lengths(header_len, payload_len)?;
        let expected = prefix
            .checked_add(8)
            .and_then(|length| length.checked_add(header_len))
            .and_then(|length| length.checked_add(payload_len))
            .ok_or(SignedOpWireError::LengthMismatch)?;
        if bytes.len() != expected {
            return Err(SignedOpWireError::LengthMismatch);
        }
        let header_start = prefix + 8;
        let payload_start = header_start + header_len;
        Ok(Self {
            header: bytes[header_start..payload_start].to_vec(),
            payload: bytes[payload_start..].to_vec(),
        })
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SignedOpWireError {
    #[error("signed operation frame has an invalid generation marker")]
    InvalidMagic,
    #[error("signed operation frame lengths do not match its bytes")]
    LengthMismatch,
    #[error("signed header is {actual} bytes; maximum is {max}")]
    HeaderTooLarge { actual: usize, max: usize },
    #[error("signed payload is {actual} bytes; maximum is {max}")]
    PayloadTooLarge { actual: usize, max: usize },
}

fn validate_wire_lengths(header_len: usize, payload_len: usize) -> Result<(), SignedOpWireError> {
    if header_len > MAX_SIGNED_HEADER_BYTES {
        return Err(SignedOpWireError::HeaderTooLarge {
            actual: header_len,
            max: MAX_SIGNED_HEADER_BYTES,
        });
    }
    if payload_len > MAX_SIGNED_PAYLOAD_BYTES {
        return Err(SignedOpWireError::PayloadTooLarge {
            actual: payload_len,
            max: MAX_SIGNED_PAYLOAD_BYTES,
        });
    }
    Ok(())
}

/// A successfully verified op. Fields are **private**: the only constructor is
/// [`verify_signed_op`], so a store write that takes a `VerifiedOp` cannot be handed
/// unverified data.
#[derive(Debug, Clone)]
pub struct VerifiedOp {
    author: AuthorId,
    payload: WalkieOp,
    topic: Option<String>,
    observed: Vec<[u8; 32]>,
    timestamp_ms: u64,
    seq_num: u64,
    backlink: Option<[u8; 32]>,
    hash: [u8; 32],
    header_bytes: Vec<u8>,
    payload_bytes: Vec<u8>,
}

impl VerifiedOp {
    pub fn author(&self) -> AuthorId {
        self.author
    }
    pub fn payload(&self) -> &WalkieOp {
        &self.payload
    }
    pub fn topic(&self) -> Option<&str> {
        self.topic.as_deref()
    }
    /// The op ids this author had observed when signing (its causal horizon).
    pub fn observed(&self) -> &[[u8; 32]] {
        &self.observed
    }
    pub fn timestamp_ms(&self) -> u64 {
        self.timestamp_ms
    }
    pub fn seq_num(&self) -> u64 {
        self.seq_num
    }
    pub fn backlink(&self) -> Option<[u8; 32]> {
        self.backlink
    }
    /// The operation id (`blake3(header bytes)`).
    pub fn hash(&self) -> [u8; 32] {
        self.hash
    }
    /// This op's id as an [`OpId`] (stable, cross-peer).
    pub fn id(&self) -> OpId {
        OpId(self.hash)
    }
    /// The verbatim signed bytes, for durable persistence / rebroadcast / HHHS lift.
    pub fn signed(&self) -> SignedOp {
        SignedOp {
            header: self.header_bytes.clone(),
            payload: self.payload_bytes.clone(),
        }
    }
    /// The log head *after* this op — what the author's next op signs against.
    pub fn advanced_head(&self) -> LogHead {
        LogHead {
            next_seq: self.seq_num + 1,
            backlink: Some(self.hash),
        }
    }
}

/// Build a signing key from a 32-byte seed.
pub fn signing_key_from_seed(seed: &[u8; 32]) -> SigningKey {
    SigningKey::from_bytes(seed)
}

/// Sign one op bound to a room topic, stamping its observed causal horizon.
///
/// `observed` is the set of op ids this author had accepted at signing time (the store
/// frontier). It is load-bearing: the HHHS mirror lifts it into the entry's
/// predecessors so cross-author causality is expressible.
pub fn sign_op_for_topic_observing(
    signing_key: &SigningKey,
    head: &LogHead,
    ts_micros: u64,
    topic: &str,
    observed: impl IntoIterator<Item = [u8; 32]>,
    op: WalkieOp,
) -> (SignedOp, LogHead) {
    sign_versioned_op(
        signing_key,
        head,
        VersionedOp::current_for_topic(op, ts_micros, topic).observing(observed),
    )
}

/// Sign a topic-agnostic op with no observed horizon (tests / non-room-scoped uses).
pub fn sign_op(
    signing_key: &SigningKey,
    head: &LogHead,
    ts_micros: u64,
    op: WalkieOp,
) -> (SignedOp, LogHead) {
    sign_versioned_op(signing_key, head, VersionedOp::current(op, ts_micros))
}

/// The general signing primitive behind the helpers above.
pub fn sign_versioned_op(
    signing_key: &SigningKey,
    head: &LogHead,
    versioned: VersionedOp,
) -> (SignedOp, LogHead) {
    let payload = encode_cbor(&versioned).expect("VersionedOp is always CBOR-encodable");
    let body = Body::new(&payload);

    let mut header: Header<()> = Header {
        version: 1,
        verifying_key: signing_key.verifying_key(),
        signature: None,
        payload_size: body.size(),
        payload_hash: Some(body.hash()),
        // p2panda-core 0.7's `seq_num` is a `u32`; our `LogHead` keeps `u64`. Guard the
        // narrowing rather than silently truncating a pathological log.
        seq_num: u32::try_from(head.next_seq).expect("op-log seq_num exceeds u32::MAX"),
        backlink: head.backlink.map(Hash::from_bytes),
        extensions: (),
    };
    header.sign(signing_key);

    let hash = header.hash();
    let signed = SignedOp {
        header: header.to_bytes(),
        payload,
    };
    let advanced = LogHead {
        next_seq: head.next_seq + 1,
        backlink: Some(*hash.as_bytes()),
    };
    (signed, advanced)
}

/// Why a [`SignedOp`] failed verification.
#[derive(Debug)]
pub enum OpVerifyError {
    /// The header bytes did not CBOR-decode to a `Header<()>`.
    HeaderDecode(DecodeError),
    /// p2panda's `validate_operation` rejected it — bad signature, wrong version,
    /// payload hash/size mismatch, or a structural seq/backlink rule.
    Invalid(OperationError),
    /// The header verified, but the payload did not CBOR-decode to a [`VersionedOp`].
    PayloadDecode(DecodeError),
    /// A well-formed op from a peer on a newer schema than this build can apply.
    UnsupportedVersion(u16),
    /// The signed payload exceeds a configured resource bound.
    PayloadTooLarge { actual: usize, max: usize },
    /// The signature is valid, but the domain payload is invalid or excessive.
    InvalidDomain(String),
    /// A room-scoped read required a topic, but the signed body did not carry one.
    MissingTopic,
    /// The signed body belongs to another room topic.
    TopicMismatch { expected: String, actual: String },
}

impl std::fmt::Display for OpVerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpVerifyError::HeaderDecode(e) => write!(f, "header decode failed: {e}"),
            OpVerifyError::Invalid(e) => write!(f, "operation invalid: {e}"),
            OpVerifyError::PayloadDecode(e) => write!(f, "payload decode failed: {e}"),
            OpVerifyError::UnsupportedVersion(v) => write!(f, "unsupported op version: {v}"),
            OpVerifyError::PayloadTooLarge { actual, max } => {
                write!(f, "signed payload is {actual} bytes; maximum is {max}")
            }
            OpVerifyError::InvalidDomain(error) => {
                write!(f, "signed domain payload is invalid: {error}")
            }
            OpVerifyError::MissingTopic => write!(f, "signed operation is missing its room topic"),
            OpVerifyError::TopicMismatch { expected, actual } => {
                write!(
                    f,
                    "signed operation belongs to topic {actual}, expected {expected}"
                )
            }
        }
    }
}

impl std::error::Error for OpVerifyError {}

/// Verify a signed op. Pure: checks the signature and internal consistency, but NOT
/// log continuity against a stored head (that is store state — see [`LogHead`]). Run
/// identically at every peer's ingress.
pub fn verify_signed_op(signed: &SignedOp) -> Result<VerifiedOp, OpVerifyError> {
    if signed.payload.len() > MAX_SIGNED_PAYLOAD_BYTES {
        return Err(OpVerifyError::PayloadTooLarge {
            actual: signed.payload.len(),
            max: MAX_SIGNED_PAYLOAD_BYTES,
        });
    }
    let header: Header<()> =
        decode_cbor(signed.header.as_slice()).map_err(OpVerifyError::HeaderDecode)?;

    let hash = header.hash();
    let body = Body::from(signed.payload.clone());
    let operation = Operation {
        hash,
        header: header.clone(),
        body: Some(body),
    };
    validate_operation(&operation).map_err(OpVerifyError::Invalid)?;

    let versioned: VersionedOp =
        decode_cbor(signed.payload.as_slice()).map_err(OpVerifyError::PayloadDecode)?;
    if !versioned.is_supported() {
        return Err(OpVerifyError::UnsupportedVersion(versioned.version));
    }
    if versioned
        .topic
        .as_ref()
        .is_some_and(|topic| topic.len() > MAX_TOPIC_BYTES)
    {
        return Err(OpVerifyError::InvalidDomain(format!(
            "room topic exceeds {MAX_TOPIC_BYTES} UTF-8 bytes"
        )));
    }
    if versioned.observed.len() > MAX_OBSERVED_OPS {
        return Err(OpVerifyError::InvalidDomain(format!(
            "causal horizon exceeds {MAX_OBSERVED_OPS} operations"
        )));
    }
    versioned
        .op
        .validate_wire()
        .map_err(OpVerifyError::InvalidDomain)?;

    Ok(VerifiedOp {
        author: AuthorId(*header.verifying_key.as_bytes()),
        timestamp_ms: versioned.ts_micros / 1_000,
        payload: versioned.op,
        topic: versioned.topic,
        observed: versioned.observed,
        seq_num: header.seq_num as u64,
        backlink: header.backlink.map(|h| *h.as_bytes()),
        hash: *hash.as_bytes(),
        header_bytes: signed.header.clone(),
        payload_bytes: signed.payload.clone(),
    })
}

/// Verify a signed op and require it to be bound to `expected_topic`.
pub fn verify_signed_op_for_topic(
    signed: &SignedOp,
    expected_topic: &str,
) -> Result<VerifiedOp, OpVerifyError> {
    let verified = verify_signed_op(signed)?;
    match verified.topic() {
        None => Err(OpVerifyError::MissingTopic),
        Some(actual) if actual != expected_topic => Err(OpVerifyError::TopicMismatch {
            expected: expected_topic.to_string(),
            actual: actual.to_string(),
        }),
        Some(_) => Ok(verified),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tuning::{TunedDegree, TunedPeriodicPitch, Tuning};

    const SEED_A: [u8; 32] = [7u8; 32];
    const SEED_B: [u8; 32] = [9u8; 32];
    const TS: u64 = 1_700_000_000_000_000; // µs

    fn degree(index: u16) -> TunedDegree {
        TunedDegree::new(&Tuning::twelve_tet(), index).unwrap()
    }

    fn pitch(absolute: i32) -> TunedPeriodicPitch {
        let relative = absolute - 60;
        TunedPeriodicPitch::new(
            &Tuning::twelve_tet(),
            relative.rem_euclid(12) as u16,
            relative.div_euclid(12),
        )
        .unwrap()
    }

    #[test]
    fn sign_then_verify_round_trips() {
        let key = signing_key_from_seed(&SEED_A);
        let op = WalkieOp::AddDegree { pitch: degree(4) };
        let (signed, advanced) = sign_op(&key, &LogHead::genesis(), TS, op.clone());

        let verified = verify_signed_op(&signed).expect("valid op verifies");
        assert_eq!(verified.author(), AuthorId(*key.verifying_key().as_bytes()));
        assert_eq!(verified.payload(), &op);
        assert_eq!(verified.seq_num(), 0);
        assert_eq!(verified.backlink(), None);
        assert_eq!(verified.timestamp_ms(), TS / 1_000);
        assert_eq!(verified.id(), OpId(verified.hash()));
        assert_eq!(advanced.next_seq, 1);
        assert_eq!(advanced.backlink, Some(verified.hash()));
        assert_eq!(advanced, verified.advanced_head());
    }

    #[test]
    fn second_op_chains_onto_the_first() {
        let key = signing_key_from_seed(&SEED_A);
        let (_s0, head1) = sign_op(
            &key,
            &LogHead::genesis(),
            TS,
            WalkieOp::AddDegree { pitch: degree(0) },
        );
        let (signed1, _head2) = sign_op(
            &key,
            &head1,
            TS + 1_000,
            WalkieOp::AddDegree { pitch: degree(7) },
        );
        let v1 = verify_signed_op(&signed1).expect("chained op verifies");
        assert_eq!(v1.seq_num(), 1);
        assert_eq!(v1.backlink(), head1.backlink);
    }

    #[test]
    fn observed_horizon_round_trips() {
        let key = signing_key_from_seed(&SEED_A);
        let obs = [[1u8; 32], [2u8; 32]];
        let (signed, _) = sign_op_for_topic_observing(
            &key,
            &LogHead::genesis(),
            TS,
            "sunny-garden-melody",
            obs,
            WalkieOp::RemoveDegree { pitch: degree(4) },
        );
        let v = verify_signed_op(&signed).expect("verifies");
        assert_eq!(v.observed(), &obs);
        assert_eq!(v.topic(), Some("sunny-garden-melody"));
    }

    #[test]
    fn piece_ops_reference_an_op_id() {
        let key = signing_key_from_seed(&SEED_A);
        let (put, head1) = sign_op(
            &key,
            &LogHead::genesis(),
            TS,
            WalkieOp::PutPiece {
                emoji: "🌵".into(),
                pitch: pitch(60),
            },
        );
        let put = verify_signed_op(&put).unwrap();
        let move_op = WalkieOp::MovePiece {
            piece: put.id(),
            pitch: pitch(72),
        };
        let (mv, _) = sign_op(&key, &head1, TS + 1_000, move_op.clone());
        let mv = verify_signed_op(&mv).unwrap();
        assert_eq!(mv.payload(), &move_op);
    }

    #[test]
    fn topic_scoped_verification_rejects_missing_and_wrong_topics() {
        let key = signing_key_from_seed(&SEED_A);
        let topic = "sunny-garden-melody";
        let (scoped, _) = sign_op_for_topic_observing(
            &key,
            &LogHead::genesis(),
            TS,
            topic,
            [],
            WalkieOp::AddDegree { pitch: degree(7) },
        );
        let verified = verify_signed_op_for_topic(&scoped, topic).expect("matching topic verifies");
        assert_eq!(verified.topic(), Some(topic));
        assert!(matches!(
            verify_signed_op_for_topic(&scoped, "other-room"),
            Err(OpVerifyError::TopicMismatch { .. })
        ));

        let (topicless, _) = sign_op(
            &key,
            &LogHead::genesis(),
            TS,
            WalkieOp::AddDegree { pitch: degree(7) },
        );
        assert!(matches!(
            verify_signed_op_for_topic(&topicless, topic),
            Err(OpVerifyError::MissingTopic)
        ));
    }

    #[test]
    fn tampered_payload_fails_verification() {
        let key = signing_key_from_seed(&SEED_A);
        let (mut signed, _) = sign_op(
            &key,
            &LogHead::genesis(),
            TS,
            WalkieOp::AddDegree { pitch: degree(4) },
        );
        let last = signed.payload.len() - 1;
        signed.payload[last] ^= 0xff;
        let err = verify_signed_op(&signed).unwrap_err();
        assert!(
            matches!(err, OpVerifyError::Invalid(OperationError::PayloadMismatch)),
            "expected PayloadMismatch, got {err:?}"
        );
    }

    #[test]
    fn signature_from_the_wrong_key_fails_verification() {
        let key_a = signing_key_from_seed(&SEED_A);
        let key_b = signing_key_from_seed(&SEED_B);
        let (signed, _) = sign_op(
            &key_a,
            &LogHead::genesis(),
            TS,
            WalkieOp::SetConfig {
                pieces_locked: Some(true),
                available_emojis: None,
            },
        );

        let mut header: Header<()> = decode_cbor(signed.header.as_slice()).unwrap();
        header.verifying_key = key_b.verifying_key();
        let forged = SignedOp {
            header: header.to_bytes(),
            payload: signed.payload.clone(),
        };

        let err = verify_signed_op(&forged).unwrap_err();
        assert!(
            matches!(
                err,
                OpVerifyError::Invalid(OperationError::SignatureMismatch)
            ),
            "expected SignatureMismatch, got {err:?}"
        );
    }

    #[test]
    fn distinct_authors_have_distinct_ids() {
        let a = signing_key_from_seed(&SEED_A);
        let b = signing_key_from_seed(&SEED_B);
        let ida = AuthorId(*a.verifying_key().as_bytes());
        let idb = AuthorId(*b.verifying_key().as_bytes());
        assert_ne!(ida, idb);
        assert_eq!(ida.to_hex().len(), 64);
    }

    #[test]
    fn signed_wire_frame_round_trips_and_rejects_trailing_bytes() {
        let key = signing_key_from_seed(&SEED_A);
        let (signed, _) = sign_op(
            &key,
            &LogHead::genesis(),
            TS,
            WalkieOp::AddDegree { pitch: degree(4) },
        );
        let bytes = signed.to_wire_bytes().unwrap();
        assert_eq!(SignedOp::from_wire_bytes(&bytes).unwrap(), signed);

        let mut trailing = bytes;
        trailing.push(0);
        assert_eq!(
            SignedOp::from_wire_bytes(&trailing),
            Err(SignedOpWireError::LengthMismatch)
        );
    }
}
