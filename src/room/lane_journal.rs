//! v4 lane-tagged op-journal codecs. The generation marker and storage key
//! keep them disjoint from v3 journals.
//!
//! Both v4 journals persist the SAME record spelling,
//! `[lane:u8][wire_len:u32le][verbatim L::wire]`, differing only in container:
//!
//! * **Native file journal** ([`FileLaneJournal`]): magic
//!   [`OP_JOURNAL_MAGIC_V4`] then records, appended with an fsync per record.
//!   The v3 [`FileOpJournal`](crate::room::journal::FileOpJournal) (magic
//!   `.../3\n`, untagged records) is untouched; each opener refuses the
//!   other's magic.
//! * **Browser IndexedDB journal**: one blob under the key
//!   [`idb_op_journal_key_v4`] starting with [`IDB_OP_JOURNAL_MAGIC_V4`],
//!   then the same records. The v3 blob (key `opjournal:{topic}`, no marker)
//!   is never read as v4 — [`decode_idb_op_journal_v4`] refuses any blob
//!   without the marker, and the v4 key is disjoint, so there is no fallback
//!   path to fall down. The codec here is pure and proven natively; the
//!   IndexedDB RUNTIME (get/put, reload survival) is browser-only and lives
//!   behind `web::storage`.
//!
//! Failure discipline (both containers): a TORN FINAL record truncates to the
//! last complete boundary; a COMPLETE record with an unknown lane tag, a
//! tag/wire-magic mismatch, or an over-limit length is corruption and errors.

use std::collections::BTreeSet;

use tutti_core::SignedOp;

use crate::room::ops::{MAX_SIGNED_HEADER_BYTES, MAX_SIGNED_PAYLOAD_BYTES, OpId};
use crate::room::v4::{ExtensionLang, LaneRecord, MusicLang, RoomLane};

/// v4 native file-journal generation marker. The v3 marker is
/// `b"walkie-songie/op-journal/3\n"`; each generation's opener refuses the
/// other's.
pub const OP_JOURNAL_MAGIC_V4: &[u8] = b"walkie-songie/op-journal/4\n";

/// v4 browser journal-blob marker. The v3 blob has NO marker (bare length
/// records), so any v3 blob fails this check — by design, never a fallback.
pub const IDB_OP_JOURNAL_MAGIC_V4: &[u8] = b"walkie-songie/idb-op-journal/4\0";

/// The settings-store key holding a room's v4 journal blob. Disjoint from the
/// v3 key (`opjournal:{topic}`), which v4 code never reads.
pub fn idb_op_journal_key_v4(topic_hex: &str) -> String {
    format!("opjournal:v4:{topic_hex}")
}

/// Same record ceiling as the v3 journal: the largest legal signed wire plus
/// slack. A length field above this is corruption, not a big record.
pub const MAX_LANE_RECORD_BYTES: usize = MAX_SIGNED_HEADER_BYTES + MAX_SIGNED_PAYLOAD_BYTES + 256;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LaneJournalError {
    #[cfg(not(target_arch = "wasm32"))]
    #[error("lane journal I/O failed at {path}: {detail}")]
    Io {
        path: std::path::PathBuf,
        detail: String,
    },
    #[error("lane journal has an incompatible generation marker")]
    InvalidMagic,
    #[error("lane journal record carries unknown lane tag {0:#04x}")]
    UnknownLaneTag(u8),
    #[error("lane journal {lane:?} record does not start with its lane's wire magic")]
    LaneMagicMismatch { lane: RoomLane },
    #[error("lane journal {lane:?} record is malformed: {detail}")]
    InvalidRecord { lane: RoomLane, detail: String },
    #[error("lane journal record is {actual} bytes; maximum is {max}")]
    RecordTooLarge { actual: usize, max: usize },
}

/// Frame one record onto `out`: `[lane:u8][wire_len:u32le][wire]`.
fn encode_lane_record(out: &mut Vec<u8>, lane: RoomLane, wire: &[u8]) {
    out.push(lane.tag());
    out.extend_from_slice(&(wire.len() as u32).to_le_bytes());
    out.extend_from_slice(wire);
}

/// Check one complete record's lane discipline: known tag, and the wire must
/// carry that lane's magic (a music frame can never sit under an extension
/// tag). Shared by parsing and appending, so a journal can neither write nor
/// read a cross-tagged record.
fn check_lane_record(tag: u8, wire: &[u8]) -> Result<RoomLane, LaneJournalError> {
    let lane = RoomLane::from_tag(tag).ok_or(LaneJournalError::UnknownLaneTag(tag))?;
    if wire.len() > MAX_LANE_RECORD_BYTES {
        return Err(LaneJournalError::RecordTooLarge {
            actual: wire.len(),
            max: MAX_LANE_RECORD_BYTES,
        });
    }
    if !wire.starts_with(lane.wire_magic()) {
        return Err(LaneJournalError::LaneMagicMismatch { lane });
    }
    let valid = match lane {
        RoomLane::Music => SignedOp::from_wire_bytes_in::<MusicLang>(wire).map(|_| ()),
        RoomLane::Extension => SignedOp::from_wire_bytes_in::<ExtensionLang>(wire).map(|_| ()),
    };
    if let Err(error) = valid {
        return Err(LaneJournalError::InvalidRecord {
            lane,
            detail: error.to_string(),
        });
    }
    Ok(lane)
}

/// Parse `[lane:u8][wire_len:u32le][wire]…` from `bytes[offset..]`.
///
/// Returns the complete records plus the offset one past the last complete
/// record (the truncation point for a torn tail). A torn FINAL record stops
/// parsing; a COMPLETE record that is malformed (unknown tag, tag/magic
/// mismatch, over-limit length) errors.
fn parse_lane_records(
    bytes: &[u8],
    mut offset: usize,
) -> Result<(Vec<LaneRecord>, usize), LaneJournalError> {
    let mut records = Vec::new();
    let mut complete_end = offset;
    while offset < bytes.len() {
        if bytes.len() - offset < 5 {
            break; // torn header
        }
        let tag = bytes[offset];
        let length = u32::from_le_bytes(
            bytes[offset + 1..offset + 5]
                .try_into()
                .expect("fixed slice"),
        ) as usize;
        if length > MAX_LANE_RECORD_BYTES {
            return Err(LaneJournalError::RecordTooLarge {
                actual: length,
                max: MAX_LANE_RECORD_BYTES,
            });
        }
        let start = offset + 5;
        let Some(end) = start.checked_add(length) else {
            return Err(LaneJournalError::RecordTooLarge {
                actual: usize::MAX,
                max: MAX_LANE_RECORD_BYTES,
            });
        };
        if end > bytes.len() {
            break; // torn payload
        }
        let wire = &bytes[start..end];
        let lane = check_lane_record(tag, wire)?;
        records.push(LaneRecord {
            lane,
            wire: wire.to_vec(),
        });
        offset = end;
        complete_end = end;
    }
    Ok((records, complete_end))
}

// ---------------------------------------------------------------------------
// Browser (IndexedDB) blob codec — pure, target-independent, proven natively.
// ---------------------------------------------------------------------------

/// Frame lane records into one v4 journal blob: the marker, then records.
/// Every record is validated before any bytes are returned, so callers cannot
/// persist a cross-tagged, malformed, or over-limit frame through this codec.
pub fn encode_idb_op_journal_v4(records: &[LaneRecord]) -> Result<Vec<u8>, LaneJournalError> {
    let total: usize = IDB_OP_JOURNAL_MAGIC_V4.len()
        + records
            .iter()
            .map(|record| record.wire.len() + 5)
            .sum::<usize>();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(IDB_OP_JOURNAL_MAGIC_V4);
    for record in records {
        check_lane_record(record.lane.tag(), &record.wire)?;
        encode_lane_record(&mut out, record.lane, &record.wire);
    }
    Ok(out)
}

/// Recover lane records from a v4 journal blob. A blob without the v4 marker
/// — including every v3 blob, which has none — is refused outright: there is
/// deliberately NO fallback decode of the old spelling. A torn tail returns
/// the completed prefix; a malformed complete record errors.
pub fn decode_idb_op_journal_v4(bytes: &[u8]) -> Result<Vec<LaneRecord>, LaneJournalError> {
    if !bytes.starts_with(IDB_OP_JOURNAL_MAGIC_V4) {
        return Err(LaneJournalError::InvalidMagic);
    }
    let (records, _) = parse_lane_records(bytes, IDB_OP_JOURNAL_MAGIC_V4.len())?;
    Ok(records)
}

/// The browser room journal, v4 shape: lane-tagged records plus the
/// `(lane, OpId)` dedup set that keeps one admitted op from being journaled
/// twice.
#[derive(Debug, Default)]
pub struct RoomJournalV4 {
    known: BTreeSet<(RoomLane, OpId)>,
    records: Vec<LaneRecord>,
}

impl RoomJournalV4 {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adopt already-decoded records (a fresh load). `known` starts empty —
    /// the host marks each op via [`RoomJournalV4::admit`] as it verifies and
    /// seeds it, so the dedup set only ever names ops the store accepted.
    pub fn from_records(records: Vec<LaneRecord>) -> Result<Self, LaneJournalError> {
        for record in &records {
            check_lane_record(record.lane.tag(), &record.wire)?;
        }
        Ok(Self {
            known: BTreeSet::new(),
            records,
        })
    }

    /// Record one admitted op's verbatim wire under its lane. Returns `false`
    /// (and appends nothing) if `(lane, id)` was already journaled. A wire
    /// frame that does not fully decode in the claimed lane is refused before
    /// the dedup set or record list can be mutated.
    pub fn admit(
        &mut self,
        lane: RoomLane,
        id: OpId,
        wire: &[u8],
    ) -> Result<bool, LaneJournalError> {
        check_lane_record(lane.tag(), wire)?;
        if !self.known.insert((lane, id)) {
            return Ok(false);
        }
        self.records.push(LaneRecord {
            lane,
            wire: wire.to_vec(),
        });
        Ok(true)
    }

    /// Mark an op as known WITHOUT appending — for records already present at
    /// load time, so a reloaded journal does not re-append its own contents.
    pub fn mark_known(&mut self, lane: RoomLane, id: OpId) -> bool {
        self.known.insert((lane, id))
    }

    pub fn contains(&self, lane: RoomLane, id: OpId) -> bool {
        self.known.contains(&(lane, id))
    }

    pub fn records(&self) -> &[LaneRecord] {
        &self.records
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// The persisted blob (marker + records).
    pub fn encode(&self) -> Result<Vec<u8>, LaneJournalError> {
        encode_idb_op_journal_v4(&self.records)
    }
}

// ---------------------------------------------------------------------------
// Native file journal.
// ---------------------------------------------------------------------------

/// The v4 append-only lane journal: crash-tolerant storage for verbatim lane
/// wire frames, fsynced per append. Mirrors the v3
/// [`FileOpJournal`](crate::room::journal::FileOpJournal)'s file discipline
/// (private parent/file permissions, magic-then-records, torn-tail
/// truncation) under the v4 magic and lane-tagged records.
#[cfg(not(target_arch = "wasm32"))]
pub struct FileLaneJournal {
    path: std::path::PathBuf,
    file: std::fs::File,
}

#[cfg(not(target_arch = "wasm32"))]
impl std::fmt::Debug for FileLaneJournal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FileLaneJournal")
            .field("path", &self.path)
            .finish()
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl FileLaneJournal {
    /// Open (creating if absent), returning every complete lane record. A torn
    /// final record is truncated away; a malformed complete record — or a v3
    /// (or any foreign) generation marker — is an error.
    pub fn open(
        path: impl Into<std::path::PathBuf>,
    ) -> Result<(Self, Vec<LaneRecord>), LaneJournalError> {
        use std::io::{Read as _, Seek as _, SeekFrom, Write as _};

        let path = path.into();
        fs_private::ensure_private_parent(&path)?;
        let existed = path.exists();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| io_error(&path, error))?;
        fs_private::set_private_file_permissions(&path)?;

        if !existed
            || file
                .metadata()
                .map_err(|error| io_error(&path, error))?
                .len()
                == 0
        {
            file.write_all(OP_JOURNAL_MAGIC_V4)
                .map_err(|error| io_error(&path, error))?;
            file.sync_all().map_err(|error| io_error(&path, error))?;
            fs_private::sync_parent(&path)?;
        }

        file.seek(SeekFrom::Start(0))
            .map_err(|error| io_error(&path, error))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| io_error(&path, error))?;
        if !bytes.starts_with(OP_JOURNAL_MAGIC_V4) {
            return Err(LaneJournalError::InvalidMagic);
        }

        let (records, complete_end) = parse_lane_records(&bytes, OP_JOURNAL_MAGIC_V4.len())?;
        if complete_end != bytes.len() {
            file.set_len(complete_end as u64)
                .map_err(|error| io_error(&path, error))?;
            file.sync_all().map_err(|error| io_error(&path, error))?;
        }
        file.seek(SeekFrom::End(0))
            .map_err(|error| io_error(&path, error))?;
        Ok((Self { path, file }, records))
    }

    /// Append one lane-tagged record and fsync it — the durability point of
    /// the two-phase local commit. Refuses a frame that does not carry its
    /// claimed lane's wire magic, so a cross-tagged record can never be
    /// WRITTEN, not merely never read.
    pub fn append(&mut self, lane: RoomLane, wire: &[u8]) -> Result<(), LaneJournalError> {
        use std::io::Write as _;

        check_lane_record(lane.tag(), wire)?;
        let mut framed = Vec::with_capacity(wire.len() + 5);
        encode_lane_record(&mut framed, lane, wire);
        self.file
            .write_all(&framed)
            .and_then(|_| self.file.sync_data())
            .map_err(|error| io_error(&self.path, error))
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn io_error(path: &std::path::Path, error: std::io::Error) -> LaneJournalError {
    LaneJournalError::Io {
        path: path.to_owned(),
        detail: error.to_string(),
    }
}

/// The same private-permissions file discipline as the v3 journal, kept local
/// so the v3 module stays byte-for-byte untouched.
#[cfg(not(target_arch = "wasm32"))]
mod fs_private {
    use std::fs::{self, File};
    use std::path::Path;

    use super::{LaneJournalError, io_error};

    pub(super) fn ensure_private_parent(path: &Path) -> Result<(), LaneJournalError> {
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

    pub(super) fn set_private_file_permissions(path: &Path) -> Result<(), LaneJournalError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                .map_err(|error| io_error(path, error))?;
        }
        Ok(())
    }

    pub(super) fn sync_parent(path: &Path) -> Result<(), LaneJournalError> {
        #[cfg(unix)]
        if let Some(parent) = path.parent() {
            File::open(parent)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| io_error(parent, error))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::OpLanguage;

    use crate::room::ops::signing_key_from_seed;
    use crate::room::test_support::{SEED_A, SEED_B, TOPIC, tet_degree, tet_pitch};
    use crate::room::v4::{
        ExtensionLang, ExtensionOp, MusicLang, MusicOp, Room, verify_extension_op, verify_music_op,
    };

    const TS: u64 = 1_700_000_000_000_000;

    /// A real music-lane wire frame, deterministic from a fixed seed/topic/ts.
    fn music_wire() -> Vec<u8> {
        let key = signing_key_from_seed(&SEED_A);
        let room = Room::new();
        let signed = room.prepare_music(
            &key,
            TOPIC,
            TS,
            MusicOp::AddDegree {
                degree: tet_degree(0),
            },
        );
        signed.to_wire_bytes_in::<MusicLang>().unwrap()
    }

    fn extension_wire() -> Vec<u8> {
        let key = signing_key_from_seed(&SEED_B);
        let room = Room::new();
        let signed = room.prepare_extension(
            &key,
            TOPIC,
            TS,
            ExtensionOp::PutPiece {
                emoji: "🌵".into(),
                pitch: tet_pitch(60),
            },
        );
        signed.to_wire_bytes_in::<ExtensionLang>().unwrap()
    }

    // -----------------------------------------------------------------
    // Record framing: `[lane:u8][wire_len:u32le][wire]`, exact layout.
    // -----------------------------------------------------------------

    #[test]
    fn encode_lane_record_layout_is_exact() {
        let wire = music_wire();
        let mut out = Vec::new();
        encode_lane_record(&mut out, RoomLane::Music, &wire);

        let mut expected = Vec::new();
        expected.push(0x01);
        expected.extend_from_slice(&(wire.len() as u32).to_le_bytes());
        expected.extend_from_slice(&wire);
        assert_eq!(out, expected);
    }

    // -----------------------------------------------------------------
    // Browser (IndexedDB) blob codec.
    // -----------------------------------------------------------------

    #[test]
    fn idb_journal_blob_has_the_v4_marker_then_records_exactly() {
        let records = vec![
            LaneRecord {
                lane: RoomLane::Music,
                wire: music_wire(),
            },
            LaneRecord {
                lane: RoomLane::Extension,
                wire: extension_wire(),
            },
        ];
        let blob = encode_idb_op_journal_v4(&records).unwrap();

        let mut expected = IDB_OP_JOURNAL_MAGIC_V4.to_vec();
        for record in &records {
            expected.push(record.lane.tag());
            expected.extend_from_slice(&(record.wire.len() as u32).to_le_bytes());
            expected.extend_from_slice(&record.wire);
        }
        assert_eq!(blob, expected);
        assert_eq!(decode_idb_op_journal_v4(&blob).unwrap(), records);
    }

    #[test]
    fn idb_journal_key_is_disjoint_from_the_v3_key() {
        let key = idb_op_journal_key_v4("abcd");
        assert_eq!(key, "opjournal:v4:abcd");
        assert_ne!(
            key,
            format!("opjournal:abcd"),
            "must never collide with the v3 key"
        );
    }

    /// The v3 blob has NO marker at all (bare length-prefixed records) — the
    /// v4 decoder must refuse it outright, never fall back.
    #[test]
    fn idb_decode_rejects_a_v3_blob_with_no_marker() {
        let mut v3_style = Vec::new();
        let wire = music_wire();
        v3_style.extend_from_slice(&(wire.len() as u32).to_le_bytes());
        v3_style.extend_from_slice(&wire);
        assert_eq!(
            decode_idb_op_journal_v4(&v3_style),
            Err(LaneJournalError::InvalidMagic)
        );
    }

    #[test]
    fn idb_decode_rejects_unknown_lane_tag() {
        let mut blob = IDB_OP_JOURNAL_MAGIC_V4.to_vec();
        let wire = music_wire();
        blob.push(0x07); // neither Music (0x01) nor Extension (0x02)
        blob.extend_from_slice(&(wire.len() as u32).to_le_bytes());
        blob.extend_from_slice(&wire);
        assert_eq!(
            decode_idb_op_journal_v4(&blob),
            Err(LaneJournalError::UnknownLaneTag(0x07))
        );
    }

    /// A complete record whose tag claims one lane but whose wire carries the
    /// OTHER lane's magic — a music frame can never be filed under the
    /// extension tag or vice versa.
    #[test]
    fn idb_decode_rejects_a_lane_wire_magic_mismatch() {
        let mut blob = IDB_OP_JOURNAL_MAGIC_V4.to_vec();
        let wire = extension_wire(); // ExtensionLang::WIRE_MAGIC prefixed...
        blob.push(RoomLane::Music.tag()); // ...but tagged Music.
        blob.extend_from_slice(&(wire.len() as u32).to_le_bytes());
        blob.extend_from_slice(&wire);
        assert_eq!(
            decode_idb_op_journal_v4(&blob),
            Err(LaneJournalError::LaneMagicMismatch {
                lane: RoomLane::Music
            })
        );
    }

    #[test]
    fn idb_decode_rejects_a_complete_malformed_lane_frame() {
        let wire = MusicLang::WIRE_MAGIC.to_vec();
        let mut blob = IDB_OP_JOURNAL_MAGIC_V4.to_vec();
        blob.push(RoomLane::Music.tag());
        blob.extend_from_slice(&(wire.len() as u32).to_le_bytes());
        blob.extend_from_slice(&wire);
        assert!(matches!(
            decode_idb_op_journal_v4(&blob),
            Err(LaneJournalError::InvalidRecord {
                lane: RoomLane::Music,
                ..
            })
        ));
    }

    #[test]
    fn idb_decode_truncates_a_torn_tail() {
        let good = LaneRecord {
            lane: RoomLane::Music,
            wire: music_wire(),
        };
        let mut blob = encode_idb_op_journal_v4(std::slice::from_ref(&good)).unwrap();
        // A torn header (too short to even carry tag + length).
        blob.push(RoomLane::Extension.tag());
        blob.extend_from_slice(&7u32.to_le_bytes()[..2]);
        let (records, complete_end) =
            parse_lane_records(&blob, IDB_OP_JOURNAL_MAGIC_V4.len()).unwrap();
        assert_eq!(records, vec![good]);
        assert_eq!(
            complete_end,
            blob.len() - 3,
            "truncates to the last complete record"
        );
    }

    #[test]
    fn idb_decode_rejects_an_oversize_declared_length() {
        let mut blob = IDB_OP_JOURNAL_MAGIC_V4.to_vec();
        blob.push(RoomLane::Music.tag());
        blob.extend_from_slice(&((MAX_LANE_RECORD_BYTES + 1) as u32).to_le_bytes());
        assert_eq!(
            decode_idb_op_journal_v4(&blob),
            Err(LaneJournalError::RecordTooLarge {
                actual: MAX_LANE_RECORD_BYTES + 1,
                max: MAX_LANE_RECORD_BYTES,
            })
        );
    }

    // -----------------------------------------------------------------
    // `RoomJournalV4` bookkeeping.
    // -----------------------------------------------------------------

    #[test]
    fn room_journal_v4_dedups_admits_and_encodes() {
        let mut journal = RoomJournalV4::new();
        let wire = music_wire();
        let id = crate::room::ops::OpId([9; 32]);
        assert!(journal.admit(RoomLane::Music, id, &wire).unwrap());
        assert!(
            !journal.admit(RoomLane::Music, id, &wire).unwrap(),
            "the same (lane, id) is never journaled twice"
        );
        assert_eq!(journal.len(), 1);
        assert!(journal.contains(RoomLane::Music, id));
        assert_eq!(
            journal.encode().unwrap(),
            encode_idb_op_journal_v4(journal.records()).unwrap()
        );
    }

    #[test]
    fn room_journal_v4_rejects_a_cross_tagged_admit_without_mutating() {
        let mut journal = RoomJournalV4::new();
        let id = crate::room::ops::OpId([10; 32]);
        assert_eq!(
            journal.admit(RoomLane::Extension, id, &music_wire()),
            Err(LaneJournalError::LaneMagicMismatch {
                lane: RoomLane::Extension
            })
        );
        assert!(journal.is_empty());
        assert!(!journal.contains(RoomLane::Extension, id));
    }

    #[test]
    fn idb_encoder_and_loaded_journal_reject_cross_tagged_records() {
        let record = LaneRecord {
            lane: RoomLane::Extension,
            wire: music_wire(),
        };
        assert_eq!(
            encode_idb_op_journal_v4(std::slice::from_ref(&record)),
            Err(LaneJournalError::LaneMagicMismatch {
                lane: RoomLane::Extension
            })
        );
        assert!(matches!(
            RoomJournalV4::from_records(vec![record]),
            Err(LaneJournalError::LaneMagicMismatch {
                lane: RoomLane::Extension
            })
        ));
    }

    // -----------------------------------------------------------------
    // Native file journal.
    // -----------------------------------------------------------------

    fn temp_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir()
            .join(format!(
                "walkie-lane-journal-{label}-{}",
                uuid::Uuid::new_v4()
            ))
            .join("room.ops")
    }

    #[test]
    fn file_journal_roundtrips_and_rejects_the_v3_magic() {
        let path = temp_path("roundtrip");
        let music = music_wire();
        let extension = extension_wire();
        {
            let (mut journal, existing) = FileLaneJournal::open(&path).unwrap();
            assert!(existing.is_empty());
            journal.append(RoomLane::Music, &music).unwrap();
            journal.append(RoomLane::Extension, &extension).unwrap();
        }
        let (_, existing) = FileLaneJournal::open(&path).unwrap();
        assert_eq!(
            existing,
            vec![
                LaneRecord {
                    lane: RoomLane::Music,
                    wire: music.clone()
                },
                LaneRecord {
                    lane: RoomLane::Extension,
                    wire: extension.clone()
                },
            ]
        );

        // The v3 journal's magic must not open as a v4 journal.
        let v3_path = temp_path("v3-magic");
        std::fs::create_dir_all(v3_path.parent().unwrap()).unwrap();
        std::fs::write(&v3_path, b"walkie-songie/op-journal/3\n").unwrap();
        assert!(matches!(
            FileLaneJournal::open(&v3_path),
            Err(LaneJournalError::InvalidMagic)
        ));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(path.parent().unwrap());
        let _ = std::fs::remove_file(&v3_path);
        let _ = std::fs::remove_dir(v3_path.parent().unwrap());
    }

    /// A refused write must never smuggle a cross-tagged record onto disk in
    /// the first place — `append` checks lane discipline before any byte hits
    /// the file, not only on the next `open`.
    #[test]
    fn file_journal_append_refuses_a_lane_wire_mismatch() {
        let path = temp_path("append-mismatch");
        let (mut journal, _) = FileLaneJournal::open(&path).unwrap();
        let extension = extension_wire();
        assert!(matches!(
            journal.append(RoomLane::Music, &extension),
            Err(LaneJournalError::LaneMagicMismatch {
                lane: RoomLane::Music
            })
        ));
        drop(journal);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(path.parent().unwrap());
    }

    #[test]
    fn file_journal_torn_tail_is_truncated_on_reopen() {
        let path = temp_path("torn");
        let music = music_wire();
        let expected_len;
        {
            let (mut journal, _) = FileLaneJournal::open(&path).unwrap();
            journal.append(RoomLane::Music, &music).unwrap();
            expected_len = OP_JOURNAL_MAGIC_V4.len() as u64 + 5 + music.len() as u64;
            use std::io::Write as _;
            journal
                .file
                .write_all(&[RoomLane::Extension.tag()])
                .unwrap();
            journal.file.write_all(&999u32.to_le_bytes()).unwrap();
            journal.file.write_all(b"partial-tail").unwrap();
            journal.file.sync_all().unwrap();
        }
        let (journal, existing) = FileLaneJournal::open(&path).unwrap();
        assert_eq!(
            existing,
            vec![LaneRecord {
                lane: RoomLane::Music,
                wire: music
            }]
        );
        assert_eq!(
            std::fs::metadata(journal.path()).unwrap().len(),
            expected_len
        );
        drop(journal);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(path.parent().unwrap());
    }

    // -----------------------------------------------------------------
    // Room + journal integration: the two-phase discipline's durability
    // half, and full recovery through a torn tail.
    // -----------------------------------------------------------------

    /// fsync precedes visibility, and survives a "crash": the op is journaled
    /// (fsynced) but NEVER ingested into the in-memory `Room` before it is
    /// dropped; reopening the journal and recovering still reconstructs it.
    #[test]
    fn fsync_before_visible_and_recoverable_after_a_simulated_crash() {
        let path = temp_path("fsync-before-visible");
        let key = signing_key_from_seed(&SEED_A);
        let (mut journal, existing) = FileLaneJournal::open(&path).unwrap();
        assert!(existing.is_empty());

        let room = Room::new();
        let prepared = room.prepare_music(
            &key,
            TOPIC,
            TS,
            MusicOp::AddDegree {
                degree: tet_degree(0),
            },
        );
        // NOT ingested yet: the room must show nothing.
        assert!(room.view().pitches.is_empty());

        let wire = prepared.to_wire_bytes_in::<MusicLang>().unwrap();
        journal.append(RoomLane::Music, &wire).unwrap(); // the durability point
        // Simulate a crash: drop everything in memory without ever ingesting.
        drop(room);
        drop(journal);

        let (_, records) = FileLaneJournal::open(&path).unwrap();
        let recovered = Room::recover(TOPIC, &records).unwrap();
        assert!(
            recovered.view().pitches.contains(&tet_degree(0)),
            "fsynced-but-never-ingested op must still recover"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(path.parent().unwrap());
    }

    /// The full Step 3 gate: interleaved music/extension records, a torn
    /// final record, reopen, recover — both lanes drain (`pending_len() ==
    /// 0`), and the recovered room matches a room built by direct commit of
    /// only the COMPLETE records (the torn one never happened).
    #[cfg(feature = "merkle")]
    #[test]
    fn torn_tail_recovery_matches_a_fresh_room_built_from_the_surviving_ops() {
        let path = temp_path("torn-tail-recovery");
        let key_a = signing_key_from_seed(&SEED_A);
        let key_b = signing_key_from_seed(&SEED_B);
        let mut room = Room::new();

        let p1 = room.prepare_music(
            &key_a,
            TOPIC,
            TS,
            MusicOp::AddDegree {
                degree: tet_degree(0),
            },
        );
        room.ingest_music(verify_music_op(&p1, TOPIC).unwrap());
        let p2 = room.prepare_extension(
            &key_b,
            TOPIC,
            TS + 1,
            ExtensionOp::PutPiece {
                emoji: "🌵".into(),
                pitch: tet_pitch(60),
            },
        );
        room.ingest_extension(verify_extension_op(&p2, TOPIC).unwrap());
        let p3 = room.prepare_music(
            &key_b,
            TOPIC,
            TS + 2,
            MusicOp::AddDegree {
                degree: tet_degree(4),
            },
        );
        room.ingest_music(verify_music_op(&p3, TOPIC).unwrap());

        {
            let (mut journal, existing) = FileLaneJournal::open(&path).unwrap();
            assert!(existing.is_empty());
            journal
                .append(
                    RoomLane::Music,
                    &p1.to_wire_bytes_in::<MusicLang>().unwrap(),
                )
                .unwrap();
            journal
                .append(
                    RoomLane::Extension,
                    &p2.to_wire_bytes_in::<ExtensionLang>().unwrap(),
                )
                .unwrap();
            journal
                .append(
                    RoomLane::Music,
                    &p3.to_wire_bytes_in::<MusicLang>().unwrap(),
                )
                .unwrap();
            // A torn fourth record: a well-formed header claiming more bytes
            // than actually follow (the crash mid-write case).
            use std::io::Write as _;
            journal
                .file
                .write_all(&[RoomLane::Extension.tag()])
                .unwrap();
            journal.file.write_all(&500u32.to_le_bytes()).unwrap();
            journal.file.write_all(b"not enough bytes").unwrap();
            journal.file.sync_all().unwrap();
        }

        let (_, recovered_records) = FileLaneJournal::open(&path).unwrap();
        assert_eq!(recovered_records.len(), 3, "the torn 4th record is dropped");
        let recovered = Room::recover(TOPIC, &recovered_records).unwrap();
        assert_eq!(recovered.music().pending_len(), 0);
        assert_eq!(recovered.extension().pending_len(), 0);

        // A fresh room built by directly committing only the 3 surviving ops.
        let mut fresh = Room::new();
        fresh.commit_music(
            &key_a,
            TOPIC,
            TS,
            MusicOp::AddDegree {
                degree: tet_degree(0),
            },
        );
        fresh.commit_extension(
            &key_b,
            TOPIC,
            TS + 1,
            ExtensionOp::PutPiece {
                emoji: "🌵".into(),
                pitch: tet_pitch(60),
            },
        );
        fresh.commit_music(
            &key_b,
            TOPIC,
            TS + 2,
            MusicOp::AddDegree {
                degree: tet_degree(4),
            },
        );

        assert_eq!(recovered.music().ops_root(), fresh.music().ops_root());
        assert_eq!(
            recovered.extension().ops_root(),
            fresh.extension().ops_root()
        );
        assert_eq!(recovered.view(), fresh.view());
        assert_eq!(recovered.state_root(), fresh.state_root());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(path.parent().unwrap());
    }
}
