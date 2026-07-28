//! Immutable v2 segment sealing and read-only manifest verification.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use rustix::fs::{AtFlags, CWD, FileType, FlockOperation, Mode, OFlags, RenameFlags};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    frame::{SCHEMA_VERSION, WalFormat},
    prologue::{self, PrologueError, SegmentMetadata, StreamDescriptor},
    recovery::{self, MAX_RECOVERABLE_SEGMENT_LENGTH, RecoveredRecord, RecoveryError},
    segment::{Segment, SegmentError},
};

pub const MANIFEST_SCHEMA_VERSION: u16 = 1;
pub const MAX_MANIFEST_LENGTH: usize = 2 * 1024 * 1024;

const ACTIVE_SUFFIX: &str = ".active.wal";
const SEALED_SUFFIX: &str = ".wal";
const MANIFEST_SUFFIX: &str = ".seal.json";
const PENDING_MANIFEST_SUFFIX: &str = ".seal.pending";
const TEMP_CREATE_ATTEMPTS: u64 = 128;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ManifestError {
    #[error("WAL seal manifest exceeds the maximum encoded length")]
    TooLarge,
    #[error("WAL seal manifest JSON is invalid: {0}")]
    Json(String),
    #[error("WAL seal manifest is not in canonical encoding")]
    NonCanonical,
    #[error("WAL seal manifest self-digest does not match its body")]
    DigestMismatch,
    #[error("WAL seal manifest field {0} is invalid")]
    InvalidField(&'static str),
    #[error("WAL seal manifest contains invalid segment metadata: {0}")]
    Prologue(#[from] PrologueError),
}

#[derive(Debug, Error)]
pub enum SealingError {
    #[error("WAL seal path is invalid")]
    InvalidPath,
    #[error("WAL seal manifest already exists")]
    ManifestExists,
    #[error("WAL seal manifest is missing")]
    ManifestMissing,
    #[error("sealed WAL target already exists")]
    SealedExists,
    #[error("WAL pending seal state conflicts with the expected manifest")]
    PendingConflict,
    #[error("WAL seal file violates the regular single-link file contract")]
    UnsafeFile,
    #[error("WAL active filename does not match its prologue segment ID")]
    SegmentIdMismatch,
    #[error("WAL segment has a live writer or verifier")]
    AlreadyOpen,
    #[error("WAL segment lifecycle failed: {0}")]
    Segment(#[from] SegmentError),
    #[error("WAL segment prologue is invalid: {0}")]
    Prologue(#[from] PrologueError),
    #[error("WAL recovery failed: {0}")]
    Recovery(#[from] RecoveryError),
    #[error("WAL seal manifest failed validation: {0}")]
    Manifest(#[from] ManifestError),
    #[error("WAL seal I/O failed: {0}")]
    Io(#[from] io::Error),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SequenceRange {
    stream_id: u32,
    connection_epoch: u64,
    first_sequence: u64,
    last_sequence: u64,
    record_count: u64,
}

impl SequenceRange {
    pub const fn stream_id(&self) -> u32 {
        self.stream_id
    }

    pub const fn connection_epoch(&self) -> u64 {
        self.connection_epoch
    }

    pub const fn first_sequence(&self) -> u64 {
        self.first_sequence
    }

    pub const fn last_sequence(&self) -> u64 {
        self.last_sequence
    }

    pub const fn record_count(&self) -> u64 {
        self.record_count
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentPredecessor {
    segment_id: [u8; 16],
    segment_blake3: [u8; 32],
}

impl SegmentPredecessor {
    pub fn new(segment_id: [u8; 16], segment_blake3: [u8; 32]) -> Result<Self, ManifestError> {
        if segment_id == [0_u8; 16] {
            return Err(ManifestError::InvalidField("predecessor_segment_id"));
        }
        if segment_blake3 == [0_u8; 32] {
            return Err(ManifestError::InvalidField("predecessor_segment_blake3"));
        }
        Ok(Self {
            segment_id,
            segment_blake3,
        })
    }

    pub const fn segment_id(&self) -> &[u8; 16] {
        &self.segment_id
    }

    pub const fn segment_blake3(&self) -> &[u8; 32] {
        &self.segment_blake3
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SealedSegmentManifest {
    segment_ordinal: u64,
    segment_file: String,
    metadata: SegmentMetadata,
    predecessor: Option<SegmentPredecessor>,
    sealed_wall_time_ns: i64,
    segment_length: u64,
    segment_blake3: [u8; 32],
    prologue_length: u64,
    record_count: u64,
    first_record_offset: u64,
    next_offset: u64,
    sequence_ranges: Vec<SequenceRange>,
    min_receive_wall_time_ns: Option<i64>,
    max_receive_wall_time_ns: Option<i64>,
    manifest_blake3: [u8; 32],
}

impl SealedSegmentManifest {
    pub const fn segment_ordinal(&self) -> u64 {
        self.segment_ordinal
    }

    pub const fn segment_id(&self) -> &[u8; 16] {
        self.metadata.segment_id()
    }

    pub const fn predecessor(&self) -> Option<&SegmentPredecessor> {
        self.predecessor.as_ref()
    }

    pub fn as_predecessor(&self) -> SegmentPredecessor {
        SegmentPredecessor {
            segment_id: *self.segment_id(),
            segment_blake3: self.segment_blake3,
        }
    }

    pub fn segment_file(&self) -> &str {
        &self.segment_file
    }

    pub const fn created_wall_time_ns(&self) -> i64 {
        self.metadata.created_wall_time_ns()
    }

    pub const fn sealed_wall_time_ns(&self) -> i64 {
        self.sealed_wall_time_ns
    }

    pub fn schema_id(&self) -> &str {
        self.metadata.schema_id()
    }

    pub fn installation_id(&self) -> &str {
        self.metadata.installation_id()
    }

    pub fn build_id(&self) -> &str {
        self.metadata.build_id()
    }

    pub fn streams(&self) -> &[StreamDescriptor] {
        self.metadata.streams()
    }

    pub const fn segment_length(&self) -> u64 {
        self.segment_length
    }

    pub const fn segment_blake3(&self) -> &[u8; 32] {
        &self.segment_blake3
    }

    pub const fn prologue_length(&self) -> u64 {
        self.prologue_length
    }

    pub const fn record_count(&self) -> u64 {
        self.record_count
    }

    pub const fn first_record_offset(&self) -> u64 {
        self.first_record_offset
    }

    pub const fn next_offset(&self) -> u64 {
        self.next_offset
    }

    pub fn sequence_ranges(&self) -> &[SequenceRange] {
        &self.sequence_ranges
    }

    pub const fn min_receive_wall_time_ns(&self) -> Option<i64> {
        self.min_receive_wall_time_ns
    }

    pub const fn max_receive_wall_time_ns(&self) -> Option<i64> {
        self.max_receive_wall_time_ns
    }

    pub const fn manifest_blake3(&self) -> &[u8; 32] {
        &self.manifest_blake3
    }

    pub fn encode(&self) -> Result<Vec<u8>, ManifestError> {
        let body = self.to_body_wire();
        let manifest_blake3 = digest_body(&body)?;
        if manifest_blake3 != self.manifest_blake3 {
            return Err(ManifestError::DigestMismatch);
        }
        let encoded = serde_json::to_vec(&ManifestWire {
            body,
            manifest_body_blake3: hex::encode(manifest_blake3),
        })
        .map_err(json_error)?;
        if encoded.len() > MAX_MANIFEST_LENGTH {
            return Err(ManifestError::TooLarge);
        }
        Ok(encoded)
    }

    fn to_body_wire(&self) -> ManifestBodyWire {
        ManifestBodyWire {
            manifest_schema_version: MANIFEST_SCHEMA_VERSION,
            wal_format_version: SCHEMA_VERSION,
            segment_ordinal: self.segment_ordinal,
            segment_id: hex::encode(self.metadata.segment_id()),
            predecessor_segment_id: self
                .predecessor
                .as_ref()
                .map(|predecessor| hex::encode(predecessor.segment_id)),
            predecessor_segment_blake3: self
                .predecessor
                .as_ref()
                .map(|predecessor| hex::encode(predecessor.segment_blake3)),
            segment_file: self.segment_file.clone(),
            segment_bytes: self.segment_length,
            segment_blake3: hex::encode(self.segment_blake3),
            prologue_bytes: self.prologue_length,
            record_count: self.record_count,
            first_frame_offset: self.first_record_offset,
            end_offset: self.next_offset,
            created_wall_time_ns: self.metadata.created_wall_time_ns(),
            sealed_wall_time_ns: self.sealed_wall_time_ns,
            schema_id: self.metadata.schema_id().to_owned(),
            installation_id: self.metadata.installation_id().to_owned(),
            build_id: self.metadata.build_id().to_owned(),
            streams: self
                .metadata
                .streams()
                .iter()
                .map(|stream| StreamWire {
                    stream_id: stream.stream_id(),
                    source_name: stream.source_name().to_owned(),
                    stream_name: stream.stream_name().to_owned(),
                })
                .collect(),
            sequence_ranges: self
                .sequence_ranges
                .iter()
                .map(|range| SequenceRangeWire {
                    stream_id: range.stream_id,
                    connection_epoch: range.connection_epoch,
                    first_sequence: range.first_sequence,
                    last_sequence: range.last_sequence,
                    record_count: range.record_count,
                })
                .collect(),
            min_receive_wall_time_ns: self.min_receive_wall_time_ns,
            max_receive_wall_time_ns: self.max_receive_wall_time_ns,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompressionJob {
    manifest_path: PathBuf,
    source_path: PathBuf,
    destination_path: PathBuf,
    expected_uncompressed_bytes: u64,
    expected_uncompressed_blake3: [u8; 32],
}

impl CompressionJob {
    pub fn manifest_path(&self) -> &Path {
        &self.manifest_path
    }

    pub fn source_path(&self) -> &Path {
        &self.source_path
    }

    pub fn destination_path(&self) -> &Path {
        &self.destination_path
    }

    pub const fn expected_uncompressed_bytes(&self) -> u64 {
        self.expected_uncompressed_bytes
    }

    pub const fn expected_uncompressed_blake3(&self) -> &[u8; 32] {
        &self.expected_uncompressed_blake3
    }
}

#[derive(Debug)]
pub struct SealedSegment {
    manifest: SealedSegmentManifest,
    compression_job: CompressionJob,
}

impl SealedSegment {
    pub const fn manifest(&self) -> &SealedSegmentManifest {
        &self.manifest
    }

    pub const fn compression_job(&self) -> &CompressionJob {
        &self.compression_job
    }

    pub fn into_manifest(self) -> SealedSegmentManifest {
        self.manifest
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ManifestWire {
    body: ManifestBodyWire,
    manifest_body_blake3: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ManifestBodyWire {
    manifest_schema_version: u16,
    wal_format_version: u16,
    segment_ordinal: u64,
    segment_id: String,
    predecessor_segment_id: Option<String>,
    predecessor_segment_blake3: Option<String>,
    segment_file: String,
    segment_bytes: u64,
    segment_blake3: String,
    prologue_bytes: u64,
    record_count: u64,
    first_frame_offset: u64,
    end_offset: u64,
    created_wall_time_ns: i64,
    sealed_wall_time_ns: i64,
    schema_id: String,
    installation_id: String,
    build_id: String,
    streams: Vec<StreamWire>,
    sequence_ranges: Vec<SequenceRangeWire>,
    min_receive_wall_time_ns: Option<i64>,
    max_receive_wall_time_ns: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StreamWire {
    stream_id: u32,
    source_name: String,
    stream_name: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct SequenceRangeWire {
    stream_id: u32,
    connection_epoch: u64,
    first_sequence: u64,
    last_sequence: u64,
    record_count: u64,
}

#[derive(Default)]
struct ScanStats {
    ranges: BTreeMap<(u32, u64), SequenceRange>,
    min_receive_wall_time_ns: Option<i64>,
    max_receive_wall_time_ns: Option<i64>,
    record_count_overflowed: bool,
}

type ScanParts = (Vec<SequenceRange>, Option<i64>, Option<i64>);

impl ScanStats {
    fn observe(&mut self, record: RecoveredRecord<'_>) {
        let Some(metadata) = record.metadata() else {
            return;
        };
        self.ranges
            .entry((metadata.stream_id, metadata.connection_epoch))
            .and_modify(|range| {
                range.last_sequence = metadata.record_sequence;
                if let Some(record_count) = range.record_count.checked_add(1) {
                    range.record_count = record_count;
                } else {
                    self.record_count_overflowed = true;
                }
            })
            .or_insert(SequenceRange {
                stream_id: metadata.stream_id,
                connection_epoch: metadata.connection_epoch,
                first_sequence: metadata.record_sequence,
                last_sequence: metadata.record_sequence,
                record_count: 1,
            });
        self.min_receive_wall_time_ns = Some(
            self.min_receive_wall_time_ns
                .map_or(metadata.receive_wall_time_ns, |current| {
                    current.min(metadata.receive_wall_time_ns)
                }),
        );
        self.max_receive_wall_time_ns = Some(
            self.max_receive_wall_time_ns
                .map_or(metadata.receive_wall_time_ns, |current| {
                    current.max(metadata.receive_wall_time_ns)
                }),
        );
    }

    fn into_parts(self) -> Result<ScanParts, ManifestError> {
        if self.record_count_overflowed {
            return Err(ManifestError::InvalidField("record_count"));
        }
        Ok((
            self.ranges.into_values().collect(),
            self.min_receive_wall_time_ns,
            self.max_receive_wall_time_ns,
        ))
    }
}

pub fn sealed_path_for(active_path: &Path) -> Result<PathBuf, SealingError> {
    let file_name = utf8_file_name(active_path)?;
    let base = file_name
        .strip_suffix(ACTIVE_SUFFIX)
        .ok_or(SealingError::InvalidPath)?;
    parse_segment_base(base)?;
    Ok(active_path.with_file_name(format!("{base}{SEALED_SUFFIX}")))
}

pub fn manifest_path_for(sealed_path: &Path) -> Result<PathBuf, SealingError> {
    let file_name = utf8_file_name(sealed_path)?;
    let base = file_name
        .strip_suffix(SEALED_SUFFIX)
        .ok_or(SealingError::InvalidPath)?;
    if base.ends_with(".active") {
        return Err(SealingError::InvalidPath);
    }
    parse_segment_base(base)?;
    Ok(sealed_path.with_file_name(format!("{base}{MANIFEST_SUFFIX}")))
}

pub fn pending_manifest_path_for(sealed_path: &Path) -> Result<PathBuf, SealingError> {
    let file_name = utf8_file_name(sealed_path)?;
    let base = file_name
        .strip_suffix(SEALED_SUFFIX)
        .ok_or(SealingError::InvalidPath)?;
    parse_segment_base(base)?;
    Ok(sealed_path.with_file_name(format!("{base}{PENDING_MANIFEST_SUFFIX}")))
}

struct SealPaths {
    active: PathBuf,
    sealed: PathBuf,
    manifest: PathBuf,
    pending: PathBuf,
}

impl SealPaths {
    fn new(active_path: &Path) -> Result<Self, SealingError> {
        let sealed = sealed_path_for(active_path)?;
        Ok(Self {
            active: active_path.to_owned(),
            manifest: manifest_path_for(&sealed)?,
            pending: pending_manifest_path_for(&sealed)?,
            sealed,
        })
    }
}

pub fn seal_v2_segment(
    active_path: &Path,
    sealed_wall_time_ns: i64,
) -> Result<SealedSegment, SealingError> {
    seal_v2_segment_with_predecessor(active_path, None, sealed_wall_time_ns)
}

pub fn seal_v2_segment_after(
    active_path: &Path,
    predecessor: SegmentPredecessor,
    sealed_wall_time_ns: i64,
) -> Result<SealedSegment, SealingError> {
    seal_v2_segment_with_predecessor(active_path, Some(predecessor), sealed_wall_time_ns)
}

fn seal_v2_segment_with_predecessor(
    active_path: &Path,
    predecessor: Option<SegmentPredecessor>,
    sealed_wall_time_ns: i64,
) -> Result<SealedSegment, SealingError> {
    let paths = SealPaths::new(active_path)?;
    let active_exists = path_entry_exists(active_path)?;
    let sealed_exists = path_entry_exists(&paths.sealed)?;
    let manifest_exists = path_entry_exists(&paths.manifest)?;
    let pending_exists = path_entry_exists(&paths.pending)?;

    if !active_exists && sealed_exists && manifest_exists {
        let manifest = verify_sealed_v2_segment(&paths.sealed)?;
        return sealed_result(manifest, paths.manifest, paths.sealed);
    }
    if !active_exists && sealed_exists && pending_exists && !manifest_exists {
        return resume_pending_seal(&paths.sealed, &paths.manifest, &paths.pending);
    }
    if !active_exists && sealed_exists && !manifest_exists {
        return Err(SealingError::ManifestMissing);
    }
    if manifest_exists {
        return Err(SealingError::ManifestExists);
    }
    if sealed_exists {
        return Err(SealingError::SealedExists);
    }
    let pending_manifest = load_pending_manifest(&paths, predecessor.as_ref(), pending_exists)?;
    let active_file = open_nofollow(active_path, OFlags::RDWR)?;
    let active_identity = validate_regular_single_link(&active_file)?;
    let segment = Segment::open_v2_file(active_file)?;
    finish_locked_seal(
        &paths,
        predecessor,
        sealed_wall_time_ns,
        pending_manifest,
        segment,
        active_identity,
    )
}

pub(crate) fn seal_v2_segment_at(
    directory: &File,
    display_directory: &Path,
    active_name: &Path,
    predecessor: Option<SegmentPredecessor>,
    sealed_wall_time_ns: i64,
) -> Result<SealedSegment, SealingError> {
    let paths = SealPaths::new(active_name)?;
    let active_exists = path_entry_exists_at(directory, &paths.active)?;
    let sealed_exists = path_entry_exists_at(directory, &paths.sealed)?;
    let manifest_exists = path_entry_exists_at(directory, &paths.manifest)?;
    let pending_exists = path_entry_exists_at(directory, &paths.pending)?;
    if !active_exists && sealed_exists && manifest_exists {
        let manifest = verify_sealed_v2_segment_at(directory, &paths.sealed)?;
        return sealed_result(
            manifest,
            display_directory.join(&paths.manifest),
            display_directory.join(&paths.sealed),
        );
    }
    if !active_exists && sealed_exists && pending_exists && !manifest_exists {
        return resume_pending_seal_at(directory, display_directory, &paths);
    }
    if !active_exists && sealed_exists && !manifest_exists {
        return Err(SealingError::ManifestMissing);
    }
    if manifest_exists {
        return Err(SealingError::ManifestExists);
    }
    if sealed_exists {
        return Err(SealingError::SealedExists);
    }
    let pending_manifest =
        load_pending_manifest_at(directory, &paths, predecessor.as_ref(), pending_exists)?;
    let active_file = open_nofollow_at(directory, &paths.active, OFlags::RDWR)?;
    let active_identity = validate_regular_single_link(&active_file)?;
    let segment = Segment::open_v2_file(active_file)?;
    finish_locked_seal_at(
        directory,
        display_directory,
        &paths,
        predecessor,
        sealed_wall_time_ns,
        pending_manifest,
        segment,
        active_identity,
    )
}

pub(crate) fn seal_owned_v2_segment_at(
    directory: &File,
    display_directory: &Path,
    active_name: &Path,
    segment: Segment,
    predecessor: Option<SegmentPredecessor>,
    sealed_wall_time_ns: i64,
) -> Result<SealedSegment, SealingError> {
    let paths = SealPaths::new(active_name)?;
    if !path_entry_exists_at(directory, &paths.active)? {
        return Err(SealingError::InvalidPath);
    }
    if path_entry_exists_at(directory, &paths.manifest)? {
        return Err(SealingError::ManifestExists);
    }
    if path_entry_exists_at(directory, &paths.sealed)? {
        return Err(SealingError::SealedExists);
    }
    let pending_exists = path_entry_exists_at(directory, &paths.pending)?;
    let pending_manifest =
        load_pending_manifest_at(directory, &paths, predecessor.as_ref(), pending_exists)?;
    let active_identity = validate_regular_single_link(segment.file())?;
    finish_locked_seal_at(
        directory,
        display_directory,
        &paths,
        predecessor,
        sealed_wall_time_ns,
        pending_manifest,
        segment,
        active_identity,
    )
}

fn load_pending_manifest(
    paths: &SealPaths,
    predecessor: Option<&SegmentPredecessor>,
    pending_exists: bool,
) -> Result<Option<SealedSegmentManifest>, SealingError> {
    if !pending_exists {
        return Ok(None);
    }
    let encoded = read_bounded(&paths.pending, MAX_MANIFEST_LENGTH)?;
    let manifest = decode_manifest(&encoded)?;
    if manifest.segment_file() != utf8_file_name(&paths.sealed)?
        || manifest.predecessor() != predecessor
    {
        return Err(SealingError::PendingConflict);
    }
    Ok(Some(manifest))
}

fn load_pending_manifest_at(
    directory: &File,
    paths: &SealPaths,
    predecessor: Option<&SegmentPredecessor>,
    pending_exists: bool,
) -> Result<Option<SealedSegmentManifest>, SealingError> {
    if !pending_exists {
        return Ok(None);
    }
    let encoded = read_bounded_at(directory, &paths.pending, MAX_MANIFEST_LENGTH)?;
    let manifest = decode_manifest(&encoded)?;
    if manifest.segment_file() != utf8_file_name(&paths.sealed)?
        || manifest.predecessor() != predecessor
    {
        return Err(SealingError::PendingConflict);
    }
    Ok(Some(manifest))
}

pub(crate) fn read_pending_manifest_for_recovery_at(
    directory: &File,
    pending_name: &Path,
) -> Result<SealedSegmentManifest, SealingError> {
    let encoded = read_bounded_at(directory, pending_name, MAX_MANIFEST_LENGTH)?;
    decode_manifest(&encoded).map_err(SealingError::from)
}

fn finish_locked_seal(
    paths: &SealPaths,
    predecessor: Option<SegmentPredecessor>,
    sealed_wall_time_ns: i64,
    pending_manifest: Option<SealedSegmentManifest>,
    mut segment: Segment,
    active_identity: FileIdentity,
) -> Result<SealedSegment, SealingError> {
    let (ordinal, filename_segment_id) =
        parse_active_identity(&paths.active).ok_or(SealingError::InvalidPath)?;
    let metadata = segment
        .segment_metadata()
        .cloned()
        .ok_or(SealingError::InvalidPath)?;
    if metadata.segment_id() != &filename_segment_id {
        return Err(SealingError::SegmentIdMismatch);
    }
    let sealed_wall_time_ns = pending_manifest.as_ref().map_or(
        sealed_wall_time_ns,
        SealedSegmentManifest::sealed_wall_time_ns,
    );
    if sealed_wall_time_ns < metadata.created_wall_time_ns() {
        return Err(ManifestError::InvalidField("sealed_wall_time_ns").into());
    }
    validate_predecessor(ordinal, metadata.segment_id(), predecessor.as_ref())?;

    let mut stats = ScanStats::default();
    let summary = segment.verify_without_repair(|record| stats.observe(record))?;
    segment.sync_all()?;
    let segment_length = segment.file_mut().metadata()?.len();
    if summary.last_valid_offset() != segment_length {
        return Err(ManifestError::InvalidField("end_offset").into());
    }
    let segment_blake3 = hash_file(segment.file_mut())?;
    let prologue_length =
        u64::try_from(prologue::encode(&metadata)?.len()).map_err(|_| ManifestError::TooLarge)?;
    let (sequence_ranges, min_receive_wall_time_ns, max_receive_wall_time_ns) =
        stats.into_parts()?;
    let segment_file = utf8_file_name(&paths.sealed)?.to_owned();
    let mut manifest = SealedSegmentManifest {
        segment_ordinal: ordinal,
        segment_file,
        metadata,
        predecessor,
        sealed_wall_time_ns,
        segment_length,
        segment_blake3,
        prologue_length,
        record_count: summary.record_count(),
        first_record_offset: prologue_length,
        next_offset: summary.last_valid_offset(),
        sequence_ranges,
        min_receive_wall_time_ns,
        max_receive_wall_time_ns,
        manifest_blake3: [0_u8; 32],
    };
    validate_manifest(&manifest)?;
    manifest.manifest_blake3 = digest_body(&manifest.to_body_wire())?;
    let encoded = manifest.encode()?;
    prepare_pending_manifest(&paths.pending, &encoded)?;

    verify_path_identity(&paths.active, active_identity)?;
    rustix::fs::renameat_with(
        CWD,
        &paths.active,
        CWD,
        &paths.sealed,
        RenameFlags::NOREPLACE,
    )
    .map_err(|error| {
        if error == rustix::io::Errno::EXIST {
            SealingError::SealedExists
        } else {
            SealingError::Io(error.into())
        }
    })?;
    let parent = usable_parent(&paths.sealed)?;
    File::open(parent)?.sync_all()?;
    verify_path_identity(&paths.sealed, active_identity)?;
    install_pending_manifest(&paths.pending, &paths.manifest)?;
    sealed_result(manifest, paths.manifest.clone(), paths.sealed.clone())
}

#[allow(clippy::too_many_arguments)]
fn finish_locked_seal_at(
    directory: &File,
    display_directory: &Path,
    paths: &SealPaths,
    predecessor: Option<SegmentPredecessor>,
    sealed_wall_time_ns: i64,
    pending_manifest: Option<SealedSegmentManifest>,
    mut segment: Segment,
    active_identity: FileIdentity,
) -> Result<SealedSegment, SealingError> {
    let (ordinal, filename_segment_id) =
        parse_active_identity(&paths.active).ok_or(SealingError::InvalidPath)?;
    let metadata = segment
        .segment_metadata()
        .cloned()
        .ok_or(SealingError::InvalidPath)?;
    if metadata.segment_id() != &filename_segment_id {
        return Err(SealingError::SegmentIdMismatch);
    }
    let sealed_wall_time_ns = pending_manifest.as_ref().map_or(
        sealed_wall_time_ns,
        SealedSegmentManifest::sealed_wall_time_ns,
    );
    if sealed_wall_time_ns < metadata.created_wall_time_ns() {
        return Err(ManifestError::InvalidField("sealed_wall_time_ns").into());
    }
    validate_predecessor(ordinal, metadata.segment_id(), predecessor.as_ref())?;

    let mut stats = ScanStats::default();
    let summary = segment.verify_without_repair(|record| stats.observe(record))?;
    segment.sync_all()?;
    let segment_length = segment.file_mut().metadata()?.len();
    if summary.last_valid_offset() != segment_length {
        return Err(ManifestError::InvalidField("end_offset").into());
    }
    let segment_blake3 = hash_file(segment.file_mut())?;
    let prologue_length =
        u64::try_from(prologue::encode(&metadata)?.len()).map_err(|_| ManifestError::TooLarge)?;
    let (sequence_ranges, min_receive_wall_time_ns, max_receive_wall_time_ns) =
        stats.into_parts()?;
    let segment_file = utf8_file_name(&paths.sealed)?.to_owned();
    let mut manifest = SealedSegmentManifest {
        segment_ordinal: ordinal,
        segment_file,
        metadata,
        predecessor,
        sealed_wall_time_ns,
        segment_length,
        segment_blake3,
        prologue_length,
        record_count: summary.record_count(),
        first_record_offset: prologue_length,
        next_offset: summary.last_valid_offset(),
        sequence_ranges,
        min_receive_wall_time_ns,
        max_receive_wall_time_ns,
        manifest_blake3: [0_u8; 32],
    };
    validate_manifest(&manifest)?;
    manifest.manifest_blake3 = digest_body(&manifest.to_body_wire())?;
    let encoded = manifest.encode()?;
    prepare_pending_manifest_at(directory, &paths.pending, &encoded)?;
    verify_name_identity_at(directory, &paths.active, active_identity)?;
    rustix::fs::renameat_with(
        directory,
        &paths.active,
        directory,
        &paths.sealed,
        RenameFlags::NOREPLACE,
    )
    .map_err(|error| {
        if error == rustix::io::Errno::EXIST {
            SealingError::SealedExists
        } else {
            SealingError::Io(error.into())
        }
    })?;
    directory.sync_all()?;
    verify_name_identity_at(directory, &paths.sealed, active_identity)?;
    install_pending_manifest_at(directory, &paths.pending, &paths.manifest)?;
    sealed_result(
        manifest,
        display_directory.join(&paths.manifest),
        display_directory.join(&paths.sealed),
    )
}

pub fn verify_sealed_v2_segment(sealed_path: &Path) -> Result<SealedSegmentManifest, SealingError> {
    let manifest_path = manifest_path_for(sealed_path)?;
    let manifest_bytes = read_bounded(&manifest_path, MAX_MANIFEST_LENGTH)?;
    let manifest = decode_manifest(&manifest_bytes)?;
    if manifest.segment_file() != utf8_file_name(sealed_path)? {
        return Err(ManifestError::InvalidField("segment_file").into());
    }
    verify_sealed_against_manifest(sealed_path, &manifest)?;
    Ok(manifest)
}

pub(crate) fn verify_sealed_v2_segment_at(
    directory: &File,
    sealed_name: &Path,
) -> Result<SealedSegmentManifest, SealingError> {
    visit_verified_sealed_v2_segment_at(directory, sealed_name, |_| {})
}

pub(crate) fn visit_verified_sealed_v2_segment_at(
    directory: &File,
    sealed_name: &Path,
    visitor: impl FnMut(RecoveredRecord<'_>),
) -> Result<SealedSegmentManifest, SealingError> {
    let manifest_name = manifest_path_for(sealed_name)?;
    let manifest_bytes = read_bounded_at(directory, &manifest_name, MAX_MANIFEST_LENGTH)?;
    let manifest = decode_manifest(&manifest_bytes)?;
    if manifest.segment_file() != utf8_file_name(sealed_name)? {
        return Err(ManifestError::InvalidField("segment_file").into());
    }
    let file = open_nofollow_at(directory, sealed_name, OFlags::RDONLY)?;
    visit_sealed_file_against_manifest(file, &manifest, visitor)?;
    Ok(manifest)
}

fn verify_sealed_against_manifest(
    sealed_path: &Path,
    manifest: &SealedSegmentManifest,
) -> Result<(), SealingError> {
    let file = open_nofollow(sealed_path, OFlags::RDONLY)?;
    verify_sealed_file_against_manifest(file, manifest)
}

fn verify_sealed_file_against_manifest(
    file: File,
    manifest: &SealedSegmentManifest,
) -> Result<(), SealingError> {
    visit_sealed_file_against_manifest(file, manifest, |_| {})
}

fn visit_sealed_file_against_manifest(
    mut file: File,
    manifest: &SealedSegmentManifest,
    mut visitor: impl FnMut(RecoveredRecord<'_>),
) -> Result<(), SealingError> {
    acquire_shared_lock(&file)?;
    validate_regular_single_link(&file)?;
    let actual_length = file.metadata()?.len();
    if actual_length > maximum_segment_length(manifest.prologue_length)? {
        return Err(ManifestError::InvalidField("segment_bytes").into());
    }
    if actual_length != manifest.segment_length {
        return Err(ManifestError::InvalidField("segment_bytes").into());
    }
    let (metadata, prologue_length) = read_prologue(&mut file)?;
    if metadata != manifest.metadata || prologue_length != manifest.prologue_length {
        return Err(ManifestError::InvalidField("prologue").into());
    }
    let actual_blake3 = hash_file(&mut file)?;
    if actual_blake3 != manifest.segment_blake3 {
        return Err(ManifestError::InvalidField("segment_blake3").into());
    }

    let mut stats = ScanStats::default();
    let summary = recovery::verify_with_from(
        &mut file,
        prologue_length,
        Some(WalFormat::V2),
        Some(&metadata),
        |record| {
            stats.observe(record);
            visitor(record);
        },
    )?;
    let (ranges, minimum, maximum) = stats.into_parts()?;
    if summary.record_count() != manifest.record_count
        || summary.last_valid_offset() != manifest.next_offset
        || ranges != manifest.sequence_ranges
        || minimum != manifest.min_receive_wall_time_ns
        || maximum != manifest.max_receive_wall_time_ns
    {
        return Err(ManifestError::InvalidField("record_summary").into());
    }
    Ok(())
}

pub fn decode_manifest(bytes: &[u8]) -> Result<SealedSegmentManifest, ManifestError> {
    if bytes.len() > MAX_MANIFEST_LENGTH {
        return Err(ManifestError::TooLarge);
    }
    let wire: ManifestWire = serde_json::from_slice(bytes).map_err(json_error)?;
    let canonical = serde_json::to_vec(&wire).map_err(json_error)?;
    if canonical != bytes {
        return Err(ManifestError::NonCanonical);
    }
    let supplied_digest =
        parse_hex_array::<32>("manifest_body_blake3", &wire.manifest_body_blake3)?;
    if digest_body(&wire.body)? != supplied_digest {
        return Err(ManifestError::DigestMismatch);
    }
    manifest_from_wire(wire.body, supplied_digest)
}

fn manifest_from_wire(
    body: ManifestBodyWire,
    manifest_blake3: [u8; 32],
) -> Result<SealedSegmentManifest, ManifestError> {
    if body.manifest_schema_version != MANIFEST_SCHEMA_VERSION {
        return Err(ManifestError::InvalidField("manifest_schema_version"));
    }
    if body.wal_format_version != SCHEMA_VERSION {
        return Err(ManifestError::InvalidField("wal_format_version"));
    }
    let segment_id = parse_hex_array::<16>("segment_id", &body.segment_id)?;
    let segment_blake3 = parse_hex_array::<32>("segment_blake3", &body.segment_blake3)?;
    if !body
        .streams
        .windows(2)
        .all(|pair| pair[0].stream_id < pair[1].stream_id)
    {
        return Err(ManifestError::InvalidField("streams"));
    }
    let predecessor = match (body.predecessor_segment_id, body.predecessor_segment_blake3) {
        (None, None) => None,
        (Some(segment_id), Some(segment_blake3)) => Some(SegmentPredecessor::new(
            parse_hex_array::<16>("predecessor_segment_id", &segment_id)?,
            parse_hex_array::<32>("predecessor_segment_blake3", &segment_blake3)?,
        )?),
        _ => return Err(ManifestError::InvalidField("predecessor")),
    };
    let streams = body
        .streams
        .into_iter()
        .map(|stream| {
            StreamDescriptor::new(stream.stream_id, stream.source_name, stream.stream_name)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let metadata = SegmentMetadata::new(
        segment_id,
        body.created_wall_time_ns,
        body.schema_id,
        body.installation_id,
        body.build_id,
        streams,
    )?;
    let sequence_ranges = body
        .sequence_ranges
        .into_iter()
        .map(|range| SequenceRange {
            stream_id: range.stream_id,
            connection_epoch: range.connection_epoch,
            first_sequence: range.first_sequence,
            last_sequence: range.last_sequence,
            record_count: range.record_count,
        })
        .collect();
    let manifest = SealedSegmentManifest {
        segment_ordinal: body.segment_ordinal,
        segment_file: body.segment_file,
        metadata,
        predecessor,
        sealed_wall_time_ns: body.sealed_wall_time_ns,
        segment_length: body.segment_bytes,
        segment_blake3,
        prologue_length: body.prologue_bytes,
        record_count: body.record_count,
        first_record_offset: body.first_frame_offset,
        next_offset: body.end_offset,
        sequence_ranges,
        min_receive_wall_time_ns: body.min_receive_wall_time_ns,
        max_receive_wall_time_ns: body.max_receive_wall_time_ns,
        manifest_blake3,
    };
    validate_manifest(&manifest)?;
    Ok(manifest)
}

fn validate_manifest(manifest: &SealedSegmentManifest) -> Result<(), ManifestError> {
    if manifest.segment_ordinal == 0 {
        return Err(ManifestError::InvalidField("segment_ordinal"));
    }
    validate_predecessor(
        manifest.segment_ordinal,
        manifest.metadata.segment_id(),
        manifest.predecessor.as_ref(),
    )?;
    let expected_base = format!(
        "{:020}-{}",
        manifest.segment_ordinal,
        hex::encode(manifest.metadata.segment_id())
    );
    if manifest.segment_file != format!("{expected_base}{SEALED_SUFFIX}") {
        return Err(ManifestError::InvalidField("segment_file"));
    }
    if manifest.sealed_wall_time_ns < manifest.metadata.created_wall_time_ns() {
        return Err(ManifestError::InvalidField("sealed_wall_time_ns"));
    }
    let expected_prologue_length = u64::try_from(prologue::encode(&manifest.metadata)?.len())
        .map_err(|_| ManifestError::TooLarge)?;
    let maximum_segment_length = maximum_segment_length(expected_prologue_length)?;
    if manifest.prologue_length != expected_prologue_length
        || manifest.first_record_offset != manifest.prologue_length
        || manifest.next_offset != manifest.segment_length
        || manifest.next_offset < manifest.first_record_offset
        || manifest.segment_length > maximum_segment_length
    {
        return Err(ManifestError::InvalidField("offsets"));
    }
    let mut range_records = 0_u64;
    let mut previous_key = None;
    for range in &manifest.sequence_ranges {
        let key = (range.stream_id, range.connection_epoch);
        if previous_key.is_some_and(|previous| previous >= key)
            || !manifest.metadata.contains_stream(range.stream_id)
            || range.record_count == 0
            || range.first_sequence > range.last_sequence
        {
            return Err(ManifestError::InvalidField("sequence_ranges"));
        }
        range_records = range_records
            .checked_add(range.record_count)
            .ok_or(ManifestError::InvalidField("record_count"))?;
        previous_key = Some(key);
    }
    if range_records != manifest.record_count {
        return Err(ManifestError::InvalidField("record_count"));
    }
    let times_are_valid = if manifest.record_count == 0 {
        manifest.sequence_ranges.is_empty()
            && manifest.min_receive_wall_time_ns.is_none()
            && manifest.max_receive_wall_time_ns.is_none()
    } else {
        manifest
            .min_receive_wall_time_ns
            .zip(manifest.max_receive_wall_time_ns)
            .is_some_and(|(minimum, maximum)| minimum <= maximum)
    };
    if !times_are_valid {
        return Err(ManifestError::InvalidField("receive_wall_time"));
    }
    Ok(())
}

fn maximum_segment_length(prologue_length: u64) -> Result<u64, ManifestError> {
    MAX_RECOVERABLE_SEGMENT_LENGTH
        .checked_add(prologue_length)
        .ok_or(ManifestError::InvalidField("segment_bytes"))
}

fn validate_predecessor(
    segment_ordinal: u64,
    segment_id: &[u8; 16],
    predecessor: Option<&SegmentPredecessor>,
) -> Result<(), ManifestError> {
    if (segment_ordinal == 1) != predecessor.is_none() {
        return Err(ManifestError::InvalidField("predecessor"));
    }
    if predecessor.is_some_and(|predecessor| predecessor.segment_id == *segment_id) {
        return Err(ManifestError::InvalidField("predecessor_segment_id"));
    }
    Ok(())
}

fn digest_body(body: &ManifestBodyWire) -> Result<[u8; 32], ManifestError> {
    let encoded = serde_json::to_vec(body).map_err(json_error)?;
    if encoded.len() > MAX_MANIFEST_LENGTH {
        return Err(ManifestError::TooLarge);
    }
    Ok(*blake3::hash(&encoded).as_bytes())
}

fn hash_file(file: &mut File) -> Result<[u8; 32], io::Error> {
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(&mut *file)?;
    file.seek(SeekFrom::End(0))?;
    Ok(*hasher.finalize().as_bytes())
}

fn read_prologue(file: &mut File) -> Result<(SegmentMetadata, u64), SealingError> {
    let file_length = file.metadata()?.len();
    if file_length < 12 {
        return Err(PrologueError::Incomplete.into());
    }
    let mut prefix = [0_u8; 12];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut prefix)?;
    let header_length = prologue::declared_header_length(&prefix)?;
    if file_length < header_length as u64 {
        return Err(PrologueError::Incomplete.into());
    }
    let mut encoded = vec![0_u8; header_length];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut encoded)?;
    let decoded = prologue::decode(&encoded)?;
    Ok((
        decoded.into_metadata(),
        u64::try_from(header_length).map_err(|_| ManifestError::TooLarge)?,
    ))
}

fn sealed_result(
    manifest: SealedSegmentManifest,
    manifest_path: PathBuf,
    sealed_path: PathBuf,
) -> Result<SealedSegment, SealingError> {
    let destination_path =
        sealed_path.with_file_name(format!("{}.zst", utf8_file_name(&sealed_path)?));
    let compression_job = CompressionJob {
        manifest_path,
        source_path: sealed_path,
        destination_path,
        expected_uncompressed_bytes: manifest.segment_length,
        expected_uncompressed_blake3: manifest.segment_blake3,
    };
    Ok(SealedSegment {
        manifest,
        compression_job,
    })
}

pub(crate) fn compression_job_for_verified_segment(
    sealed_path: &Path,
    manifest: &SealedSegmentManifest,
) -> Result<CompressionJob, SealingError> {
    let manifest_path = manifest_path_for(sealed_path)?;
    sealed_result(manifest.clone(), manifest_path, sealed_path.to_owned())
        .map(|sealed| sealed.compression_job)
}

fn resume_pending_seal(
    sealed_path: &Path,
    manifest_path: &Path,
    pending_path: &Path,
) -> Result<SealedSegment, SealingError> {
    let encoded = read_bounded(pending_path, MAX_MANIFEST_LENGTH)?;
    let manifest = decode_manifest(&encoded)?;
    if manifest.segment_file() != utf8_file_name(sealed_path)? {
        return Err(ManifestError::InvalidField("segment_file").into());
    }
    verify_sealed_against_manifest(sealed_path, &manifest)?;
    install_pending_manifest(pending_path, manifest_path)?;
    sealed_result(manifest, manifest_path.to_owned(), sealed_path.to_owned())
}

fn resume_pending_seal_at(
    directory: &File,
    display_directory: &Path,
    paths: &SealPaths,
) -> Result<SealedSegment, SealingError> {
    let encoded = read_bounded_at(directory, &paths.pending, MAX_MANIFEST_LENGTH)?;
    let manifest = decode_manifest(&encoded)?;
    if manifest.segment_file() != utf8_file_name(&paths.sealed)? {
        return Err(ManifestError::InvalidField("segment_file").into());
    }
    let file = open_nofollow_at(directory, &paths.sealed, OFlags::RDONLY)?;
    verify_sealed_file_against_manifest(file, &manifest)?;
    install_pending_manifest_at(directory, &paths.pending, &paths.manifest)?;
    sealed_result(
        manifest,
        display_directory.join(&paths.manifest),
        display_directory.join(&paths.sealed),
    )
}

fn prepare_pending_manifest(path: &Path, expected: &[u8]) -> Result<(), SealingError> {
    match publish_new_file(path, expected) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let existing = read_bounded(path, MAX_MANIFEST_LENGTH)?;
            if existing == expected {
                Ok(())
            } else {
                Err(SealingError::PendingConflict)
            }
        }
        Err(error) => Err(error.into()),
    }
}

fn prepare_pending_manifest_at(
    directory: &File,
    name: &Path,
    expected: &[u8],
) -> Result<(), SealingError> {
    match publish_new_file_at(directory, name, expected) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let existing = read_bounded_at(directory, name, MAX_MANIFEST_LENGTH)?;
            if existing == expected {
                Ok(())
            } else {
                Err(SealingError::PendingConflict)
            }
        }
        Err(error) => Err(error.into()),
    }
}

fn install_pending_manifest(pending_path: &Path, manifest_path: &Path) -> Result<(), SealingError> {
    rustix::fs::renameat_with(
        CWD,
        pending_path,
        CWD,
        manifest_path,
        RenameFlags::NOREPLACE,
    )
    .map_err(|error| {
        if error == rustix::io::Errno::EXIST {
            SealingError::ManifestExists
        } else {
            SealingError::Io(error.into())
        }
    })?;
    File::open(usable_parent(manifest_path)?)?.sync_all()?;
    Ok(())
}

fn install_pending_manifest_at(
    directory: &File,
    pending_name: &Path,
    manifest_name: &Path,
) -> Result<(), SealingError> {
    rustix::fs::renameat_with(
        directory,
        pending_name,
        directory,
        manifest_name,
        RenameFlags::NOREPLACE,
    )
    .map_err(|error| {
        if error == rustix::io::Errno::EXIST {
            SealingError::ManifestExists
        } else {
            SealingError::Io(error.into())
        }
    })?;
    directory.sync_all()?;
    Ok(())
}

fn publish_new_file(path: &Path, bytes: &[u8]) -> Result<(), io::Error> {
    let parent = usable_parent(path)?;
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let mut temporary = create_temporary_file(parent, file_name)?;
    temporary.file.write_all(bytes)?;
    temporary.file.sync_all()?;
    rustix::fs::renameat_with(CWD, &temporary.path, CWD, path, RenameFlags::NOREPLACE)
        .map_err(io::Error::from)?;
    temporary.installed = true;
    File::open(parent)?.sync_all()
}

fn publish_new_file_at(directory: &File, final_name: &Path, bytes: &[u8]) -> io::Result<()> {
    let process_id = std::process::id();
    let first_sequence = TEMP_SEQUENCE.fetch_add(TEMP_CREATE_ATTEMPTS, Ordering::Relaxed);
    for attempt in 0..TEMP_CREATE_ATTEMPTS {
        let mut temporary_name = OsString::from(".");
        temporary_name.push(final_name.as_os_str());
        temporary_name.push(format!(
            ".create-{process_id}-{}.tmp",
            first_sequence.wrapping_add(attempt)
        ));
        let file = match rustix::fs::openat(
            directory,
            &temporary_name,
            OFlags::CREATE | OFlags::EXCL | OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        ) {
            Ok(file) => File::from(file),
            Err(error) if error == rustix::io::Errno::EXIST => continue,
            Err(error) => return Err(error.into()),
        };
        let mut file = file;
        let result = (|| -> io::Result<()> {
            file.write_all(bytes)?;
            file.sync_all()?;
            rustix::fs::renameat_with(
                directory,
                &temporary_name,
                directory,
                final_name,
                RenameFlags::NOREPLACE,
            )
            .map_err(io::Error::from)?;
            directory.sync_all()
        })();
        if result.is_err() {
            let _ = rustix::fs::unlinkat(directory, &temporary_name, AtFlags::empty());
        }
        return result;
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary manifest name",
    ))
}

struct TemporaryFile {
    path: PathBuf,
    file: File,
    installed: bool,
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if !self.installed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn create_temporary_file(parent: &Path, final_name: &std::ffi::OsStr) -> io::Result<TemporaryFile> {
    let process_id = std::process::id();
    let first_sequence = TEMP_SEQUENCE.fetch_add(TEMP_CREATE_ATTEMPTS, Ordering::Relaxed);
    for attempt in 0..TEMP_CREATE_ATTEMPTS {
        let mut temporary_name = OsString::from(".");
        temporary_name.push(final_name);
        temporary_name.push(format!(
            ".create-{process_id}-{}.tmp",
            first_sequence.wrapping_add(attempt)
        ));
        let path = parent.join(temporary_name);
        match OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => {
                return Ok(TemporaryFile {
                    path,
                    file,
                    installed: false,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary manifest path",
    ))
}

fn read_bounded(path: &Path, maximum: usize) -> Result<Vec<u8>, SealingError> {
    let mut file = open_nofollow(path, OFlags::RDONLY).map_err(|error| {
        if matches!(
            &error,
            SealingError::Io(source) if source.kind() == io::ErrorKind::NotFound
        ) {
            SealingError::ManifestMissing
        } else {
            error
        }
    })?;
    validate_regular_single_link(&file)?;
    let metadata = file.metadata()?;
    let length = metadata.len();
    if length > maximum as u64 {
        return Err(ManifestError::TooLarge.into());
    }
    let length = usize::try_from(length).map_err(|_| ManifestError::TooLarge)?;
    let mut bytes = vec![0_u8; length];
    file.read_exact(&mut bytes)?;
    let mut trailing = [0_u8; 1];
    if file.read(&mut trailing)? != 0 {
        return Err(ManifestError::InvalidField("manifest_length").into());
    }
    Ok(bytes)
}

fn read_bounded_at(directory: &File, name: &Path, maximum: usize) -> Result<Vec<u8>, SealingError> {
    let mut file = open_nofollow_at(directory, name, OFlags::RDONLY).map_err(|error| {
        if matches!(
            &error,
            SealingError::Io(source) if source.kind() == io::ErrorKind::NotFound
        ) {
            SealingError::ManifestMissing
        } else {
            error
        }
    })?;
    validate_regular_single_link(&file)?;
    let length = file.metadata()?.len();
    if length > maximum as u64 {
        return Err(ManifestError::TooLarge.into());
    }
    let length = usize::try_from(length).map_err(|_| ManifestError::TooLarge)?;
    let mut bytes = vec![0_u8; length];
    file.read_exact(&mut bytes)?;
    let mut trailing = [0_u8; 1];
    if file.read(&mut trailing)? != 0 {
        return Err(ManifestError::InvalidField("manifest_length").into());
    }
    Ok(bytes)
}

type FileIdentity = rustix::fs::Stat;

fn open_nofollow(path: &Path, access: OFlags) -> Result<File, SealingError> {
    rustix::fs::openat(
        CWD,
        path,
        access | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| {
        if error == rustix::io::Errno::LOOP {
            SealingError::UnsafeFile
        } else {
            SealingError::Io(error.into())
        }
    })
}

fn open_nofollow_at(directory: &File, name: &Path, access: OFlags) -> Result<File, SealingError> {
    rustix::fs::openat(
        directory,
        name,
        access | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| {
        if error == rustix::io::Errno::LOOP {
            SealingError::UnsafeFile
        } else {
            SealingError::Io(error.into())
        }
    })
}

fn validate_regular_single_link(file: &File) -> Result<FileIdentity, SealingError> {
    let stat = rustix::fs::fstat(file).map_err(io::Error::from)?;
    if !FileType::from_raw_mode(stat.st_mode).is_file() || stat.st_nlink != 1 {
        return Err(SealingError::UnsafeFile);
    }
    Ok(stat)
}

fn verify_path_identity(path: &Path, expected: FileIdentity) -> Result<(), SealingError> {
    let actual =
        rustix::fs::statat(CWD, path, AtFlags::SYMLINK_NOFOLLOW).map_err(io::Error::from)?;
    if !FileType::from_raw_mode(actual.st_mode).is_file()
        || actual.st_nlink != 1
        || actual.st_dev != expected.st_dev
        || actual.st_ino != expected.st_ino
    {
        return Err(SealingError::UnsafeFile);
    }
    Ok(())
}

fn verify_name_identity_at(
    directory: &File,
    name: &Path,
    expected: FileIdentity,
) -> Result<(), SealingError> {
    let actual =
        rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW).map_err(io::Error::from)?;
    if !FileType::from_raw_mode(actual.st_mode).is_file()
        || actual.st_nlink != 1
        || actual.st_dev != expected.st_dev
        || actual.st_ino != expected.st_ino
    {
        return Err(SealingError::UnsafeFile);
    }
    Ok(())
}

fn path_entry_exists(path: &Path) -> Result<bool, io::Error> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn path_entry_exists_at(directory: &File, name: &Path) -> Result<bool, io::Error> {
    match rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => Ok(true),
        Err(error) if error == rustix::io::Errno::NOENT => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn acquire_shared_lock(file: &File) -> Result<(), SealingError> {
    match rustix::fs::flock(file, FlockOperation::NonBlockingLockShared) {
        Ok(()) => Ok(()),
        Err(error) if error == rustix::io::Errno::WOULDBLOCK => Err(SealingError::AlreadyOpen),
        Err(error) => Err(io::Error::from(error).into()),
    }
}

fn parse_active_identity(path: &Path) -> Option<(u64, [u8; 16])> {
    let file_name = path.file_name()?.to_str()?;
    let base = file_name.strip_suffix(ACTIVE_SUFFIX)?;
    parse_segment_base(base).ok()
}

fn parse_segment_base(base: &str) -> Result<(u64, [u8; 16]), SealingError> {
    if base.len() != 20 + 1 + 32 || base.as_bytes().get(20) != Some(&b'-') {
        return Err(SealingError::InvalidPath);
    }
    let ordinal = base[..20]
        .parse::<u64>()
        .map_err(|_| SealingError::InvalidPath)?;
    if ordinal == 0 || format!("{ordinal:020}") != base[..20] {
        return Err(SealingError::InvalidPath);
    }
    let segment_id =
        parse_hex_array::<16>("segment_id", &base[21..]).map_err(SealingError::Manifest)?;
    if segment_id == [0_u8; 16] {
        return Err(SealingError::InvalidPath);
    }
    Ok((ordinal, segment_id))
}

fn parse_hex_array<const N: usize>(
    field: &'static str,
    value: &str,
) -> Result<[u8; N], ManifestError> {
    if value.len() != N * 2
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ManifestError::InvalidField(field));
    }
    let decoded = hex::decode(value).map_err(|_| ManifestError::InvalidField(field))?;
    decoded
        .try_into()
        .map_err(|_| ManifestError::InvalidField(field))
}

fn utf8_file_name(path: &Path) -> Result<&str, SealingError> {
    path.file_name()
        .and_then(|name| name.to_str())
        .ok_or(SealingError::InvalidPath)
}

fn usable_parent(path: &Path) -> Result<&Path, io::Error> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    Ok(if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    })
}

fn json_error(error: serde_json::Error) -> ManifestError {
    ManifestError::Json(format!("{:?}", error.classify()))
}
