//! Point-in-time feature materialization through direct, WAL, and Parquet paths.

use std::{
    collections::BTreeMap,
    fs,
    num::NonZeroU32,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::Path,
    sync::Arc,
};

use arrow::{
    array::{Array, Int64Array, StringArray, UInt32Array, UInt64Array},
    datatypes::{DataType, Field, Schema},
    record_batch::RecordBatch,
};
use domain::{AssetId, AssetNamespace, SourceId, SourceKind, UnixNanos, VenueId};
use feature_registry::{
    CodeRevision, DurationNanos, FeatureDatum, FeatureEntity, FeatureId, FeatureObservation,
    FeatureObservationInput, FeatureValue, FeatureValueType, FinalityState, FiniteF64, FormulaHash,
    LineageHash, MissingnessReason, ObservationRevision, QualityScore,
    SourceCoverage as FeatureSourceCoverage, SourceCoverageEntry, WindowId,
};
use fixed_decimal::FixedDecimal;
use parquet_store::{
    BatchId, CorrectionLineage, DatasetMetadata, DatasetMetadataInput, DatasetPolicy,
    DatasetPolicyInput, DatasetReader, DatasetWriter, PartitionKey, PartitionKeyInput,
    SourceCoverage as DatasetSourceCoverage,
};
use quality::SourceHealthState;
use raw_wal::{
    Segment,
    frame::RecordMetadata,
    prologue::{SegmentMetadata, StreamDescriptor},
};
use semver::Version;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{FeatureLineage, FeatureLineageInput, LineageError};

const MAXIMUM_FIXTURE_BYTES: usize = 1_048_576;
const MAXIMUM_FEATURE_ROWS: usize = 4_096;
const FEATURE_EVENT_TYPE: &str = "feature-observation";
const FEATURE_WAL_NAME: &str = "feature-observations.wal";
const FEATURE_DATASET_DIRECTORY: &str = "feature-dataset";
const REPORT_FIXED_DOMAIN: &[u8] = b"cmti:materialized-fixed-features:v1\0";
const REPORT_LINEAGE_DOMAIN: &[u8] = b"cmti:materialized-lineage:v1\0";
const REPORT_FINALITY_DOMAIN: &[u8] = b"cmti:materialized-finality-quality:v1\0";

/// Distinct input path exercised by a materialization run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MaterializationMode {
    LiveSimulation,
    WalReplay,
    ParquetReplay,
}

/// Canonical row stored in the point-in-time feature dataset.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializedFeatureRow {
    effective_event_time_ns: i64,
    exchange_event_time_ns: Option<i64>,
    receive_wall_time_ns: i64,
    event_type: String,
    venue: String,
    instrument_generation: u32,
    connection_epoch: u64,
    subscription_epoch: u64,
    wal_record_sequence: u64,
    feature_id: String,
    feature_version: String,
    entity_json: String,
    window_id: String,
    resolution_ns: u64,
    datum_json: String,
    value_type: String,
    event_time_start_ns: i64,
    event_time_end_ns: i64,
    as_known_at_ns: i64,
    computed_at_ns: i64,
    watermark_ns: Option<i64>,
    finality_as_known_at_ns: i64,
    finality_state: String,
    revision: u32,
    source_coverage_json: String,
    quality_millionths: u32,
    normalization_version: String,
    formula_hash: String,
    code_commit: String,
    lineage_hash: String,
    float_bits: Option<u64>,
}

impl MaterializedFeatureRow {
    fn try_from_observation(
        observation: &FeatureObservation,
        authority: RowAuthority<'_>,
    ) -> Result<Self, MaterializationError> {
        if !matches!(
            observation.finality_state(),
            FinalityState::Final | FinalityState::Corrected
        ) {
            return Err(MaterializationError::UnfinalizedObservation);
        }
        let datum_json = match observation.datum() {
            FeatureDatum::Present(value) => canonical_json_string(value)?,
            FeatureDatum::Missing(reason) => {
                canonical_json_string(&MissingDatumWire { missing: *reason })?
            }
        };
        let float_bits = match observation.datum().value() {
            Some(FeatureValue::Float64(value)) => Some(value.value().to_bits()),
            _ => None,
        };
        let row = Self {
            effective_event_time_ns: observation.event_time_end().value(),
            exchange_event_time_ns: Some(observation.event_time_end().value()),
            receive_wall_time_ns: observation.computed_at().value(),
            event_type: FEATURE_EVENT_TYPE.to_owned(),
            venue: authority.venue.to_owned(),
            instrument_generation: authority.instrument_generation,
            connection_epoch: authority.connection_epoch,
            subscription_epoch: authority.subscription_epoch,
            wal_record_sequence: authority.wal_record_sequence,
            feature_id: observation.feature_id().as_str().to_owned(),
            feature_version: observation.feature_version().to_string(),
            entity_json: canonical_json_string(observation.entity())?,
            window_id: observation.window_id().as_str().to_owned(),
            resolution_ns: observation.resolution().value(),
            datum_json,
            value_type: value_type_name(observation.value_type()).to_owned(),
            event_time_start_ns: observation.event_time_start().value(),
            event_time_end_ns: observation.event_time_end().value(),
            as_known_at_ns: observation.as_known_at().value(),
            computed_at_ns: observation.computed_at().value(),
            watermark_ns: observation.watermark().map(UnixNanos::value),
            finality_as_known_at_ns: observation.finality_as_known_at().value(),
            finality_state: finality_name(observation.finality_state()).to_owned(),
            revision: observation.revision().value(),
            source_coverage_json: canonical_json_string(observation.source_coverage())?,
            quality_millionths: observation.quality_score().millionths(),
            normalization_version: observation.normalization_version().to_string(),
            formula_hash: hex::encode(observation.formula_hash().bytes()),
            code_commit: observation.code_commit().as_str().to_owned(),
            lineage_hash: hex::encode(observation.lineage_hash().bytes()),
            float_bits,
        };
        row.validate()?;
        Ok(row)
    }

    fn validate(&self) -> Result<(), MaterializationError> {
        if self.event_type != FEATURE_EVENT_TYPE
            || self.venue.is_empty()
            || self.instrument_generation == 0
            || self.connection_epoch == 0
            || self.subscription_epoch == 0
            || self.wal_record_sequence == 0
            || FeatureId::new(self.feature_id.clone()).is_err()
            || !valid_release_version(&self.feature_version)
            || WindowId::new(self.window_id.clone()).is_err()
            || self.resolution_ns == 0
            || self.event_time_start_ns <= 0
            || self.event_time_start_ns > self.event_time_end_ns
            || self.event_time_end_ns != self.effective_event_time_ns
            || self.exchange_event_time_ns != Some(self.event_time_end_ns)
            || self.event_time_end_ns > self.as_known_at_ns
            || self.as_known_at_ns > self.computed_at_ns
            || self.receive_wall_time_ns != self.computed_at_ns
            || VenueId::new(&self.venue).is_err()
            || self.finality_as_known_at_ns <= 0
            || self.finality_as_known_at_ns > self.as_known_at_ns
            || self
                .watermark_ns
                .is_some_and(|watermark| watermark <= 0 || watermark > self.finality_as_known_at_ns)
            || !matches!(self.finality_state.as_str(), "final" | "corrected")
            || self.revision == 0
            || (self.finality_state == "corrected" && self.revision == 1)
            || self.quality_millionths > QualityScore::MAX_MILLIONTHS
            || !valid_lower_hash(&self.formula_hash)
            || !valid_lower_hash(&self.lineage_hash)
            || CodeRevision::new(self.code_commit.clone()).is_err()
            || !valid_release_version(&self.normalization_version)
            || !canonical_json(&self.entity_json)
            || !canonical_json(&self.datum_json)
            || !canonical_json(&self.source_coverage_json)
            || !matches!(
                self.value_type.as_str(),
                "fixed_decimal" | "fixed_decimal_map" | "float64" | "integer" | "boolean"
            )
            || !self.valid_datum_encoding()
        {
            return Err(MaterializationError::InvalidRow);
        }
        Ok(())
    }

    fn valid_datum_encoding(&self) -> bool {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&self.datum_json) else {
            return false;
        };
        if let Some(missing) = value.as_object()
            && missing.len() == 1
            && missing
                .get("missing")
                .and_then(serde_json::Value::as_str)
                .is_some_and(valid_missingness_name)
        {
            return self.float_bits.is_none();
        }
        match self.value_type.as_str() {
            "fixed_decimal" => {
                self.float_bits.is_none()
                    && value
                        .as_str()
                        .is_some_and(|encoded| FixedDecimal::parse_canonical(encoded).is_ok())
            }
            "fixed_decimal_map" => {
                self.float_bits.is_none()
                    && value.as_object().is_some_and(|entries| {
                        entries.len() <= feature_registry::MAX_FIXED_DECIMAL_MAP_ENTRIES
                            && entries.iter().all(|(key, value)| {
                                FixedDecimal::parse_canonical(key).is_ok()
                                    && value.as_str().is_some_and(|encoded| {
                                        FixedDecimal::parse_canonical(encoded).is_ok()
                                    })
                            })
                    })
            }
            "float64" => value.as_f64().is_some_and(|number| {
                number.is_finite()
                    && self
                        .float_bits
                        .is_some_and(|bits| f64::from_bits(bits).to_bits() == number.to_bits())
            }),
            "integer" => self.float_bits.is_none() && value.as_i64().is_some(),
            "boolean" => self.float_bits.is_none() && value.as_bool().is_some(),
            _ => false,
        }
    }

    fn identity_key(&self) -> (&str, &str, &str, &str, i64, i64) {
        (
            &self.feature_id,
            &self.feature_version,
            &self.entity_json,
            &self.window_id,
            self.event_time_start_ns,
            self.event_time_end_ns,
        )
    }

    fn parity_key(&self) -> ((&str, &str, &str, &str, i64, i64), u32) {
        (self.identity_key(), self.revision)
    }

    pub fn feature_id(&self) -> &str {
        &self.feature_id
    }

    pub fn datum_json(&self) -> &str {
        &self.datum_json
    }

    pub fn float_value(&self) -> Option<f64> {
        self.float_bits.map(f64::from_bits)
    }

    pub const fn quality_millionths(&self) -> u32 {
        self.quality_millionths
    }

    pub const fn as_known_at_ns(&self) -> i64 {
        self.as_known_at_ns
    }

    pub const fn computed_at_ns(&self) -> i64 {
        self.computed_at_ns
    }

    pub const fn finality_as_known_at_ns(&self) -> i64 {
        self.finality_as_known_at_ns
    }

    pub const fn revision(&self) -> u32 {
        self.revision
    }
}

/// Canonical parity and durable-publication evidence for one run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializationReport {
    rows: Vec<MaterializedFeatureRow>,
    fixed_point_digest: [u8; 32],
    lineage_digest: [u8; 32],
    finality_quality_digest: [u8; 32],
    wal_segment_blake3: Option<[u8; 32]>,
    sealed_manifest_blake3: Option<[u8; 32]>,
}

impl MaterializationReport {
    fn try_new(
        mut rows: Vec<MaterializedFeatureRow>,
        wal_segment_blake3: Option<[u8; 32]>,
        sealed_manifest_blake3: Option<[u8; 32]>,
    ) -> Result<Self, MaterializationError> {
        if rows.is_empty() || rows.len() > MAXIMUM_FEATURE_ROWS {
            return Err(MaterializationError::CapacityExceeded);
        }
        for row in &rows {
            row.validate()?;
        }
        rows.sort_by(|left, right| {
            left.identity_key()
                .cmp(&right.identity_key())
                .then_with(|| left.revision.cmp(&right.revision))
                .then_with(|| left.lineage_hash.cmp(&right.lineage_hash))
        });
        if rows.windows(2).any(|pair| {
            pair[0].identity_key() == pair[1].identity_key() && pair[0].revision == pair[1].revision
        }) {
            return Err(MaterializationError::DuplicateObservation);
        }

        let fixed_rows = rows
            .iter()
            .filter(|row| row.value_type != "float64")
            .collect::<Vec<_>>();
        let lineage = rows
            .iter()
            .map(|row| row.lineage_hash.as_str())
            .collect::<Vec<_>>();
        let finality_quality = rows
            .iter()
            .map(|row| {
                (
                    row.feature_id.as_str(),
                    row.finality_state.as_str(),
                    row.revision,
                    row.quality_millionths,
                )
            })
            .collect::<Vec<_>>();

        Ok(Self {
            fixed_point_digest: digest_json(REPORT_FIXED_DOMAIN, &fixed_rows)?,
            lineage_digest: digest_json(REPORT_LINEAGE_DOMAIN, &lineage)?,
            finality_quality_digest: digest_json(REPORT_FINALITY_DOMAIN, &finality_quality)?,
            rows,
            wal_segment_blake3,
            sealed_manifest_blake3,
        })
    }

    pub fn rows(&self) -> &[MaterializedFeatureRow] {
        &self.rows
    }

    pub const fn fixed_point_digest(&self) -> [u8; 32] {
        self.fixed_point_digest
    }

    pub const fn lineage_digest(&self) -> [u8; 32] {
        self.lineage_digest
    }

    pub const fn finality_quality_digest(&self) -> [u8; 32] {
        self.finality_quality_digest
    }

    pub const fn wal_segment_blake3(&self) -> Option<[u8; 32]> {
        self.wal_segment_blake3
    }

    pub const fn sealed_manifest_blake3(&self) -> Option<[u8; 32]> {
        self.sealed_manifest_blake3
    }

    pub fn float_max_abs_error(&self, other: &Self) -> Result<f64, MaterializationError> {
        let left = self
            .rows
            .iter()
            .filter_map(|row| {
                row.float_bits
                    .map(|bits| (row.parity_key(), f64::from_bits(bits)))
            })
            .collect::<Vec<_>>();
        let right = other
            .rows
            .iter()
            .filter_map(|row| {
                row.float_bits
                    .map(|bits| (row.parity_key(), f64::from_bits(bits)))
            })
            .collect::<Vec<_>>();
        if left.len() != right.len()
            || left
                .iter()
                .zip(&right)
                .any(|((left_key, _), (right_key, _))| left_key != right_key)
        {
            return Err(MaterializationError::ParityShapeMismatch);
        }
        Ok(left
            .iter()
            .zip(right)
            .map(|((_, left), (_, right))| (left - right).abs())
            .fold(0.0_f64, f64::max))
    }
}

/// Revision-aware point-in-time view over immutable materialized rows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PointInTimeFeatureSet {
    rows: Vec<MaterializedFeatureRow>,
}

impl PointInTimeFeatureSet {
    pub fn try_new(mut rows: Vec<MaterializedFeatureRow>) -> Result<Self, MaterializationError> {
        if rows.is_empty() || rows.len() > MAXIMUM_FEATURE_ROWS {
            return Err(MaterializationError::CapacityExceeded);
        }
        for row in &rows {
            row.validate()?;
        }
        rows.sort_by(|left, right| {
            left.identity_key()
                .cmp(&right.identity_key())
                .then_with(|| left.revision.cmp(&right.revision))
        });
        for history in rows.chunk_by(|left, right| left.identity_key() == right.identity_key()) {
            if history[0].revision != 1
                || history.windows(2).any(|pair| {
                    pair[1].revision != pair[0].revision + 1
                        || pair[1].as_known_at_ns < pair[0].as_known_at_ns
                        || pair[1].finality_state != "corrected"
                })
            {
                return Err(MaterializationError::InvalidCorrectionHistory);
            }
        }
        Ok(Self { rows })
    }

    pub fn as_known_at(&self, prediction_time_ns: i64) -> Vec<MaterializedFeatureRow> {
        let mut selected = BTreeMap::new();
        for row in &self.rows {
            if row.as_known_at_ns <= prediction_time_ns
                && row.computed_at_ns <= prediction_time_ns
                && row.finality_as_known_at_ns <= prediction_time_ns
            {
                selected.insert(row.identity_key(), row);
            }
        }
        selected.values().map(|row| (*row).clone()).collect()
    }
}

/// Inspectable SQL plan that cannot omit point-in-time training guards.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PointInTimeTrainingPlan {
    sql: String,
}

impl PointInTimeTrainingPlan {
    pub fn try_new(
        prediction_table: &str,
        feature_table: &str,
    ) -> Result<Self, MaterializationError> {
        if !valid_sql_identifier(prediction_table) || !valid_sql_identifier(feature_table) {
            return Err(MaterializationError::InvalidQueryPlan);
        }
        Ok(Self {
            sql: format!(
                "SELECT p.prediction_id, p.prediction_time_ns, f.* \
FROM {prediction_table} AS p \
JOIN {feature_table} AS f ON f.entity_json = p.entity_json \
AND f.event_time_end_ns <= p.prediction_time_ns \
AND f.as_known_at_ns <= p.prediction_time_ns \
AND f.computed_at_ns <= p.prediction_time_ns \
AND f.finality_as_known_at_ns <= p.prediction_time_ns \
QUALIFY ROW_NUMBER() OVER (\
PARTITION BY p.prediction_id, f.feature_id, f.feature_version, f.entity_json, f.window_id, \
f.event_time_start_ns, f.event_time_end_ns ORDER BY f.revision DESC) = 1"
            ),
        })
    }

    pub fn sql(&self) -> &str {
        &self.sql
    }
}

/// Stateless bounded feature materializer.
pub struct Materializer;

impl Materializer {
    pub fn run_fixture(
        manifest_path: impl AsRef<Path>,
        mode: MaterializationMode,
        root: &Path,
    ) -> Result<MaterializationReport, MaterializationError> {
        validate_private_root(root)?;
        let fixture = FixtureManifest::from_path(manifest_path.as_ref())?;
        if fixture.correction.is_some() {
            return Err(MaterializationError::CorrectionAuthorityRequired);
        }
        let rows = fixture.rows()?;
        match mode {
            MaterializationMode::LiveSimulation => MaterializationReport::try_new(rows, None, None),
            MaterializationMode::WalReplay => {
                let (replayed, wal_hash) = wal_round_trip(root, &fixture, &rows)?;
                MaterializationReport::try_new(replayed, Some(wal_hash), None)
            }
            MaterializationMode::ParquetReplay => {
                let (replayed, manifest_hash) = parquet_round_trip(root, &fixture, &rows, None)?;
                MaterializationReport::try_new(replayed, None, Some(manifest_hash))
            }
        }
    }

    pub fn run_correction_fixture(
        manifest_path: impl AsRef<Path>,
        root: &Path,
        base_manifest_blake3: [u8; 32],
    ) -> Result<MaterializationReport, MaterializationError> {
        validate_private_root(root)?;
        let fixture = FixtureManifest::from_path(manifest_path.as_ref())?;
        let correction = fixture
            .correction
            .as_ref()
            .ok_or(MaterializationError::CorrectionAuthorityRequired)?;
        let rows = fixture.rows()?;
        let correction_key = partition_key(&fixture, &rows)?;
        let base_key = PartitionKey::try_new(PartitionKeyInput {
            event_type: correction_key.event_type().to_owned(),
            dataset_version: NonZeroU32::new(correction.base_dataset_version)
                .ok_or(MaterializationError::InvalidManifest)?,
            schema_version: correction_key.schema_version(),
            venue: correction_key.venue().to_owned(),
            instrument_generation: correction_key.instrument_generation(),
            utc_date: correction_key.utc_date(),
            hour: correction_key.hour(),
        })?;
        let canonical_root = fs::canonicalize(root)?;
        validate_private_root(&canonical_root)?;
        let dataset_root = canonical_root.join(FEATURE_DATASET_DIRECTORY);
        let (base_manifest, base_batches) =
            DatasetReader::open(&dataset_root, materialization_policy()?)?
                .read_partition(&base_key)?;
        if base_manifest.manifest_hash() != &base_manifest_blake3 {
            return Err(MaterializationError::InvalidCorrectionHistory);
        }
        let base_rows = rows_from_batches(&base_batches)?;
        validate_correction_snapshot(&base_rows, &rows, correction.known_at_ns)?;
        let lineage = CorrectionLineage::try_new(
            NonZeroU32::new(correction.base_dataset_version)
                .ok_or(MaterializationError::InvalidManifest)?,
            hex::encode(base_manifest_blake3),
            correction.known_at_ns,
            correction.reason.clone(),
            correction.provenance.clone(),
        )?;
        let (replayed, manifest_hash) = parquet_round_trip(root, &fixture, &rows, Some(lineage))?;
        MaterializationReport::try_new(replayed, None, Some(manifest_hash))
    }

    pub fn verify_fixture_dataset(
        manifest_path: impl AsRef<Path>,
        root: &Path,
    ) -> Result<MaterializationReport, MaterializationError> {
        validate_private_root(root)?;
        let fixture = FixtureManifest::from_path(manifest_path.as_ref())?;
        let expected_rows = fixture.rows()?;
        let canonical_root = fs::canonicalize(root)?;
        validate_private_root(&canonical_root)?;
        let dataset_root = canonical_root.join(FEATURE_DATASET_DIRECTORY);
        let key = partition_key(&fixture, &expected_rows)?;
        let (verified, batches) =
            DatasetReader::open(&dataset_root, materialization_policy()?)?.read_partition(&key)?;
        let rows = rows_from_batches(&batches)?;
        if rows != expected_rows {
            return Err(MaterializationError::Integrity);
        }
        MaterializationReport::try_new(rows, None, Some(*verified.manifest_hash()))
    }
}

fn validate_correction_snapshot(
    base_rows: &[MaterializedFeatureRow],
    correction_rows: &[MaterializedFeatureRow],
    correction_known_at_ns: i64,
) -> Result<(), MaterializationError> {
    if base_rows.len() != correction_rows.len() || base_rows.is_empty() {
        return Err(MaterializationError::InvalidCorrectionHistory);
    }
    let mut base_rows = base_rows.to_vec();
    let mut correction_rows = correction_rows.to_vec();
    base_rows.sort_by(|left, right| left.identity_key().cmp(&right.identity_key()));
    correction_rows.sort_by(|left, right| left.identity_key().cmp(&right.identity_key()));
    let mut corrected = 0_usize;
    for (base, correction) in base_rows.iter().zip(&correction_rows) {
        if base.identity_key() != correction.identity_key() {
            return Err(MaterializationError::InvalidCorrectionHistory);
        }
        if base == correction {
            continue;
        }
        if base.revision.checked_add(1) != Some(correction.revision)
            || correction.finality_state != "corrected"
            || correction.as_known_at_ns < base.as_known_at_ns
            || correction.as_known_at_ns > correction_known_at_ns
            || correction.computed_at_ns < base.computed_at_ns
            || correction.finality_as_known_at_ns < base.finality_as_known_at_ns
        {
            return Err(MaterializationError::InvalidCorrectionHistory);
        }
        corrected = corrected
            .checked_add(1)
            .ok_or(MaterializationError::CapacityExceeded)?;
    }
    if corrected == 0 {
        return Err(MaterializationError::InvalidCorrectionHistory);
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct RowAuthority<'a> {
    venue: &'a str,
    instrument_generation: u32,
    connection_epoch: u64,
    subscription_epoch: u64,
    wal_record_sequence: u64,
}

#[derive(Serialize)]
struct MissingDatumWire {
    missing: MissingnessReason,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureManifest {
    schema_version: u32,
    artifact_id: String,
    status: String,
    local_only: bool,
    maximum_observations: usize,
    dataset_version: u32,
    partition_schema_version: u32,
    feature_set_version: String,
    parser_version: String,
    normalizer_version: String,
    normalization_version: String,
    code_commit: String,
    venue: String,
    instrument_generation: u32,
    asset_namespace: FixtureAssetNamespace,
    asset_chain_id: String,
    asset_contract_or_mint: String,
    asset_symbol: String,
    asset_generation: u32,
    source_name: String,
    stream_name: String,
    stream_id: u32,
    connection_epoch: u64,
    subscription_epoch: u64,
    wal_segment_id_hex: String,
    input_wal_segment_blake3: String,
    wal_start_offset: u64,
    wal_end_offset: u64,
    created_wall_time_ns: i64,
    correction: Option<FixtureCorrection>,
    source: Vec<FixtureSource>,
    observation: Vec<FixtureObservation>,
}

impl FixtureManifest {
    fn from_path(path: &Path) -> Result<Self, MaterializationError> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(MaterializationError::UnsafePath);
        }
        let size =
            usize::try_from(metadata.len()).map_err(|_| MaterializationError::CapacityExceeded)?;
        if size == 0 || size > MAXIMUM_FIXTURE_BYTES {
            return Err(MaterializationError::CapacityExceeded);
        }
        let bytes = fs::read(path)?;
        if bytes.len() != size {
            return Err(MaterializationError::Integrity);
        }
        let fixture: Self = toml::from_str(
            std::str::from_utf8(&bytes).map_err(|_| MaterializationError::InvalidManifest)?,
        )?;
        fixture.validate()?;
        Ok(fixture)
    }

    fn validate(&self) -> Result<(), MaterializationError> {
        let valid_artifact_id = matches!(
            self.artifact_id.as_str(),
            "fixtures/golden-replays/features-v1/manifest.toml"
                | "fixtures/golden-replays/features-v1/correction-v2.toml"
        );
        let valid_correction_version = match &self.correction {
            None => self.dataset_version == 1,
            Some(correction) => {
                correction.base_dataset_version > 0
                    && correction
                        .base_dataset_version
                        .checked_add(1)
                        .is_some_and(|version| version == self.dataset_version)
                    && correction.known_at_ns > 0
                    && !correction.reason.is_empty()
                    && !correction.provenance.is_empty()
            }
        };
        if self.schema_version != 1
            || !valid_artifact_id
            || self.status != "implemented"
            || !self.local_only
            || self.maximum_observations == 0
            || self.maximum_observations > MAXIMUM_FEATURE_ROWS
            || self.observation.len() != self.maximum_observations
            || self.dataset_version == 0
            || self.partition_schema_version == 0
            || self.feature_set_version.is_empty()
            || self.parser_version.is_empty()
            || self.normalizer_version.is_empty()
            || Version::parse(&self.normalization_version).is_err()
            || CodeRevision::new(self.code_commit.clone()).is_err()
            || self.venue.is_empty()
            || self.instrument_generation == 0
            || self.asset_generation == 0
            || self.source_name.is_empty()
            || self.stream_name.is_empty()
            || self.stream_id == 0
            || self.connection_epoch == 0
            || self.subscription_epoch == 0
            || decode_fixed_hash::<16>(&self.wal_segment_id_hex).is_err()
            || !valid_lower_hash(&self.input_wal_segment_blake3)
            || self.wal_start_offset >= self.wal_end_offset
            || self.created_wall_time_ns <= 0
            || !valid_correction_version
            || self.source.is_empty()
            || self.source.len() > 64
        {
            return Err(MaterializationError::InvalidManifest);
        }
        let mut sequences = self
            .observation
            .iter()
            .map(|observation| observation.wal_record_sequence)
            .collect::<Vec<_>>();
        sequences.sort_unstable();
        if sequences.first().copied() != Some(1)
            || sequences
                .iter()
                .copied()
                .enumerate()
                .any(|(index, sequence)| sequence != index as u64 + 1)
        {
            return Err(MaterializationError::InvalidManifest);
        }
        Ok(())
    }

    fn feature_source_coverage(&self) -> Result<FeatureSourceCoverage, MaterializationError> {
        let entries = self
            .source
            .iter()
            .map(FixtureSource::coverage_entry)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(FeatureSourceCoverage::try_new(entries)?)
    }

    fn rows(&self) -> Result<Vec<MaterializedFeatureRow>, MaterializationError> {
        let asset = AssetId::new(
            self.asset_namespace.into_domain(),
            &self.asset_chain_id,
            &self.asset_contract_or_mint,
            &self.asset_symbol,
            self.asset_generation,
        )?;
        let coverage = self.feature_source_coverage()?;
        let code_commit = CodeRevision::new(self.code_commit.clone())?;
        let normalization_version = Version::parse(&self.normalization_version)
            .map_err(|_| MaterializationError::InvalidManifest)?;
        self.observation
            .iter()
            .map(|wire| {
                let feature_id = FeatureId::new(wire.feature_id.clone())?;
                let feature_version = Version::parse(&wire.feature_version)
                    .map_err(|_| MaterializationError::InvalidManifest)?;
                let formula_hash = FormulaHash::new(decode_fixed_hash(&wire.formula_hash)?)?;
                let input_event_hashes = wire
                    .input_event_hashes
                    .iter()
                    .map(|hash| decode_fixed_hash(hash))
                    .collect::<Result<Vec<_>, _>>()?;
                let lineage = FeatureLineage::try_new(FeatureLineageInput {
                    feature_id: feature_id.clone(),
                    feature_version: feature_version.clone(),
                    formula_hash,
                    input_event_hashes,
                    source_coverage: coverage.clone(),
                    code_commit: code_commit.clone(),
                })?;
                let observation = FeatureObservation::try_new(FeatureObservationInput {
                    feature_id,
                    feature_version,
                    entity: FeatureEntity::Asset(asset.clone()),
                    window_id: WindowId::new(wire.window_id.clone())?,
                    resolution: DurationNanos::new(wire.resolution_ns),
                    datum: wire.datum()?,
                    value_type: wire.value_type.into_registry(),
                    event_time_start: UnixNanos::new(wire.event_time_start_ns),
                    event_time_end: UnixNanos::new(wire.event_time_end_ns),
                    as_known_at: UnixNanos::new(wire.as_known_at_ns),
                    computed_at: UnixNanos::new(wire.computed_at_ns),
                    watermark: wire.watermark_ns.map(UnixNanos::new),
                    finality_as_known_at: UnixNanos::new(wire.finality_as_known_at_ns),
                    finality_state: wire.finality_state.into_registry(),
                    revision: ObservationRevision::new(wire.revision)?,
                    source_coverage: coverage.clone(),
                    quality_score: QualityScore::from_millionths(wire.quality_millionths)?,
                    normalization_version: normalization_version.clone(),
                    formula_hash,
                    code_commit: code_commit.clone(),
                    lineage_hash: LineageHash::new(lineage.digest()?)?,
                })?;
                MaterializedFeatureRow::try_from_observation(
                    &observation,
                    RowAuthority {
                        venue: &self.venue,
                        instrument_generation: self.instrument_generation,
                        connection_epoch: self.connection_epoch,
                        subscription_epoch: self.subscription_epoch,
                        wal_record_sequence: wire.wal_record_sequence,
                    },
                )
            })
            .collect()
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureCorrection {
    base_dataset_version: u32,
    known_at_ns: i64,
    reason: String,
    provenance: String,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FixtureAssetNamespace {
    Native,
    Evm,
    Solana,
    Fiat,
    Synthetic,
}

impl FixtureAssetNamespace {
    const fn into_domain(self) -> AssetNamespace {
        match self {
            Self::Native => AssetNamespace::Native,
            Self::Evm => AssetNamespace::Evm,
            Self::Solana => AssetNamespace::Solana,
            Self::Fiat => AssetNamespace::Fiat,
            Self::Synthetic => AssetNamespace::Synthetic,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureSource {
    kind: FixtureSourceKind,
    name: String,
    generation: u32,
    health: FixtureHealth,
}

impl FixtureSource {
    fn coverage_entry(&self) -> Result<SourceCoverageEntry, MaterializationError> {
        Ok(SourceCoverageEntry::new(
            SourceId::new(self.kind.into_domain(), &self.name, self.generation)?,
            self.health.into_quality(),
        ))
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FixtureSourceKind {
    Exchange,
    Chain,
    Macro,
    News,
    Derived,
    System,
}

impl FixtureSourceKind {
    const fn into_domain(self) -> SourceKind {
        match self {
            Self::Exchange => SourceKind::Exchange,
            Self::Chain => SourceKind::Chain,
            Self::Macro => SourceKind::Macro,
            Self::News => SourceKind::News,
            Self::Derived => SourceKind::Derived,
            Self::System => SourceKind::System,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FixtureHealth {
    Healthy,
    Degraded,
    Unhealthy,
    Quarantined,
    Recovering,
}

impl FixtureHealth {
    const fn into_quality(self) -> SourceHealthState {
        match self {
            Self::Healthy => SourceHealthState::Healthy,
            Self::Degraded => SourceHealthState::Degraded,
            Self::Unhealthy => SourceHealthState::Unhealthy,
            Self::Quarantined => SourceHealthState::Quarantined,
            Self::Recovering => SourceHealthState::Recovering,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureObservation {
    feature_id: String,
    feature_version: String,
    window_id: String,
    resolution_ns: u64,
    value_type: FixtureValueType,
    value: Option<String>,
    missingness_reason: Option<FixtureMissingness>,
    event_time_start_ns: i64,
    event_time_end_ns: i64,
    as_known_at_ns: i64,
    computed_at_ns: i64,
    watermark_ns: Option<i64>,
    finality_as_known_at_ns: i64,
    finality_state: FixtureFinality,
    revision: u32,
    quality_millionths: u32,
    formula_hash: String,
    input_event_hashes: Vec<String>,
    wal_record_sequence: u64,
}

impl FixtureObservation {
    fn datum(&self) -> Result<FeatureDatum, MaterializationError> {
        match (&self.value, self.missingness_reason) {
            (Some(value), None) => Ok(FeatureDatum::Present(match self.value_type {
                FixtureValueType::FixedDecimal => {
                    FeatureValue::FixedDecimal(FixedDecimal::parse_canonical(value)?)
                }
                FixtureValueType::Float64 => FeatureValue::Float64(FiniteF64::new(
                    value
                        .parse::<f64>()
                        .map_err(|_| MaterializationError::InvalidManifest)?,
                )?),
                FixtureValueType::Integer => FeatureValue::Integer(
                    value
                        .parse::<i64>()
                        .map_err(|_| MaterializationError::InvalidManifest)?,
                ),
                FixtureValueType::Boolean => FeatureValue::Boolean(match value.as_str() {
                    "true" => true,
                    "false" => false,
                    _ => return Err(MaterializationError::InvalidManifest),
                }),
                FixtureValueType::FixedDecimalMap => {
                    return Err(MaterializationError::InvalidManifest);
                }
            })),
            (None, Some(reason)) => Ok(FeatureDatum::Missing(reason.into_registry())),
            _ => Err(MaterializationError::InvalidManifest),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FixtureValueType {
    FixedDecimal,
    FixedDecimalMap,
    Float64,
    Integer,
    Boolean,
}

impl FixtureValueType {
    const fn into_registry(self) -> FeatureValueType {
        match self {
            Self::FixedDecimal => FeatureValueType::FixedDecimal,
            Self::FixedDecimalMap => FeatureValueType::FixedDecimalMap,
            Self::Float64 => FeatureValueType::Float64,
            Self::Integer => FeatureValueType::Integer,
            Self::Boolean => FeatureValueType::Boolean,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FixtureFinality {
    Provisional,
    Final,
    Corrected,
    Invalid,
}

impl FixtureFinality {
    const fn into_registry(self) -> FinalityState {
        match self {
            Self::Provisional => FinalityState::Provisional,
            Self::Final => FinalityState::Final,
            Self::Corrected => FinalityState::Corrected,
            Self::Invalid => FinalityState::Invalid,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FixtureMissingness {
    NotListed,
    SourceNotSupported,
    SourceDisconnected,
    SequenceGap,
    Stale,
    InsufficientHistory,
    WindowNotFinal,
    BelowLiquidityThreshold,
    VendorRevisionPending,
    ModelNotApplicable,
    PrivacyOrLicenseRestriction,
    Unknown,
}

impl FixtureMissingness {
    const fn into_registry(self) -> MissingnessReason {
        match self {
            Self::NotListed => MissingnessReason::NotListed,
            Self::SourceNotSupported => MissingnessReason::SourceNotSupported,
            Self::SourceDisconnected => MissingnessReason::SourceDisconnected,
            Self::SequenceGap => MissingnessReason::SequenceGap,
            Self::Stale => MissingnessReason::Stale,
            Self::InsufficientHistory => MissingnessReason::InsufficientHistory,
            Self::WindowNotFinal => MissingnessReason::WindowNotFinal,
            Self::BelowLiquidityThreshold => MissingnessReason::BelowLiquidityThreshold,
            Self::VendorRevisionPending => MissingnessReason::VendorRevisionPending,
            Self::ModelNotApplicable => MissingnessReason::ModelNotApplicable,
            Self::PrivacyOrLicenseRestriction => MissingnessReason::PrivacyOrLicenseRestriction,
            Self::Unknown => MissingnessReason::Unknown,
        }
    }
}

fn wal_round_trip(
    root: &Path,
    fixture: &FixtureManifest,
    rows: &[MaterializedFeatureRow],
) -> Result<(Vec<MaterializedFeatureRow>, [u8; 32]), MaterializationError> {
    let path = root.join(FEATURE_WAL_NAME);
    let metadata = SegmentMetadata::new(
        decode_fixed_hash(&fixture.wal_segment_id_hex)?,
        fixture.created_wall_time_ns,
        "feature-observation-v1",
        "golden-features-v1",
        "feature-engine-v1",
        vec![StreamDescriptor::new(
            fixture.stream_id,
            &fixture.source_name,
            &fixture.stream_name,
        )?],
    )?;
    {
        let mut segment = Segment::create_v2(&path, metadata)?;
        for row in rows {
            let payload = serde_json::to_vec(row)?;
            segment.append_record_synced(
                RecordMetadata {
                    flags: 0,
                    stream_id: fixture.stream_id,
                    connection_epoch: fixture.connection_epoch,
                    record_sequence: row.wal_record_sequence,
                    receive_wall_time_ns: row.receive_wall_time_ns,
                    receive_monotonic_time_ns: row.wal_record_sequence,
                },
                &payload,
            )?;
        }
    }
    let wal_bytes = fs::read(&path)?;
    if wal_bytes.is_empty()
        || wal_bytes.len()
            > MAXIMUM_FIXTURE_BYTES
                .checked_mul(rows.len())
                .ok_or(MaterializationError::CapacityExceeded)?
    {
        return Err(MaterializationError::CapacityExceeded);
    }
    let wal_hash = *blake3::hash(&wal_bytes).as_bytes();

    let mut payloads = Vec::new();
    let mut missing_metadata = false;
    let mut segment = Segment::open_v2(&path)?;
    let summary = segment.recover_with(|record| {
        if let Some(metadata) = record.metadata() {
            payloads.push((metadata, record.payload().to_vec()));
        } else {
            missing_metadata = true;
        }
    })?;
    if missing_metadata
        || summary.record_count() != rows.len() as u64
        || payloads.len() != rows.len()
    {
        return Err(MaterializationError::Integrity);
    }
    let replayed = payloads
        .into_iter()
        .map(|(metadata, payload)| {
            let row: MaterializedFeatureRow = serde_json::from_slice(&payload)?;
            if metadata.stream_id != fixture.stream_id
                || metadata.connection_epoch != row.connection_epoch
                || metadata.record_sequence != row.wal_record_sequence
                || metadata.receive_wall_time_ns != row.receive_wall_time_ns
            {
                return Err(MaterializationError::Integrity);
            }
            row.validate()?;
            Ok(row)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((replayed, wal_hash))
}

fn parquet_round_trip(
    root: &Path,
    fixture: &FixtureManifest,
    rows: &[MaterializedFeatureRow],
    correction_lineage: Option<CorrectionLineage>,
) -> Result<(Vec<MaterializedFeatureRow>, [u8; 32]), MaterializationError> {
    let canonical_root = fs::canonicalize(root)?;
    validate_private_root(&canonical_root)?;
    let dataset_root = canonical_root.join(FEATURE_DATASET_DIRECTORY);
    match fs::DirBuilder::new().mode(0o700).create(&dataset_root) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            validate_private_root(&dataset_root)?;
        }
        Err(error) => return Err(error.into()),
    }
    let key = partition_key(fixture, rows)?;
    let source_coverage = DatasetSourceCoverage::try_new(
        &fixture.source_name,
        &fixture.stream_name,
        fixture.connection_epoch,
        fixture.subscription_epoch,
        rows.first()
            .ok_or(MaterializationError::InvalidRow)?
            .wal_record_sequence,
        rows.last()
            .ok_or(MaterializationError::InvalidRow)?
            .wal_record_sequence,
    )?;
    let metadata = DatasetMetadata::try_new(DatasetMetadataInput {
        feature_version: fixture.feature_set_version.clone(),
        parser_version: fixture.parser_version.clone(),
        normalizer_version: fixture.normalizer_version.clone(),
        code_identity: format!("git_sha1:{}", fixture.code_commit),
        source_coverage: vec![source_coverage.clone()],
        correction_lineage,
    })?;
    let batch_id = BatchId::try_new(
        source_coverage,
        &fixture.input_wal_segment_blake3,
        fixture.wal_start_offset,
        fixture.wal_end_offset,
    )?;
    let batch = rows_to_batch(rows)?;
    let policy = materialization_policy()?;
    let manifest = {
        let mut writer = DatasetWriter::open(&dataset_root, policy.clone())?;
        writer.write_batch(&key, &metadata, &batch_id, &batch)?;
        writer.seal(&key)?
    };
    let expected_manifest_hash = *manifest.manifest_hash();
    let (verified, batches) = DatasetReader::open(&dataset_root, policy)?.read_partition(&key)?;
    if verified.manifest_hash() != &expected_manifest_hash {
        return Err(MaterializationError::Integrity);
    }
    let replayed = rows_from_batches(&batches)?;
    if replayed != rows {
        return Err(MaterializationError::Integrity);
    }
    Ok((replayed, expected_manifest_hash))
}

fn partition_key(
    fixture: &FixtureManifest,
    rows: &[MaterializedFeatureRow],
) -> Result<PartitionKey, MaterializationError> {
    let first = rows.first().ok_or(MaterializationError::InvalidRow)?;
    let instant =
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(first.effective_event_time_ns))
            .map_err(|_| MaterializationError::InvalidRow)?;
    if rows.iter().any(|row| {
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(row.effective_event_time_ns))
            .map(|candidate| {
                candidate.date() != instant.date() || candidate.hour() != instant.hour()
            })
            .unwrap_or(true)
    }) {
        return Err(MaterializationError::PartitionMismatch);
    }
    Ok(PartitionKey::try_new(PartitionKeyInput {
        event_type: FEATURE_EVENT_TYPE.to_owned(),
        dataset_version: NonZeroU32::new(fixture.dataset_version)
            .ok_or(MaterializationError::InvalidManifest)?,
        schema_version: NonZeroU32::new(fixture.partition_schema_version)
            .ok_or(MaterializationError::InvalidManifest)?,
        venue: fixture.venue.clone(),
        instrument_generation: NonZeroU32::new(fixture.instrument_generation)
            .ok_or(MaterializationError::InvalidManifest)?,
        utc_date: instant.date(),
        hour: instant.hour(),
    })?)
}

fn materialization_policy() -> Result<DatasetPolicy, MaterializationError> {
    Ok(DatasetPolicy::try_new(DatasetPolicyInput {
        maximum_batch_rows: MAXIMUM_FEATURE_ROWS,
        maximum_batch_columns: 64,
        maximum_batch_bytes: 16 * 1024 * 1024,
        maximum_value_bytes: 1024 * 1024,
        maximum_schema_bytes: 256 * 1024,
        maximum_schema_fields: 128,
        maximum_schema_depth: 16,
        maximum_writer_memory_bytes: 64 * 1024 * 1024,
        maximum_row_group_rows: MAXIMUM_FEATURE_ROWS,
        target_row_group_bytes: 16 * 1024 * 1024,
        maximum_file_bytes: 64 * 1024 * 1024,
        maximum_partition_bytes: 64 * 1024 * 1024,
        maximum_partition_rows: MAXIMUM_FEATURE_ROWS as u64,
        maximum_active_fragments: 16,
        maximum_footer_bytes: 1024 * 1024,
        maximum_sealed_files: 16,
        reader_batch_rows: 256,
        maximum_correction_depth: 64,
    })?)
}

fn rows_to_batch(rows: &[MaterializedFeatureRow]) -> Result<RecordBatch, MaterializationError> {
    let fields = vec![
        Field::new("effective_event_time_ns", DataType::Int64, false),
        Field::new("exchange_event_time_ns", DataType::Int64, true),
        Field::new("receive_wall_time_ns", DataType::Int64, false),
        Field::new("event_type", DataType::Utf8, false),
        Field::new("venue", DataType::Utf8, false),
        Field::new("instrument_generation", DataType::UInt32, false),
        Field::new("connection_epoch", DataType::UInt64, false),
        Field::new("subscription_epoch", DataType::UInt64, false),
        Field::new("wal_record_sequence", DataType::UInt64, false),
        Field::new("feature_id", DataType::Utf8, false),
        Field::new("feature_version", DataType::Utf8, false),
        Field::new("entity_json", DataType::Utf8, false),
        Field::new("window_id", DataType::Utf8, false),
        Field::new("resolution_ns", DataType::UInt64, false),
        Field::new("datum_json", DataType::Utf8, false),
        Field::new("value_type", DataType::Utf8, false),
        Field::new("event_time_start_ns", DataType::Int64, false),
        Field::new("event_time_end_ns", DataType::Int64, false),
        Field::new("as_known_at_ns", DataType::Int64, false),
        Field::new("computed_at_ns", DataType::Int64, false),
        Field::new("watermark_ns", DataType::Int64, true),
        Field::new("finality_as_known_at_ns", DataType::Int64, false),
        Field::new("finality_state", DataType::Utf8, false),
        Field::new("revision", DataType::UInt32, false),
        Field::new("source_coverage_json", DataType::Utf8, false),
        Field::new("quality_millionths", DataType::UInt32, false),
        Field::new("normalization_version", DataType::Utf8, false),
        Field::new("formula_hash", DataType::Utf8, false),
        Field::new("code_commit", DataType::Utf8, false),
        Field::new("lineage_hash", DataType::Utf8, false),
        Field::new("float_bits", DataType::UInt64, true),
    ];
    let schema = Arc::new(Schema::new(fields));
    Ok(RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|row| row.effective_event_time_ns),
            )),
            Arc::new(Int64Array::from(
                rows.iter()
                    .map(|row| row.exchange_event_time_ns)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|row| row.receive_wall_time_ns),
            )),
            string_array(rows, |row| &row.event_type),
            string_array(rows, |row| &row.venue),
            Arc::new(UInt32Array::from_iter_values(
                rows.iter().map(|row| row.instrument_generation),
            )),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|row| row.connection_epoch),
            )),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|row| row.subscription_epoch),
            )),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|row| row.wal_record_sequence),
            )),
            string_array(rows, |row| &row.feature_id),
            string_array(rows, |row| &row.feature_version),
            string_array(rows, |row| &row.entity_json),
            string_array(rows, |row| &row.window_id),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|row| row.resolution_ns),
            )),
            string_array(rows, |row| &row.datum_json),
            string_array(rows, |row| &row.value_type),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|row| row.event_time_start_ns),
            )),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|row| row.event_time_end_ns),
            )),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|row| row.as_known_at_ns),
            )),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|row| row.computed_at_ns),
            )),
            Arc::new(Int64Array::from(
                rows.iter().map(|row| row.watermark_ns).collect::<Vec<_>>(),
            )),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|row| row.finality_as_known_at_ns),
            )),
            string_array(rows, |row| &row.finality_state),
            Arc::new(UInt32Array::from_iter_values(
                rows.iter().map(|row| row.revision),
            )),
            string_array(rows, |row| &row.source_coverage_json),
            Arc::new(UInt32Array::from_iter_values(
                rows.iter().map(|row| row.quality_millionths),
            )),
            string_array(rows, |row| &row.normalization_version),
            string_array(rows, |row| &row.formula_hash),
            string_array(rows, |row| &row.code_commit),
            string_array(rows, |row| &row.lineage_hash),
            Arc::new(UInt64Array::from(
                rows.iter().map(|row| row.float_bits).collect::<Vec<_>>(),
            )),
        ],
    )?)
}

fn string_array(
    rows: &[MaterializedFeatureRow],
    field: impl Fn(&MaterializedFeatureRow) -> &str,
) -> Arc<StringArray> {
    Arc::new(StringArray::from_iter_values(rows.iter().map(field)))
}

fn rows_from_batches(
    batches: &[RecordBatch],
) -> Result<Vec<MaterializedFeatureRow>, MaterializationError> {
    let mut rows = Vec::new();
    for batch in batches {
        let effective = int64_column(batch, "effective_event_time_ns")?;
        let exchange = int64_column(batch, "exchange_event_time_ns")?;
        let receive = int64_column(batch, "receive_wall_time_ns")?;
        let event_type = string_column(batch, "event_type")?;
        let venue = string_column(batch, "venue")?;
        let generation = uint32_column(batch, "instrument_generation")?;
        let connection = uint64_column(batch, "connection_epoch")?;
        let subscription = uint64_column(batch, "subscription_epoch")?;
        let sequence = uint64_column(batch, "wal_record_sequence")?;
        let feature_id = string_column(batch, "feature_id")?;
        let feature_version = string_column(batch, "feature_version")?;
        let entity_json = string_column(batch, "entity_json")?;
        let window_id = string_column(batch, "window_id")?;
        let resolution = uint64_column(batch, "resolution_ns")?;
        let datum_json = string_column(batch, "datum_json")?;
        let value_type = string_column(batch, "value_type")?;
        let event_start = int64_column(batch, "event_time_start_ns")?;
        let event_end = int64_column(batch, "event_time_end_ns")?;
        let as_known = int64_column(batch, "as_known_at_ns")?;
        let computed = int64_column(batch, "computed_at_ns")?;
        let watermark = int64_column(batch, "watermark_ns")?;
        let finality_known = int64_column(batch, "finality_as_known_at_ns")?;
        let finality = string_column(batch, "finality_state")?;
        let revision = uint32_column(batch, "revision")?;
        let source_coverage = string_column(batch, "source_coverage_json")?;
        let quality = uint32_column(batch, "quality_millionths")?;
        let normalization = string_column(batch, "normalization_version")?;
        let formula = string_column(batch, "formula_hash")?;
        let commit = string_column(batch, "code_commit")?;
        let lineage = string_column(batch, "lineage_hash")?;
        let float_bits = uint64_column(batch, "float_bits")?;
        for index in 0..batch.num_rows() {
            let row = MaterializedFeatureRow {
                effective_event_time_ns: required_i64(effective, index)?,
                exchange_event_time_ns: optional_i64(exchange, index),
                receive_wall_time_ns: required_i64(receive, index)?,
                event_type: required_string(event_type, index)?.to_owned(),
                venue: required_string(venue, index)?.to_owned(),
                instrument_generation: required_u32(generation, index)?,
                connection_epoch: required_u64(connection, index)?,
                subscription_epoch: required_u64(subscription, index)?,
                wal_record_sequence: required_u64(sequence, index)?,
                feature_id: required_string(feature_id, index)?.to_owned(),
                feature_version: required_string(feature_version, index)?.to_owned(),
                entity_json: required_string(entity_json, index)?.to_owned(),
                window_id: required_string(window_id, index)?.to_owned(),
                resolution_ns: required_u64(resolution, index)?,
                datum_json: required_string(datum_json, index)?.to_owned(),
                value_type: required_string(value_type, index)?.to_owned(),
                event_time_start_ns: required_i64(event_start, index)?,
                event_time_end_ns: required_i64(event_end, index)?,
                as_known_at_ns: required_i64(as_known, index)?,
                computed_at_ns: required_i64(computed, index)?,
                watermark_ns: optional_i64(watermark, index),
                finality_as_known_at_ns: required_i64(finality_known, index)?,
                finality_state: required_string(finality, index)?.to_owned(),
                revision: required_u32(revision, index)?,
                source_coverage_json: required_string(source_coverage, index)?.to_owned(),
                quality_millionths: required_u32(quality, index)?,
                normalization_version: required_string(normalization, index)?.to_owned(),
                formula_hash: required_string(formula, index)?.to_owned(),
                code_commit: required_string(commit, index)?.to_owned(),
                lineage_hash: required_string(lineage, index)?.to_owned(),
                float_bits: optional_u64(float_bits, index),
            };
            row.validate()?;
            rows.push(row);
        }
    }
    Ok(rows)
}

fn int64_column<'a>(
    batch: &'a RecordBatch,
    name: &'static str,
) -> Result<&'a Int64Array, MaterializationError> {
    batch
        .column_by_name(name)
        .and_then(|column| column.as_any().downcast_ref())
        .ok_or(MaterializationError::InvalidRow)
}

fn uint32_column<'a>(
    batch: &'a RecordBatch,
    name: &'static str,
) -> Result<&'a UInt32Array, MaterializationError> {
    batch
        .column_by_name(name)
        .and_then(|column| column.as_any().downcast_ref())
        .ok_or(MaterializationError::InvalidRow)
}

fn uint64_column<'a>(
    batch: &'a RecordBatch,
    name: &'static str,
) -> Result<&'a UInt64Array, MaterializationError> {
    batch
        .column_by_name(name)
        .and_then(|column| column.as_any().downcast_ref())
        .ok_or(MaterializationError::InvalidRow)
}

fn string_column<'a>(
    batch: &'a RecordBatch,
    name: &'static str,
) -> Result<&'a StringArray, MaterializationError> {
    batch
        .column_by_name(name)
        .and_then(|column| column.as_any().downcast_ref())
        .ok_or(MaterializationError::InvalidRow)
}

fn required_i64(array: &Int64Array, index: usize) -> Result<i64, MaterializationError> {
    (!array.is_null(index))
        .then(|| array.value(index))
        .ok_or(MaterializationError::InvalidRow)
}

fn required_u32(array: &UInt32Array, index: usize) -> Result<u32, MaterializationError> {
    (!array.is_null(index))
        .then(|| array.value(index))
        .ok_or(MaterializationError::InvalidRow)
}

fn required_u64(array: &UInt64Array, index: usize) -> Result<u64, MaterializationError> {
    (!array.is_null(index))
        .then(|| array.value(index))
        .ok_or(MaterializationError::InvalidRow)
}

fn required_string(array: &StringArray, index: usize) -> Result<&str, MaterializationError> {
    (!array.is_null(index))
        .then(|| array.value(index))
        .ok_or(MaterializationError::InvalidRow)
}

fn optional_i64(array: &Int64Array, index: usize) -> Option<i64> {
    (!array.is_null(index)).then(|| array.value(index))
}

fn optional_u64(array: &UInt64Array, index: usize) -> Option<u64> {
    (!array.is_null(index)).then(|| array.value(index))
}

fn value_type_name(value_type: FeatureValueType) -> &'static str {
    match value_type {
        FeatureValueType::FixedDecimal => "fixed_decimal",
        FeatureValueType::FixedDecimalMap => "fixed_decimal_map",
        FeatureValueType::Float64 => "float64",
        FeatureValueType::Integer => "integer",
        FeatureValueType::Boolean => "boolean",
    }
}

fn finality_name(finality: FinalityState) -> &'static str {
    match finality {
        FinalityState::Provisional => "provisional",
        FinalityState::Final => "final",
        FinalityState::Corrected => "corrected",
        FinalityState::Invalid => "invalid",
    }
}

fn valid_lower_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        && !value.bytes().all(|byte| byte == b'0')
}

fn valid_release_version(value: &str) -> bool {
    Version::parse(value).is_ok_and(|version| {
        version.major > 0 && version.pre.is_empty() && version.build.is_empty()
    })
}

fn valid_missingness_name(value: &str) -> bool {
    matches!(
        value,
        "not_listed"
            | "source_not_supported"
            | "source_disconnected"
            | "sequence_gap"
            | "stale"
            | "insufficient_history"
            | "window_not_final"
            | "below_liquidity_threshold"
            | "vendor_revision_pending"
            | "model_not_applicable"
            | "privacy_or_license_restriction"
            | "unknown"
    )
}

fn valid_sql_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn canonical_json(value: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(value)
        .ok()
        .and_then(|decoded| serde_json::to_string(&decoded).ok())
        .is_some_and(|encoded| encoded == value)
}

fn canonical_json_string(value: &impl Serialize) -> Result<String, MaterializationError> {
    let encoded = serde_json::to_value(value)?;
    Ok(serde_json::to_string(&encoded)?)
}

fn decode_fixed_hash<const N: usize>(value: &str) -> Result<[u8; N], MaterializationError> {
    let bytes = hex::decode(value).map_err(|_| MaterializationError::InvalidManifest)?;
    bytes
        .try_into()
        .map_err(|_| MaterializationError::InvalidManifest)
}

fn digest_json(domain: &[u8], value: &impl Serialize) -> Result<[u8; 32], MaterializationError> {
    let bytes = serde_json::to_vec(value)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain);
    hasher.update(&bytes);
    Ok(*hasher.finalize().as_bytes())
}

fn validate_private_root(root: &Path) -> Result<(), MaterializationError> {
    let metadata = fs::metadata(root)?;
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(MaterializationError::UnsafePath);
    }
    Ok(())
}

/// Fail-closed materialization failure.
#[derive(Debug, thiserror::Error)]
pub enum MaterializationError {
    #[error("feature materialization manifest is invalid")]
    InvalidManifest,
    #[error("feature materialization path is unsafe")]
    UnsafePath,
    #[error("feature materialization input exceeds its bound")]
    CapacityExceeded,
    #[error("feature materialization row is invalid")]
    InvalidRow,
    #[error("only final or corrected observations may be sealed")]
    UnfinalizedObservation,
    #[error("publishing a correction requires explicit base-manifest authority")]
    CorrectionAuthorityRequired,
    #[error("feature materialization contains a duplicate observation revision")]
    DuplicateObservation,
    #[error("feature correction history is not consecutive and immutable")]
    InvalidCorrectionHistory,
    #[error("feature observations do not belong to one UTC partition")]
    PartitionMismatch,
    #[error("materialization parity reports have different floating-point shapes")]
    ParityShapeMismatch,
    #[error("feature materialization integrity verification failed")]
    Integrity,
    #[error("point-in-time query plan is unsafe or incomplete")]
    InvalidQueryPlan,
    #[error("feature lineage failed: {0}")]
    Lineage(#[from] LineageError),
    #[error("feature registry validation failed: {0}")]
    Registry(#[from] feature_registry::RegistryError),
    #[error("feature identity validation failed: {0}")]
    Domain(#[from] domain::DomainError),
    #[error("feature decimal validation failed: {0}")]
    Decimal(#[from] fixed_decimal::DecimalError),
    #[error("Parquet feature store failed: {0}")]
    Store(#[from] parquet_store::StoreError),
    #[error("feature WAL failed: {0}")]
    WalSegment(#[from] raw_wal::segment::SegmentError),
    #[error("feature WAL recovery failed: {0}")]
    WalRecovery(#[from] raw_wal::RecoveryError),
    #[error("feature WAL prologue failed: {0}")]
    WalPrologue(#[from] raw_wal::prologue::PrologueError),
    #[error("feature Arrow conversion failed: {0}")]
    Arrow(#[from] arrow::error::ArrowError),
    #[error("feature JSON conversion failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("feature TOML conversion failed: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("feature materialization I/O failed: {0}")]
    Io(#[from] std::io::Error),
}
