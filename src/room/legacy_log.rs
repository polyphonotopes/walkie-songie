//! Pure validation/planning for the one-way pre-sealed browser log migration.

use hhhs_store::{MemoryStorage, ReplicaStorage, StorageRecoveryState, StorageTransaction};

pub(crate) const MAX_LEGACY_TRANSACTIONS: usize = 65_536;
pub(crate) const MAX_LEGACY_ENCODED_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub(crate) enum LegacyMigrationSource {
    TrustedRoot,
    Legacy(usize),
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct LegacyMigrationAppend {
    pub source: LegacyMigrationSource,
    pub expected_state: StorageRecoveryState,
}

pub(crate) fn decode_manifest_count(bytes: Option<&[u8]>) -> Result<usize, String> {
    let Some(bytes) = bytes else {
        return Ok(0);
    };
    let bytes: [u8; 8] = bytes
        .try_into()
        .map_err(|_| "invalid legacy Room-v5 replica manifest length")?;
    let count = usize::try_from(u64::from_le_bytes(bytes))
        .map_err(|_| "legacy Room-v5 manifest exceeds this browser's limits")?;
    if count > MAX_LEGACY_TRANSACTIONS {
        return Err(format!(
            "legacy Room-v5 manifest has {count} transactions; maximum is {MAX_LEGACY_TRANSACTIONS}"
        ));
    }
    Ok(count)
}

pub(crate) fn decode_legacy_row(
    sequence: usize,
    row: Option<Vec<u8>>,
    retained_bytes: &mut usize,
) -> Result<StorageTransaction, String> {
    let row = row.ok_or_else(|| format!("legacy Room-v5 transaction {sequence} is missing"))?;
    let next = retained_bytes
        .checked_add(row.len())
        .ok_or("legacy Room-v5 encoded-byte count overflow")?;
    if next > MAX_LEGACY_ENCODED_BYTES {
        return Err(format!(
            "legacy Room-v5 log exceeds its {}-byte migration budget",
            MAX_LEGACY_ENCODED_BYTES
        ));
    }
    *retained_bytes = next;
    hhhs_store::decode_storage_transaction(&row)
        .map_err(|error| format!("invalid legacy Room-v5 transaction {sequence}: {error}"))
}

pub(crate) fn require_stable_manifest(
    before: Option<&[u8]>,
    after: Option<&[u8]>,
) -> Result<(), String> {
    if before == after {
        Ok(())
    } else {
        Err("legacy Room-v5 manifest changed during sealed migration capture".into())
    }
}

/// Validate the complete legacy placement and decide which suffix, if any,
/// must be appended to the already checked new log.
pub(crate) fn plan_migration(
    trusted_root: &StorageTransaction,
    legacy: &[StorageTransaction],
    retained: &[StorageTransaction],
) -> Result<Vec<LegacyMigrationAppend>, String> {
    let replay = MemoryStorage::new();
    let mut appends = Vec::new();
    replay
        .commit(trusted_root.clone())
        .map_err(|error| format!("invalid trusted-root transaction: {error}"))?;
    if let Some(existing) = retained.first() {
        if hhhs_store::encode_storage_transaction(existing)
            != hhhs_store::encode_storage_transaction(trusted_root)
        {
            return Err("sealed Room-v5 log has the wrong trusted-root transaction".into());
        }
    } else {
        appends.push(LegacyMigrationAppend {
            source: LegacyMigrationSource::TrustedRoot,
            expected_state: replay.recovery_state(),
        });
    }
    let mut logical_index = 1_usize;
    for (physical_index, transaction) in legacy.iter().enumerate() {
        let before = replay.recovery_state();
        replay
            .commit(transaction.clone())
            .map_err(|error| format!("invalid legacy Room-v5 transaction: {error}"))?;
        let after = replay.recovery_state();
        if after == before {
            if physical_index + 1 != legacy.len() {
                return Err(format!(
                    "legacy Room-v5 log has a no-effect row before its tail at {physical_index}"
                ));
            }
            continue;
        }

        if let Some(existing) = retained.get(logical_index) {
            if hhhs_store::encode_storage_transaction(existing)
                != hhhs_store::encode_storage_transaction(transaction)
            {
                return Err(format!(
                    "sealed Room-v5 log diverges from legacy prefix at {logical_index}"
                ));
            }
        } else {
            appends.push(LegacyMigrationAppend {
                source: LegacyMigrationSource::Legacy(physical_index),
                expected_state: after,
            });
        }
        logical_index = logical_index
            .checked_add(1)
            .ok_or("legacy Room-v5 logical sequence overflow")?;
    }
    Ok(appends)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        room::v5::{
            ActorId, MusicOp, RoomIdentity, RoomLane, RoomReplicas, trusted_root_transaction,
        },
        tuning::{TunedDegree, Tuning},
    };
    use hhhs_proof::SigningKey;
    use hhhs_store::{SecretKey, SecretValue};

    fn put(sequence: u64, value: u8) -> StorageTransaction {
        let mut transaction = StorageTransaction::new();
        transaction.expect_sequence(sequence).put_secret(
            SecretKey::new("legacy-test").unwrap(),
            SecretValue::new(vec![value]).unwrap(),
        );
        transaction
    }

    #[test]
    fn empty_and_exact_prefix_need_no_append() {
        let root = put(0, 7);
        assert_eq!(plan_migration(&root, &[], &[]).unwrap().len(), 1);
        assert!(
            plan_migration(&root, &[], std::slice::from_ref(&root))
                .unwrap()
                .is_empty()
        );
        let legacy = [put(1, 1), put(2, 2)];
        let retained = [root.clone(), legacy[0].clone(), legacy[1].clone()];
        assert!(
            plan_migration(&root, &legacy, &retained)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn partial_prefix_resumes_with_exact_recovery_states() {
        let root = put(0, 7);
        let legacy = [put(1, 1), put(2, 2)];
        let retained = [root.clone(), legacy[0].clone()];
        let appends = plan_migration(&root, &legacy, &retained).unwrap();
        assert_eq!(appends.len(), 1);
        let replay =
            MemoryStorage::from_transactions([root, legacy[0].clone(), legacy[1].clone()].to_vec())
                .unwrap();
        assert!(matches!(
            appends[0].source,
            LegacyMigrationSource::Legacy(1)
        ));
        assert_eq!(appends[0].expected_state, replay.recovery_state());
    }

    #[test]
    fn multi_append_steps_carry_each_successive_expected_state() {
        let root = put(0, 7);
        let legacy = [put(1, 1), put(2, 2), put(3, 3)];
        let appends = plan_migration(&root, &legacy, &[]).unwrap();
        assert_eq!(appends.len(), 4);
        let replay = MemoryStorage::new();
        for append in &appends {
            let transaction = match append.source {
                LegacyMigrationSource::TrustedRoot => &root,
                LegacyMigrationSource::Legacy(index) => &legacy[index],
            };
            replay.commit(transaction.clone()).unwrap();
            assert_eq!(append.expected_state, replay.recovery_state());
        }
    }

    #[test]
    fn divergent_prefix_is_refused() {
        let root = put(0, 7);
        let error = plan_migration(&root, &[put(1, 1)], &[root.clone(), put(1, 9)]).unwrap_err();
        assert!(error.contains("diverges"));
    }

    #[test]
    fn root_mismatch_is_refused() {
        let error = plan_migration(&put(0, 7), &[put(1, 1)], &[put(0, 8)]).unwrap_err();
        assert!(error.contains("trusted-root"));
    }

    #[test]
    fn production_capability_root_and_authored_suffix_migrate_exactly() {
        let owner_key = SigningKey::from_bytes(&[0x41; 32]);
        let owner = ActorId::from_signing_key(&owner_key);
        let identity = RoomIdentity::from_name("legacy-production-fixture");
        let room = RoomReplicas::memory("legacy-production-fixture", owner).unwrap();
        let prepared = room
            .prepare_author(
                &owner_key,
                &room.owner_capabilities(),
                MusicOp::AddDegree {
                    degree: TunedDegree::new(&Tuning::twelve_tet(), 5).unwrap(),
                }
                .into(),
            )
            .unwrap();
        let root = trusted_root_transaction(&identity, owner, RoomLane::Music).unwrap();
        let legacy_suffix = [prepared.transaction().clone()];

        let complete = plan_migration(&root, &legacy_suffix, &[]).unwrap();
        assert!(matches!(
            complete[0].source,
            LegacyMigrationSource::TrustedRoot
        ));
        assert!(matches!(
            complete[1].source,
            LegacyMigrationSource::Legacy(0)
        ));

        let partial = plan_migration(&root, &legacy_suffix, std::slice::from_ref(&root)).unwrap();
        assert_eq!(partial.len(), 1);
        assert!(matches!(
            partial[0].source,
            LegacyMigrationSource::Legacy(0)
        ));
        let replay =
            MemoryStorage::from_transactions(vec![root, legacy_suffix[0].clone()]).unwrap();
        assert_eq!(partial[0].expected_state, replay.recovery_state());
    }

    #[test]
    fn post_migration_sealed_suffix_reopens_without_replaying_legacy() {
        let owner_key = SigningKey::from_bytes(&[0x42; 32]);
        let owner = ActorId::from_signing_key(&owner_key);
        let room_name = "legacy-post-migration-edit";
        let identity = RoomIdentity::from_name(room_name);
        let room = RoomReplicas::memory(room_name, owner).unwrap();
        let first = room
            .prepare_author(
                &owner_key,
                &room.owner_capabilities(),
                MusicOp::AddDegree {
                    degree: TunedDegree::new(&Tuning::twelve_tet(), 2).unwrap(),
                }
                .into(),
            )
            .unwrap();
        let first_transaction = first.transaction().clone();
        room.commit_prepared(first).unwrap();
        let post_migration = room
            .prepare_author(
                &owner_key,
                &room.owner_capabilities(),
                MusicOp::AddDegree {
                    degree: TunedDegree::new(&Tuning::twelve_tet(), 9).unwrap(),
                }
                .into(),
            )
            .unwrap();
        let post_migration_transaction = post_migration.transaction().clone();
        room.commit_prepared(post_migration).unwrap();

        let music_root = trusted_root_transaction(&identity, owner, RoomLane::Music).unwrap();
        let retained = vec![
            music_root.clone(),
            first_transaction.clone(),
            post_migration_transaction,
        ];
        assert!(
            plan_migration(&music_root, &[first_transaction], &retained)
                .unwrap()
                .is_empty()
        );
        let extension_root =
            trusted_root_transaction(&identity, owner, RoomLane::Extension).unwrap();
        let reopened =
            RoomReplicas::from_transaction_logs(identity, owner, retained, vec![extension_root])
                .unwrap();
        assert_eq!(reopened.view(), room.view());
        assert_eq!(
            hhhs_store::history_root(&reopened.music_snapshot().history),
            hhhs_store::history_root(&room.music_snapshot().history)
        );
    }

    #[test]
    fn terminal_no_effect_is_discarded_but_embedded_is_refused() {
        let root = put(0, 7);
        let first = put(1, 1);
        let duplicate = put(2, 1);
        assert_eq!(
            plan_migration(&root, &[first.clone(), duplicate.clone()], &[])
                .unwrap()
                .len(),
            2
        );
        let error = plan_migration(&root, &[first, duplicate, put(2, 2)], &[]).unwrap_err();
        assert!(error.contains("before its tail"));
    }

    #[test]
    fn manifest_gap_corruption_and_capture_change_are_refused() {
        assert!(decode_manifest_count(Some(&[1, 2])).is_err());
        assert!(
            decode_manifest_count(Some(
                &(u64::try_from(MAX_LEGACY_TRANSACTIONS).unwrap() + 1).to_le_bytes()
            ))
            .is_err()
        );
        assert!(
            decode_legacy_row(3, None, &mut 0)
                .unwrap_err()
                .contains("missing")
        );
        let mut retained = 0;
        assert!(decode_legacy_row(0, Some(vec![0xff]), &mut retained).is_err());
        assert!(
            require_stable_manifest(Some(&0_u64.to_le_bytes()), Some(&1_u64.to_le_bytes()))
                .is_err()
        );
    }

    #[test]
    fn cumulative_row_budget_is_enforced_before_decode() {
        let mut retained = MAX_LEGACY_ENCODED_BYTES;
        let error = decode_legacy_row(0, Some(vec![0]), &mut retained).unwrap_err();
        assert!(error.contains("migration budget"));
    }
}
