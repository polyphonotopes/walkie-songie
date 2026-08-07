//! Crash-tolerant append-only storage for verbatim signed operations.

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use thiserror::Error;

use super::ops::{MAX_SIGNED_HEADER_BYTES, MAX_SIGNED_PAYLOAD_BYTES, SignedOp, SignedOpWireError};

const JOURNAL_MAGIC: &[u8] = b"walkie-songie/op-journal/3\n";
const MAX_RECORD_BYTES: usize = MAX_SIGNED_HEADER_BYTES + MAX_SIGNED_PAYLOAD_BYTES + 256;

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("operation journal I/O failed at {path}: {detail}")]
    Io { path: PathBuf, detail: String },
    #[error("operation journal has an incompatible generation marker")]
    InvalidMagic,
    #[error("operation journal record is {actual} bytes; maximum is {max}")]
    RecordTooLarge { actual: usize, max: usize },
    #[error("operation journal contains a malformed signed-operation record: {0}")]
    InvalidRecord(#[from] SignedOpWireError),
}

/// An append-only journal. A torn final record is truncated to its last
/// complete boundary on open; corruption inside a complete record is rejected.
pub struct FileOpJournal {
    path: PathBuf,
    file: File,
}

impl std::fmt::Debug for FileOpJournal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FileOpJournal")
            .field("path", &self.path)
            .finish()
    }
}

impl FileOpJournal {
    pub fn open(path: impl Into<PathBuf>) -> Result<(Self, Vec<SignedOp>), JournalError> {
        let path = path.into();
        ensure_private_parent(&path)?;
        let existed = path.exists();
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| io_error(&path, error))?;
        set_private_file_permissions(&path)?;

        if !existed
            || file
                .metadata()
                .map_err(|error| io_error(&path, error))?
                .len()
                == 0
        {
            file.write_all(JOURNAL_MAGIC)
                .map_err(|error| io_error(&path, error))?;
            file.sync_all().map_err(|error| io_error(&path, error))?;
            sync_parent(&path)?;
        }

        file.seek(SeekFrom::Start(0))
            .map_err(|error| io_error(&path, error))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| io_error(&path, error))?;
        if !bytes.starts_with(JOURNAL_MAGIC) {
            return Err(JournalError::InvalidMagic);
        }

        let mut records = Vec::new();
        let mut offset = JOURNAL_MAGIC.len();
        let mut complete_end = offset;
        while offset < bytes.len() {
            if bytes.len() - offset < 4 {
                break;
            }
            let length =
                u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("fixed length"))
                    as usize;
            if length > MAX_RECORD_BYTES {
                return Err(JournalError::RecordTooLarge {
                    actual: length,
                    max: MAX_RECORD_BYTES,
                });
            }
            let record_start = offset + 4;
            let Some(record_end) = record_start.checked_add(length) else {
                return Err(JournalError::RecordTooLarge {
                    actual: usize::MAX,
                    max: MAX_RECORD_BYTES,
                });
            };
            if record_end > bytes.len() {
                break;
            }
            records.push(SignedOp::from_wire_bytes(&bytes[record_start..record_end])?);
            offset = record_end;
            complete_end = record_end;
        }

        if complete_end != bytes.len() {
            file.set_len(complete_end as u64)
                .map_err(|error| io_error(&path, error))?;
            file.sync_all().map_err(|error| io_error(&path, error))?;
        }
        file.seek(SeekFrom::End(0))
            .map_err(|error| io_error(&path, error))?;
        Ok((Self { path, file }, records))
    }

    pub fn append(&mut self, signed: &SignedOp) -> Result<(), JournalError> {
        let bytes = signed.to_wire_bytes()?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(JournalError::RecordTooLarge {
                actual: bytes.len(),
                max: MAX_RECORD_BYTES,
            });
        }
        self.file
            .write_all(&(bytes.len() as u32).to_le_bytes())
            .and_then(|_| self.file.write_all(&bytes))
            .and_then(|_| self.file.sync_data())
            .map_err(|error| io_error(&self.path, error))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn ensure_private_parent(path: &Path) -> Result<(), JournalError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|error| io_error(parent, error))?;
    }
    Ok(())
}

fn set_private_file_permissions(path: &Path) -> Result<(), JournalError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| io_error(path, error))?;
    }
    Ok(())
}

fn sync_parent(path: &Path) -> Result<(), JournalError> {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| io_error(parent, error))?;
    }
    Ok(())
}

fn io_error(path: &Path, error: std::io::Error) -> JournalError {
    JournalError::Io {
        path: path.to_owned(),
        detail: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        room::{
            ops::{WalkieOp, signing_key_from_seed},
            store::RoomStore,
        },
        tuning::{TunedDegree, Tuning},
    };

    const TOPIC: &str = "journal-test-topic";

    fn temp_path(label: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("walkie-journal-{label}-{}", uuid::Uuid::new_v4()))
            .join("room.ops")
    }

    fn one_op() -> SignedOp {
        let tuning = Tuning::twelve_tet();
        let mut store = RoomStore::new();
        store.commit(
            &signing_key_from_seed(&[7; 32]),
            TOPIC,
            1,
            WalkieOp::AddDegree {
                pitch: TunedDegree::new(&tuning, 9).unwrap(),
            },
        )
    }

    #[test]
    fn append_reopen_preserves_verbatim_signed_bytes() {
        let path = temp_path("roundtrip");
        let signed = one_op();
        {
            let (mut journal, existing) = FileOpJournal::open(&path).unwrap();
            assert!(existing.is_empty());
            journal.append(&signed).unwrap();
        }
        let (_, existing) = FileOpJournal::open(&path).unwrap();
        assert_eq!(existing, vec![signed]);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(path.parent().unwrap());
    }

    #[test]
    fn torn_tail_is_truncated_to_last_complete_record() {
        let path = temp_path("torn");
        let signed = one_op();
        {
            let (mut journal, _) = FileOpJournal::open(&path).unwrap();
            journal.append(&signed).unwrap();
            journal.file.write_all(&123_u32.to_le_bytes()).unwrap();
            journal.file.write_all(b"partial").unwrap();
            journal.file.sync_all().unwrap();
        }
        let (journal, existing) = FileOpJournal::open(&path).unwrap();
        assert_eq!(existing, vec![signed]);
        let expected = JOURNAL_MAGIC.len() + 4 + existing[0].to_wire_bytes().unwrap().len();
        assert_eq!(fs::metadata(journal.path()).unwrap().len(), expected as u64);
        drop(journal);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(path.parent().unwrap());
    }
}
