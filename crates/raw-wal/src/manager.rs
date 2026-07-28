//! Directory-scoped segmented WAL ownership, rotation, and clean-chain recovery.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs::File,
    io,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use rustix::fs::{AtFlags, CWD, Dir, FileType, FlockOperation, Mode, OFlags};
use thiserror::Error;

use crate::{
    frame::{self, FrameError, RecordMetadata},
    prologue::{self, PrologueError, SegmentMetadata},
    recovery::{RecoveredRecord, RecoveryError},
    seal::{
        CompressionJob, ManifestError, SealedSegmentManifest, SealingError, SegmentPredecessor,
        compression_job_for_verified_segment, manifest_path_for,
        read_pending_manifest_for_recovery_at, seal_owned_v2_segment_at, seal_v2_segment_at,
        sealed_path_for, verify_sealed_v2_segment_at, visit_verified_sealed_v2_segment_at,
    },
    segment::{Segment, SegmentError},
};

const ACTIVE_SUFFIX: &str = ".active.wal";
const SEALED_SUFFIX: &str = ".wal";
const MANIFEST_SUFFIX: &str = ".seal.json";
const PENDING_MANIFEST_SUFFIX: &str = ".seal.pending";
const SUCCESSOR_ID_DOMAIN: &[u8] = b"cmti-wal-successor-id-v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RotationPolicy {
    max_segment_bytes: u64,
    max_segment_age_ns: u64,
}

impl RotationPolicy {
    pub const DEFAULT_MAX_SEGMENT_BYTES: u64 = 256 * 1024 * 1024;
    pub const DEFAULT_MAX_SEGMENT_AGE_NS: u64 = 300 * 1_000_000_000;

    pub fn new(max_segment_bytes: u64, max_segment_age_ns: u64) -> Result<Self, ManagerError> {
        if max_segment_bytes == 0 || max_segment_age_ns == 0 {
            return Err(ManagerError::InvalidPolicy);
        }
        Ok(Self {
            max_segment_bytes,
            max_segment_age_ns,
        })
    }

    pub const fn max_segment_bytes(self) -> u64 {
        self.max_segment_bytes
    }

    pub const fn max_segment_age_ns(self) -> u64 {
        self.max_segment_age_ns
    }
}

impl Default for RotationPolicy {
    fn default() -> Self {
        Self {
            max_segment_bytes: Self::DEFAULT_MAX_SEGMENT_BYTES,
            max_segment_age_ns: Self::DEFAULT_MAX_SEGMENT_AGE_NS,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WalPosition {
    segment_ordinal: u64,
    segment_id: [u8; 16],
    frame_offset: u64,
    next_offset: u64,
}

impl WalPosition {
    pub const fn segment_ordinal(self) -> u64 {
        self.segment_ordinal
    }

    pub const fn segment_id(&self) -> &[u8; 16] {
        &self.segment_id
    }

    pub const fn frame_offset(self) -> u64 {
        self.frame_offset
    }

    pub const fn next_offset(self) -> u64 {
        self.next_offset
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppendOutcome {
    position: WalPosition,
    compression_job: Option<CompressionJob>,
}

impl AppendOutcome {
    pub const fn position(&self) -> WalPosition {
        self.position
    }

    pub const fn compression_job(&self) -> Option<&CompressionJob> {
        self.compression_job.as_ref()
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum InventoryError {
    #[error("managed WAL directory contains an unsafe artifact")]
    UnsafeArtifact,
    #[error("managed WAL directory contains an unknown artifact")]
    UnknownArtifact,
    #[error("managed WAL directory is not empty")]
    NotEmpty,
    #[error("managed WAL directory contains multiple active segments")]
    MultipleActiveSegments,
    #[error("managed WAL directory contains multiple pending seal transitions")]
    MultiplePendingSeals,
    #[error("managed WAL pending seal transition has an invalid artifact set")]
    InvalidPendingSealState,
    #[error("managed WAL directory has no active segment")]
    MissingActiveSegment,
    #[error("managed WAL chain expected ordinal {expected}, found {actual}")]
    ChainGap { expected: u64, actual: u64 },
    #[error("managed WAL chain predecessor does not match at ordinal {ordinal}")]
    PredecessorMismatch { ordinal: u64 },
    #[error("managed WAL active identity does not match its chain position")]
    ActiveIdentityMismatch,
    #[error("managed WAL segment identity does not match its chain position at ordinal {ordinal}")]
    SegmentIdentityMismatch { ordinal: u64 },
    #[error("managed WAL segment metadata changed unexpectedly at ordinal {ordinal}")]
    SegmentMetadataMismatch { ordinal: u64 },
    #[error("managed WAL chain repeats a segment ID")]
    DuplicateSegmentId,
    #[error("managed WAL manifest has no sealed segment")]
    ManifestWithoutSegment,
    #[error("managed WAL sequence regressed across segments")]
    CrossSegmentSequenceRegression,
}

#[derive(Debug, Error)]
pub enum ManagerError {
    #[error("WAL rotation policy requires nonzero byte and age limits")]
    InvalidPolicy,
    #[error("segmented WAL manager already has a live owner")]
    AlreadyOpen,
    #[error("segmented WAL directory pathname no longer identifies the locked directory")]
    DirectoryReplaced,
    #[error(
        "WAL segment {sealed_ordinal} rotated durably in the locked directory, but its pathname was replaced before a safe compression handoff"
    )]
    DirectoryReplacedAfterRotation { sealed_ordinal: u64 },
    #[error(
        "WAL rotation of segment {sealed_ordinal} and append at {position:?} committed durably, but the directory pathname was replaced before a safe compression handoff"
    )]
    DirectoryReplacedAfterAppend {
        sealed_ordinal: u64,
        position: WalPosition,
    },
    #[error(
        "WAL recovery may have reconciled durable state in the locked directory, but its pathname was replaced before safe compression handoff"
    )]
    DirectoryReplacedAfterRecovery,
    #[error("segmented WAL monotonic clock regressed from {previous} to {current}")]
    ClockRegression { previous: u64, current: u64 },
    #[error(
        "WAL record sequence regressed across segments for stream {stream_id} epoch {connection_epoch}: previous {previous}, current {current}"
    )]
    SequenceRegression {
        stream_id: u32,
        connection_epoch: u64,
        previous: u64,
        current: u64,
    },
    #[error("segmented WAL manager has no writable active segment")]
    Inactive,
    #[error("WAL segment sealed durably but successor creation failed: {source}")]
    SuccessorCreation {
        compression_job: Box<CompressionJob>,
        #[source]
        source: Box<SegmentError>,
    },
    #[error("WAL segment rotated durably but the pending append failed: {source}")]
    PostRotationAppend {
        compression_job: Box<CompressionJob>,
        #[source]
        source: Box<SegmentError>,
    },
    #[error("segmented WAL ordinal overflow")]
    OrdinalOverflow,
    #[error("segmented WAL inventory is invalid: {0}")]
    Inventory(#[from] InventoryError),
    #[error("segmented WAL frame is invalid: {0}")]
    Frame(#[from] FrameError),
    #[error("segmented WAL prologue is invalid: {0}")]
    Prologue(#[from] PrologueError),
    #[error("segmented WAL recovery failed: {0}")]
    Recovery(#[from] RecoveryError),
    #[error("segmented WAL segment failed: {0}")]
    Segment(#[from] SegmentError),
    #[error("segmented WAL sealing failed: {0}")]
    Sealing(#[from] SealingError),
    #[error("segmented WAL manifest is invalid: {0}")]
    Manifest(#[from] ManifestError),
    #[error("segmented WAL I/O failed: {0}")]
    Io(#[from] io::Error),
}

pub struct SegmentedWalWriter {
    directory_lock: File,
    directory: PathBuf,
    policy: RotationPolicy,
    active: Option<Segment>,
    active_name: PathBuf,
    active_path: PathBuf,
    active_metadata: SegmentMetadata,
    active_ordinal: u64,
    active_opened_monotonic_ns: u64,
    active_length: u64,
    active_record_count: u64,
    last_sequences: BTreeMap<(u32, u64), u64>,
    predecessor: Option<SegmentPredecessor>,
    sealed_names: Vec<PathBuf>,
    recovered_compression_jobs: Vec<CompressionJob>,
}

impl SegmentedWalWriter {
    pub fn create(
        directory: &Path,
        metadata: SegmentMetadata,
        policy: RotationPolicy,
        opened_monotonic_ns: u64,
    ) -> Result<Self, ManagerError> {
        let directory_lock = acquire_directory_lock(directory)?;
        Self::create_locked(
            directory_lock,
            directory,
            metadata,
            policy,
            opened_monotonic_ns,
        )
    }

    pub fn open_or_create_in(
        directory: File,
        display_path: &Path,
        initial_metadata: SegmentMetadata,
        policy: RotationPolicy,
        now_monotonic_ns: u64,
    ) -> Result<Self, ManagerError> {
        let directory_lock = lock_directory_file(directory)?;
        ensure_directory_path_identity(display_path, &directory_lock)?;
        if directory_entry_names(&directory_lock)?.is_empty() {
            Self::create_locked(
                directory_lock,
                display_path,
                initial_metadata,
                policy,
                now_monotonic_ns,
            )
        } else {
            Self::recover_locked(directory_lock, display_path, policy, now_monotonic_ns)
        }
    }

    fn create_locked(
        directory_lock: File,
        directory: &Path,
        metadata: SegmentMetadata,
        policy: RotationPolicy,
        opened_monotonic_ns: u64,
    ) -> Result<Self, ManagerError> {
        ensure_directory_path_identity(directory, &directory_lock)?;
        ensure_new_directory(&directory_lock)?;
        let active_name = active_name(1, metadata.segment_id());
        let active_path = directory.join(&active_name);
        let active_length = u64::try_from(prologue::encode(&metadata)?.len())
            .map_err(|_| ManagerError::Inactive)?;
        let active =
            Segment::create_v2_at(&directory_lock, active_name.as_os_str(), metadata.clone())?;
        Ok(Self {
            directory_lock,
            directory: directory.to_owned(),
            policy,
            active: Some(active),
            active_name,
            active_path,
            active_metadata: metadata,
            active_ordinal: 1,
            active_opened_monotonic_ns: opened_monotonic_ns,
            active_length,
            active_record_count: 0,
            last_sequences: BTreeMap::new(),
            predecessor: None,
            sealed_names: Vec::new(),
            recovered_compression_jobs: Vec::new(),
        })
    }

    pub fn recover(
        directory: &Path,
        policy: RotationPolicy,
        now_monotonic_ns: u64,
    ) -> Result<Self, ManagerError> {
        let directory_lock = acquire_directory_lock(directory)?;
        Self::recover_locked(directory_lock, directory, policy, now_monotonic_ns)
    }

    fn recover_locked(
        directory_lock: File,
        directory: &Path,
        policy: RotationPolicy,
        now_monotonic_ns: u64,
    ) -> Result<Self, ManagerError> {
        ensure_directory_path_identity(directory, &directory_lock)?;
        let mut inventory = inventory(&directory_lock)?;
        let temporary_paths = std::mem::take(&mut inventory.temporary);
        let pending_transition = take_pending_transition(&directory_lock, &mut inventory)?;
        let mut manifests = Vec::with_capacity(inventory.sealed.len());
        let mut recovered_compression_jobs = Vec::with_capacity(inventory.sealed.len() + 1);
        let mut manifest_names = BTreeSet::new();
        for path in &inventory.manifests {
            manifest_names.insert(
                path.file_name()
                    .and_then(|name| name.to_str())
                    .ok_or(InventoryError::UnknownArtifact)?
                    .to_owned(),
            );
        }
        for sealed_name in inventory.sealed {
            let expected_manifest = manifest_path_for(&sealed_name)?;
            let expected_name = expected_manifest
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or(InventoryError::UnknownArtifact)?;
            if !manifest_names.remove(expected_name) {
                return Err(SealingError::ManifestMissing.into());
            }
            let manifest = verify_sealed_v2_segment_at(&directory_lock, &sealed_name)?;
            let sealed_path = directory.join(&sealed_name);
            recovered_compression_jobs.push(compression_job_for_verified_segment(
                &sealed_path,
                &manifest,
            )?);
            manifests.push(manifest);
        }
        if !manifest_names.is_empty() {
            return Err(InventoryError::ManifestWithoutSegment.into());
        }
        manifests.sort_unstable_by_key(SealedSegmentManifest::segment_ordinal);
        let mut segment_ids = BTreeSet::new();
        let mut last_sequences = BTreeMap::new();
        let mut previous_manifest: Option<&SealedSegmentManifest> = None;
        for (index, manifest) in manifests.iter().enumerate() {
            let expected_ordinal =
                u64::try_from(index + 1).map_err(|_| ManagerError::OrdinalOverflow)?;
            if manifest.segment_ordinal() != expected_ordinal {
                return Err(InventoryError::ChainGap {
                    expected: expected_ordinal,
                    actual: manifest.segment_ordinal(),
                }
                .into());
            }
            if !segment_ids.insert(*manifest.segment_id()) {
                return Err(InventoryError::DuplicateSegmentId.into());
            }
            if let Some(previous) = previous_manifest {
                if manifest.segment_id()
                    != &derive_successor_id(previous.segment_id(), expected_ordinal)
                {
                    return Err(InventoryError::SegmentIdentityMismatch {
                        ordinal: expected_ordinal,
                    }
                    .into());
                }
                let predecessor =
                    manifest
                        .predecessor()
                        .ok_or(InventoryError::PredecessorMismatch {
                            ordinal: expected_ordinal,
                        })?;
                if predecessor.segment_id() != previous.segment_id()
                    || predecessor.segment_blake3() != previous.segment_blake3()
                {
                    return Err(InventoryError::PredecessorMismatch {
                        ordinal: expected_ordinal,
                    }
                    .into());
                }
                validate_manifest_metadata_continuity(previous, manifest, expected_ordinal)?;
            } else if manifest.predecessor().is_some() {
                return Err(InventoryError::PredecessorMismatch {
                    ordinal: expected_ordinal,
                }
                .into());
            }
            merge_manifest_sequences(&mut last_sequences, manifest)?;
            previous_manifest = Some(manifest);
        }

        if let Some(transition) = pending_transition {
            if !inventory.active.is_empty() {
                return Err(InventoryError::InvalidPendingSealState.into());
            }
            let expected_ordinal =
                u64::try_from(manifests.len() + 1).map_err(|_| ManagerError::OrdinalOverflow)?;
            validate_pending_transition(&transition, manifests.last(), expected_ordinal)?;
            let mut pending_sequences = last_sequences.clone();
            merge_manifest_sequences(&mut pending_sequences, &transition.manifest)?;
            let predecessor = manifests.last().map(SealedSegmentManifest::as_predecessor);
            let sealed = seal_v2_segment_at(
                &directory_lock,
                directory,
                &transition.active_path,
                predecessor,
                transition.manifest.sealed_wall_time_ns(),
            )?;
            let manifest = sealed.manifest().clone();
            last_sequences = pending_sequences;
            recovered_compression_jobs.push(sealed.compression_job().clone());
            manifests.push(manifest);
        }

        if inventory.active.len() > 1 {
            return Err(InventoryError::MultipleActiveSegments.into());
        }
        let active_name = if let Some(active_name) = inventory.active.into_iter().next() {
            active_name
        } else if let Some(previous) = manifests.last() {
            let expected_ordinal = previous
                .segment_ordinal()
                .checked_add(1)
                .ok_or(ManagerError::OrdinalOverflow)?;
            let next_segment_id = derive_successor_id(previous.segment_id(), expected_ordinal);
            let next_metadata = successor_metadata(previous, next_segment_id)?;
            let name = active_name(expected_ordinal, &next_segment_id);
            drop(Segment::create_v2_at(
                &directory_lock,
                name.as_os_str(),
                next_metadata,
            )?);
            name
        } else {
            return Err(InventoryError::MissingActiveSegment.into());
        };
        let expected_ordinal =
            u64::try_from(manifests.len() + 1).map_err(|_| ManagerError::OrdinalOverflow)?;
        let (active_ordinal, filename_segment_id) =
            parse_active_identity(&active_name).ok_or(InventoryError::ActiveIdentityMismatch)?;
        if active_ordinal != expected_ordinal {
            return Err(InventoryError::ActiveIdentityMismatch.into());
        }
        if let Some(previous) = manifests.last()
            && filename_segment_id != derive_successor_id(previous.segment_id(), expected_ordinal)
        {
            return Err(InventoryError::ActiveIdentityMismatch.into());
        }
        if !segment_ids.insert(filename_segment_id) {
            return Err(InventoryError::DuplicateSegmentId.into());
        }

        let active_file = open_managed_file_at(&directory_lock, &active_name, OFlags::RDWR)?;
        let mut active = Segment::open_v2_file(active_file)?;
        let active_metadata = active
            .segment_metadata()
            .cloned()
            .ok_or(InventoryError::ActiveIdentityMismatch)?;
        if active_metadata.segment_id() != &filename_segment_id {
            return Err(InventoryError::ActiveIdentityMismatch.into());
        }
        if let Some(previous) = manifests.last() {
            validate_active_metadata_continuity(previous, &active_metadata, active_ordinal)?;
        }
        let mut verified_sequences = last_sequences.clone();
        let mut sequence_regressed = false;
        let _ = active.verify_without_repair(|record| {
            merge_active_record(&mut verified_sequences, record, &mut sequence_regressed);
        });
        if sequence_regressed {
            return Err(InventoryError::CrossSegmentSequenceRegression.into());
        }
        let mut recovery_regressed = false;
        let summary = active.recover_with(|record| {
            merge_active_record(&mut last_sequences, record, &mut recovery_regressed);
        })?;
        if recovery_regressed {
            return Err(InventoryError::CrossSegmentSequenceRegression.into());
        }
        let predecessor = manifests.last().map(SealedSegmentManifest::as_predecessor);
        let sealed_names = manifests
            .iter()
            .map(|manifest| PathBuf::from(manifest.segment_file()))
            .collect();
        cleanup_temporary_files(&directory_lock, &temporary_paths)?;
        let active_path = directory.join(&active_name);
        if ensure_directory_path_identity(directory, &directory_lock).is_err() {
            return Err(ManagerError::DirectoryReplacedAfterRecovery);
        }
        Ok(Self {
            directory_lock,
            directory: directory.to_owned(),
            policy,
            active: Some(active),
            active_name,
            active_path,
            active_metadata,
            active_ordinal,
            active_opened_monotonic_ns: now_monotonic_ns,
            active_length: summary.last_valid_offset(),
            active_record_count: summary.record_count(),
            last_sequences,
            predecessor,
            sealed_names,
            recovered_compression_jobs,
        })
    }

    pub fn append(
        &mut self,
        metadata: RecordMetadata,
        payload: &[u8],
        now_monotonic_ns: u64,
        wall_time_ns: i64,
    ) -> Result<AppendOutcome, ManagerError> {
        self.ensure_directory_identity()?;
        self.validate_global_sequence(metadata)?;
        if !self.active_metadata.contains_stream(metadata.stream_id) {
            return Err(SegmentError::UndeclaredStream(metadata.stream_id).into());
        }
        let frame_length = u64::try_from(frame::encoded_length(metadata, payload.len())?)
            .map_err(|_| ManagerError::Inactive)?;
        let projected_length = self
            .active_length
            .checked_add(frame_length)
            .ok_or(ManagerError::Inactive)?;
        let age_due = self.rotation_age_due(now_monotonic_ns)?;
        let bytes_due =
            self.active_record_count > 0 && projected_length > self.policy.max_segment_bytes;
        let mut compression_job = if age_due || bytes_due {
            Some(self.rotate(now_monotonic_ns, wall_time_ns)?)
        } else {
            None
        };

        let durable = match self
            .active
            .as_mut()
            .ok_or(ManagerError::Inactive)?
            .append_record_synced(metadata, payload)
        {
            Ok(durable) => durable,
            Err(source) => {
                if let Some(compression_job) = compression_job.take() {
                    let sealed_ordinal = self.active_ordinal.saturating_sub(1);
                    if self.ensure_directory_identity().is_err() {
                        return Err(ManagerError::DirectoryReplacedAfterRotation {
                            sealed_ordinal,
                        });
                    }
                    return Err(ManagerError::PostRotationAppend {
                        compression_job: Box::new(compression_job),
                        source: Box::new(source),
                    });
                }
                return Err(source.into());
            }
        };
        self.active_length = durable.next_offset();
        self.active_record_count = self
            .active_record_count
            .checked_add(1)
            .ok_or(ManagerError::Inactive)?;
        self.last_sequences.insert(
            (metadata.stream_id, metadata.connection_epoch),
            metadata.record_sequence,
        );
        let outcome = AppendOutcome {
            position: WalPosition {
                segment_ordinal: self.active_ordinal,
                segment_id: *durable.segment_id(),
                frame_offset: durable.frame_offset(),
                next_offset: durable.next_offset(),
            },
            compression_job,
        };
        if outcome.compression_job.is_some() && self.ensure_directory_identity().is_err() {
            return Err(ManagerError::DirectoryReplacedAfterAppend {
                sealed_ordinal: outcome.position.segment_ordinal.saturating_sub(1),
                position: outcome.position,
            });
        }
        Ok(outcome)
    }

    pub fn poll_rotation(
        &mut self,
        now_monotonic_ns: u64,
        wall_time_ns: i64,
    ) -> Result<Option<CompressionJob>, ManagerError> {
        self.ensure_directory_identity()?;
        if !self.rotation_age_due(now_monotonic_ns)? {
            return Ok(None);
        }
        self.rotate(now_monotonic_ns, wall_time_ns).map(Some)
    }

    pub const fn active_ordinal(&self) -> u64 {
        self.active_ordinal
    }

    pub const fn active_record_count(&self) -> u64 {
        self.active_record_count
    }

    pub const fn active_length(&self) -> u64 {
        self.active_length
    }

    pub fn active_path(&self) -> Result<&Path, ManagerError> {
        self.ensure_directory_identity()?;
        Ok(&self.active_path)
    }

    pub fn take_recovered_compression_jobs(&mut self) -> Result<Vec<CompressionJob>, ManagerError> {
        self.ensure_directory_identity()?;
        Ok(std::mem::take(&mut self.recovered_compression_jobs))
    }

    pub fn sync(&mut self) -> Result<(), ManagerError> {
        self.ensure_directory_identity()?;
        self.active.as_mut().ok_or(ManagerError::Inactive)?.sync()?;
        Ok(())
    }

    pub fn visit_records(
        &mut self,
        mut visitor: impl FnMut(RecoveredRecord<'_>),
    ) -> Result<(), ManagerError> {
        self.ensure_directory_identity()?;
        for sealed_name in &self.sealed_names {
            visit_verified_sealed_v2_segment_at(&self.directory_lock, sealed_name, &mut visitor)?;
        }
        self.active
            .as_mut()
            .ok_or(ManagerError::Inactive)?
            .verify_without_repair(visitor)?;
        Ok(())
    }

    fn validate_global_sequence(&self, metadata: RecordMetadata) -> Result<(), ManagerError> {
        let key = (metadata.stream_id, metadata.connection_epoch);
        if let Some(previous) = self.last_sequences.get(&key).copied()
            && metadata.record_sequence <= previous
        {
            return Err(ManagerError::SequenceRegression {
                stream_id: metadata.stream_id,
                connection_epoch: metadata.connection_epoch,
                previous,
                current: metadata.record_sequence,
            });
        }
        Ok(())
    }

    fn ensure_directory_identity(&self) -> Result<(), ManagerError> {
        ensure_directory_path_identity(&self.directory, &self.directory_lock)
    }

    fn rotation_age_due(&self, now_monotonic_ns: u64) -> Result<bool, ManagerError> {
        let age = now_monotonic_ns
            .checked_sub(self.active_opened_monotonic_ns)
            .ok_or(ManagerError::ClockRegression {
                previous: self.active_opened_monotonic_ns,
                current: now_monotonic_ns,
            })?;
        Ok(self.active_record_count > 0 && age >= self.policy.max_segment_age_ns)
    }

    fn rotate(
        &mut self,
        now_monotonic_ns: u64,
        wall_time_ns: i64,
    ) -> Result<CompressionJob, ManagerError> {
        self.ensure_directory_identity()?;
        let effective_wall_time = wall_time_ns.max(self.active_metadata.created_wall_time_ns());
        let next_ordinal = self
            .active_ordinal
            .checked_add(1)
            .ok_or(ManagerError::OrdinalOverflow)?;
        let next_segment_id = derive_successor_id(self.active_metadata.segment_id(), next_ordinal);
        let next_metadata = SegmentMetadata::new(
            next_segment_id,
            effective_wall_time,
            self.active_metadata.schema_id().to_owned(),
            self.active_metadata.installation_id().to_owned(),
            self.active_metadata.build_id().to_owned(),
            self.active_metadata.streams().to_vec(),
        )?;
        let next_name = active_name(next_ordinal, &next_segment_id);
        let next_path = self.directory.join(&next_name);
        let next_length = u64::try_from(prologue::encode(&next_metadata)?.len())
            .map_err(|_| ManagerError::Inactive)?;

        let segment = self.active.take().ok_or(ManagerError::Inactive)?;
        let sealed = seal_owned_v2_segment_at(
            &self.directory_lock,
            &self.directory,
            &self.active_name,
            segment,
            self.predecessor.clone(),
            effective_wall_time,
        )?;
        let sealed_ordinal = sealed.manifest().segment_ordinal();
        let job = sealed.compression_job().clone();
        let next_segment = match Segment::create_v2_at(
            &self.directory_lock,
            next_name.as_os_str(),
            next_metadata.clone(),
        ) {
            Ok(segment) => segment,
            Err(source) => {
                if self.ensure_directory_identity().is_err() {
                    return Err(ManagerError::DirectoryReplacedAfterRotation { sealed_ordinal });
                }
                return Err(ManagerError::SuccessorCreation {
                    compression_job: Box::new(job),
                    source: Box::new(source),
                });
            }
        };

        self.predecessor = Some(sealed.manifest().as_predecessor());
        self.sealed_names
            .push(PathBuf::from(sealed.manifest().segment_file()));
        self.active = Some(next_segment);
        self.active_name = next_name;
        self.active_path = next_path;
        self.active_metadata = next_metadata;
        self.active_ordinal = next_ordinal;
        self.active_opened_monotonic_ns = now_monotonic_ns;
        self.active_length = next_length;
        self.active_record_count = 0;
        if self.ensure_directory_identity().is_err() {
            return Err(ManagerError::DirectoryReplacedAfterRotation { sealed_ordinal });
        }
        Ok(job)
    }
}

struct Inventory {
    active: Vec<PathBuf>,
    sealed: Vec<PathBuf>,
    manifests: Vec<PathBuf>,
    pending: Vec<PathBuf>,
    temporary: Vec<PathBuf>,
}

struct PendingTransition {
    active_path: PathBuf,
    sealed_path: PathBuf,
    manifest: SealedSegmentManifest,
}

fn take_pending_transition(
    directory: &File,
    inventory: &mut Inventory,
) -> Result<Option<PendingTransition>, ManagerError> {
    if inventory.pending.len() > 1 {
        return Err(InventoryError::MultiplePendingSeals.into());
    }
    let Some(pending_path) = inventory.pending.pop() else {
        return Ok(None);
    };
    let pending_name = pending_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(InventoryError::UnknownArtifact)?;
    let base = pending_name
        .strip_suffix(PENDING_MANIFEST_SUFFIX)
        .ok_or(InventoryError::InvalidPendingSealState)?;
    let parent = pending_path
        .parent()
        .ok_or(InventoryError::InvalidPendingSealState)?;
    let active_path = parent.join(format!("{base}{ACTIVE_SUFFIX}"));
    let sealed_path = parent.join(format!("{base}{SEALED_SUFFIX}"));
    let final_manifest_path = parent.join(format!("{base}{MANIFEST_SUFFIX}"));
    if inventory
        .manifests
        .iter()
        .any(|path| path == &final_manifest_path)
    {
        return Err(InventoryError::InvalidPendingSealState.into());
    }

    let active_count = remove_matching_path(&mut inventory.active, &active_path);
    let sealed_count = remove_matching_path(&mut inventory.sealed, &sealed_path);
    if active_count + sealed_count != 1 {
        return Err(InventoryError::InvalidPendingSealState.into());
    }
    let manifest = read_pending_manifest_for_recovery_at(directory, &pending_path)?;
    let sealed_name = sealed_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(InventoryError::InvalidPendingSealState)?;
    if manifest.segment_file() != sealed_name {
        return Err(InventoryError::InvalidPendingSealState.into());
    }
    Ok(Some(PendingTransition {
        active_path,
        sealed_path,
        manifest,
    }))
}

fn remove_matching_path(paths: &mut Vec<PathBuf>, expected: &Path) -> usize {
    let original_length = paths.len();
    paths.retain(|path| path != expected);
    original_length - paths.len()
}

fn validate_pending_transition(
    transition: &PendingTransition,
    previous: Option<&SealedSegmentManifest>,
    expected_ordinal: u64,
) -> Result<(), ManagerError> {
    let (ordinal, segment_id) = parse_active_identity(&transition.active_path)
        .ok_or(InventoryError::InvalidPendingSealState)?;
    if ordinal != expected_ordinal
        || transition.manifest.segment_ordinal() != expected_ordinal
        || transition.manifest.segment_id() != &segment_id
    {
        return Err(InventoryError::InvalidPendingSealState.into());
    }
    let expected_sealed = sealed_path_for(&transition.active_path)?;
    if expected_sealed != transition.sealed_path {
        return Err(InventoryError::InvalidPendingSealState.into());
    }
    match previous {
        Some(previous) => {
            if segment_id != derive_successor_id(previous.segment_id(), expected_ordinal) {
                return Err(InventoryError::ActiveIdentityMismatch.into());
            }
            let predecessor =
                transition
                    .manifest
                    .predecessor()
                    .ok_or(InventoryError::PredecessorMismatch {
                        ordinal: expected_ordinal,
                    })?;
            if predecessor.segment_id() != previous.segment_id()
                || predecessor.segment_blake3() != previous.segment_blake3()
            {
                return Err(InventoryError::PredecessorMismatch {
                    ordinal: expected_ordinal,
                }
                .into());
            }
            validate_manifest_metadata_continuity(
                previous,
                &transition.manifest,
                expected_ordinal,
            )?;
        }
        None if transition.manifest.predecessor().is_some() => {
            return Err(InventoryError::PredecessorMismatch {
                ordinal: expected_ordinal,
            }
            .into());
        }
        None => {}
    }
    Ok(())
}

fn validate_manifest_metadata_continuity(
    previous: &SealedSegmentManifest,
    current: &SealedSegmentManifest,
    ordinal: u64,
) -> Result<(), ManagerError> {
    if current.created_wall_time_ns() != previous.sealed_wall_time_ns()
        || current.schema_id() != previous.schema_id()
        || current.installation_id() != previous.installation_id()
        || current.build_id() != previous.build_id()
        || current.streams() != previous.streams()
    {
        return Err(InventoryError::SegmentMetadataMismatch { ordinal }.into());
    }
    Ok(())
}

fn validate_active_metadata_continuity(
    previous: &SealedSegmentManifest,
    current: &SegmentMetadata,
    ordinal: u64,
) -> Result<(), ManagerError> {
    if current.created_wall_time_ns() != previous.sealed_wall_time_ns()
        || current.schema_id() != previous.schema_id()
        || current.installation_id() != previous.installation_id()
        || current.build_id() != previous.build_id()
        || current.streams() != previous.streams()
    {
        return Err(InventoryError::SegmentMetadataMismatch { ordinal }.into());
    }
    Ok(())
}

fn inventory(directory: &File) -> Result<Inventory, ManagerError> {
    let mut inventory = Inventory {
        active: Vec::new(),
        sealed: Vec::new(),
        manifests: Vec::new(),
        pending: Vec::new(),
        temporary: Vec::new(),
    };
    for path in directory_entry_names(directory)? {
        let name = path
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or(InventoryError::UnknownArtifact)?
            .to_owned();
        if name.ends_with(ACTIVE_SUFFIX) {
            validate_inventory_file_at(directory, &path)?;
            inventory.active.push(path);
        } else if name.ends_with(PENDING_MANIFEST_SUFFIX) {
            validate_inventory_file_at(directory, &path)?;
            inventory.pending.push(path);
        } else if name.ends_with(MANIFEST_SUFFIX) {
            validate_inventory_file_at(directory, &path)?;
            inventory.manifests.push(path);
        } else if name.ends_with(SEALED_SUFFIX) {
            validate_inventory_file_at(directory, &path)?;
            inventory.sealed.push(path);
        } else if is_protocol_temporary_name(&name) {
            validate_inventory_file_at(directory, &path)?;
            inventory.temporary.push(path);
        } else {
            return Err(InventoryError::UnknownArtifact.into());
        }
    }
    Ok(inventory)
}

fn is_protocol_temporary_name(name: &str) -> bool {
    let Some(without_dot) = name.strip_prefix('.') else {
        return false;
    };
    let Some((final_name, create_suffix)) = without_dot.rsplit_once(".create-") else {
        return false;
    };
    let Some(process_and_sequence) = create_suffix.strip_suffix(".tmp") else {
        return false;
    };
    let Some((process_id, sequence)) = process_and_sequence.split_once('-') else {
        return false;
    };
    if process_id.is_empty()
        || sequence.is_empty()
        || !process_id.bytes().all(|byte| byte.is_ascii_digit())
        || !sequence.bytes().all(|byte| byte.is_ascii_digit())
    {
        return false;
    }
    if parse_active_identity(Path::new(final_name)).is_some() {
        return true;
    }
    let Some(base) = final_name.strip_suffix(PENDING_MANIFEST_SUFFIX) else {
        return false;
    };
    parse_active_identity(Path::new(&format!("{base}{ACTIVE_SUFFIX}"))).is_some()
}

fn cleanup_temporary_files(
    directory: &File,
    temporary_paths: &[PathBuf],
) -> Result<(), ManagerError> {
    if temporary_paths.is_empty() {
        return Ok(());
    }
    for name in temporary_paths {
        validate_inventory_file_at(directory, name)?;
        rustix::fs::unlinkat(directory, name, AtFlags::empty()).map_err(io::Error::from)?;
    }
    directory.sync_all()?;
    Ok(())
}

fn successor_metadata(
    previous: &SealedSegmentManifest,
    segment_id: [u8; 16],
) -> Result<SegmentMetadata, PrologueError> {
    SegmentMetadata::new(
        segment_id,
        previous.sealed_wall_time_ns(),
        previous.schema_id().to_owned(),
        previous.installation_id().to_owned(),
        previous.build_id().to_owned(),
        previous.streams().to_vec(),
    )
}

fn ensure_new_directory(directory: &File) -> Result<(), ManagerError> {
    if !directory_entry_names(directory)?.is_empty() {
        return Err(InventoryError::NotEmpty.into());
    }
    Ok(())
}

fn acquire_directory_lock(directory: &Path) -> Result<File, ManagerError> {
    let owned = rustix::fs::openat(
        CWD,
        directory,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::DIRECTORY,
        Mode::empty(),
    )
    .map_err(map_open_error)?;
    lock_directory_file(File::from(owned))
}

fn lock_directory_file(file: File) -> Result<File, ManagerError> {
    validate_managed_directory(&file)?;
    match rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(error) if error == rustix::io::Errno::WOULDBLOCK => {
            return Err(ManagerError::AlreadyOpen);
        }
        Err(error) => return Err(io::Error::from(error).into()),
    }
    Ok(file)
}

fn ensure_directory_path_identity(directory: &Path, locked: &File) -> Result<(), ManagerError> {
    let locked_stat = rustix::fs::fstat(locked).map_err(io::Error::from)?;
    let current = rustix::fs::openat(
        CWD,
        directory,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::DIRECTORY,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| ManagerError::DirectoryReplaced)?;
    validate_managed_directory(&current)?;
    let current_stat = rustix::fs::fstat(&current).map_err(io::Error::from)?;
    if current_stat.st_dev != locked_stat.st_dev || current_stat.st_ino != locked_stat.st_ino {
        return Err(ManagerError::DirectoryReplaced);
    }
    Ok(())
}

fn open_managed_file_at(
    directory: &File,
    name: &Path,
    access: OFlags,
) -> Result<File, ManagerError> {
    let owned = rustix::fs::openat(
        directory,
        name,
        access | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(map_open_error)?;
    let file = File::from(owned);
    validate_managed_file(&file)?;
    Ok(file)
}

fn map_open_error(error: rustix::io::Errno) -> ManagerError {
    if error == rustix::io::Errno::LOOP {
        InventoryError::UnsafeArtifact.into()
    } else {
        io::Error::from(error).into()
    }
}

fn validate_inventory_file_at(directory: &File, name: &Path) -> Result<(), ManagerError> {
    let file = open_managed_file_at(directory, name, OFlags::RDONLY)?;
    drop(file);
    Ok(())
}

fn directory_entry_names(directory: &File) -> Result<Vec<PathBuf>, ManagerError> {
    let mut entries = Dir::read_from(directory).map_err(io::Error::from)?;
    let mut names = Vec::new();
    for entry in &mut entries {
        let entry = entry.map_err(io::Error::from)?;
        let bytes = entry.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        names.push(PathBuf::from(OsStr::from_bytes(bytes)));
    }
    Ok(names)
}

fn validate_managed_file(file: &File) -> Result<(), ManagerError> {
    let stat = rustix::fs::fstat(file).map_err(io::Error::from)?;
    if !FileType::from_raw_mode(stat.st_mode).is_file()
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || stat.st_nlink != 1
        || stat.st_mode & 0o077 != 0
    {
        return Err(InventoryError::UnsafeArtifact.into());
    }
    Ok(())
}

fn validate_managed_directory(file: &File) -> Result<(), ManagerError> {
    let stat = rustix::fs::fstat(file).map_err(io::Error::from)?;
    if !FileType::from_raw_mode(stat.st_mode).is_dir()
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || stat.st_mode & 0o022 != 0
    {
        return Err(InventoryError::UnsafeArtifact.into());
    }
    Ok(())
}

fn merge_manifest_sequences(
    sequences: &mut BTreeMap<(u32, u64), u64>,
    manifest: &SealedSegmentManifest,
) -> Result<(), ManagerError> {
    for range in manifest.sequence_ranges() {
        let key = (range.stream_id(), range.connection_epoch());
        if sequences
            .get(&key)
            .is_some_and(|previous| range.first_sequence() <= *previous)
        {
            return Err(InventoryError::CrossSegmentSequenceRegression.into());
        }
        sequences.insert(key, range.last_sequence());
    }
    Ok(())
}

fn merge_active_record(
    sequences: &mut BTreeMap<(u32, u64), u64>,
    record: RecoveredRecord<'_>,
    regressed: &mut bool,
) {
    let Some(metadata) = record.metadata() else {
        return;
    };
    let key = (metadata.stream_id, metadata.connection_epoch);
    if sequences
        .get(&key)
        .is_some_and(|previous| metadata.record_sequence <= *previous)
    {
        *regressed = true;
    }
    sequences.insert(key, metadata.record_sequence);
}

fn active_name(ordinal: u64, segment_id: &[u8; 16]) -> PathBuf {
    PathBuf::from(format!(
        "{ordinal:020}-{}.active.wal",
        hex::encode(segment_id)
    ))
}

fn derive_successor_id(previous_segment_id: &[u8; 16], ordinal: u64) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(SUCCESSOR_ID_DOMAIN);
    hasher.update(previous_segment_id);
    hasher.update(&ordinal.to_be_bytes());
    let mut segment_id = [0_u8; 16];
    segment_id.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    segment_id
}

fn parse_active_identity(path: &Path) -> Option<(u64, [u8; 16])> {
    let name = path.file_name()?.to_str()?;
    let base = name.strip_suffix(ACTIVE_SUFFIX)?;
    if base.len() != 53 || base.as_bytes().get(20) != Some(&b'-') {
        return None;
    }
    let ordinal = base[..20].parse::<u64>().ok()?;
    if ordinal == 0 || format!("{ordinal:020}") != base[..20] {
        return None;
    }
    let decoded = hex::decode(&base[21..]).ok()?;
    let segment_id: [u8; 16] = decoded.try_into().ok()?;
    (segment_id != [0_u8; 16]).then_some((ordinal, segment_id))
}
