use std::{
    num::NonZeroU32,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use time::{Date, Month};

use crate::StoreError;

pub(crate) const MAXIMUM_IDENTIFIER_BYTES: usize = 96;
pub(crate) const MAXIMUM_LINEAGE_TEXT_BYTES: usize = 256;
pub(crate) const MAXIMUM_SOURCE_COVERAGE: usize = 64;

/// Fully validated identity of one hourly normalized-event partition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartitionKey {
    event_type: String,
    dataset_version: NonZeroU32,
    schema_version: NonZeroU32,
    venue: String,
    instrument_generation: NonZeroU32,
    utc_date: Date,
    hour: u8,
}

/// Construction input for [`PartitionKey`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartitionKeyInput {
    pub event_type: String,
    pub dataset_version: NonZeroU32,
    pub schema_version: NonZeroU32,
    pub venue: String,
    pub instrument_generation: NonZeroU32,
    pub utc_date: Date,
    pub hour: u8,
}

impl PartitionKey {
    pub fn try_new(input: PartitionKeyInput) -> Result<Self, StoreError> {
        if !valid_identifier(&input.event_type)
            || !valid_identifier(&input.venue)
            || input.hour > 23
        {
            return Err(StoreError::InvalidPartitionKey);
        }
        Ok(Self {
            event_type: input.event_type,
            dataset_version: input.dataset_version,
            schema_version: input.schema_version,
            venue: input.venue,
            instrument_generation: input.instrument_generation,
            utc_date: input.utc_date,
            hour: input.hour,
        })
    }

    pub fn event_type(&self) -> &str {
        &self.event_type
    }

    pub const fn dataset_version(&self) -> NonZeroU32 {
        self.dataset_version
    }

    pub const fn schema_version(&self) -> NonZeroU32 {
        self.schema_version
    }

    pub fn venue(&self) -> &str {
        &self.venue
    }

    pub const fn instrument_generation(&self) -> NonZeroU32 {
        self.instrument_generation
    }

    pub const fn utc_date(&self) -> Date {
        self.utc_date
    }

    pub const fn hour(&self) -> u8 {
        self.hour
    }

    /// Canonical traversal-free path relative to the store root.
    pub fn relative_path(&self) -> PathBuf {
        self.components().iter().collect()
    }

    pub(crate) fn components(&self) -> [String; 9] {
        [
            "normalized".to_owned(),
            self.event_type.clone(),
            self.venue.clone(),
            self.utc_date.to_string(),
            format!("{:02}", self.hour),
            format!("version={}", self.dataset_version),
            format!("schema={}", self.schema_version),
            format!("generation={}", self.instrument_generation),
            "partition".to_owned(),
        ]
    }

    pub(crate) fn hour_bounds_ns(&self) -> Result<(i64, i64), StoreError> {
        let start = self
            .utc_date
            .with_hms(self.hour, 0, 0)
            .map_err(|_| StoreError::InvalidPartitionKey)?
            .assume_utc()
            .unix_timestamp_nanos();
        let end = start
            .checked_add(3_600_000_000_000)
            .ok_or(StoreError::IntegerRange)?;
        Ok((
            i64::try_from(start).map_err(|_| StoreError::IntegerRange)?,
            i64::try_from(end).map_err(|_| StoreError::IntegerRange)?,
        ))
    }

    pub(crate) fn to_wire(&self) -> PartitionKeyWire {
        PartitionKeyWire {
            event_type: self.event_type.clone(),
            dataset_version: self.dataset_version.get(),
            schema_version: self.schema_version.get(),
            venue: self.venue.clone(),
            instrument_generation: self.instrument_generation.get(),
            utc_date: self.utc_date.to_string(),
            hour: self.hour,
        }
    }

    pub(crate) fn from_wire(wire: PartitionKeyWire) -> Result<Self, StoreError> {
        Self::try_new(PartitionKeyInput {
            event_type: wire.event_type,
            dataset_version: NonZeroU32::new(wire.dataset_version)
                .ok_or(StoreError::InvalidPartitionKey)?,
            schema_version: NonZeroU32::new(wire.schema_version)
                .ok_or(StoreError::InvalidPartitionKey)?,
            venue: wire.venue,
            instrument_generation: NonZeroU32::new(wire.instrument_generation)
                .ok_or(StoreError::InvalidPartitionKey)?,
            utc_date: parse_date(&wire.utc_date)?,
            hour: wire.hour,
        })
    }
}

/// Stable identity of one normalized input range.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceCoverage {
    source: String,
    stream: String,
    connection_epoch: u64,
    subscription_epoch: u64,
    first_sequence: u64,
    last_sequence: u64,
}

impl SourceCoverage {
    pub fn try_new(
        source: impl Into<String>,
        stream: impl Into<String>,
        connection_epoch: u64,
        subscription_epoch: u64,
        first_sequence: u64,
        last_sequence: u64,
    ) -> Result<Self, StoreError> {
        let source = source.into();
        let stream = stream.into();
        if !valid_identifier(&source)
            || !valid_identifier(&stream)
            || connection_epoch == 0
            || subscription_epoch == 0
            || first_sequence > last_sequence
        {
            return Err(StoreError::InvalidDatasetMetadata);
        }
        Ok(Self {
            source,
            stream,
            connection_epoch,
            subscription_epoch,
            first_sequence,
            last_sequence,
        })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn stream(&self) -> &str {
        &self.stream
    }

    pub const fn connection_epoch(&self) -> u64 {
        self.connection_epoch
    }

    pub const fn subscription_epoch(&self) -> u64 {
        self.subscription_epoch
    }

    pub const fn first_sequence(&self) -> u64 {
        self.first_sequence
    }

    pub const fn last_sequence(&self) -> u64 {
        self.last_sequence
    }

    pub(crate) fn same_epoch(&self, other: &Self) -> bool {
        self.source == other.source
            && self.stream == other.stream
            && self.connection_epoch == other.connection_epoch
            && self.subscription_epoch == other.subscription_epoch
    }
}

/// Durable idempotency key tying an Arrow batch to the raw WAL bytes that produced it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchId {
    coverage: SourceCoverage,
    wal_segment_blake3: String,
    wal_start_offset: u64,
    wal_end_offset: u64,
}

impl BatchId {
    pub fn try_new(
        coverage: SourceCoverage,
        wal_segment_blake3: impl Into<String>,
        wal_start_offset: u64,
        wal_end_offset: u64,
    ) -> Result<Self, StoreError> {
        let wal_segment_blake3 = wal_segment_blake3.into();
        if !valid_blake3(&wal_segment_blake3) || wal_start_offset >= wal_end_offset {
            return Err(StoreError::InvalidBatchIdentity);
        }
        Ok(Self {
            coverage,
            wal_segment_blake3,
            wal_start_offset,
            wal_end_offset,
        })
    }

    pub fn coverage(&self) -> &SourceCoverage {
        &self.coverage
    }

    pub fn wal_segment_blake3(&self) -> &str {
        &self.wal_segment_blake3
    }

    pub const fn wal_start_offset(&self) -> u64 {
        self.wal_start_offset
    }

    pub const fn wal_end_offset(&self) -> u64 {
        self.wal_end_offset
    }

    pub(crate) fn digest(&self) -> Result<[u8; 32], StoreError> {
        let bytes = serde_json::to_vec(self).map_err(|_| StoreError::InvalidBatchIdentity)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"cmti:normalized-batch-id:v1\0");
        hasher.update(&bytes);
        Ok(*hasher.finalize().as_bytes())
    }
}

/// Provenance required when a sealed dataset is corrected under a new version.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorrectionLineage {
    base_dataset_version: u32,
    base_manifest_blake3: String,
    known_at_ns: i64,
    reason: String,
    provenance: String,
}

impl CorrectionLineage {
    pub fn try_new(
        base_dataset_version: NonZeroU32,
        base_manifest_blake3: impl Into<String>,
        known_at_ns: i64,
        reason: impl Into<String>,
        provenance: impl Into<String>,
    ) -> Result<Self, StoreError> {
        let base_manifest_blake3 = base_manifest_blake3.into();
        let reason = reason.into();
        let provenance = provenance.into();
        if !valid_blake3(&base_manifest_blake3)
            || known_at_ns < 0
            || !valid_lineage_text(&reason)
            || !valid_lineage_text(&provenance)
        {
            return Err(StoreError::InvalidDatasetMetadata);
        }
        Ok(Self {
            base_dataset_version: base_dataset_version.get(),
            base_manifest_blake3,
            known_at_ns,
            reason,
            provenance,
        })
    }

    pub const fn base_dataset_version(&self) -> u32 {
        self.base_dataset_version
    }

    pub fn base_manifest_blake3(&self) -> &str {
        &self.base_manifest_blake3
    }

    pub const fn known_at_ns(&self) -> i64 {
        self.known_at_ns
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }

    pub fn provenance(&self) -> &str {
        &self.provenance
    }
}

/// Validated lineage metadata required in every Parquet file and manifest.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatasetMetadata {
    feature_version: String,
    parser_version: String,
    normalizer_version: String,
    code_identity: String,
    source_coverage: Vec<SourceCoverage>,
    correction_lineage: Option<CorrectionLineage>,
}

/// Construction input for [`DatasetMetadata`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DatasetMetadataInput {
    pub feature_version: String,
    pub parser_version: String,
    pub normalizer_version: String,
    pub code_identity: String,
    pub source_coverage: Vec<SourceCoverage>,
    pub correction_lineage: Option<CorrectionLineage>,
}

impl DatasetMetadata {
    pub fn try_new(mut input: DatasetMetadataInput) -> Result<Self, StoreError> {
        if !valid_identifier(&input.feature_version)
            || !valid_identifier(&input.parser_version)
            || !valid_identifier(&input.normalizer_version)
            || !valid_code_identity(&input.code_identity)
            || input.source_coverage.is_empty()
            || input.source_coverage.len() > MAXIMUM_SOURCE_COVERAGE
        {
            return Err(StoreError::InvalidDatasetMetadata);
        }
        input.source_coverage.sort();
        if input.source_coverage.windows(2).any(|pair| {
            pair[0].same_epoch(&pair[1]) && pair[1].first_sequence <= pair[0].last_sequence
        }) {
            return Err(StoreError::InvalidDatasetMetadata);
        }
        Ok(Self {
            feature_version: input.feature_version,
            parser_version: input.parser_version,
            normalizer_version: input.normalizer_version,
            code_identity: input.code_identity,
            source_coverage: input.source_coverage,
            correction_lineage: input.correction_lineage,
        })
    }

    pub fn feature_version(&self) -> &str {
        &self.feature_version
    }

    pub fn parser_version(&self) -> &str {
        &self.parser_version
    }

    pub fn normalizer_version(&self) -> &str {
        &self.normalizer_version
    }

    pub fn code_identity(&self) -> &str {
        &self.code_identity
    }

    pub fn source_coverage(&self) -> &[SourceCoverage] {
        &self.source_coverage
    }

    pub fn correction_lineage(&self) -> Option<&CorrectionLineage> {
        self.correction_lineage.as_ref()
    }

    pub(crate) fn validates_version(&self, version: NonZeroU32) -> bool {
        match (version.get(), self.correction_lineage.as_ref()) {
            (1, None) => true,
            (1, Some(_)) | (_, None) => false,
            (current, Some(lineage)) => lineage
                .base_dataset_version
                .checked_add(1)
                .is_some_and(|expected| expected == current),
        }
    }

    pub(crate) fn same_dataset_identity(&self, other: &Self) -> bool {
        self.feature_version == other.feature_version
            && self.parser_version == other.parser_version
            && self.normalizer_version == other.normalizer_version
            && self.code_identity == other.code_identity
            && self.correction_lineage == other.correction_lineage
    }

    pub(crate) fn with_source_coverage(
        &self,
        source_coverage: Vec<SourceCoverage>,
    ) -> Result<Self, StoreError> {
        Self::try_new(DatasetMetadataInput {
            feature_version: self.feature_version.clone(),
            parser_version: self.parser_version.clone(),
            normalizer_version: self.normalizer_version.clone(),
            code_identity: self.code_identity.clone(),
            source_coverage,
            correction_lineage: self.correction_lineage.clone(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PartitionKeyWire {
    pub event_type: String,
    pub dataset_version: u32,
    pub schema_version: u32,
    pub venue: String,
    pub instrument_generation: u32,
    pub utc_date: String,
    pub hour: u8,
}

pub(crate) fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAXIMUM_IDENTIFIER_BYTES
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn valid_code_identity(value: &str) -> bool {
    value.len() == 49
        && value.starts_with("git_sha1:")
        && value[9..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_blake3(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_lineage_text(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAXIMUM_LINEAGE_TEXT_BYTES
        && !value.bytes().any(|byte| byte.is_ascii_control())
}

fn parse_date(value: &str) -> Result<Date, StoreError> {
    let mut parts = value.split('-');
    let year = parts
        .next()
        .and_then(|part| part.parse::<i32>().ok())
        .ok_or(StoreError::InvalidPartitionKey)?;
    let month = parts
        .next()
        .and_then(|part| part.parse::<u8>().ok())
        .and_then(|month| Month::try_from(month).ok())
        .ok_or(StoreError::InvalidPartitionKey)?;
    let day = parts
        .next()
        .and_then(|part| part.parse::<u8>().ok())
        .ok_or(StoreError::InvalidPartitionKey)?;
    if parts.next().is_some() {
        return Err(StoreError::InvalidPartitionKey);
    }
    let date =
        Date::from_calendar_date(year, month, day).map_err(|_| StoreError::InvalidPartitionKey)?;
    if date.to_string() != value {
        return Err(StoreError::InvalidPartitionKey);
    }
    Ok(date)
}

pub(crate) fn safe_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}
