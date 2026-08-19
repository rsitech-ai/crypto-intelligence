#[cfg(feature = "test-support")]
use std::cell::Cell;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use arrow::{
    array::{
        Array, BinaryArray, Int64Array, LargeBinaryArray, LargeStringArray, StringArray,
        UInt32Array, UInt64Array,
    },
    datatypes::{DataType, Schema, SchemaRef},
    record_batch::RecordBatch,
};
use parquet::{
    arrow::{ArrowWriter, arrow_reader::ParquetRecordBatchReaderBuilder},
    basic::{Compression, ZstdLevel},
    file::{metadata::KeyValue, properties::WriterProperties},
};
use rustix::fs::{AtFlags, CWD, Dir, FileType, FlockOperation, Mode, OFlags, RenameFlags};

use crate::{
    BatchId, DatasetMetadata, Manifest, ManifestFile, PartitionKey, PartitionKeyInput,
    SourceCoverage, SourceFragmentLineage, StoreError, VerifiedPartition,
    layout::safe_relative_path, manifest::ManifestInput,
};

pub(crate) const MAXIMUM_MANIFEST_BYTES: usize = 1_048_576;
const MANIFEST_NAME: &str = "manifest.json";
const ACTIVE_DIRECTORY: &str = "active";
const SEALED_DIRECTORY: &str = "sealed";
const SEALED_FILE_NAME: &str = "data.parquet";
const TEMP_CREATE_ATTEMPTS: u64 = 128;
const MAXIMUM_STALE_TEMPORARY_FILES: usize = 256;
const MAXIMUM_MANAGED_DIRECTORY_ENTRIES: usize = 4_352;
const FOOTER_FORMAT_VERSION: &str = "1";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Explicit resource limits for the local Parquet store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DatasetPolicy {
    maximum_batch_rows: usize,
    maximum_batch_columns: usize,
    maximum_batch_bytes: usize,
    maximum_value_bytes: usize,
    maximum_schema_bytes: usize,
    maximum_schema_fields: usize,
    maximum_schema_depth: usize,
    maximum_writer_memory_bytes: usize,
    maximum_row_group_rows: usize,
    target_row_group_bytes: usize,
    maximum_file_bytes: u64,
    maximum_partition_bytes: u64,
    maximum_partition_rows: u64,
    maximum_active_fragments: usize,
    maximum_footer_bytes: usize,
    maximum_sealed_files: usize,
    reader_batch_rows: usize,
    maximum_correction_depth: usize,
}

/// Construction input for [`DatasetPolicy`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DatasetPolicyInput {
    pub maximum_batch_rows: usize,
    pub maximum_batch_columns: usize,
    pub maximum_batch_bytes: usize,
    pub maximum_value_bytes: usize,
    pub maximum_schema_bytes: usize,
    pub maximum_schema_fields: usize,
    pub maximum_schema_depth: usize,
    pub maximum_writer_memory_bytes: usize,
    pub maximum_row_group_rows: usize,
    pub target_row_group_bytes: usize,
    pub maximum_file_bytes: u64,
    pub maximum_partition_bytes: u64,
    pub maximum_partition_rows: u64,
    pub maximum_active_fragments: usize,
    pub maximum_footer_bytes: usize,
    pub maximum_sealed_files: usize,
    pub reader_batch_rows: usize,
    pub maximum_correction_depth: usize,
}

impl DatasetPolicy {
    pub fn try_new(input: DatasetPolicyInput) -> Result<Self, StoreError> {
        if input.maximum_batch_rows == 0
            || input.maximum_batch_columns == 0
            || input.maximum_batch_bytes == 0
            || input.maximum_value_bytes == 0
            || input.maximum_schema_bytes == 0
            || input.maximum_schema_fields == 0
            || input.maximum_schema_depth == 0
            || input.maximum_writer_memory_bytes == 0
            || input.maximum_row_group_rows == 0
            || input.target_row_group_bytes == 0
            || input.maximum_file_bytes == 0
            || input.maximum_partition_bytes != input.maximum_file_bytes
            || input.maximum_partition_rows == 0
            || input.maximum_active_fragments == 0
            || input.maximum_active_fragments > crate::layout::MAXIMUM_SOURCE_COVERAGE
            || input.maximum_footer_bytes < 8
            || input.maximum_footer_bytes as u64 > input.maximum_file_bytes
            || input.maximum_sealed_files == 0
            || input.maximum_sealed_files < input.maximum_active_fragments
            || input.maximum_sealed_files > crate::manifest::MAXIMUM_SEALED_FILES
            || input.reader_batch_rows == 0
            || input.maximum_correction_depth == 0
            || input.maximum_correction_depth > 4_096
        {
            return Err(StoreError::InvalidPolicy);
        }
        Ok(Self {
            maximum_batch_rows: input.maximum_batch_rows,
            maximum_batch_columns: input.maximum_batch_columns,
            maximum_batch_bytes: input.maximum_batch_bytes,
            maximum_value_bytes: input.maximum_value_bytes,
            maximum_schema_bytes: input.maximum_schema_bytes,
            maximum_schema_fields: input.maximum_schema_fields,
            maximum_schema_depth: input.maximum_schema_depth,
            maximum_writer_memory_bytes: input.maximum_writer_memory_bytes,
            maximum_row_group_rows: input.maximum_row_group_rows,
            target_row_group_bytes: input.target_row_group_bytes,
            maximum_file_bytes: input.maximum_file_bytes,
            maximum_partition_bytes: input.maximum_partition_bytes,
            maximum_partition_rows: input.maximum_partition_rows,
            maximum_active_fragments: input.maximum_active_fragments,
            maximum_footer_bytes: input.maximum_footer_bytes,
            maximum_sealed_files: input.maximum_sealed_files,
            reader_batch_rows: input.reader_batch_rows,
            maximum_correction_depth: input.maximum_correction_depth,
        })
    }
}

/// Durable identity returned for an active fragment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Fragment {
    relative_path: PathBuf,
    size_bytes: u64,
    blake3: [u8; 32],
    schema_digest: [u8; 32],
    minimum_event_time_ns: i64,
    maximum_event_time_ns: i64,
    row_count: u64,
    batch_id_digest: [u8; 32],
}

impl Fragment {
    pub fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    pub const fn size_bytes(&self) -> u64 {
        self.size_bytes
    }

    pub const fn blake3(&self) -> &[u8; 32] {
        &self.blake3
    }

    pub const fn row_count(&self) -> u64 {
        self.row_count
    }
}

/// Idempotent acknowledgement for one committed batch.
pub type WriteReceipt = Fragment;

/// One-shot deterministic fault available only to storage crash-recovery tests.
#[cfg(feature = "test-support")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultPoint {
    AfterParquetCloseBeforeFileSync,
    AfterFileSyncBeforeHash,
    AfterFileHashBeforeRename,
    AfterDataRenameBeforeDirectorySync,
    AfterManifestTempSyncBeforeRename,
    AfterManifestRenameBeforeDirectorySync,
    AfterFragmentPublication,
    AfterSealedDataPublication,
    AfterManifestPublication,
    CompactedFileExceedsCapacity,
}

#[cfg(feature = "test-support")]
struct FaultInjector(Cell<Option<FaultPoint>>);

#[cfg(feature = "test-support")]
impl FaultInjector {
    const fn new() -> Self {
        Self(Cell::new(None))
    }

    fn inject_once(&self, point: FaultPoint) {
        self.0.set(Some(point));
    }

    fn trip(&self, point: FaultPoint) -> Result<(), StoreError> {
        if self.0.get() == Some(point) {
            self.0.set(None);
            return Err(StoreError::InjectedFault);
        }
        Ok(())
    }

    fn trip_capacity(&self, point: FaultPoint) -> Result<(), StoreError> {
        if self.0.get() == Some(point) {
            self.0.set(None);
            return Err(StoreError::CapacityExceeded);
        }
        Ok(())
    }
}

/// Exclusive local writer for active and sealed normalized partitions.
pub struct DatasetWriter {
    root_path: PathBuf,
    root: Directory,
    policy: DatasetPolicy,
    #[cfg(feature = "test-support")]
    faults: FaultInjector,
}

impl DatasetWriter {
    pub fn open(root: &Path, policy: DatasetPolicy) -> Result<Self, StoreError> {
        let root_handle = open_root(root)?;
        match rustix::fs::flock(&root_handle, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(error) if error == rustix::io::Errno::WOULDBLOCK => {
                return Err(StoreError::AlreadyOpen);
            }
            Err(error) => return Err(io::Error::from(error).into()),
        }
        Ok(Self {
            root_path: root.to_path_buf(),
            root: Directory {
                relative_path: PathBuf::new(),
                file: root_handle,
            },
            policy,
            #[cfg(feature = "test-support")]
            faults: FaultInjector::new(),
        })
    }

    #[cfg(feature = "test-support")]
    pub fn inject_fault_once(&mut self, fault_point: FaultPoint) {
        self.faults.inject_once(fault_point);
    }

    pub fn write_batch(
        &mut self,
        key: &PartitionKey,
        metadata: &DatasetMetadata,
        batch_id: &BatchId,
        batch: &RecordBatch,
    ) -> Result<WriteReceipt, StoreError> {
        self.ensure_root_identity()?;
        validate_metadata_for_partition(&self.root, key, metadata, &self.policy)?;
        validate_batch(key, metadata, batch_id, batch, &self.policy, true)?;
        let partition = self.open_partition(key, true)?;
        clean_temporary_files(&partition)?;
        if entry_exists(&partition.file, MANIFEST_NAME)? {
            return Err(StoreError::PartitionSealed);
        }
        let sealed = partition.open_or_create_child(SEALED_DIRECTORY)?;
        clean_temporary_files(&sealed)?;
        if !directory_names(
            &sealed.file,
            self.policy
                .maximum_sealed_files
                .checked_add(MAXIMUM_STALE_TEMPORARY_FILES)
                .ok_or(StoreError::IntegerRange)?,
        )?
        .is_empty()
        {
            return Err(StoreError::Integrity);
        }
        let active = partition.open_or_create_child(ACTIVE_DIRECTORY)?;
        clean_temporary_files(&active)?;
        let inventory = active_fragment_names(&active.file, self.policy.maximum_active_fragments)?;
        let batch_id_digest = batch_id.digest()?;
        let final_name = format!("fragment-{}.parquet", hex::encode(batch_id_digest));
        let is_retry = inventory.iter().any(|name| name == OsStr::new(&final_name));
        if !is_retry && inventory.len() >= self.policy.maximum_active_fragments {
            return Err(StoreError::FragmentCapacityExceeded);
        }

        let schema_digest = schema_digest(batch.schema().as_ref(), &self.policy)?;
        let candidate_rows =
            u64::try_from(batch.num_rows()).map_err(|_| StoreError::IntegerRange)?;
        let existing_bytes = validate_active_admission(
            &active,
            &inventory,
            key,
            ActiveAdmission {
                metadata,
                schema_digest,
                candidate_rows,
                is_retry,
            },
            &self.policy,
        )?;
        let (minimum_event_time_ns, maximum_event_time_ns) = event_time_bounds(batch)?;
        let footer = footer_metadata(
            key,
            metadata,
            schema_digest,
            minimum_event_time_ns,
            maximum_event_time_ns,
            candidate_rows,
            Some(batch_id),
        )?;
        let temporary = write_parquet_temporary(
            &active,
            OsStr::new(&final_name),
            batch.schema(),
            std::iter::once(batch.clone()),
            footer,
            &self.policy,
            #[cfg(feature = "test-support")]
            &self.faults,
        )?;
        if !is_retry
            && existing_bytes
                .checked_add(temporary.size_bytes)
                .ok_or(StoreError::IntegerRange)?
                > self.policy.maximum_partition_bytes
        {
            discard_temporary(&active, &temporary)?;
            return Err(StoreError::CapacityExceeded);
        }
        let relative_path = key.relative_path().join(ACTIVE_DIRECTORY).join(&final_name);
        let expected = Fragment {
            relative_path,
            size_bytes: temporary.size_bytes,
            blake3: temporary.blake3,
            schema_digest,
            minimum_event_time_ns,
            maximum_event_time_ns,
            row_count: candidate_rows,
            batch_id_digest,
        };
        install_fragment_or_verify_retry(
            &active,
            temporary,
            &final_name,
            key,
            &expected,
            &self.policy,
            #[cfg(feature = "test-support")]
            &self.faults,
        )?;
        #[cfg(feature = "test-support")]
        self.faults.trip(FaultPoint::AfterFragmentPublication)?;
        Ok(expected)
    }

    pub fn seal(&mut self, key: &PartitionKey) -> Result<Manifest, StoreError> {
        self.ensure_root_identity()?;
        let partition = self.open_partition(key, false)?;
        clean_temporary_files(&partition)?;
        if entry_exists(&partition.file, MANIFEST_NAME)? {
            let manifest = load_manifest_at(&partition, key)?;
            verify_manifest_at(&partition, &manifest, &self.policy)?;
            if entry_exists(&partition.file, ACTIVE_DIRECTORY)? {
                let active = partition.open_existing_child(ACTIVE_DIRECTORY)?;
                clean_temporary_files(&active)?;
                let names =
                    active_fragment_names(&active.file, self.policy.maximum_active_fragments)?;
                remove_active_fragments(&active, &names)?;
            }
            return Ok(manifest);
        }
        let active = partition.open_existing_child(ACTIVE_DIRECTORY)?;
        let sealed = partition.open_or_create_child(SEALED_DIRECTORY)?;
        clean_temporary_files(&active)?;
        clean_temporary_files(&sealed)?;
        let names = active_fragment_names(&active.file, self.policy.maximum_active_fragments)?;
        if names.is_empty() {
            return Err(StoreError::PartitionEmpty);
        }
        let fragments = names
            .iter()
            .map(|name| read_fragment(&active, name, key, &self.policy))
            .collect::<Result<Vec<_>, _>>()?;
        let first = fragments.first().ok_or(StoreError::PartitionEmpty)?;
        for fragment in &fragments[1..] {
            if !fragment.metadata.same_dataset_identity(&first.metadata)
                || fragment.schema_digest != first.schema_digest
            {
                return Err(StoreError::SchemaMismatch);
            }
        }
        let combined_coverage = fragments
            .iter()
            .flat_map(|fragment| fragment.metadata.source_coverage().iter().cloned())
            .collect::<Vec<_>>();
        let combined_metadata = first.metadata.with_source_coverage(combined_coverage)?;
        validate_metadata_for_partition(&self.root, key, &combined_metadata, &self.policy)?;
        let total_rows = fragments.iter().try_fold(0_u64, |total, fragment| {
            total
                .checked_add(fragment.row_count)
                .ok_or(StoreError::IntegerRange)
        })?;
        if total_rows > self.policy.maximum_partition_rows {
            return Err(StoreError::CapacityExceeded);
        }
        let total_source_bytes = fragments.iter().try_fold(0_u64, |total, fragment| {
            total
                .checked_add(fragment.size_bytes)
                .ok_or(StoreError::IntegerRange)
        })?;
        if total_source_bytes > self.policy.maximum_partition_bytes {
            return Err(StoreError::CapacityExceeded);
        }
        let minimum_event_time_ns = fragments
            .iter()
            .map(|fragment| fragment.minimum_event_time_ns)
            .min()
            .ok_or(StoreError::PartitionEmpty)?;
        let maximum_event_time_ns = fragments
            .iter()
            .map(|fragment| fragment.maximum_event_time_ns)
            .max()
            .ok_or(StoreError::PartitionEmpty)?;
        let manifest_files = publish_sealed_files(
            &sealed,
            SealingRequest {
                active: &active,
                key,
                metadata: &combined_metadata,
                fragments: &fragments,
                total_rows,
                minimum_event_time_ns,
                maximum_event_time_ns,
            },
            &self.policy,
            #[cfg(feature = "test-support")]
            &self.faults,
        )?;
        #[cfg(feature = "test-support")]
        self.faults.trip(FaultPoint::AfterSealedDataPublication)?;
        let manifest = Manifest::create(ManifestInput {
            partition: key.clone(),
            metadata: combined_metadata,
            schema_digest: first.schema_digest,
            minimum_event_time_ns,
            maximum_event_time_ns,
            row_count: total_rows,
            files: manifest_files,
            source_fragments: fragments
                .iter()
                .map(|fragment| {
                    SourceFragmentLineage::new(
                        fragment.batch_id.clone(),
                        fragment.fragment_blake3,
                        fragment.row_count,
                    )
                })
                .collect::<Result<Vec<_>, _>>()?,
        })?;
        publish_manifest(
            &partition,
            &manifest,
            #[cfg(feature = "test-support")]
            &self.faults,
        )?;
        #[cfg(feature = "test-support")]
        self.faults.trip(FaultPoint::AfterManifestPublication)?;
        verify_manifest_at(&partition, &manifest, &self.policy)?;
        remove_active_fragments(&active, &names)?;
        Ok(manifest)
    }

    fn open_partition(&self, key: &PartitionKey, create: bool) -> Result<Directory, StoreError> {
        let mut directory = self.root.try_clone()?;
        for component in key.components() {
            directory = if create {
                directory.open_or_create_child(&component)?
            } else {
                directory.open_existing_child(&component)?
            };
        }
        Ok(directory)
    }

    fn ensure_root_identity(&self) -> Result<(), StoreError> {
        let current = open_root(&self.root_path).map_err(|_| StoreError::UnsafeRoot)?;
        if identity(&current)? != identity(&self.root.file)? {
            return Err(StoreError::UnsafeRoot);
        }
        Ok(())
    }
}

/// Read-only publication verifier for sealed partitions.
pub struct DatasetReader {
    root: Directory,
    policy: DatasetPolicy,
}

impl DatasetReader {
    pub fn open(root: &Path, policy: DatasetPolicy) -> Result<Self, StoreError> {
        Ok(Self {
            root: Directory {
                relative_path: PathBuf::new(),
                file: open_root(root)?,
            },
            policy,
        })
    }

    pub fn verify_partition(&self, key: &PartitionKey) -> Result<VerifiedPartition, StoreError> {
        let mut partition = self.root.try_clone()?;
        for component in key.components() {
            partition = partition.open_existing_child(&component)?;
        }
        let manifest = load_manifest_at(&partition, key)?;
        verify_manifest_at(&partition, &manifest, &self.policy)?;
        validate_metadata_for_partition(&self.root, key, manifest.metadata(), &self.policy)?;
        Ok(VerifiedPartition::from_manifest(&manifest))
    }

    /// Reads batches only after verifying the installed manifest, complete
    /// file inventory, file hashes, Parquet footers, and correction lineage.
    ///
    /// Every returned batch is decoded from the same descriptor that was
    /// revalidated immediately before reading, so a path replacement cannot
    /// substitute bytes between verification and decoding.
    pub fn read_partition(
        &self,
        key: &PartitionKey,
    ) -> Result<(VerifiedPartition, Vec<RecordBatch>), StoreError> {
        let mut partition = self.root.try_clone()?;
        for component in key.components() {
            partition = partition.open_existing_child(&component)?;
        }
        let manifest = load_manifest_at(&partition, key)?;
        verify_manifest_at(&partition, &manifest, &self.policy)?;
        validate_metadata_for_partition(&self.root, key, manifest.metadata(), &self.policy)?;

        let sealed = partition.open_existing_child(SEALED_DIRECTORY)?;
        let mut batches = Vec::new();
        let mut row_count = 0_u64;
        for entry in manifest.files() {
            let name = entry
                .relative_path()
                .file_name()
                .ok_or(StoreError::Integrity)?;
            let file = sealed.open_file(name, OFlags::RDONLY)?;
            let (size_bytes, blake3) = hash_validated_file(&file, self.policy.maximum_file_bytes)?;
            if size_bytes != entry.size_bytes() || &blake3 != entry.blake3() {
                return Err(StoreError::Integrity);
            }
            let file_metadata = manifest
                .metadata()
                .with_source_coverage(entry.source_coverage().to_vec())
                .map_err(|_| StoreError::Integrity)?;
            verify_parquet_file(
                &file,
                ParquetExpectation {
                    key: manifest.partition(),
                    metadata: &file_metadata,
                    schema_digest: *manifest.schema_digest(),
                    minimum_event_time_ns: entry.minimum_event_time_ns(),
                    maximum_event_time_ns: entry.maximum_event_time_ns(),
                    row_count: entry.row_count(),
                },
                &self.policy,
            )?;
            let builder = ParquetRecordBatchReaderBuilder::try_new(file)?
                .with_batch_size(self.policy.reader_batch_rows);
            if builder.metadata().memory_size() > self.policy.maximum_schema_bytes {
                return Err(StoreError::CapacityExceeded);
            }
            for batch in builder.build()? {
                let batch = batch?;
                row_count = row_count
                    .checked_add(
                        u64::try_from(batch.num_rows()).map_err(|_| StoreError::IntegerRange)?,
                    )
                    .ok_or(StoreError::IntegerRange)?;
                if row_count > self.policy.maximum_partition_rows {
                    return Err(StoreError::CapacityExceeded);
                }
                batches.push(batch);
            }
        }
        if row_count != manifest.row_count() {
            return Err(StoreError::Integrity);
        }
        Ok((VerifiedPartition::from_manifest(&manifest), batches))
    }
}

#[derive(Debug)]
struct Directory {
    relative_path: PathBuf,
    file: File,
}

impl Directory {
    fn try_clone(&self) -> Result<Self, StoreError> {
        Ok(Self {
            relative_path: self.relative_path.clone(),
            file: self.file.try_clone()?,
        })
    }

    fn open_or_create_child(&self, name: impl AsRef<OsStr>) -> Result<Self, StoreError> {
        let name = name.as_ref();
        validate_component(name)?;
        let created =
            match rustix::fs::mkdirat(&self.file, name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
                Ok(()) => true,
                Err(error) if error == rustix::io::Errno::EXIST => false,
                Err(error) => return Err(io::Error::from(error).into()),
            };
        let child = self.open_existing_child(name)?;
        if created {
            self.file.sync_all()?;
        }
        Ok(child)
    }

    fn open_existing_child(&self, name: impl AsRef<OsStr>) -> Result<Self, StoreError> {
        let name = name.as_ref();
        validate_component(name)?;
        let owned = rustix::fs::openat(
            &self.file,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(map_path_error)?;
        let file = File::from(owned);
        validate_private_directory(&file)?;
        Ok(Self {
            relative_path: self.relative_path.join(name),
            file,
        })
    }

    fn open_file(&self, name: impl AsRef<OsStr>, access: OFlags) -> Result<File, StoreError> {
        let name = name.as_ref();
        validate_component(name)?;
        let owned = rustix::fs::openat(
            &self.file,
            name,
            access | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(map_path_error)?;
        let file = File::from(owned);
        validate_private_file(&file)?;
        Ok(file)
    }
}

struct TemporaryFile {
    name: OsString,
    file: File,
    identity: FileIdentity,
    size_bytes: u64,
    blake3: [u8; 32],
}

struct ReadFragment {
    file_name: OsString,
    schema: SchemaRef,
    schema_digest: [u8; 32],
    metadata: DatasetMetadata,
    batch_id: BatchId,
    fragment_blake3: [u8; 32],
    minimum_event_time_ns: i64,
    maximum_event_time_ns: i64,
    row_count: u64,
    size_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u128,
    inode: u128,
}

fn open_root(path: &Path) -> Result<File, StoreError> {
    if path.as_os_str().is_empty() {
        return Err(StoreError::UnsafeRoot);
    }
    let starting_directory = if path.is_absolute() { "/" } else { "." };
    let owned = rustix::fs::openat(
        CWD,
        starting_directory,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|_| StoreError::UnsafeRoot)?;
    let mut file = File::from(owned);
    for component in path.components() {
        let name = match component {
            std::path::Component::RootDir | std::path::Component::CurDir => continue,
            std::path::Component::Normal(name) => name,
            std::path::Component::ParentDir | std::path::Component::Prefix(_) => {
                return Err(StoreError::UnsafeRoot);
            }
        };
        let next = rustix::fs::openat(
            &file,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|_| StoreError::UnsafeRoot)?;
        file = File::from(next);
    }
    validate_private_directory(&file).map_err(|_| StoreError::UnsafeRoot)?;
    Ok(file)
}

fn validate_private_directory(file: &File) -> Result<(), StoreError> {
    let stat = rustix::fs::fstat(file).map_err(io::Error::from)?;
    if !FileType::from_raw_mode(stat.st_mode).is_dir()
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || stat.st_mode & 0o077 != 0
    {
        return Err(StoreError::UnsafePath);
    }
    Ok(())
}

fn validate_private_file(file: &File) -> Result<FileIdentity, StoreError> {
    let stat = rustix::fs::fstat(file).map_err(io::Error::from)?;
    if !FileType::from_raw_mode(stat.st_mode).is_file()
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || stat.st_nlink != 1
        || stat.st_mode & 0o077 != 0
    {
        return Err(StoreError::UnsafePath);
    }
    Ok(FileIdentity {
        device: stat.st_dev as u128,
        inode: stat.st_ino as u128,
    })
}

fn identity(file: &File) -> Result<FileIdentity, StoreError> {
    let stat = rustix::fs::fstat(file).map_err(io::Error::from)?;
    Ok(FileIdentity {
        device: stat.st_dev as u128,
        inode: stat.st_ino as u128,
    })
}

fn verify_installed_identity(
    directory: &File,
    name: &OsStr,
    expected: FileIdentity,
) -> Result<(), StoreError> {
    let stat =
        rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW).map_err(io::Error::from)?;
    if !FileType::from_raw_mode(stat.st_mode).is_file()
        || stat.st_nlink != 1
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || stat.st_mode & 0o077 != 0
        || stat.st_dev as u128 != expected.device
        || stat.st_ino as u128 != expected.inode
    {
        return Err(StoreError::UnsafePath);
    }
    Ok(())
}

fn validate_component(name: &OsStr) -> Result<(), StoreError> {
    let path = Path::new(name);
    if name.is_empty()
        || path.components().count() != 1
        || !matches!(
            path.components().next(),
            Some(std::path::Component::Normal(_))
        )
    {
        return Err(StoreError::UnsafePath);
    }
    Ok(())
}

fn map_path_error(error: rustix::io::Errno) -> StoreError {
    if error == rustix::io::Errno::LOOP {
        StoreError::UnsafePath
    } else {
        StoreError::Io(error.into())
    }
}

fn validate_metadata_for_partition(
    root: &Directory,
    key: &PartitionKey,
    metadata: &DatasetMetadata,
    policy: &DatasetPolicy,
) -> Result<(), StoreError> {
    let mut current_key = key.clone();
    let mut current_metadata = metadata.clone();
    let mut depth = 0_usize;
    loop {
        if !current_metadata.validates_version(current_key.dataset_version()) {
            return Err(StoreError::InvalidCorrectionLineage);
        }
        let Some(lineage) = current_metadata.correction_lineage() else {
            return Ok(());
        };
        if depth >= policy.maximum_correction_depth {
            return Err(StoreError::InvalidCorrectionLineage);
        }
        let base_version = std::num::NonZeroU32::new(lineage.base_dataset_version())
            .ok_or(StoreError::InvalidCorrectionLineage)?;
        let base_key = PartitionKey::try_new(PartitionKeyInput {
            event_type: current_key.event_type().to_owned(),
            dataset_version: base_version,
            schema_version: current_key.schema_version(),
            venue: current_key.venue().to_owned(),
            instrument_generation: current_key.instrument_generation(),
            utc_date: current_key.utc_date(),
            hour: current_key.hour(),
        })
        .map_err(|_| StoreError::InvalidCorrectionLineage)?;
        let base_manifest = (|| {
            let mut partition = root.try_clone()?;
            for component in base_key.components() {
                partition = partition.open_existing_child(component)?;
            }
            let manifest = load_manifest_at(&partition, &base_key)?;
            verify_manifest_at(&partition, &manifest, policy)?;
            Ok::<Manifest, StoreError>(manifest)
        })()
        .map_err(|_| StoreError::InvalidCorrectionLineage)?;
        if hex::encode(base_manifest.manifest_hash()) != lineage.base_manifest_blake3() {
            return Err(StoreError::InvalidCorrectionLineage);
        }
        let base_known_at_ns = base_manifest
            .metadata()
            .correction_lineage()
            .map_or(base_manifest.maximum_event_time_ns(), |base_lineage| {
                base_lineage.known_at_ns()
            });
        if lineage.known_at_ns() < base_known_at_ns {
            return Err(StoreError::InvalidCorrectionLineage);
        }
        current_key = base_key;
        current_metadata = base_manifest.metadata().clone();
        depth = depth.checked_add(1).ok_or(StoreError::IntegerRange)?;
    }
}

fn validate_batch(
    key: &PartitionKey,
    metadata: &DatasetMetadata,
    batch_id: &BatchId,
    batch: &RecordBatch,
    policy: &DatasetPolicy,
    require_complete_coverage: bool,
) -> Result<(), StoreError> {
    if batch.num_rows() == 0
        || batch.num_rows() > policy.maximum_batch_rows
        || batch.num_columns() == 0
        || batch.num_columns() > policy.maximum_batch_columns
        || batch.get_array_memory_size() > policy.maximum_batch_bytes
    {
        return Err(StoreError::InvalidBatch);
    }
    if !batch.schema().metadata.is_empty()
        || batch
            .schema()
            .fields
            .iter()
            .any(|field| !field.metadata().is_empty())
    {
        return Err(StoreError::SchemaMismatch);
    }
    schema_digest(batch.schema().as_ref(), policy)?;
    validate_value_lengths(batch, policy.maximum_value_bytes)?;

    let effective = required_int64(batch, "effective_event_time_ns", false)?;
    let exchange = required_int64(batch, "exchange_event_time_ns", true)?;
    let receive = required_int64(batch, "receive_wall_time_ns", false)?;
    let event_type = required_string(batch, "event_type")?;
    let venue = required_string(batch, "venue")?;
    let generation = required_u32(batch, "instrument_generation")?;
    let connection_epoch = required_u64(batch, "connection_epoch")?;
    let subscription_epoch = required_u64(batch, "subscription_epoch")?;
    let wal_sequence = required_u64(batch, "wal_record_sequence")?;
    let coverage = batch_id.coverage();
    let (hour_start, hour_end) = key.hour_bounds_ns()?;
    for index in 0..batch.num_rows() {
        if effective.is_null(index)
            || receive.is_null(index)
            || event_type.is_null(index)
            || venue.is_null(index)
            || generation.is_null(index)
            || connection_epoch.is_null(index)
            || subscription_epoch.is_null(index)
            || wal_sequence.is_null(index)
        {
            return Err(StoreError::SchemaMismatch);
        }
        let expected_effective = if exchange.is_null(index) {
            receive.value(index)
        } else {
            exchange.value(index)
        };
        if effective.value(index) != expected_effective {
            return Err(StoreError::InvalidEventTimeColumn);
        }
        if effective.value(index) < hour_start || effective.value(index) >= hour_end {
            return Err(StoreError::EventTimeOutsidePartition);
        }
        if event_type.value(index) != key.event_type()
            || venue.value(index) != key.venue()
            || generation.value(index) != key.instrument_generation().get()
            || connection_epoch.value(index) != coverage.connection_epoch()
            || subscription_epoch.value(index) != coverage.subscription_epoch()
        {
            return Err(StoreError::PartitionDimensionMismatch);
        }
        if wal_sequence.value(index) < coverage.first_sequence()
            || wal_sequence.value(index) > coverage.last_sequence()
        {
            return Err(StoreError::InvalidBatchIdentity);
        }
    }
    let minimum_sequence = (0..wal_sequence.len())
        .map(|index| wal_sequence.value(index))
        .min()
        .ok_or(StoreError::InvalidBatch)?;
    let maximum_sequence = (0..wal_sequence.len())
        .map(|index| wal_sequence.value(index))
        .max()
        .ok_or(StoreError::InvalidBatch)?;
    if metadata.source_coverage() != std::slice::from_ref(coverage)
        || (require_complete_coverage
            && (minimum_sequence != coverage.first_sequence()
                || maximum_sequence != coverage.last_sequence()))
    {
        return Err(StoreError::InvalidBatchIdentity);
    }
    Ok(())
}

fn required_int64<'a>(
    batch: &'a RecordBatch,
    name: &'static str,
    nullable: bool,
) -> Result<&'a Int64Array, StoreError> {
    let schema = batch.schema();
    let (index, field) = schema
        .column_with_name(name)
        .ok_or(StoreError::InvalidEventTimeColumn)?;
    if field.data_type() != &DataType::Int64 || field.is_nullable() != nullable {
        return Err(StoreError::InvalidEventTimeColumn);
    }
    batch
        .column(index)
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or(StoreError::InvalidEventTimeColumn)
}

fn required_string<'a>(
    batch: &'a RecordBatch,
    name: &'static str,
) -> Result<&'a StringArray, StoreError> {
    let schema = batch.schema();
    let (index, field) = schema
        .column_with_name(name)
        .ok_or(StoreError::SchemaMismatch)?;
    if field.data_type() != &DataType::Utf8 || field.is_nullable() {
        return Err(StoreError::SchemaMismatch);
    }
    batch
        .column(index)
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or(StoreError::SchemaMismatch)
}

fn required_u32<'a>(
    batch: &'a RecordBatch,
    name: &'static str,
) -> Result<&'a UInt32Array, StoreError> {
    let schema = batch.schema();
    let (index, field) = schema
        .column_with_name(name)
        .ok_or(StoreError::SchemaMismatch)?;
    if field.data_type() != &DataType::UInt32 || field.is_nullable() {
        return Err(StoreError::SchemaMismatch);
    }
    batch
        .column(index)
        .as_any()
        .downcast_ref::<UInt32Array>()
        .ok_or(StoreError::SchemaMismatch)
}

fn required_u64<'a>(
    batch: &'a RecordBatch,
    name: &'static str,
) -> Result<&'a UInt64Array, StoreError> {
    let schema = batch.schema();
    let (index, field) = schema
        .column_with_name(name)
        .ok_or(StoreError::SchemaMismatch)?;
    if field.data_type() != &DataType::UInt64 || field.is_nullable() {
        return Err(StoreError::SchemaMismatch);
    }
    batch
        .column(index)
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or(StoreError::SchemaMismatch)
}

fn event_time_bounds(batch: &RecordBatch) -> Result<(i64, i64), StoreError> {
    let times = required_int64(batch, "effective_event_time_ns", false)?;
    let minimum = (0..times.len())
        .map(|index| times.value(index))
        .min()
        .ok_or(StoreError::InvalidBatch)?;
    let maximum = (0..times.len())
        .map(|index| times.value(index))
        .max()
        .ok_or(StoreError::InvalidBatch)?;
    Ok((minimum, maximum))
}

fn validate_value_lengths(batch: &RecordBatch, maximum: usize) -> Result<(), StoreError> {
    for column in batch.columns() {
        match column.data_type() {
            DataType::Utf8 => {
                let values = column
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .ok_or(StoreError::SchemaMismatch)?;
                if values.iter().flatten().any(|value| value.len() > maximum) {
                    return Err(StoreError::InvalidBatch);
                }
            }
            DataType::LargeUtf8 => {
                let values = column
                    .as_any()
                    .downcast_ref::<LargeStringArray>()
                    .ok_or(StoreError::SchemaMismatch)?;
                if values.iter().flatten().any(|value| value.len() > maximum) {
                    return Err(StoreError::InvalidBatch);
                }
            }
            DataType::Binary => {
                let values = column
                    .as_any()
                    .downcast_ref::<BinaryArray>()
                    .ok_or(StoreError::SchemaMismatch)?;
                if values.iter().flatten().any(|value| value.len() > maximum) {
                    return Err(StoreError::InvalidBatch);
                }
            }
            DataType::LargeBinary => {
                let values = column
                    .as_any()
                    .downcast_ref::<LargeBinaryArray>()
                    .ok_or(StoreError::SchemaMismatch)?;
                if values.iter().flatten().any(|value| value.len() > maximum) {
                    return Err(StoreError::InvalidBatch);
                }
            }
            data_type if data_type.is_nested() => return Err(StoreError::SchemaMismatch),
            _ => {}
        }
    }
    Ok(())
}

fn schema_digest(schema: &Schema, policy: &DatasetPolicy) -> Result<[u8; 32], StoreError> {
    let stable_schema = stable_schema(schema);
    let value = serde_json::to_value(stable_schema).map_err(|_| StoreError::SchemaMismatch)?;
    let (field_count, depth) = schema_shape(&value, 0)?;
    let canonical = canonical_json(&value)?;
    if canonical.len() > policy.maximum_schema_bytes
        || field_count > policy.maximum_schema_fields
        || depth > policy.maximum_schema_depth
    {
        return Err(StoreError::SchemaMismatch);
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"cmti:arrow-schema:v1\0");
    hasher.update(&canonical);
    Ok(*hasher.finalize().as_bytes())
}

fn stable_schema(schema: &Schema) -> Schema {
    Schema::new_with_metadata(
        schema.fields.clone(),
        schema
            .metadata
            .iter()
            .filter(|(key, _)| !key.starts_with("cmti."))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    )
}

fn schema_shape(value: &serde_json::Value, depth: usize) -> Result<(usize, usize), StoreError> {
    match value {
        serde_json::Value::Array(values) => {
            let mut fields = 0_usize;
            let mut maximum_depth = depth;
            for value in values {
                let (child_fields, child_depth) = schema_shape(value, depth + 1)?;
                fields = fields
                    .checked_add(child_fields)
                    .ok_or(StoreError::IntegerRange)?;
                maximum_depth = maximum_depth.max(child_depth);
            }
            Ok((fields, maximum_depth))
        }
        serde_json::Value::Object(values) => {
            let mut fields = usize::from(values.contains_key("name"));
            let mut maximum_depth = depth;
            for value in values.values() {
                let (child_fields, child_depth) = schema_shape(value, depth + 1)?;
                fields = fields
                    .checked_add(child_fields)
                    .ok_or(StoreError::IntegerRange)?;
                maximum_depth = maximum_depth.max(child_depth);
            }
            Ok((fields, maximum_depth))
        }
        _ => Ok((0, depth)),
    }
}

fn canonical_json(value: &serde_json::Value) -> Result<Vec<u8>, StoreError> {
    fn normalize(value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Array(values) => {
                serde_json::Value::Array(values.iter().map(normalize).collect())
            }
            serde_json::Value::Object(values) => {
                let sorted = values
                    .iter()
                    .map(|(key, value)| (key.clone(), normalize(value)))
                    .collect::<BTreeMap<_, _>>();
                serde_json::Value::Object(sorted.into_iter().collect())
            }
            _ => value.clone(),
        }
    }
    serde_json::to_vec(&normalize(value)).map_err(StoreError::from)
}

fn footer_metadata(
    key: &PartitionKey,
    metadata: &DatasetMetadata,
    schema_digest: [u8; 32],
    minimum_event_time_ns: i64,
    maximum_event_time_ns: i64,
    row_count: u64,
    batch_id: Option<&BatchId>,
) -> Result<Vec<KeyValue>, StoreError> {
    let source_coverage = serde_json::to_string(metadata.source_coverage())?;
    let mut values = BTreeMap::from([
        ("cmti.format_version", FOOTER_FORMAT_VERSION.to_owned()),
        ("cmti.event_type", key.event_type().to_owned()),
        ("cmti.venue", key.venue().to_owned()),
        (
            "cmti.instrument_generation",
            key.instrument_generation().to_string(),
        ),
        ("cmti.dataset_version", key.dataset_version().to_string()),
        ("cmti.schema_version", key.schema_version().to_string()),
        (
            "cmti.feature_version",
            metadata.feature_version().to_owned(),
        ),
        ("cmti.parser_version", metadata.parser_version().to_owned()),
        (
            "cmti.normalizer_version",
            metadata.normalizer_version().to_owned(),
        ),
        ("cmti.code_identity", metadata.code_identity().to_owned()),
        ("cmti.source_coverage", source_coverage),
        ("cmti.schema_blake3", hex::encode(schema_digest)),
        (
            "cmti.minimum_event_time_ns",
            minimum_event_time_ns.to_string(),
        ),
        (
            "cmti.maximum_event_time_ns",
            maximum_event_time_ns.to_string(),
        ),
        ("cmti.row_count", row_count.to_string()),
    ]);
    if let Some(batch_id) = batch_id {
        values.insert("cmti.batch_id", serde_json::to_string(batch_id)?);
        values.insert("cmti.batch_id_blake3", hex::encode(batch_id.digest()?));
    }
    if let Some(lineage) = metadata.correction_lineage() {
        values.insert("cmti.correction_lineage", serde_json::to_string(lineage)?);
    }
    Ok(values
        .into_iter()
        .map(|(key, value)| KeyValue::new(key.to_owned(), value))
        .collect())
}

fn write_parquet_temporary(
    directory: &Directory,
    final_name: &OsStr,
    schema: SchemaRef,
    batches: impl IntoIterator<Item = RecordBatch>,
    metadata: Vec<KeyValue>,
    policy: &DatasetPolicy,
    #[cfg(feature = "test-support")] faults: &FaultInjector,
) -> Result<TemporaryFile, StoreError> {
    let (name, mut file) = create_temporary_file(directory, final_name)?;
    let result = (|| {
        let properties = WriterProperties::builder()
            .set_compression(Compression::ZSTD(ZstdLevel::try_new(1)?))
            .set_max_row_group_row_count(Some(policy.maximum_row_group_rows))
            .set_max_row_group_bytes(Some(policy.target_row_group_bytes))
            .set_key_value_metadata(Some(metadata))
            .build();
        {
            let mut writer = ArrowWriter::try_new(&mut file, schema, Some(properties))?;
            for batch in batches {
                if writer
                    .memory_size()
                    .checked_add(batch.get_array_memory_size())
                    .ok_or(StoreError::IntegerRange)?
                    > policy.maximum_writer_memory_bytes
                {
                    return Err(StoreError::CapacityExceeded);
                }
                writer.write(&batch)?;
                if writer.memory_size() > policy.maximum_writer_memory_bytes
                    || writer.bytes_written()
                        > usize::try_from(policy.maximum_file_bytes).unwrap_or(usize::MAX)
                {
                    return Err(StoreError::CapacityExceeded);
                }
            }
            writer.close()?;
        }
        #[cfg(feature = "test-support")]
        faults.trip(FaultPoint::AfterParquetCloseBeforeFileSync)?;
        file.sync_all()?;
        #[cfg(feature = "test-support")]
        faults.trip(FaultPoint::AfterFileSyncBeforeHash)?;
        let identity = validate_private_file(&file)?;
        validate_parquet_footer(&file, policy.maximum_footer_bytes)?;
        let (size_bytes, blake3) = hash_validated_file(&file, policy.maximum_file_bytes)?;
        #[cfg(feature = "test-support")]
        faults.trip(FaultPoint::AfterFileHashBeforeRename)?;
        Ok(TemporaryFile {
            name: name.clone(),
            file,
            identity,
            size_bytes,
            blake3,
        })
    })();
    if result.is_err() && !is_injected_fault(&result) {
        let _ = rustix::fs::unlinkat(&directory.file, &name, AtFlags::empty());
    }
    result
}

struct SealingRequest<'a> {
    active: &'a Directory,
    key: &'a PartitionKey,
    metadata: &'a DatasetMetadata,
    fragments: &'a [ReadFragment],
    total_rows: u64,
    minimum_event_time_ns: i64,
    maximum_event_time_ns: i64,
}

fn publish_sealed_files(
    sealed: &Directory,
    request: SealingRequest<'_>,
    policy: &DatasetPolicy,
    #[cfg(feature = "test-support")] faults: &FaultInjector,
) -> Result<Vec<ManifestFile>, StoreError> {
    let first = request
        .fragments
        .first()
        .ok_or(StoreError::PartitionEmpty)?;
    let fallback_names = request
        .fragments
        .iter()
        .enumerate()
        .map(|(index, fragment)| fallback_sealed_name(index, fragment))
        .collect::<Result<Vec<_>, _>>()?;
    let fallback_inventory = fallback_names.iter().cloned().collect::<BTreeSet<_>>();
    let existing_inventory = directory_names(
        &sealed.file,
        policy
            .maximum_sealed_files
            .checked_add(MAXIMUM_STALE_TEMPORARY_FILES)
            .ok_or(StoreError::IntegerRange)?,
    )?
    .into_iter()
    .collect::<BTreeSet<_>>();
    let compacted_inventory = BTreeSet::from([OsString::from(SEALED_FILE_NAME)]);

    if existing_inventory == compacted_inventory {
        return publish_compacted_file(
            sealed,
            &request,
            first,
            policy,
            #[cfg(feature = "test-support")]
            faults,
        );
    }
    if !existing_inventory.is_empty() {
        if existing_inventory.is_subset(&fallback_inventory) {
            return publish_fallback_files(
                sealed,
                &request,
                &fallback_names,
                policy,
                #[cfg(feature = "test-support")]
                faults,
            );
        }
        return Err(StoreError::Integrity);
    }

    match publish_compacted_file(
        sealed,
        &request,
        first,
        policy,
        #[cfg(feature = "test-support")]
        faults,
    ) {
        Ok(files) => Ok(files),
        Err(StoreError::CapacityExceeded) => publish_fallback_files(
            sealed,
            &request,
            &fallback_names,
            policy,
            #[cfg(feature = "test-support")]
            faults,
        ),
        Err(error) => Err(error),
    }
}

fn publish_compacted_file(
    sealed: &Directory,
    request: &SealingRequest<'_>,
    first: &ReadFragment,
    policy: &DatasetPolicy,
    #[cfg(feature = "test-support")] faults: &FaultInjector,
) -> Result<Vec<ManifestFile>, StoreError> {
    #[cfg(feature = "test-support")]
    faults.trip_capacity(FaultPoint::CompactedFileExceedsCapacity)?;
    let footer = footer_metadata(
        request.key,
        request.metadata,
        first.schema_digest,
        request.minimum_event_time_ns,
        request.maximum_event_time_ns,
        request.total_rows,
        None,
    )?;
    let temporary = write_compacted_temporary(
        sealed,
        CompactionRequest {
            final_name: OsStr::new(SEALED_FILE_NAME),
            key: request.key,
            schema: Arc::clone(&first.schema),
            schema_digest: first.schema_digest,
            active: request.active,
            fragments: request.fragments,
            footer,
            expected_rows: request.total_rows,
        },
        policy,
        #[cfg(feature = "test-support")]
        faults,
    )?;
    verify_parquet_file(
        &temporary.file,
        ParquetExpectation {
            key: request.key,
            metadata: request.metadata,
            schema_digest: first.schema_digest,
            minimum_event_time_ns: request.minimum_event_time_ns,
            maximum_event_time_ns: request.maximum_event_time_ns,
            row_count: request.total_rows,
        },
        policy,
    )?;
    install_sealed_or_verify_retry(
        sealed,
        temporary,
        OsStr::new(SEALED_FILE_NAME),
        policy,
        #[cfg(feature = "test-support")]
        faults,
    )?;
    let installed = sealed.open_file(OsStr::new(SEALED_FILE_NAME), OFlags::RDONLY)?;
    let (size_bytes, blake3) = hash_validated_file(&installed, policy.maximum_file_bytes)?;
    Ok(vec![ManifestFile::new(
        request
            .key
            .relative_path()
            .join(SEALED_DIRECTORY)
            .join(SEALED_FILE_NAME),
        size_bytes,
        blake3,
        request.total_rows,
        request.minimum_event_time_ns,
        request.maximum_event_time_ns,
        request.metadata.source_coverage().to_vec(),
    )?])
}

fn publish_fallback_files(
    sealed: &Directory,
    request: &SealingRequest<'_>,
    names: &[OsString],
    policy: &DatasetPolicy,
    #[cfg(feature = "test-support")] faults: &FaultInjector,
) -> Result<Vec<ManifestFile>, StoreError> {
    if names.len() != request.fragments.len() || names.len() > policy.maximum_sealed_files {
        return Err(StoreError::FileCapacityExceeded);
    }
    request
        .fragments
        .iter()
        .zip(names)
        .map(|(fragment, name)| {
            visit_validated_fragment_batches(
                request.active,
                fragment,
                request.key,
                &fragment.schema,
                fragment.schema_digest,
                policy,
                |_| Ok(()),
            )?;
            let temporary = copy_fragment_temporary(
                sealed,
                request.active,
                name,
                fragment,
                policy,
                #[cfg(feature = "test-support")]
                faults,
            )?;
            install_sealed_or_verify_retry(
                sealed,
                temporary,
                name,
                policy,
                #[cfg(feature = "test-support")]
                faults,
            )?;
            let installed = sealed.open_file(name, OFlags::RDONLY)?;
            let (size_bytes, blake3) = hash_validated_file(&installed, policy.maximum_file_bytes)?;
            if size_bytes != fragment.size_bytes || blake3 != fragment.fragment_blake3 {
                return Err(StoreError::Integrity);
            }
            verify_parquet_file(
                &installed,
                ParquetExpectation {
                    key: request.key,
                    metadata: &fragment.metadata,
                    schema_digest: fragment.schema_digest,
                    minimum_event_time_ns: fragment.minimum_event_time_ns,
                    maximum_event_time_ns: fragment.maximum_event_time_ns,
                    row_count: fragment.row_count,
                },
                policy,
            )?;
            ManifestFile::new(
                request
                    .key
                    .relative_path()
                    .join(SEALED_DIRECTORY)
                    .join(name),
                size_bytes,
                blake3,
                fragment.row_count,
                fragment.minimum_event_time_ns,
                fragment.maximum_event_time_ns,
                fragment.metadata.source_coverage().to_vec(),
            )
        })
        .collect()
}

fn fallback_sealed_name(index: usize, fragment: &ReadFragment) -> Result<OsString, StoreError> {
    let ordinal = u32::try_from(index).map_err(|_| StoreError::IntegerRange)?;
    Ok(OsString::from(format!(
        "data-{ordinal:04}-{}.parquet",
        hex::encode(fragment.fragment_blake3)
    )))
}

fn copy_fragment_temporary(
    sealed: &Directory,
    active: &Directory,
    final_name: &OsStr,
    fragment: &ReadFragment,
    policy: &DatasetPolicy,
    #[cfg(feature = "test-support")] faults: &FaultInjector,
) -> Result<TemporaryFile, StoreError> {
    let (name, mut output) = create_temporary_file(sealed, final_name)?;
    let result = (|| {
        let input = active.open_file(&fragment.file_name, OFlags::RDONLY)?;
        let copied = io::copy(
            &mut input.take(
                fragment
                    .size_bytes
                    .checked_add(1)
                    .ok_or(StoreError::IntegerRange)?,
            ),
            &mut output,
        )?;
        if copied != fragment.size_bytes {
            return Err(StoreError::Integrity);
        }
        #[cfg(feature = "test-support")]
        faults.trip(FaultPoint::AfterParquetCloseBeforeFileSync)?;
        output.sync_all()?;
        #[cfg(feature = "test-support")]
        faults.trip(FaultPoint::AfterFileSyncBeforeHash)?;
        let identity = validate_private_file(&output)?;
        let (size_bytes, blake3) = hash_validated_file(&output, policy.maximum_file_bytes)?;
        if size_bytes != fragment.size_bytes || blake3 != fragment.fragment_blake3 {
            return Err(StoreError::Integrity);
        }
        #[cfg(feature = "test-support")]
        faults.trip(FaultPoint::AfterFileHashBeforeRename)?;
        Ok(TemporaryFile {
            name: name.clone(),
            file: output,
            identity,
            size_bytes,
            blake3,
        })
    })();
    if result.is_err() && !is_injected_fault(&result) {
        let _ = rustix::fs::unlinkat(&sealed.file, &name, AtFlags::empty());
    }
    result
}

struct CompactionRequest<'a> {
    final_name: &'a OsStr,
    key: &'a PartitionKey,
    schema: SchemaRef,
    schema_digest: [u8; 32],
    active: &'a Directory,
    fragments: &'a [ReadFragment],
    footer: Vec<KeyValue>,
    expected_rows: u64,
}

fn visit_validated_fragment_batches(
    active: &Directory,
    fragment: &ReadFragment,
    key: &PartitionKey,
    schema: &SchemaRef,
    expected_schema_digest: [u8; 32],
    policy: &DatasetPolicy,
    mut visit: impl FnMut(&RecordBatch) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    let input = active.open_file(&fragment.file_name, OFlags::RDONLY)?;
    validate_parquet_footer(&input, policy.maximum_footer_bytes)?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(input)?
        .with_batch_size(policy.reader_batch_rows)
        .build()?;
    let mut fragment_rows = 0_u64;
    let mut fragment_minimum = None;
    let mut fragment_maximum = None;
    let mut fragment_sequence_minimum = None;
    let mut fragment_sequence_maximum = None;
    for batch in reader {
        let batch = batch?;
        if batch.get_array_memory_size() > policy.maximum_batch_bytes {
            return Err(StoreError::CapacityExceeded);
        }
        if schema_digest(batch.schema().as_ref(), policy)? != expected_schema_digest {
            return Err(StoreError::SchemaMismatch);
        }
        let stable_batch = RecordBatch::try_new(Arc::clone(schema), batch.columns().to_vec())?;
        validate_batch(
            key,
            &fragment.metadata,
            &fragment.batch_id,
            &stable_batch,
            policy,
            false,
        )?;
        let sequences = required_u64(&stable_batch, "wal_record_sequence")?;
        let batch_sequence_minimum = (0..sequences.len())
            .map(|index| sequences.value(index))
            .min()
            .ok_or(StoreError::Integrity)?;
        let batch_sequence_maximum = (0..sequences.len())
            .map(|index| sequences.value(index))
            .max()
            .ok_or(StoreError::Integrity)?;
        fragment_sequence_minimum = Some(
            fragment_sequence_minimum.map_or(batch_sequence_minimum, |value: u64| {
                value.min(batch_sequence_minimum)
            }),
        );
        fragment_sequence_maximum = Some(
            fragment_sequence_maximum.map_or(batch_sequence_maximum, |value: u64| {
                value.max(batch_sequence_maximum)
            }),
        );
        let (batch_minimum, batch_maximum) = event_time_bounds(&stable_batch)?;
        fragment_minimum =
            Some(fragment_minimum.map_or(batch_minimum, |value: i64| value.min(batch_minimum)));
        fragment_maximum =
            Some(fragment_maximum.map_or(batch_maximum, |value: i64| value.max(batch_maximum)));
        fragment_rows = fragment_rows
            .checked_add(
                u64::try_from(stable_batch.num_rows()).map_err(|_| StoreError::IntegerRange)?,
            )
            .ok_or(StoreError::IntegerRange)?;
        visit(&stable_batch)?;
    }
    if fragment_rows != fragment.row_count
        || fragment_minimum != Some(fragment.minimum_event_time_ns)
        || fragment_maximum != Some(fragment.maximum_event_time_ns)
        || fragment_sequence_minimum != Some(fragment.batch_id.coverage().first_sequence())
        || fragment_sequence_maximum != Some(fragment.batch_id.coverage().last_sequence())
    {
        return Err(StoreError::Integrity);
    }
    Ok(())
}

fn write_compacted_temporary(
    directory: &Directory,
    request: CompactionRequest<'_>,
    policy: &DatasetPolicy,
    #[cfg(feature = "test-support")] faults: &FaultInjector,
) -> Result<TemporaryFile, StoreError> {
    let (name, mut file) = create_temporary_file(directory, request.final_name)?;
    let result = (|| {
        let properties = WriterProperties::builder()
            .set_compression(Compression::ZSTD(ZstdLevel::try_new(1)?))
            .set_max_row_group_row_count(Some(policy.maximum_row_group_rows))
            .set_max_row_group_bytes(Some(policy.target_row_group_bytes))
            .set_key_value_metadata(Some(request.footer))
            .build();
        let mut rows_written = 0_u64;
        {
            let mut writer =
                ArrowWriter::try_new(&mut file, Arc::clone(&request.schema), Some(properties))?;
            for fragment in request.fragments {
                visit_validated_fragment_batches(
                    request.active,
                    fragment,
                    request.key,
                    &request.schema,
                    request.schema_digest,
                    policy,
                    |stable_batch| {
                        if writer
                            .memory_size()
                            .checked_add(stable_batch.get_array_memory_size())
                            .ok_or(StoreError::IntegerRange)?
                            > policy.maximum_writer_memory_bytes
                        {
                            return Err(StoreError::CapacityExceeded);
                        }
                        rows_written = rows_written
                            .checked_add(
                                u64::try_from(stable_batch.num_rows())
                                    .map_err(|_| StoreError::IntegerRange)?,
                            )
                            .ok_or(StoreError::IntegerRange)?;
                        if rows_written > policy.maximum_partition_rows {
                            return Err(StoreError::CapacityExceeded);
                        }
                        writer.write(stable_batch)?;
                        if writer.memory_size() > policy.maximum_writer_memory_bytes
                            || writer.bytes_written()
                                > usize::try_from(policy.maximum_file_bytes).unwrap_or(usize::MAX)
                        {
                            return Err(StoreError::CapacityExceeded);
                        }
                        Ok(())
                    },
                )?;
            }
            if rows_written != request.expected_rows {
                return Err(StoreError::Integrity);
            }
            writer.close()?;
        }
        #[cfg(feature = "test-support")]
        faults.trip(FaultPoint::AfterParquetCloseBeforeFileSync)?;
        file.sync_all()?;
        #[cfg(feature = "test-support")]
        faults.trip(FaultPoint::AfterFileSyncBeforeHash)?;
        let identity = validate_private_file(&file)?;
        validate_parquet_footer(&file, policy.maximum_footer_bytes)?;
        let (size_bytes, blake3) = hash_validated_file(&file, policy.maximum_file_bytes)?;
        #[cfg(feature = "test-support")]
        faults.trip(FaultPoint::AfterFileHashBeforeRename)?;
        Ok(TemporaryFile {
            name: name.clone(),
            file,
            identity,
            size_bytes,
            blake3,
        })
    })();
    if result.is_err() && !is_injected_fault(&result) {
        let _ = rustix::fs::unlinkat(&directory.file, &name, AtFlags::empty());
    }
    result
}

fn create_temporary_file(
    directory: &Directory,
    final_name: &OsStr,
) -> Result<(OsString, File), StoreError> {
    let process_id = std::process::id();
    let first_sequence = TEMP_SEQUENCE.fetch_add(TEMP_CREATE_ATTEMPTS, Ordering::Relaxed);
    for attempt in 0..TEMP_CREATE_ATTEMPTS {
        let mut name = OsString::from(".");
        name.push(final_name);
        name.push(format!(
            ".create-{process_id}-{}.tmp",
            first_sequence.wrapping_add(attempt)
        ));
        match rustix::fs::openat(
            &directory.file,
            &name,
            OFlags::CREATE | OFlags::EXCL | OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        ) {
            Ok(file) => return Ok((name, File::from(file))),
            Err(error) if error == rustix::io::Errno::EXIST => {}
            Err(error) => return Err(io::Error::from(error).into()),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique Parquet temporary file",
    )
    .into())
}

fn discard_temporary(directory: &Directory, temporary: &TemporaryFile) -> Result<(), StoreError> {
    rustix::fs::unlinkat(&directory.file, &temporary.name, AtFlags::empty())
        .map_err(io::Error::from)?;
    directory.file.sync_all()?;
    Ok(())
}

fn install_fragment_or_verify_retry(
    directory: &Directory,
    temporary: TemporaryFile,
    final_name: &str,
    key: &PartitionKey,
    expected: &Fragment,
    policy: &DatasetPolicy,
    #[cfg(feature = "test-support")] faults: &FaultInjector,
) -> Result<(), StoreError> {
    if identity(&temporary.file)? != temporary.identity {
        return Err(StoreError::UnsafePath);
    }
    match rustix::fs::renameat_with(
        &directory.file,
        &temporary.name,
        &directory.file,
        final_name,
        RenameFlags::NOREPLACE,
    ) {
        Ok(()) => {
            verify_installed_identity(&directory.file, OsStr::new(final_name), temporary.identity)?;
            #[cfg(feature = "test-support")]
            faults.trip(FaultPoint::AfterDataRenameBeforeDirectorySync)?;
            directory.file.sync_all()?;
            Ok(())
        }
        Err(error) if error == rustix::io::Errno::EXIST => {
            let _ = rustix::fs::unlinkat(&directory.file, &temporary.name, AtFlags::empty());
            let existing = directory.open_file(final_name, OFlags::RDONLY)?;
            let (size_bytes, blake3) = hash_validated_file(&existing, policy.maximum_file_bytes)?;
            let fragment = read_fragment(directory, OsStr::new(final_name), key, policy)?;
            if size_bytes == expected.size_bytes
                && blake3 == expected.blake3
                && fragment.schema_digest == expected.schema_digest
                && fragment.minimum_event_time_ns == expected.minimum_event_time_ns
                && fragment.maximum_event_time_ns == expected.maximum_event_time_ns
                && fragment.row_count == expected.row_count
                && footer_value(
                    &existing,
                    "cmti.batch_id_blake3",
                    policy.maximum_footer_bytes,
                )? == hex::encode(expected.batch_id_digest)
            {
                Ok(())
            } else {
                Err(StoreError::BatchIdentityConflict)
            }
        }
        Err(error) => {
            let _ = rustix::fs::unlinkat(&directory.file, &temporary.name, AtFlags::empty());
            Err(io::Error::from(error).into())
        }
    }
}

fn install_sealed_or_verify_retry(
    directory: &Directory,
    temporary: TemporaryFile,
    final_name: &OsStr,
    policy: &DatasetPolicy,
    #[cfg(feature = "test-support")] faults: &FaultInjector,
) -> Result<(), StoreError> {
    if identity(&temporary.file)? != temporary.identity {
        return Err(StoreError::UnsafePath);
    }
    match rustix::fs::renameat_with(
        &directory.file,
        &temporary.name,
        &directory.file,
        final_name,
        RenameFlags::NOREPLACE,
    ) {
        Ok(()) => {
            verify_installed_identity(&directory.file, final_name, temporary.identity)?;
            #[cfg(feature = "test-support")]
            faults.trip(FaultPoint::AfterDataRenameBeforeDirectorySync)?;
            directory.file.sync_all()?;
            Ok(())
        }
        Err(error) if error == rustix::io::Errno::EXIST => {
            let _ = rustix::fs::unlinkat(&directory.file, &temporary.name, AtFlags::empty());
            let existing = directory.open_file(final_name, OFlags::RDONLY)?;
            let (size_bytes, blake3) = hash_validated_file(&existing, policy.maximum_file_bytes)?;
            if size_bytes == temporary.size_bytes && blake3 == temporary.blake3 {
                Ok(())
            } else {
                Err(StoreError::Integrity)
            }
        }
        Err(error) => {
            let _ = rustix::fs::unlinkat(&directory.file, &temporary.name, AtFlags::empty());
            Err(io::Error::from(error).into())
        }
    }
}

fn publish_manifest(
    directory: &Directory,
    manifest: &Manifest,
    #[cfg(feature = "test-support")] faults: &FaultInjector,
) -> Result<(), StoreError> {
    let bytes = manifest.canonical_bytes()?;
    let (temporary_name, mut file) = create_temporary_file(directory, OsStr::new(MANIFEST_NAME))?;
    let result = (|| {
        use std::io::Write;
        file.write_all(&bytes)?;
        file.sync_all()?;
        #[cfg(feature = "test-support")]
        faults.trip(FaultPoint::AfterManifestTempSyncBeforeRename)?;
        let expected_identity = validate_private_file(&file)?;
        match rustix::fs::renameat_with(
            &directory.file,
            &temporary_name,
            &directory.file,
            MANIFEST_NAME,
            RenameFlags::NOREPLACE,
        ) {
            Ok(()) => {
                verify_installed_identity(
                    &directory.file,
                    OsStr::new(MANIFEST_NAME),
                    expected_identity,
                )?;
                #[cfg(feature = "test-support")]
                faults.trip(FaultPoint::AfterManifestRenameBeforeDirectorySync)?;
                directory.file.sync_all()?;
                Ok(())
            }
            Err(error) if error == rustix::io::Errno::EXIST => {
                let installed = load_manifest_at(directory, manifest.partition())?;
                if installed == *manifest {
                    Ok(())
                } else {
                    Err(StoreError::Integrity)
                }
            }
            Err(error) => Err(io::Error::from(error).into()),
        }
    })();
    if !is_injected_fault(&result) {
        let _ = rustix::fs::unlinkat(&directory.file, &temporary_name, AtFlags::empty());
    }
    result
}

fn is_injected_fault<T>(result: &Result<T, StoreError>) -> bool {
    #[cfg(feature = "test-support")]
    {
        matches!(result, Err(StoreError::InjectedFault))
    }
    #[cfg(not(feature = "test-support"))]
    {
        let _ = result;
        false
    }
}

fn load_manifest_at(
    partition: &Directory,
    expected_key: &PartitionKey,
) -> Result<Manifest, StoreError> {
    let mut file = partition.open_file(MANIFEST_NAME, OFlags::RDONLY)?;
    let length = file.metadata()?.len();
    if length == 0 || length > MAXIMUM_MANIFEST_BYTES as u64 {
        return Err(StoreError::CapacityExceeded);
    }
    let mut bytes = vec![0_u8; usize::try_from(length).map_err(|_| StoreError::IntegerRange)?];
    file.read_exact(&mut bytes)?;
    let mut trailing = [0_u8; 1];
    if file.read(&mut trailing)? != 0 {
        return Err(StoreError::Integrity);
    }
    let manifest = Manifest::from_canonical_bytes(&bytes)?;
    if manifest.partition() != expected_key {
        return Err(StoreError::Integrity);
    }
    Ok(manifest)
}

fn verify_manifest_at(
    partition: &Directory,
    manifest: &Manifest,
    policy: &DatasetPolicy,
) -> Result<(), StoreError> {
    if manifest.files().is_empty() || manifest.files().len() > policy.maximum_sealed_files {
        return Err(StoreError::FileCapacityExceeded);
    }
    let partition_size_bytes = manifest.files().iter().try_fold(0_u64, |total, file| {
        total
            .checked_add(file.size_bytes())
            .ok_or(StoreError::IntegerRange)
    })?;
    if partition_size_bytes > policy.maximum_partition_bytes
        || manifest.row_count() > policy.maximum_partition_rows
    {
        return Err(StoreError::CapacityExceeded);
    }
    let sealed = partition.open_existing_child(SEALED_DIRECTORY)?;
    let declared = manifest
        .files()
        .iter()
        .map(|entry| {
            entry
                .relative_path()
                .file_name()
                .map(OsStr::to_owned)
                .ok_or(StoreError::Integrity)
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    let actual = directory_names(
        &sealed.file,
        policy
            .maximum_sealed_files
            .checked_add(MAXIMUM_STALE_TEMPORARY_FILES)
            .ok_or(StoreError::IntegerRange)?,
    )?
    .into_iter()
    .collect::<BTreeSet<_>>();
    if actual != declared {
        return Err(StoreError::Integrity);
    }
    for entry in manifest.files() {
        let expected_prefix = manifest.partition().relative_path().join(SEALED_DIRECTORY);
        if !safe_relative_path(entry.relative_path())
            || entry.relative_path().parent() != Some(expected_prefix.as_path())
        {
            return Err(StoreError::Integrity);
        }
        let name = entry
            .relative_path()
            .file_name()
            .ok_or(StoreError::Integrity)?;
        let file = sealed.open_file(name, OFlags::RDONLY)?;
        let (size_bytes, blake3) = hash_validated_file(&file, policy.maximum_file_bytes)?;
        if size_bytes != entry.size_bytes() || &blake3 != entry.blake3() {
            return Err(StoreError::Integrity);
        }
        let file_metadata = manifest
            .metadata()
            .with_source_coverage(entry.source_coverage().to_vec())
            .map_err(|_| StoreError::Integrity)?;
        verify_parquet_file(
            &file,
            ParquetExpectation {
                key: manifest.partition(),
                metadata: &file_metadata,
                schema_digest: *manifest.schema_digest(),
                minimum_event_time_ns: entry.minimum_event_time_ns(),
                maximum_event_time_ns: entry.maximum_event_time_ns(),
                row_count: entry.row_count(),
            },
            policy,
        )?;
    }
    Ok(())
}

fn read_fragment(
    directory: &Directory,
    name: &OsStr,
    key: &PartitionKey,
    policy: &DatasetPolicy,
) -> Result<ReadFragment, StoreError> {
    let file = directory.open_file(name, OFlags::RDONLY)?;
    let size_bytes = file.metadata()?.len();
    if size_bytes == 0 || size_bytes > policy.maximum_file_bytes {
        return Err(StoreError::CapacityExceeded);
    }
    validate_parquet_footer(&file, policy.maximum_footer_bytes)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file.try_clone()?)?;
    if builder.metadata().memory_size() > policy.maximum_schema_bytes {
        return Err(StoreError::CapacityExceeded);
    }
    let schema_digest = schema_digest(builder.schema().as_ref(), policy)?;
    let schema = Arc::new(stable_schema(builder.schema().as_ref()));
    let footer = unique_footer(builder.metadata())?;
    let metadata = metadata_from_footer(&footer)?;
    let batch_id = batch_id_from_footer(&footer)?;
    if metadata.source_coverage() != std::slice::from_ref(batch_id.coverage()) {
        return Err(StoreError::Integrity);
    }
    let batch_id_digest = batch_id.digest()?;
    let expected_name = format!("fragment-{}.parquet", hex::encode(batch_id_digest));
    if name != OsStr::new(&expected_name) {
        return Err(StoreError::Integrity);
    }
    let (_, fragment_blake3) = hash_validated_file(&file, policy.maximum_file_bytes)?;
    let minimum_event_time_ns = parse_footer(&footer, "cmti.minimum_event_time_ns")?;
    let maximum_event_time_ns = parse_footer(&footer, "cmti.maximum_event_time_ns")?;
    let row_count = parse_footer(&footer, "cmti.row_count")?;
    verify_footer_identity(
        &footer,
        key,
        &metadata,
        schema_digest,
        minimum_event_time_ns,
        maximum_event_time_ns,
        row_count,
    )?;
    Ok(ReadFragment {
        file_name: name.to_owned(),
        schema,
        schema_digest,
        metadata,
        batch_id,
        fragment_blake3,
        minimum_event_time_ns,
        maximum_event_time_ns,
        row_count,
        size_bytes,
    })
}

struct ActiveAdmission<'a> {
    metadata: &'a DatasetMetadata,
    schema_digest: [u8; 32],
    candidate_rows: u64,
    is_retry: bool,
}

fn validate_active_admission(
    active: &Directory,
    inventory: &[OsString],
    key: &PartitionKey,
    admission: ActiveAdmission<'_>,
    policy: &DatasetPolicy,
) -> Result<u64, StoreError> {
    let mut existing_rows = 0_u64;
    let mut existing_bytes = 0_u64;
    let mut combined_coverage = Vec::with_capacity(
        inventory
            .len()
            .checked_add(usize::from(!admission.is_retry))
            .ok_or(StoreError::IntegerRange)?,
    );
    for name in inventory {
        let fragment = read_fragment(active, name, key, policy)?;
        if fragment.schema_digest != admission.schema_digest
            || !fragment.metadata.same_dataset_identity(admission.metadata)
        {
            return Err(StoreError::SchemaMismatch);
        }
        existing_rows = existing_rows
            .checked_add(fragment.row_count)
            .ok_or(StoreError::IntegerRange)?;
        existing_bytes = existing_bytes
            .checked_add(fragment.size_bytes)
            .ok_or(StoreError::IntegerRange)?;
        combined_coverage.extend(fragment.metadata.source_coverage().iter().cloned());
    }
    if existing_rows > policy.maximum_partition_rows
        || existing_bytes > policy.maximum_partition_bytes
    {
        return Err(StoreError::CapacityExceeded);
    }
    if !admission.is_retry {
        let admitted_rows = existing_rows
            .checked_add(admission.candidate_rows)
            .ok_or(StoreError::IntegerRange)?;
        if admitted_rows > policy.maximum_partition_rows {
            return Err(StoreError::CapacityExceeded);
        }
        combined_coverage.extend(admission.metadata.source_coverage().iter().cloned());
    }
    admission
        .metadata
        .with_source_coverage(combined_coverage)
        .map_err(|_| StoreError::InvalidBatchIdentity)?;
    Ok(existing_bytes)
}

fn batch_id_from_footer(footer: &BTreeMap<String, String>) -> Result<BatchId, StoreError> {
    let parsed = serde_json::from_str::<BatchId>(footer_value_map(footer, "cmti.batch_id")?)
        .map_err(|_| StoreError::Integrity)?;
    let batch_id = BatchId::try_new(
        parsed.coverage().clone(),
        parsed.wal_segment_blake3(),
        parsed.wal_start_offset(),
        parsed.wal_end_offset(),
    )
    .map_err(|_| StoreError::Integrity)?;
    if footer_value_map(footer, "cmti.batch_id_blake3")? != hex::encode(batch_id.digest()?) {
        return Err(StoreError::Integrity);
    }
    Ok(batch_id)
}

struct ParquetExpectation<'a> {
    key: &'a PartitionKey,
    metadata: &'a DatasetMetadata,
    schema_digest: [u8; 32],
    minimum_event_time_ns: i64,
    maximum_event_time_ns: i64,
    row_count: u64,
}

fn verify_parquet_file(
    file: &File,
    expected: ParquetExpectation<'_>,
    policy: &DatasetPolicy,
) -> Result<(), StoreError> {
    validate_parquet_footer(file, policy.maximum_footer_bytes)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file.try_clone()?)?
        .with_batch_size(policy.reader_batch_rows);
    if builder.metadata().memory_size() > policy.maximum_schema_bytes {
        return Err(StoreError::CapacityExceeded);
    }
    let digest = schema_digest(builder.schema().as_ref(), policy)?;
    let footer = unique_footer(builder.metadata())?;
    verify_footer_identity(
        &footer,
        expected.key,
        expected.metadata,
        digest,
        expected.minimum_event_time_ns,
        expected.maximum_event_time_ns,
        expected.row_count,
    )?;
    if digest != expected.schema_digest {
        return Err(StoreError::Integrity);
    }
    let mut rows = 0_u64;
    let mut actual_minimum = None;
    let mut actual_maximum = None;
    for batch in builder.build()? {
        let batch = batch?;
        rows = rows
            .checked_add(u64::try_from(batch.num_rows()).map_err(|_| StoreError::IntegerRange)?)
            .ok_or(StoreError::IntegerRange)?;
        let (batch_minimum, batch_maximum) = event_time_bounds(&batch)?;
        actual_minimum =
            Some(actual_minimum.map_or(batch_minimum, |value: i64| value.min(batch_minimum)));
        actual_maximum =
            Some(actual_maximum.map_or(batch_maximum, |value: i64| value.max(batch_maximum)));
    }
    if rows != expected.row_count
        || actual_minimum != Some(expected.minimum_event_time_ns)
        || actual_maximum != Some(expected.maximum_event_time_ns)
    {
        return Err(StoreError::Integrity);
    }
    Ok(())
}

fn unique_footer(
    metadata: &parquet::file::metadata::ParquetMetaData,
) -> Result<BTreeMap<String, String>, StoreError> {
    let entries = metadata
        .file_metadata()
        .key_value_metadata()
        .ok_or(StoreError::Integrity)?;
    let mut values = BTreeMap::new();
    for entry in entries {
        if !entry.key.starts_with("cmti.") {
            continue;
        }
        let value = entry.value.clone().ok_or(StoreError::Integrity)?;
        if values.insert(entry.key.clone(), value).is_some() {
            return Err(StoreError::Integrity);
        }
    }
    Ok(values)
}

fn metadata_from_footer(footer: &BTreeMap<String, String>) -> Result<DatasetMetadata, StoreError> {
    let source_coverage = serde_json::from_str::<Vec<SourceCoverage>>(
        footer
            .get("cmti.source_coverage")
            .ok_or(StoreError::Integrity)?,
    )
    .map_err(|_| StoreError::Integrity)?;
    let correction_lineage = footer
        .get("cmti.correction_lineage")
        .map(|value| serde_json::from_str(value).map_err(|_| StoreError::Integrity))
        .transpose()?;
    DatasetMetadata::try_new(crate::DatasetMetadataInput {
        feature_version: footer_value_map(footer, "cmti.feature_version")?.to_owned(),
        parser_version: footer_value_map(footer, "cmti.parser_version")?.to_owned(),
        normalizer_version: footer_value_map(footer, "cmti.normalizer_version")?.to_owned(),
        code_identity: footer_value_map(footer, "cmti.code_identity")?.to_owned(),
        source_coverage,
        correction_lineage,
    })
    .map_err(|_| StoreError::Integrity)
}

fn verify_footer_identity(
    footer: &BTreeMap<String, String>,
    key: &PartitionKey,
    metadata: &DatasetMetadata,
    schema_digest: [u8; 32],
    minimum_event_time_ns: i64,
    maximum_event_time_ns: i64,
    row_count: u64,
) -> Result<(), StoreError> {
    let expected = footer_metadata(
        key,
        metadata,
        schema_digest,
        minimum_event_time_ns,
        maximum_event_time_ns,
        row_count,
        None,
    )?
    .into_iter()
    .map(|entry| {
        entry
            .value
            .map(|value| (entry.key, value))
            .ok_or(StoreError::Integrity)
    })
    .collect::<Result<BTreeMap<_, _>, _>>()?;
    for (key, value) in expected {
        if footer.get(&key) != Some(&value) {
            return Err(StoreError::Integrity);
        }
    }
    Ok(())
}

fn parse_footer<T: std::str::FromStr>(
    footer: &BTreeMap<String, String>,
    name: &'static str,
) -> Result<T, StoreError> {
    footer_value_map(footer, name)?
        .parse()
        .map_err(|_| StoreError::Integrity)
}

fn footer_value_map<'a>(
    footer: &'a BTreeMap<String, String>,
    name: &'static str,
) -> Result<&'a str, StoreError> {
    footer
        .get(name)
        .map(String::as_str)
        .ok_or(StoreError::Integrity)
}

fn footer_value(
    file: &File,
    name: &'static str,
    maximum_footer_bytes: usize,
) -> Result<String, StoreError> {
    validate_parquet_footer(file, maximum_footer_bytes)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file.try_clone()?)?;
    Ok(footer_value_map(&unique_footer(builder.metadata())?, name)?.to_owned())
}

fn validate_parquet_footer(file: &File, maximum_footer_bytes: usize) -> Result<(), StoreError> {
    let length = file.metadata()?.len();
    if length < 8 {
        return Err(StoreError::Integrity);
    }
    let mut reader = file.try_clone()?;
    reader.seek(SeekFrom::End(-8))?;
    let mut footer = [0_u8; 8];
    reader.read_exact(&mut footer)?;
    if &footer[4..] != b"PAR1" {
        return Err(StoreError::Integrity);
    }
    let metadata_length =
        u32::from_le_bytes(footer[..4].try_into().map_err(|_| StoreError::Integrity)?) as usize;
    let total_footer = metadata_length
        .checked_add(8)
        .ok_or(StoreError::IntegerRange)?;
    if total_footer > maximum_footer_bytes
        || u64::try_from(total_footer).map_err(|_| StoreError::IntegerRange)? > length
    {
        return Err(StoreError::CapacityExceeded);
    }
    Ok(())
}

fn hash_validated_file(file: &File, maximum_bytes: u64) -> Result<(u64, [u8; 32]), StoreError> {
    validate_private_file(file)?;
    let size = file.metadata()?.len();
    if size == 0 || size > maximum_bytes {
        return Err(StoreError::CapacityExceeded);
    }
    let mut reader = file.try_clone()?;
    reader.seek(SeekFrom::Start(0))?;
    let mut take = reader.take(maximum_bytes.saturating_add(1));
    let mut hasher = blake3::Hasher::new();
    let copied = io::copy(&mut take, &mut hasher)?;
    if copied != size || copied > maximum_bytes {
        return Err(StoreError::Integrity);
    }
    Ok((size, *hasher.finalize().as_bytes()))
}

fn active_fragment_names(directory: &File, maximum: usize) -> Result<Vec<OsString>, StoreError> {
    let entries = directory_names(
        directory,
        maximum
            .checked_add(MAXIMUM_STALE_TEMPORARY_FILES)
            .ok_or(StoreError::IntegerRange)?,
    )?;
    let mut names = entries
        .iter()
        .filter(|name| {
            let value = name.to_string_lossy();
            value.starts_with("fragment-") && value.ends_with(".parquet")
        })
        .cloned()
        .collect::<Vec<_>>();
    names.sort();
    if names.len() > maximum {
        return Err(StoreError::FragmentCapacityExceeded);
    }
    if entries.into_iter().any(|name| {
        let value = name.to_string_lossy();
        !value.starts_with('.') && !names.contains(&name)
    }) {
        return Err(StoreError::UnsafePath);
    }
    Ok(names)
}

fn directory_names(directory: &File, maximum: usize) -> Result<Vec<OsString>, StoreError> {
    let mut entries = Dir::read_from(directory).map_err(io::Error::from)?;
    let mut names = Vec::new();
    for entry in &mut entries {
        let entry = entry.map_err(io::Error::from)?;
        let bytes = entry.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        if names.len() >= maximum {
            return Err(StoreError::CapacityExceeded);
        }
        names.push(OsStr::from_bytes(bytes).to_owned());
    }
    Ok(names)
}

fn clean_temporary_files(directory: &Directory) -> Result<(), StoreError> {
    let names = directory_names(&directory.file, MAXIMUM_MANAGED_DIRECTORY_ENTRIES)?;
    let mut changed = false;
    for name in names {
        let value = name.to_string_lossy();
        if !value.starts_with('.') {
            continue;
        }
        if !value.ends_with(".tmp") || !value.contains(".create-") {
            return Err(StoreError::UnsafePath);
        }
        let file = directory.open_file(&name, OFlags::RDONLY)?;
        drop(file);
        rustix::fs::unlinkat(&directory.file, &name, AtFlags::empty()).map_err(io::Error::from)?;
        changed = true;
    }
    if changed {
        directory.file.sync_all()?;
    }
    Ok(())
}

fn entry_exists(directory: &File, name: &str) -> Result<bool, StoreError> {
    match rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => Ok(true),
        Err(error) if error == rustix::io::Errno::NOENT => Ok(false),
        Err(error) => Err(io::Error::from(error).into()),
    }
}

fn remove_active_fragments(directory: &Directory, names: &[OsString]) -> Result<(), StoreError> {
    for name in names {
        rustix::fs::unlinkat(&directory.file, name, AtFlags::empty()).map_err(io::Error::from)?;
    }
    directory.file.sync_all()?;
    Ok(())
}
