use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    BatchId, CorrectionLineage, DatasetMetadata, DatasetMetadataInput, PartitionKey,
    SourceCoverage, StoreError,
    layout::{PartitionKeyWire, safe_relative_path},
    writer::MAXIMUM_MANIFEST_BYTES,
};

const MANIFEST_FORMAT_VERSION: u32 = 1;
const MANIFEST_HASH_DOMAIN: &[u8] = b"cmti:parquet-partition-manifest:v1\0";
pub(crate) const MAXIMUM_SEALED_FILES: usize = 64;
pub(crate) const MAXIMUM_SOURCE_FRAGMENTS: usize = 4_096;
const SOURCE_FRAGMENT_HASH_DOMAIN: &[u8] = b"cmti:parquet-source-fragments:v1\0";

/// Immutable lineage for one active fragment consumed by compaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceFragmentLineage {
    batch_id: BatchId,
    batch_id_digest: [u8; 32],
    fragment_blake3: [u8; 32],
    row_count: u64,
}

impl SourceFragmentLineage {
    pub fn batch_id(&self) -> &BatchId {
        &self.batch_id
    }

    pub const fn batch_id_digest(&self) -> &[u8; 32] {
        &self.batch_id_digest
    }

    pub const fn fragment_blake3(&self) -> &[u8; 32] {
        &self.fragment_blake3
    }

    pub const fn row_count(&self) -> u64 {
        self.row_count
    }

    pub(crate) fn new(
        batch_id: BatchId,
        fragment_blake3: [u8; 32],
        row_count: u64,
    ) -> Result<Self, StoreError> {
        if fragment_blake3 == [0; 32] || row_count == 0 {
            return Err(StoreError::Integrity);
        }
        let batch_id = revalidate_batch_id(batch_id)?;
        let batch_id_digest = batch_id.digest()?;
        Ok(Self {
            batch_id,
            batch_id_digest,
            fragment_blake3,
            row_count,
        })
    }
}

/// One immutable Parquet file referenced by a sealed manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestFile {
    relative_path: PathBuf,
    size_bytes: u64,
    blake3: [u8; 32],
    row_count: u64,
    minimum_event_time_ns: i64,
    maximum_event_time_ns: i64,
    source_coverage: Vec<SourceCoverage>,
}

impl ManifestFile {
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

    pub const fn minimum_event_time_ns(&self) -> i64 {
        self.minimum_event_time_ns
    }

    pub const fn maximum_event_time_ns(&self) -> i64 {
        self.maximum_event_time_ns
    }

    pub fn source_coverage(&self) -> &[SourceCoverage] {
        &self.source_coverage
    }

    pub(crate) fn new(
        relative_path: PathBuf,
        size_bytes: u64,
        blake3: [u8; 32],
        row_count: u64,
        minimum_event_time_ns: i64,
        maximum_event_time_ns: i64,
        mut source_coverage: Vec<SourceCoverage>,
    ) -> Result<Self, StoreError> {
        source_coverage.sort();
        if !safe_relative_path(&relative_path)
            || size_bytes == 0
            || row_count == 0
            || minimum_event_time_ns > maximum_event_time_ns
            || source_coverage.is_empty()
            || source_coverage.len() > crate::layout::MAXIMUM_SOURCE_COVERAGE
            || coverage_overlaps(&source_coverage)
        {
            return Err(StoreError::Integrity);
        }
        Ok(Self {
            relative_path,
            size_bytes,
            blake3,
            row_count,
            minimum_event_time_ns,
            maximum_event_time_ns,
            source_coverage,
        })
    }
}

/// Canonical manifest that publishes one immutable partition version.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Manifest {
    partition: PartitionKey,
    metadata: DatasetMetadata,
    schema_digest: [u8; 32],
    minimum_event_time_ns: i64,
    maximum_event_time_ns: i64,
    row_count: u64,
    files: Vec<ManifestFile>,
    source_fragments: Vec<SourceFragmentLineage>,
    source_fragment_set_digest: [u8; 32],
    manifest_hash: [u8; 32],
}

pub(crate) struct ManifestInput {
    pub partition: PartitionKey,
    pub metadata: DatasetMetadata,
    pub schema_digest: [u8; 32],
    pub minimum_event_time_ns: i64,
    pub maximum_event_time_ns: i64,
    pub row_count: u64,
    pub files: Vec<ManifestFile>,
    pub source_fragments: Vec<SourceFragmentLineage>,
}

impl Manifest {
    pub fn partition(&self) -> &PartitionKey {
        &self.partition
    }

    pub fn metadata(&self) -> &DatasetMetadata {
        &self.metadata
    }

    pub const fn schema_digest(&self) -> &[u8; 32] {
        &self.schema_digest
    }

    pub const fn minimum_event_time_ns(&self) -> i64 {
        self.minimum_event_time_ns
    }

    pub const fn maximum_event_time_ns(&self) -> i64 {
        self.maximum_event_time_ns
    }

    pub const fn row_count(&self) -> u64 {
        self.row_count
    }

    pub fn files(&self) -> &[ManifestFile] {
        &self.files
    }

    pub fn source_fragments(&self) -> &[SourceFragmentLineage] {
        &self.source_fragments
    }

    pub const fn source_fragment_set_digest(&self) -> &[u8; 32] {
        &self.source_fragment_set_digest
    }

    pub const fn manifest_hash(&self) -> &[u8; 32] {
        &self.manifest_hash
    }

    pub(crate) fn create(input: ManifestInput) -> Result<Self, StoreError> {
        let ManifestInput {
            partition,
            metadata,
            schema_digest,
            minimum_event_time_ns,
            maximum_event_time_ns,
            row_count,
            mut files,
            mut source_fragments,
        } = input;
        files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        source_fragments.sort_by_key(|fragment| fragment.batch_id_digest);
        let summed_rows = files.iter().try_fold(0_u64, |total, file| {
            total
                .checked_add(file.row_count)
                .ok_or(StoreError::IntegerRange)
        })?;
        let summed_source_rows = source_fragments.iter().try_fold(0_u64, |total, fragment| {
            total
                .checked_add(fragment.row_count)
                .ok_or(StoreError::IntegerRange)
        })?;
        let files_minimum_event_time_ns = files
            .iter()
            .map(|file| file.minimum_event_time_ns)
            .min()
            .ok_or(StoreError::Integrity)?;
        let files_maximum_event_time_ns = files
            .iter()
            .map(|file| file.maximum_event_time_ns)
            .max()
            .ok_or(StoreError::Integrity)?;
        if minimum_event_time_ns > maximum_event_time_ns
            || row_count == 0
            || files.is_empty()
            || files.len() > MAXIMUM_SEALED_FILES
            || summed_rows != row_count
            || files_minimum_event_time_ns != minimum_event_time_ns
            || files_maximum_event_time_ns != maximum_event_time_ns
            || source_fragments.is_empty()
            || source_fragments.len() > MAXIMUM_SOURCE_FRAGMENTS
            || summed_source_rows != row_count
            || source_fragments
                .windows(2)
                .any(|pair| pair[0].batch_id_digest == pair[1].batch_id_digest)
            || files
                .windows(2)
                .any(|pair| pair[0].relative_path == pair[1].relative_path)
            || coverage_inventory_invalid(&files, metadata.source_coverage())
            || source_coverage_invalid(&source_fragments, metadata.source_coverage())
        {
            return Err(StoreError::Integrity);
        }
        let source_fragment_set_digest = hash_source_fragments(&source_fragments)?;
        let mut manifest = Self {
            partition,
            metadata,
            schema_digest,
            minimum_event_time_ns,
            maximum_event_time_ns,
            row_count,
            files,
            source_fragments,
            source_fragment_set_digest,
            manifest_hash: [0; 32],
        };
        manifest.manifest_hash = hash_body(&ManifestBodyWire::from(&manifest))?;
        Ok(manifest)
    }

    pub(crate) fn canonical_bytes(&self) -> Result<Vec<u8>, StoreError> {
        let bytes = serde_json::to_vec(&self.to_envelope())?;
        if bytes.is_empty() || bytes.len() > MAXIMUM_MANIFEST_BYTES {
            return Err(StoreError::CapacityExceeded);
        }
        Ok(bytes)
    }

    pub(crate) fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, StoreError> {
        if bytes.is_empty() || bytes.len() > MAXIMUM_MANIFEST_BYTES {
            return Err(StoreError::CapacityExceeded);
        }
        let envelope = serde_json::from_slice::<ManifestEnvelopeWire>(bytes)
            .map_err(|_| StoreError::Integrity)?;
        if serde_json::to_vec(&envelope).map_err(|_| StoreError::Integrity)? != bytes {
            return Err(StoreError::Integrity);
        }
        let expected_hash = decode_hash(&envelope.manifest_hash)?;
        if hash_body(&envelope.manifest)? != expected_hash
            || envelope.manifest.format_version != MANIFEST_FORMAT_VERSION
            || envelope.manifest.minimum_event_time_ns > envelope.manifest.maximum_event_time_ns
            || envelope.manifest.row_count == 0
            || envelope.manifest.files.is_empty()
            || envelope.manifest.files.len() > MAXIMUM_SEALED_FILES
            || envelope.manifest.source_fragments.is_empty()
            || envelope.manifest.source_fragments.len() > MAXIMUM_SOURCE_FRAGMENTS
        {
            return Err(StoreError::Integrity);
        }
        let partition = PartitionKey::from_wire(envelope.manifest.partition)?;
        let metadata = revalidate_metadata(envelope.manifest.metadata)?;
        if !metadata.validates_version(partition.dataset_version()) {
            return Err(StoreError::Integrity);
        }
        let schema_digest = decode_hash(&envelope.manifest.schema_digest)?;
        let files = envelope
            .manifest
            .files
            .into_iter()
            .map(ManifestFile::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let source_fragments = envelope
            .manifest
            .source_fragments
            .into_iter()
            .map(SourceFragmentLineage::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let expected_source_digest = decode_hash(&envelope.manifest.source_fragment_set_digest)?;
        let manifest = Self::create(ManifestInput {
            partition,
            metadata,
            schema_digest,
            minimum_event_time_ns: envelope.manifest.minimum_event_time_ns,
            maximum_event_time_ns: envelope.manifest.maximum_event_time_ns,
            row_count: envelope.manifest.row_count,
            files,
            source_fragments,
        })?;
        if manifest.manifest_hash != expected_hash
            || manifest.source_fragment_set_digest != expected_source_digest
        {
            return Err(StoreError::Integrity);
        }
        Ok(manifest)
    }

    fn to_envelope(&self) -> ManifestEnvelopeWire {
        ManifestEnvelopeWire {
            manifest: ManifestBodyWire::from(self),
            manifest_hash: hex::encode(self.manifest_hash),
        }
    }
}

/// Reader-facing summary produced only after installed publication verification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedPartition {
    row_count: u64,
    minimum_event_time_ns: i64,
    maximum_event_time_ns: i64,
    manifest_hash: [u8; 32],
    file_count: usize,
}

impl VerifiedPartition {
    pub const fn row_count(&self) -> u64 {
        self.row_count
    }

    pub const fn minimum_event_time_ns(&self) -> i64 {
        self.minimum_event_time_ns
    }

    pub const fn maximum_event_time_ns(&self) -> i64 {
        self.maximum_event_time_ns
    }

    pub const fn manifest_hash(&self) -> &[u8; 32] {
        &self.manifest_hash
    }

    pub const fn file_count(&self) -> usize {
        self.file_count
    }

    pub(crate) fn from_manifest(manifest: &Manifest) -> Self {
        Self {
            row_count: manifest.row_count,
            minimum_event_time_ns: manifest.minimum_event_time_ns,
            maximum_event_time_ns: manifest.maximum_event_time_ns,
            manifest_hash: manifest.manifest_hash,
            file_count: manifest.files.len(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestEnvelopeWire {
    manifest: ManifestBodyWire,
    manifest_hash: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestBodyWire {
    format_version: u32,
    partition: PartitionKeyWire,
    metadata: DatasetMetadata,
    schema_digest: String,
    minimum_event_time_ns: i64,
    maximum_event_time_ns: i64,
    row_count: u64,
    files: Vec<ManifestFileWire>,
    source_fragments: Vec<SourceFragmentLineageWire>,
    source_fragment_set_digest: String,
}

impl From<&Manifest> for ManifestBodyWire {
    fn from(manifest: &Manifest) -> Self {
        Self {
            format_version: MANIFEST_FORMAT_VERSION,
            partition: manifest.partition.to_wire(),
            metadata: manifest.metadata.clone(),
            schema_digest: hex::encode(manifest.schema_digest),
            minimum_event_time_ns: manifest.minimum_event_time_ns,
            maximum_event_time_ns: manifest.maximum_event_time_ns,
            row_count: manifest.row_count,
            files: manifest.files.iter().map(ManifestFileWire::from).collect(),
            source_fragments: manifest
                .source_fragments
                .iter()
                .map(SourceFragmentLineageWire::from)
                .collect(),
            source_fragment_set_digest: hex::encode(manifest.source_fragment_set_digest),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceFragmentLineageWire {
    batch_id: BatchId,
    fragment_blake3: String,
    row_count: u64,
}

impl From<&SourceFragmentLineage> for SourceFragmentLineageWire {
    fn from(fragment: &SourceFragmentLineage) -> Self {
        Self {
            batch_id: fragment.batch_id.clone(),
            fragment_blake3: hex::encode(fragment.fragment_blake3),
            row_count: fragment.row_count,
        }
    }
}

impl TryFrom<SourceFragmentLineageWire> for SourceFragmentLineage {
    type Error = StoreError;

    fn try_from(wire: SourceFragmentLineageWire) -> Result<Self, Self::Error> {
        Self::new(
            wire.batch_id,
            decode_hash(&wire.fragment_blake3)?,
            wire.row_count,
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFileWire {
    relative_path: String,
    size_bytes: u64,
    blake3: String,
    row_count: u64,
    minimum_event_time_ns: i64,
    maximum_event_time_ns: i64,
    source_coverage: Vec<SourceCoverage>,
}

impl From<&ManifestFile> for ManifestFileWire {
    fn from(file: &ManifestFile) -> Self {
        Self {
            relative_path: file.relative_path.to_string_lossy().into_owned(),
            size_bytes: file.size_bytes,
            blake3: hex::encode(file.blake3),
            row_count: file.row_count,
            minimum_event_time_ns: file.minimum_event_time_ns,
            maximum_event_time_ns: file.maximum_event_time_ns,
            source_coverage: file.source_coverage.clone(),
        }
    }
}

impl TryFrom<ManifestFileWire> for ManifestFile {
    type Error = StoreError;

    fn try_from(wire: ManifestFileWire) -> Result<Self, Self::Error> {
        Self::new(
            PathBuf::from(wire.relative_path),
            wire.size_bytes,
            decode_hash(&wire.blake3)?,
            wire.row_count,
            wire.minimum_event_time_ns,
            wire.maximum_event_time_ns,
            wire.source_coverage,
        )
    }
}

fn revalidate_metadata(metadata: DatasetMetadata) -> Result<DatasetMetadata, StoreError> {
    let correction_lineage = metadata.correction_lineage().map(|lineage| {
        CorrectionLineage::try_new(
            std::num::NonZeroU32::new(lineage.base_dataset_version())
                .ok_or(StoreError::Integrity)?,
            lineage.base_manifest_blake3(),
            lineage.known_at_ns(),
            lineage.reason(),
            lineage.provenance(),
        )
    });
    DatasetMetadata::try_new(DatasetMetadataInput {
        feature_version: metadata.feature_version().to_owned(),
        parser_version: metadata.parser_version().to_owned(),
        normalizer_version: metadata.normalizer_version().to_owned(),
        code_identity: metadata.code_identity().to_owned(),
        source_coverage: metadata.source_coverage().to_vec(),
        correction_lineage: correction_lineage.transpose()?,
    })
    .map_err(|_| StoreError::Integrity)
}

fn coverage_overlaps(coverage: &[SourceCoverage]) -> bool {
    coverage.windows(2).any(|pair| {
        pair[0].same_epoch(&pair[1]) && pair[1].first_sequence() <= pair[0].last_sequence()
    })
}

fn coverage_inventory_invalid(files: &[ManifestFile], expected: &[SourceCoverage]) -> bool {
    let mut actual = files
        .iter()
        .flat_map(|file| file.source_coverage.iter().cloned())
        .collect::<Vec<_>>();
    actual.sort();
    coverage_overlaps(&actual) || actual != expected
}

fn source_coverage_invalid(
    fragments: &[SourceFragmentLineage],
    expected: &[SourceCoverage],
) -> bool {
    let mut actual = fragments
        .iter()
        .map(|fragment| fragment.batch_id.coverage().clone())
        .collect::<Vec<_>>();
    actual.sort();
    coverage_overlaps(&actual) || actual != expected
}

fn revalidate_batch_id(batch_id: BatchId) -> Result<BatchId, StoreError> {
    BatchId::try_new(
        batch_id.coverage().clone(),
        batch_id.wal_segment_blake3(),
        batch_id.wal_start_offset(),
        batch_id.wal_end_offset(),
    )
    .map_err(|_| StoreError::Integrity)
}

fn hash_source_fragments(fragments: &[SourceFragmentLineage]) -> Result<[u8; 32], StoreError> {
    let wire = fragments
        .iter()
        .map(SourceFragmentLineageWire::from)
        .collect::<Vec<_>>();
    let bytes = serde_json::to_vec(&wire).map_err(|_| StoreError::Integrity)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(SOURCE_FRAGMENT_HASH_DOMAIN);
    hasher.update(&bytes);
    Ok(*hasher.finalize().as_bytes())
}

fn hash_body(body: &ManifestBodyWire) -> Result<[u8; 32], StoreError> {
    let bytes = serde_json::to_vec(body).map_err(|_| StoreError::Integrity)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(MANIFEST_HASH_DOMAIN);
    hasher.update(&bytes);
    Ok(*hasher.finalize().as_bytes())
}

fn decode_hash(value: &str) -> Result<[u8; 32], StoreError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(StoreError::Integrity);
    }
    let bytes = hex::decode(value).map_err(|_| StoreError::Integrity)?;
    bytes.try_into().map_err(|_| StoreError::Integrity)
}
