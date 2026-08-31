//! Signed, bounded realtime messages for a Room-v5 gossip topic.
//!
//! These messages are deliberately outside HHHS history. The room object and
//! original Iroh identity are signed so a multi-hop gossip neighbor cannot
//! rewrite their origin, while a per-room session/sequence pair gives the host
//! a bounded replay filter. Durable musical meaning still travels as canonical
//! Replica records and repair frames.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use thiserror::Error;

use super::PeerId;
use crate::room::v5::{ROOM_PROTOCOL_GENERATION, RoomIdentity};

const DOMAIN: &[u8] = b"walkie room realtime v1\0";
const SIGNATURE_BYTES: usize = 64;
const FIXED_BYTES: usize = DOMAIN.len() + 4 + 32 + 32 + 8 + 8 + 2 + SIGNATURE_BYTES;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RoomRealtime {
    pub source: PeerId,
    pub session: u64,
    pub sequence: u64,
    pub frame: tutti_realtime::Frame,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Error)]
pub enum RoomRealtimeError {
    #[error("room realtime frame is malformed")]
    Malformed,
    #[error("room realtime frame belongs to another room or protocol generation")]
    WrongRoom,
    #[error("room realtime signature is invalid")]
    InvalidSignature,
}

impl RoomRealtime {
    pub fn encode(
        room: &RoomIdentity,
        signing_key: &SigningKey,
        session: u64,
        sequence: u64,
        frame: tutti_realtime::Frame,
    ) -> Result<Vec<u8>, RoomRealtimeError> {
        let payload = tutti_realtime::encode(frame).map_err(|_| RoomRealtimeError::Malformed)?;
        let payload = payload.as_bytes();
        let payload_len = u16::try_from(payload.len()).map_err(|_| RoomRealtimeError::Malformed)?;
        let source = signing_key.verifying_key().to_bytes();
        let mut bytes = Vec::with_capacity(FIXED_BYTES + payload.len());
        bytes.extend_from_slice(DOMAIN);
        bytes.extend_from_slice(&ROOM_PROTOCOL_GENERATION.to_le_bytes());
        bytes.extend_from_slice(room.object.as_bytes());
        bytes.extend_from_slice(&source);
        bytes.extend_from_slice(&session.to_le_bytes());
        bytes.extend_from_slice(&sequence.to_le_bytes());
        bytes.extend_from_slice(&payload_len.to_le_bytes());
        bytes.extend_from_slice(payload);
        let signature = signing_key.sign(&bytes);
        bytes.extend_from_slice(&signature.to_bytes());
        Ok(bytes)
    }

    pub fn decode(room: &RoomIdentity, bytes: &[u8]) -> Result<Self, RoomRealtimeError> {
        if bytes.len() < FIXED_BYTES || !bytes.starts_with(DOMAIN) {
            return Err(RoomRealtimeError::Malformed);
        }
        let mut cursor = DOMAIN.len();
        let generation = take_u32(bytes, &mut cursor)?;
        let object = take_array::<32>(bytes, &mut cursor)?;
        if generation != ROOM_PROTOCOL_GENERATION || object != *room.object.as_bytes() {
            return Err(RoomRealtimeError::WrongRoom);
        }
        let source = take_array::<32>(bytes, &mut cursor)?;
        let session = take_u64(bytes, &mut cursor)?;
        let sequence = take_u64(bytes, &mut cursor)?;
        let payload_len = usize::from(take_u16(bytes, &mut cursor)?);
        let signed_len = cursor
            .checked_add(payload_len)
            .ok_or(RoomRealtimeError::Malformed)?;
        if signed_len + SIGNATURE_BYTES != bytes.len() {
            return Err(RoomRealtimeError::Malformed);
        }
        let payload = &bytes[cursor..signed_len];
        let signature = Signature::from_bytes(
            &bytes[signed_len..]
                .try_into()
                .map_err(|_| RoomRealtimeError::Malformed)?,
        );
        let verifying_key =
            VerifyingKey::from_bytes(&source).map_err(|_| RoomRealtimeError::InvalidSignature)?;
        verifying_key
            .verify(&bytes[..signed_len], &signature)
            .map_err(|_| RoomRealtimeError::InvalidSignature)?;
        let frame = tutti_realtime::decode(payload).map_err(|_| RoomRealtimeError::Malformed)?;
        Ok(Self {
            source: PeerId(source),
            session,
            sequence,
            frame,
        })
    }
}

fn take_array<const N: usize>(
    bytes: &[u8],
    cursor: &mut usize,
) -> Result<[u8; N], RoomRealtimeError> {
    let end = cursor.checked_add(N).ok_or(RoomRealtimeError::Malformed)?;
    let value = bytes
        .get(*cursor..end)
        .ok_or(RoomRealtimeError::Malformed)?
        .try_into()
        .map_err(|_| RoomRealtimeError::Malformed)?;
    *cursor = end;
    Ok(value)
}

fn take_u16(bytes: &[u8], cursor: &mut usize) -> Result<u16, RoomRealtimeError> {
    Ok(u16::from_le_bytes(take_array(bytes, cursor)?))
}

fn take_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32, RoomRealtimeError> {
    Ok(u32::from_le_bytes(take_array(bytes, cursor)?))
}

fn take_u64(bytes: &[u8], cursor: &mut usize) -> Result<u64, RoomRealtimeError> {
    Ok(u64::from_le_bytes(take_array(bytes, cursor)?))
}

#[cfg(test)]
mod tests {
    use tutti_realtime::{Frame, MidiFrame, MidiKind};

    use super::*;
    use crate::room::v5::RoomIdentity;

    fn note() -> Frame {
        Frame::Midi(MidiFrame {
            voice_id: 7,
            channel: 2,
            note: 67,
            kind: MidiKind::NoteOn,
            value: 42_000,
        })
    }

    #[test]
    fn signed_room_realtime_round_trips_and_binds_room() {
        let room = RoomIdentity::from_name("bright-river-song");
        let key = SigningKey::from_bytes(&[7; 32]);
        let bytes = RoomRealtime::encode(&room, &key, 9, 11, note()).unwrap();
        let decoded = RoomRealtime::decode(&room, &bytes).unwrap();
        assert_eq!(decoded.source, PeerId(key.verifying_key().to_bytes()));
        assert_eq!(decoded.session, 9);
        assert_eq!(decoded.sequence, 11);
        assert_eq!(decoded.frame, note());
        assert_eq!(
            RoomRealtime::decode(&RoomIdentity::from_name("other-room-song"), &bytes),
            Err(RoomRealtimeError::WrongRoom)
        );
    }

    #[test]
    fn signed_room_realtime_rejects_tampering_and_trailing_bytes() {
        let room = RoomIdentity::from_name("bright-river-song");
        let key = SigningKey::from_bytes(&[7; 32]);
        let mut bytes = RoomRealtime::encode(&room, &key, 9, 11, note()).unwrap();
        let payload = bytes.len() - SIGNATURE_BYTES - 1;
        bytes[payload] ^= 1;
        assert_eq!(
            RoomRealtime::decode(&room, &bytes),
            Err(RoomRealtimeError::InvalidSignature)
        );
        let mut trailing = RoomRealtime::encode(&room, &key, 9, 11, note()).unwrap();
        trailing.push(0);
        assert_eq!(
            RoomRealtime::decode(&room, &trailing),
            Err(RoomRealtimeError::Malformed)
        );
    }
}
