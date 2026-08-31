//! Walkie-owned carrier adapter for HHHS 0.4 Replica repair.
//!
//! This module has no endpoint, discovery, or mesh actor. It only wraps the
//! application's existing framed stream and timer traits so the upstream HHHS
//! driver can pump a capability-validating `ReplicaRepairHost`.

use core::future::Future;

use hhhs::{Digest, Encoder, EntryHash, Position};
use hhhs_sync::{
    ConfirmedRepair, FrameStream, Lane, RepairFreshness, SessionLimits, SyncError,
    SyncTimer as HhhsSyncTimer, drive_initiator, drive_responder,
};

use super::{PeerId, SyncStream, SyncTimer, TransportError};
use crate::room::v5::{LANE_STRATEGY_VERSION, ROOM_PROTOCOL_GENERATION, RoomLane};

const REPAIR_HINT_DOMAIN: &[u8] = b"walkie replica repair hint v5\0";
const REPAIR_HINT_BYTES: usize = REPAIR_HINT_DOMAIN.len() + 4 + 1 + 32 + 32;
const REPAIR_PROBE_DOMAIN: &[u8] = b"walkie replica repair probe v5\0";
const REPAIR_PROBE_BYTES: usize = REPAIR_PROBE_DOMAIN.len() + 4 + 1 + 32 + 32;
const FRONTIER_DIGEST_DOMAIN: &[u8] = b"walkie replica frontier v5\0";
const LIVE_RECORD_DOMAIN: &[u8] = b"walkie replica live record v5\0";
const LIVE_RECORD_HEADER_BYTES: usize = LIVE_RECORD_DOMAIN.len() + 4 + 1 + 32;

/// The complete Room-v5 application protocol surface accepted by a Replica
/// carrier. Courier and source-log exchanges are intentionally absent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReplicaProtocol {
    Repair(RoomLane),
}

impl ReplicaProtocol {
    pub const fn alpn(self) -> &'static [u8] {
        match self {
            Self::Repair(lane) => lane.repair_alpn(),
        }
    }

    pub fn from_alpn(alpn: &[u8]) -> Option<Self> {
        [RoomLane::Music, RoomLane::Extension]
            .into_iter()
            .find(|lane| lane.repair_alpn() == alpn)
            .map(Self::Repair)
    }

    pub const fn lane(self) -> RoomLane {
        match self {
            Self::Repair(lane) => lane,
        }
    }
}

/// Small gossip wake-up. It cannot admit or authorize anything; recipients
/// authenticate the dialed endpoint at the carrier and repair through HHHS.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ReplicaRepairHint {
    pub lane: RoomLane,
    pub source: PeerId,
    pub entry: EntryHash,
}

/// Cheap periodic convergence probe. The frontier digest is only a wake-up
/// summary: a mismatch opens ordinary HHHS repair, while equality avoids
/// rebuilding a full reconciliation snapshot for already-synchronized peers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ReplicaRepairProbe {
    pub lane: RoomLane,
    pub source: PeerId,
    pub frontier: Digest,
}

/// Low-latency delivery of one opaque, public HHHS admission record.
///
/// This is a carrier optimization, never an admission shortcut: the receiver
/// decodes the `ReplicaRecord` and submits it to the same durable Replica path
/// used by repair. Missing causal closure falls back to ordinary repair.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ReplicaLiveRecord {
    pub lane: RoomLane,
    pub source: PeerId,
    pub record: hhhs_replica::ReplicaRecord,
}

/// Choose exactly one endpoint to initiate repair when both sides observe the
/// same symmetric trigger (neighbor-up, lag recovery, or a periodic sweep).
///
/// Repair itself is bidirectional, so the ordering has no authority meaning;
/// it only prevents the two hosts from opening competing sessions while each
/// lane's durable log is exclusively borrowed by one session.
pub fn is_routine_repair_initiator(local: PeerId, remote: PeerId) -> bool {
    local < remote
}

impl ReplicaRepairHint {
    pub fn encode(self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(REPAIR_HINT_BYTES);
        bytes.extend_from_slice(REPAIR_HINT_DOMAIN);
        bytes.extend_from_slice(&ROOM_PROTOCOL_GENERATION.to_le_bytes());
        bytes.push(self.lane.tag());
        bytes.extend_from_slice(self.source.as_bytes());
        bytes.extend_from_slice(self.entry.as_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != REPAIR_HINT_BYTES || !bytes.starts_with(REPAIR_HINT_DOMAIN) {
            return None;
        }
        let mut cursor = REPAIR_HINT_DOMAIN.len();
        let generation = u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().ok()?);
        cursor += 4;
        if generation != ROOM_PROTOCOL_GENERATION {
            return None;
        }
        let lane = match bytes[cursor] {
            tag if tag == RoomLane::Music.tag() => RoomLane::Music,
            tag if tag == RoomLane::Extension.tag() => RoomLane::Extension,
            _ => return None,
        };
        cursor += 1;
        let source = PeerId(bytes[cursor..cursor + 32].try_into().ok()?);
        cursor += 32;
        let entry = EntryHash(hhhs::Digest(bytes[cursor..cursor + 32].try_into().ok()?));
        Some(Self {
            lane,
            source,
            entry,
        })
    }
}

/// Digest the causally maximal entry hashes. Since every entry hash commits to
/// its predecessors and a valid Replica retains causal closure, equal frontier
/// sets identify equal reachable history without encoding every repair record.
pub fn replica_frontier_digest(frontier: &Position) -> Digest {
    let mut encoder = Encoder::new();
    encoder
        .bytes(FRONTIER_DIGEST_DOMAIN)
        .u64(frontier.0.len() as u64);
    for entry in &frontier.0 {
        encoder.digest(&entry.0);
    }
    encoder.digest_finish()
}

impl ReplicaRepairProbe {
    pub fn encode(self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(REPAIR_PROBE_BYTES);
        bytes.extend_from_slice(REPAIR_PROBE_DOMAIN);
        bytes.extend_from_slice(&ROOM_PROTOCOL_GENERATION.to_le_bytes());
        bytes.push(self.lane.tag());
        bytes.extend_from_slice(self.source.as_bytes());
        bytes.extend_from_slice(self.frontier.as_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != REPAIR_PROBE_BYTES || !bytes.starts_with(REPAIR_PROBE_DOMAIN) {
            return None;
        }
        let mut cursor = REPAIR_PROBE_DOMAIN.len();
        let generation = u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().ok()?);
        cursor += 4;
        if generation != ROOM_PROTOCOL_GENERATION {
            return None;
        }
        let lane = match bytes[cursor] {
            tag if tag == RoomLane::Music.tag() => RoomLane::Music,
            tag if tag == RoomLane::Extension.tag() => RoomLane::Extension,
            _ => return None,
        };
        cursor += 1;
        let source = PeerId(bytes[cursor..cursor + 32].try_into().ok()?);
        cursor += 32;
        let frontier = Digest(bytes[cursor..cursor + 32].try_into().ok()?);
        Some(Self {
            lane,
            source,
            frontier,
        })
    }
}

impl ReplicaLiveRecord {
    pub fn encode(&self) -> Vec<u8> {
        let record = self.record.encode();
        let mut bytes = Vec::with_capacity(LIVE_RECORD_HEADER_BYTES + record.len());
        bytes.extend_from_slice(LIVE_RECORD_DOMAIN);
        bytes.extend_from_slice(&ROOM_PROTOCOL_GENERATION.to_le_bytes());
        bytes.push(self.lane.tag());
        bytes.extend_from_slice(self.source.as_bytes());
        bytes.extend_from_slice(&record);
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() <= LIVE_RECORD_HEADER_BYTES || !bytes.starts_with(LIVE_RECORD_DOMAIN) {
            return None;
        }
        let mut cursor = LIVE_RECORD_DOMAIN.len();
        let generation = u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().ok()?);
        cursor += 4;
        if generation != ROOM_PROTOCOL_GENERATION {
            return None;
        }
        let lane = match bytes[cursor] {
            tag if tag == RoomLane::Music.tag() => RoomLane::Music,
            tag if tag == RoomLane::Extension.tag() => RoomLane::Extension,
            _ => return None,
        };
        cursor += 1;
        let source = PeerId(bytes[cursor..cursor + 32].try_into().ok()?);
        cursor += 32;
        let record = hhhs_replica::ReplicaRecord::decode(&bytes[cursor..]).ok()?;
        Some(Self {
            lane,
            source,
            record,
        })
    }
}

/// Local newtype required to adapt any walkie carrier without making HHHS know
/// about iroh, WebRTC, IPC, or loopback types.
pub struct ReplicaFrameStream<S>(pub S);

impl<S: SyncStream> FrameStream for ReplicaFrameStream<S> {
    type Error = TransportError;

    async fn send_frame(&mut self, frame: &[u8]) -> Result<(), Self::Error> {
        self.0.send_frame(frame).await
    }

    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, Self::Error> {
        self.0.recv_frame().await
    }

    async fn close(self) -> Result<(), Self::Error> {
        self.0.close().await
    }
}

/// Local clock adapter. Browser and native runtimes retain their own deadline
/// implementations.
pub struct ReplicaTimer<'a, T>(pub &'a T);

impl<T: SyncTimer> HhhsSyncTimer for ReplicaTimer<'_, T> {
    fn sleep(&self, duration: std::time::Duration) -> impl Future<Output = ()> {
        self.0.sleep(duration)
    }
}

pub fn repair_lane(lane: RoomLane) -> Lane {
    Lane::new(
        lane.repair_alpn(),
        lane.strategy_name(),
        LANE_STRATEGY_VERSION,
    )
}

pub async fn drive_replica_initiator<S, T, R>(
    stream: S,
    timer: &T,
    replica: &mut R,
    lane: RoomLane,
    limits: SessionLimits,
) -> Result<ConfirmedRepair, SyncError>
where
    S: SyncStream,
    T: SyncTimer,
    R: RepairFreshness,
    R::Snapshot: Sync,
{
    drive_initiator(
        ReplicaFrameStream(stream),
        &ReplicaTimer(timer),
        replica,
        &repair_lane(lane),
        limits,
    )
    .await
}

pub async fn drive_replica_responder<S, T, R>(
    stream: S,
    timer: &T,
    replica: &mut R,
    lane: RoomLane,
    limits: SessionLimits,
) -> Result<ConfirmedRepair, SyncError>
where
    S: SyncStream,
    T: SyncTimer,
    R: RepairFreshness,
    R::Snapshot: Sync,
{
    drive_responder(
        ReplicaFrameStream(stream),
        &ReplicaTimer(timer),
        replica,
        &repair_lane(lane),
        limits,
    )
    .await
}

#[cfg(test)]
mod tests {
    use async_channel::{Receiver, Sender, unbounded};
    use futures::{future::pending, join};
    use hhhs::DagRead;
    use hhhs_proof::SigningKey;
    use hhhs_sync::RepairHost;
    use tutti_music::{MusicOp, TunedDegree, Tuning};

    use super::*;
    use crate::room::v5::{ActorId, RoomReplicas};

    #[test]
    fn replica_protocol_hints_and_probes_are_strict_and_non_courier() {
        for lane in [RoomLane::Music, RoomLane::Extension] {
            let protocol = ReplicaProtocol::Repair(lane);
            assert_eq!(ReplicaProtocol::from_alpn(protocol.alpn()), Some(protocol));
            let hint = ReplicaRepairHint {
                lane,
                source: PeerId([7; 32]),
                entry: EntryHash(hhhs::Digest([8; 32])),
            };
            assert_eq!(ReplicaRepairHint::decode(&hint.encode()), Some(hint));
            let probe = ReplicaRepairProbe {
                lane,
                source: PeerId([7; 32]),
                frontier: Digest([9; 32]),
            };
            assert_eq!(ReplicaRepairProbe::decode(&probe.encode()), Some(probe));
        }
        assert_eq!(ReplicaProtocol::from_alpn(b"tutti/music/courier/1"), None);
        let hint = ReplicaRepairHint {
            lane: RoomLane::Music,
            source: PeerId([7; 32]),
            entry: EntryHash(hhhs::Digest([8; 32])),
        };
        let mut trailing = hint.encode();
        trailing.push(0);
        assert_eq!(ReplicaRepairHint::decode(&trailing), None);
        let probe = ReplicaRepairProbe {
            lane: RoomLane::Music,
            source: PeerId([7; 32]),
            frontier: Digest([9; 32]),
        };
        let mut trailing = probe.encode();
        trailing.push(0);
        assert_eq!(ReplicaRepairProbe::decode(&trailing), None);
    }

    #[test]
    fn frontier_probe_changes_exactly_when_canonical_history_changes() {
        let owner_key = SigningKey::from_bytes(&[3; 32]);
        let owner = ActorId::from_signing_key(&owner_key);
        let left = RoomReplicas::memory("bright-river-song", owner).unwrap();
        let right = RoomReplicas::memory("bright-river-song", owner).unwrap();

        let initial = replica_frontier_digest(&left.music_snapshot().history.frontier());
        assert_eq!(
            initial,
            replica_frontier_digest(&right.music_snapshot().history.frontier())
        );

        let degree = TunedDegree::new(&Tuning::twelve_tet(), 2).unwrap();
        left.author(
            &owner_key,
            &left.owner_capabilities(),
            MusicOp::AddDegree { degree }.into(),
        )
        .unwrap();
        assert_ne!(
            replica_frontier_digest(&left.music_snapshot().history.frontier()),
            replica_frontier_digest(&right.music_snapshot().history.frontier())
        );
    }

    #[test]
    fn routine_repair_has_exactly_one_initiator() {
        let lower = PeerId([1; 32]);
        let higher = PeerId([2; 32]);

        assert!(is_routine_repair_initiator(lower, higher));
        assert!(!is_routine_repair_initiator(higher, lower));
        assert!(!is_routine_repair_initiator(lower, lower));
    }

    #[test]
    fn live_record_round_trips_as_opaque_admission_material() {
        let owner_key = SigningKey::from_bytes(&[3; 32]);
        let owner = ActorId::from_signing_key(&owner_key);
        let room = RoomReplicas::memory("bright-river-song", owner).unwrap();
        let degree = TunedDegree::new(&Tuning::twelve_tet(), 2).unwrap();
        let prepared = room
            .prepare_author(
                &owner_key,
                &room.owner_capabilities(),
                MusicOp::AddDegree { degree }.into(),
            )
            .unwrap();
        let live = ReplicaLiveRecord {
            lane: RoomLane::Music,
            source: PeerId([9; 32]),
            record: prepared.replica_record(),
        };

        assert_eq!(ReplicaLiveRecord::decode(&live.encode()), Some(live));
    }

    #[test]
    fn live_record_uses_the_same_capability_admission_as_repair() {
        futures::executor::block_on(async {
            let owner_key = SigningKey::from_bytes(&[4; 32]);
            let owner = ActorId::from_signing_key(&owner_key);
            let source = RoomReplicas::memory("bright-river-song", owner).unwrap();
            let target = RoomReplicas::memory("bright-river-song", owner).unwrap();
            let degree = TunedDegree::new(&Tuning::twelve_tet(), 5).unwrap();
            let prepared = source
                .prepare_author(
                    &owner_key,
                    &source.owner_capabilities(),
                    MusicOp::AddDegree { degree }.into(),
                )
                .unwrap();
            let record = prepared.replica_record();
            let entry = record.entry_hash();
            source.commit_prepared(prepared).unwrap();

            let mut target_host = target.music_repair_host();
            let report = RepairHost::apply(&mut target_host, &[(entry, record.encode())])
                .await
                .unwrap();

            assert_eq!(report.admitted, vec![entry]);
            assert!(report.refused.is_empty());
            assert!(target.view().music.live.contains(&degree));
        });
    }

    struct TestStream {
        send: Sender<Vec<u8>>,
        receive: Receiver<Vec<u8>>,
    }

    fn stream_pair() -> (TestStream, TestStream) {
        let (left_send, right_receive) = unbounded();
        let (right_send, left_receive) = unbounded();
        (
            TestStream {
                send: left_send,
                receive: left_receive,
            },
            TestStream {
                send: right_send,
                receive: right_receive,
            },
        )
    }

    impl SyncStream for TestStream {
        async fn send_frame(&mut self, frame: &[u8]) -> Result<(), TransportError> {
            self.send
                .send(frame.to_vec())
                .await
                .map_err(|_| TransportError::Closed)
        }

        async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
            Ok(self.receive.recv().await.ok())
        }

        async fn close(self) -> Result<(), TransportError> {
            self.send.close();
            self.receive.close();
            Ok(())
        }
    }

    struct NoDeadline;

    impl SyncTimer for NoDeadline {
        async fn sleep(&self, _duration: std::time::Duration) {
            pending::<()>().await;
        }
    }

    #[test]
    fn upstream_driver_repairs_a_capability_native_music_lane() {
        futures::executor::block_on(async {
            let owner_key = SigningKey::from_bytes(&[1; 32]);
            let owner = ActorId::from_signing_key(&owner_key);
            let left = RoomReplicas::memory("bright-river-song", owner).unwrap();
            let right = RoomReplicas::memory("bright-river-song", owner).unwrap();
            let degree = TunedDegree::new(&Tuning::twelve_tet(), 7).unwrap();
            left.author(
                &owner_key,
                &left.owner_capabilities(),
                MusicOp::AddDegree { degree }.into(),
            )
            .unwrap();

            let (initiator_stream, responder_stream) = stream_pair();
            let mut initiator = left.music_repair_host();
            let mut responder = right.music_repair_host();
            let (dial, accept) = join!(
                drive_replica_initiator(
                    initiator_stream,
                    &NoDeadline,
                    &mut initiator,
                    RoomLane::Music,
                    SessionLimits::default(),
                ),
                drive_replica_responder(
                    responder_stream,
                    &NoDeadline,
                    &mut responder,
                    RoomLane::Music,
                    SessionLimits::default(),
                )
            );
            assert_eq!(
                dial.unwrap().disposition(),
                hhhs_sync::RepairDisposition::Synchronized
            );
            assert_eq!(
                accept.unwrap().disposition(),
                hhhs_sync::RepairDisposition::Synchronized
            );
            let left_hashes: std::collections::BTreeSet<_> = left
                .music_snapshot()
                .history
                .all_hashes()
                .into_iter()
                .collect();
            let right_hashes: std::collections::BTreeSet<_> = right
                .music_snapshot()
                .history
                .all_hashes()
                .into_iter()
                .collect();
            assert_eq!(left_hashes, right_hashes);
            assert!(right.view().music.live.contains(&degree));
            assert_eq!(right.extension_snapshot().history.all_hashes().len(), 1);
        });
    }
}
