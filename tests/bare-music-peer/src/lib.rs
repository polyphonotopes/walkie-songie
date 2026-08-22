//! Independent capability-native tutti-music Replica used by interoperability
//! tests. This package intentionally has no `walkie-songie` dependency.
//!
//! The peer shares only [`tutti_music_hhhs`]'s canonical music command,
//! admission, materialization, and repair identifiers. A carrier is supplied by
//! the embedding application through HHHS's frame seam; this crate owns no
//! endpoint, discovery, mesh, or extension-lane code.

use hhhs::{DagRead, Digest, EntryHash};
use hhhs_proof::SigningKey;
use hhhs_replica::{ReplicaError, ReplicaRepairHost};
use hhhs_store::MemoryStorage;
use hhhs_sync::{
    FrameStream, Lane, SessionLimits, SessionOutcome, SyncError, SyncTimer, drive_initiator,
};
use tutti_music::MusicOp;
use tutti_music_hhhs::{ActorId, MusicReplica, MusicView};

pub fn music_lane() -> Lane {
    Lane::new(
        tutti_music_hhhs::REPAIR_ALPN,
        tutti_music_hhhs::STRATEGY_NAME,
        tutti_music_hhhs::STRATEGY_VERSION,
    )
}

/// A music-only HHHS Replica. It can be per task, process, or device; nothing
/// here assumes a particular runtime or transport.
pub struct BareMusicPeer {
    namespace: Digest,
    root: EntryHash,
    key: SigningKey,
    replica: MusicReplica<MemoryStorage>,
}

impl BareMusicPeer {
    pub fn new(namespace: Digest, owner: ActorId, seed: [u8; 32]) -> Result<Self, ReplicaError> {
        let key = SigningKey::from_bytes(&seed);
        let (replica, root) = tutti_music_hhhs::initialize(namespace, owner, MemoryStorage::new())?;
        Ok(Self {
            namespace,
            root,
            key,
            replica,
        })
    }

    pub fn actor(&self) -> ActorId {
        ActorId::from_signing_key(&self.key)
    }

    pub const fn root(&self) -> EntryHash {
        self.root
    }

    pub fn entry_hashes(&self) -> Vec<EntryHash> {
        self.replica.snapshot().history.all_hashes()
    }

    pub fn view(&self) -> MusicView {
        tutti_music_hhhs::materialize(&self.replica.snapshot().history, &[self.root])
    }

    pub fn author(
        &self,
        presented: Vec<EntryHash>,
        command: MusicOp,
    ) -> Result<EntryHash, ReplicaError> {
        tutti_music_hhhs::author(&self.replica, self.namespace, &self.key, presented, command)
            .map(|outcome| outcome.entry)
    }

    pub fn repair_host(
        &self,
    ) -> ReplicaRepairHost<MemoryStorage, tutti_music_hhhs::MusicAdmissionPolicy> {
        ReplicaRepairHost::new(self.replica.clone())
    }

    pub async fn drive_music_initiator<S, T>(
        &self,
        stream: S,
        timer: &T,
        limits: SessionLimits,
    ) -> Result<SessionOutcome, SyncError>
    where
        S: FrameStream,
        T: SyncTimer,
    {
        let mut host = self.repair_host();
        drive_initiator(stream, timer, &mut host, &music_lane(), limits).await
    }
}

#[cfg(test)]
mod tests {
    use tutti_music::{MusicOp, TunedDegree, Tuning};

    use super::*;

    #[test]
    fn package_is_a_real_standalone_music_replica() {
        let owner_key = SigningKey::from_bytes(&[1; 32]);
        let owner = ActorId::from_signing_key(&owner_key);
        let namespace = Digest::of(b"bare music peer test");
        let peer = BareMusicPeer::new(namespace, owner, [1; 32]).unwrap();
        let degree = TunedDegree::new(&Tuning::twelve_tet(), 3).unwrap();
        peer.author(vec![peer.root()], MusicOp::AddDegree { degree })
            .unwrap();
        assert_eq!(peer.view().live, [degree].into());
    }
}
