//! Signed, sequenced, leased voice preview frames.
//!
//! Presence is intentionally separate from the durable p2panda/HHHS log. A
//! crash or dropped clear frame therefore expires locally instead of leaving a
//! permanent sounding note in room history.

use p2panda_core::Signature;
use p2panda_core::cbor::{decode_cbor, encode_cbor};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::ops::{AuthorId, MAX_ABS_PERIOD, SigningKey, VerifyingKey};
use crate::tuning::{MAX_SCALE_DEGREES, TunedPeriodicPitch};

pub const PRESENCE_VERSION: u16 = 1;
pub const DEFAULT_PRESENCE_LEASE_MS: u32 = 1_500;
pub const MAX_PRESENCE_LEASE_MS: u32 = 5_000;
pub const MAX_PRESENCE_BODY_BYTES: usize = 8 * 1024;
const PRESENCE_WIRE_MAGIC: &[u8] = b"walkie.voice-presence/1\0";
pub const PRESENCE_VERSION_V4: u16 = 4;
pub const PRESENCE_WIRE_MAGIC_V4: &[u8] = b"walkie.voice-presence/4\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceBody {
    pub version: u16,
    pub topic: [u8; 32],
    pub session: u64,
    pub sequence: u64,
    pub issued_at_ms: u64,
    pub lease_ms: u32,
    pub pitch: Option<TunedPeriodicPitch>,
}

impl PresenceBody {
    pub fn new(
        topic: [u8; 32],
        session: u64,
        sequence: u64,
        issued_at_ms: u64,
        pitch: Option<TunedPeriodicPitch>,
    ) -> Self {
        Self {
            version: PRESENCE_VERSION,
            topic,
            session,
            sequence,
            issued_at_ms,
            lease_ms: DEFAULT_PRESENCE_LEASE_MS,
            pitch,
        }
    }

    /// Construct the same presence body under the room-v4 generation.
    pub fn new_v4(
        topic: [u8; 32],
        session: u64,
        sequence: u64,
        issued_at_ms: u64,
        pitch: Option<TunedPeriodicPitch>,
    ) -> Self {
        Self {
            version: PRESENCE_VERSION_V4,
            ..Self::new(topic, session, sequence, issued_at_ms, pitch)
        }
    }

    fn validate(&self) -> Result<(), PresenceError> {
        if self.version != PRESENCE_VERSION {
            return Err(PresenceError::UnsupportedVersion(self.version));
        }
        self.validate_domain()
    }

    fn validate_v4(&self) -> Result<(), PresenceError> {
        if self.version != PRESENCE_VERSION_V4 {
            return Err(PresenceError::UnsupportedVersion(self.version));
        }
        self.validate_domain()
    }

    fn validate_domain(&self) -> Result<(), PresenceError> {
        if self.session == 0 {
            return Err(PresenceError::InvalidDomain(
                "presence session must be non-zero".into(),
            ));
        }
        if !(1..=MAX_PRESENCE_LEASE_MS).contains(&self.lease_ms) {
            return Err(PresenceError::InvalidDomain(format!(
                "presence lease must be 1..={MAX_PRESENCE_LEASE_MS} ms"
            )));
        }
        if let Some(pitch) = self.pitch {
            if usize::from(pitch.pitch.degree().index()) >= MAX_SCALE_DEGREES {
                return Err(PresenceError::InvalidDomain(
                    "presence degree exceeds the supported bound".into(),
                ));
            }
            if pitch.pitch.period().unsigned_abs() > MAX_ABS_PERIOD as u32 {
                return Err(PresenceError::InvalidDomain(
                    "presence period exceeds the supported bound".into(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedPresence {
    pub author: AuthorId,
    pub signature: [u8; 64],
    body: Vec<u8>,
}

impl SignedPresence {
    pub fn sign(signing_key: &SigningKey, body: PresenceBody) -> Result<Self, PresenceError> {
        body.validate()?;
        let encoded =
            encode_cbor(&body).map_err(|error| PresenceError::Encode(error.to_string()))?;
        if encoded.len() > MAX_PRESENCE_BODY_BYTES {
            return Err(PresenceError::BodyTooLarge(encoded.len()));
        }
        Ok(Self {
            author: AuthorId(*signing_key.verifying_key().as_bytes()),
            signature: signing_key.sign(&encoded).to_bytes(),
            body: encoded,
        })
    }

    pub fn to_wire_bytes(&self) -> Result<Vec<u8>, PresenceError> {
        if self.body.len() > MAX_PRESENCE_BODY_BYTES {
            return Err(PresenceError::BodyTooLarge(self.body.len()));
        }
        let mut bytes =
            Vec::with_capacity(PRESENCE_WIRE_MAGIC.len() + 32 + 64 + 4 + self.body.len());
        bytes.extend_from_slice(PRESENCE_WIRE_MAGIC);
        bytes.extend_from_slice(&self.author.0);
        bytes.extend_from_slice(&self.signature);
        bytes.extend_from_slice(&(self.body.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&self.body);
        Ok(bytes)
    }

    pub fn from_wire_bytes(bytes: &[u8]) -> Result<Self, PresenceError> {
        let prefix = PRESENCE_WIRE_MAGIC.len();
        let fixed = prefix + 32 + 64 + 4;
        if bytes.len() < fixed || &bytes[..prefix] != PRESENCE_WIRE_MAGIC {
            return Err(PresenceError::InvalidMagic);
        }
        let mut author = [0_u8; 32];
        author.copy_from_slice(&bytes[prefix..prefix + 32]);
        let mut signature = [0_u8; 64];
        signature.copy_from_slice(&bytes[prefix + 32..prefix + 96]);
        let body_len =
            u32::from_le_bytes(bytes[prefix + 96..fixed].try_into().expect("fixed slice")) as usize;
        if body_len > MAX_PRESENCE_BODY_BYTES {
            return Err(PresenceError::BodyTooLarge(body_len));
        }
        if bytes.len() != fixed + body_len {
            return Err(PresenceError::LengthMismatch);
        }
        Ok(Self {
            author: AuthorId(author),
            signature,
            body: bytes[fixed..].to_vec(),
        })
    }

    pub fn verify(&self, expected_topic: [u8; 32]) -> Result<VerifiedPresence, PresenceError> {
        if self.body.len() > MAX_PRESENCE_BODY_BYTES {
            return Err(PresenceError::BodyTooLarge(self.body.len()));
        }
        let key = VerifyingKey::from_bytes(&self.author.0)
            .map_err(|error| PresenceError::InvalidAuthor(error.to_string()))?;
        let signature = Signature::from_bytes(&self.signature);
        if !key.verify(&self.body, &signature) {
            return Err(PresenceError::Signature);
        }
        let body: PresenceBody = decode_cbor(self.body.as_slice())
            .map_err(|error| PresenceError::Decode(error.to_string()))?;
        body.validate()?;
        if body.topic != expected_topic {
            return Err(PresenceError::TopicMismatch);
        }
        Ok(VerifiedPresence {
            author: self.author,
            body,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifiedPresence {
    pub author: AuthorId,
    pub body: PresenceBody,
}

/// The room-v4 presence codec. Its body fields and signing rules are unchanged;
/// version 4 and the `/4` magic keep presence outside both durable lanes while
/// preventing cross-generation replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedPresenceV4 {
    pub author: AuthorId,
    pub signature: [u8; 64],
    body: Vec<u8>,
}

impl SignedPresenceV4 {
    pub fn sign(signing_key: &SigningKey, body: PresenceBody) -> Result<Self, PresenceError> {
        body.validate_v4()?;
        let encoded =
            encode_cbor(&body).map_err(|error| PresenceError::Encode(error.to_string()))?;
        if encoded.len() > MAX_PRESENCE_BODY_BYTES {
            return Err(PresenceError::BodyTooLarge(encoded.len()));
        }
        Ok(Self {
            author: AuthorId(*signing_key.verifying_key().as_bytes()),
            signature: signing_key.sign(&encoded).to_bytes(),
            body: encoded,
        })
    }

    pub fn to_wire_bytes(&self) -> Result<Vec<u8>, PresenceError> {
        if self.body.len() > MAX_PRESENCE_BODY_BYTES {
            return Err(PresenceError::BodyTooLarge(self.body.len()));
        }
        let mut bytes =
            Vec::with_capacity(PRESENCE_WIRE_MAGIC_V4.len() + 32 + 64 + 4 + self.body.len());
        bytes.extend_from_slice(PRESENCE_WIRE_MAGIC_V4);
        bytes.extend_from_slice(&self.author.0);
        bytes.extend_from_slice(&self.signature);
        bytes.extend_from_slice(&(self.body.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&self.body);
        Ok(bytes)
    }

    pub fn from_wire_bytes(bytes: &[u8]) -> Result<Self, PresenceError> {
        let prefix = PRESENCE_WIRE_MAGIC_V4.len();
        let fixed = prefix + 32 + 64 + 4;
        if bytes.len() < fixed || &bytes[..prefix] != PRESENCE_WIRE_MAGIC_V4 {
            return Err(PresenceError::InvalidMagic);
        }
        let mut author = [0_u8; 32];
        author.copy_from_slice(&bytes[prefix..prefix + 32]);
        let mut signature = [0_u8; 64];
        signature.copy_from_slice(&bytes[prefix + 32..prefix + 96]);
        let body_len =
            u32::from_le_bytes(bytes[prefix + 96..fixed].try_into().expect("fixed slice")) as usize;
        if body_len > MAX_PRESENCE_BODY_BYTES {
            return Err(PresenceError::BodyTooLarge(body_len));
        }
        if bytes.len() != fixed + body_len {
            return Err(PresenceError::LengthMismatch);
        }
        Ok(Self {
            author: AuthorId(author),
            signature,
            body: bytes[fixed..].to_vec(),
        })
    }

    pub fn verify(&self, expected_topic: [u8; 32]) -> Result<VerifiedPresence, PresenceError> {
        if self.body.len() > MAX_PRESENCE_BODY_BYTES {
            return Err(PresenceError::BodyTooLarge(self.body.len()));
        }
        let key = VerifyingKey::from_bytes(&self.author.0)
            .map_err(|error| PresenceError::InvalidAuthor(error.to_string()))?;
        let signature = Signature::from_bytes(&self.signature);
        if !key.verify(&self.body, &signature) {
            return Err(PresenceError::Signature);
        }
        let body: PresenceBody = decode_cbor(self.body.as_slice())
            .map_err(|error| PresenceError::Decode(error.to_string()))?;
        body.validate_v4()?;
        if body.topic != expected_topic {
            return Err(PresenceError::TopicMismatch);
        }
        Ok(VerifiedPresence {
            author: self.author,
            body,
        })
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PresenceError {
    #[error("presence frame has an invalid generation marker")]
    InvalidMagic,
    #[error("presence frame lengths do not match its bytes")]
    LengthMismatch,
    #[error("presence body is {0} bytes; maximum is {MAX_PRESENCE_BODY_BYTES}")]
    BodyTooLarge(usize),
    #[error("could not encode presence: {0}")]
    Encode(String),
    #[error("could not decode presence: {0}")]
    Decode(String),
    #[error("presence author key is invalid: {0}")]
    InvalidAuthor(String),
    #[error("presence signature is invalid")]
    Signature,
    #[error("presence belongs to another room topic")]
    TopicMismatch,
    #[error("unsupported presence version {0}")]
    UnsupportedVersion(u16),
    #[error("presence payload is invalid: {0}")]
    InvalidDomain(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        room::ops::signing_key_from_seed,
        tuning::{TunedPeriodicPitch, Tuning},
    };

    #[test]
    fn signed_presence_round_trips_and_detects_tampering() {
        let key = signing_key_from_seed(&[31; 32]);
        let topic = [9; 32];
        let pitch = TunedPeriodicPitch::new(&Tuning::twelve_tet(), 4, 0).unwrap();
        let signed =
            SignedPresence::sign(&key, PresenceBody::new(topic, 7, 11, 1_000, Some(pitch)))
                .unwrap();
        let wire = signed.to_wire_bytes().unwrap();
        let decoded = SignedPresence::from_wire_bytes(&wire).unwrap();
        let verified = decoded.verify(topic).unwrap();
        assert_eq!(verified.author, AuthorId(*key.verifying_key().as_bytes()));
        assert_eq!(verified.body.pitch, Some(pitch));

        let mut tampered = wire;
        *tampered.last_mut().unwrap() ^= 1;
        let tampered = SignedPresence::from_wire_bytes(&tampered).unwrap();
        assert_eq!(tampered.verify(topic), Err(PresenceError::Signature));
    }

    #[test]
    fn presence_rejects_wrong_topic_and_unbounded_lease() {
        let key = signing_key_from_seed(&[32; 32]);
        let topic = [8; 32];
        let signed =
            SignedPresence::sign(&key, PresenceBody::new(topic, 1, 0, 1_000, None)).unwrap();
        assert_eq!(signed.verify([7; 32]), Err(PresenceError::TopicMismatch));

        let mut body = PresenceBody::new(topic, 1, 0, 1_000, None);
        body.lease_ms = MAX_PRESENCE_LEASE_MS + 1;
        assert!(matches!(
            SignedPresence::sign(&key, body),
            Err(PresenceError::InvalidDomain(_))
        ));
    }

    #[test]
    fn room_v4_presence_round_trips_and_rejects_v1_both_ways() {
        let key = signing_key_from_seed(&[33; 32]);
        let topic = [6; 32];
        let body_v4 = PresenceBody::new_v4(topic, 9, 12, 1_234, None);
        let signed_v4 = SignedPresenceV4::sign(&key, body_v4).unwrap();
        let wire_v4 = signed_v4.to_wire_bytes().unwrap();
        assert!(wire_v4.starts_with(PRESENCE_WIRE_MAGIC_V4));
        assert_eq!(
            SignedPresenceV4::from_wire_bytes(&wire_v4)
                .unwrap()
                .verify(topic)
                .unwrap()
                .body,
            body_v4,
        );

        let signed_v1 =
            SignedPresence::sign(&key, PresenceBody::new(topic, 9, 12, 1_234, None)).unwrap();
        let wire_v1 = signed_v1.to_wire_bytes().unwrap();
        assert_eq!(
            SignedPresenceV4::from_wire_bytes(&wire_v1),
            Err(PresenceError::InvalidMagic)
        );
        assert_eq!(
            SignedPresence::from_wire_bytes(&wire_v4),
            Err(PresenceError::InvalidMagic)
        );
        assert!(matches!(
            SignedPresenceV4::sign(&key, PresenceBody::new(topic, 9, 12, 1_234, None)),
            Err(PresenceError::UnsupportedVersion(1))
        ));
        assert!(matches!(
            SignedPresence::sign(&key, body_v4),
            Err(PresenceError::UnsupportedVersion(4))
        ));
    }
}
