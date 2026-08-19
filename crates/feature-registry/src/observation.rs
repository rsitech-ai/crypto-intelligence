//! Point-in-time feature observation contracts.

use std::cmp::Ordering;

use domain::{AssetId, InstrumentId, SourceId, UnixNanos, VenueId};
use fixed_decimal::FixedDecimal;
use quality::SourceHealthState;
use semver::Version;
use serde::{
    Deserialize, Deserializer, Serialize, Serializer, de::Error as _, ser::SerializeStruct as _,
};

use crate::{
    DurationNanos, FeatureId, FeatureValueType, FormulaHash, QualityScore, RegistryError, WindowId,
    definition::{encode_hex, ensure_nonzero_digest, validate_version},
};

const MAX_SOURCE_COVERAGE: usize = 64;
/// Closed entity identity over the frozen canonical domain identifiers.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "scope", content = "id", rename_all = "snake_case")]
pub enum FeatureEntity {
    Instrument(InstrumentId),
    Asset(AssetId),
    AssetPair(AssetId, AssetId),
    Venue(VenueId),
    Source(SourceId),
    Global,
}

/// A finite, canonical binary floating-point value.
#[derive(Clone, Copy, Debug)]
pub struct FiniteF64(f64);

impl FiniteF64 {
    pub fn new(value: f64) -> Result<Self, RegistryError> {
        if !value.is_finite() {
            return Err(RegistryError::NonFiniteValue);
        }
        Ok(Self(if value == 0.0 { 0.0 } else { value }))
    }

    pub const fn value(self) -> f64 {
        self.0
    }
}

impl PartialEq for FiniteF64 {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_bits() == other.0.to_bits()
    }
}

impl Eq for FiniteF64 {}

impl Serialize for FiniteF64 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_f64(self.0)
    }
}

impl<'de> Deserialize<'de> for FiniteF64 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(f64::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// Typed feature value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FeatureValue {
    FixedDecimal(FixedDecimal),
    Float64(FiniteF64),
    Integer(i64),
    Boolean(bool),
}

impl Serialize for FeatureValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::FixedDecimal(value) => value.serialize(serializer),
            Self::Float64(value) => value.serialize(serializer),
            Self::Integer(value) => value.serialize(serializer),
            Self::Boolean(value) => value.serialize(serializer),
        }
    }
}

impl FeatureValue {
    pub const fn value_type(&self) -> FeatureValueType {
        match self {
            Self::FixedDecimal(_) => FeatureValueType::FixedDecimal,
            Self::Float64(_) => FeatureValueType::Float64,
            Self::Integer(_) => FeatureValueType::Integer,
            Self::Boolean(_) => FeatureValueType::Boolean,
        }
    }
}

/// Exact semantic reason for an absent value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MissingnessReason {
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

/// A value is either present or explicitly missing, never both.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", content = "detail", rename_all = "snake_case")]
pub enum FeatureDatum {
    Present(FeatureValue),
    Missing(MissingnessReason),
}

impl FeatureDatum {
    pub const fn value(&self) -> Option<&FeatureValue> {
        match self {
            Self::Present(value) => Some(value),
            Self::Missing(_) => None,
        }
    }

    pub const fn missingness_reason(&self) -> Option<MissingnessReason> {
        match self {
            Self::Present(_) => None,
            Self::Missing(reason) => Some(*reason),
        }
    }
}

/// Publication state of a point-in-time observation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FinalityState {
    Provisional,
    Final,
    Corrected,
    Invalid,
}

/// Monotonic correction revision.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ObservationRevision(u32);

impl ObservationRevision {
    pub fn new(value: u32) -> Result<Self, RegistryError> {
        if value == 0 {
            Err(RegistryError::InvalidObservationRevision)
        } else {
            Ok(Self(value))
        }
    }

    pub const fn value(self) -> u32 {
        self.0
    }
}

/// Source identity and state contributing to an observation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SourceCoverageEntry {
    source: SourceId,
    health: SourceHealthState,
}

impl SourceCoverageEntry {
    pub const fn new(source: SourceId, health: SourceHealthState) -> Self {
        Self { source, health }
    }

    pub const fn source(&self) -> &SourceId {
        &self.source
    }

    pub const fn health(&self) -> SourceHealthState {
        self.health
    }
}

/// Canonically ordered expected and observed source coverage.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SourceCoverage {
    expected_sources: Vec<SourceId>,
    observed: Vec<SourceCoverageEntry>,
}

impl SourceCoverage {
    pub fn try_new(entries: Vec<SourceCoverageEntry>) -> Result<Self, RegistryError> {
        if entries.is_empty() || entries.len() > MAX_SOURCE_COVERAGE {
            return Err(RegistryError::InvalidSourceCoverage);
        }
        let expected_sources = entries.iter().map(|entry| entry.source.clone()).collect();
        Self::try_new_partial(expected_sources, entries)
    }

    pub fn try_new_partial(
        mut expected_sources: Vec<SourceId>,
        mut observed: Vec<SourceCoverageEntry>,
    ) -> Result<Self, RegistryError> {
        if expected_sources.is_empty() || expected_sources.len() > MAX_SOURCE_COVERAGE {
            return Err(RegistryError::InvalidSourceCoverage);
        }
        expected_sources.sort_by(compare_source);
        if expected_sources.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(RegistryError::DuplicateSource);
        }
        if observed.len() > MAX_SOURCE_COVERAGE {
            return Err(RegistryError::InvalidSourceCoverage);
        }
        observed.sort_by(compare_coverage_entry);
        if observed
            .windows(2)
            .any(|pair| pair[0].source == pair[1].source)
        {
            return Err(RegistryError::DuplicateSource);
        }
        if observed
            .iter()
            .any(|entry| !expected_sources.contains(&entry.source))
        {
            return Err(RegistryError::UnexpectedSource);
        }
        Ok(Self {
            expected_sources,
            observed,
        })
    }

    pub fn entries(&self) -> &[SourceCoverageEntry] {
        &self.observed
    }

    pub fn expected_sources(&self) -> &[SourceId] {
        &self.expected_sources
    }

    pub fn coverage_millionths(&self) -> u32 {
        let numerator = self.observed.len() * QualityScore::MAX_MILLIONTHS as usize;
        u32::try_from(numerator / self.expected_sources.len())
            .expect("bounded source coverage ratio fits in u32")
    }
}

/// Validated source commit identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct CodeRevision(String);

impl CodeRevision {
    pub fn new(value: impl Into<String>) -> Result<Self, RegistryError> {
        let value = value.into();
        if !matches!(value.len(), 40 | 64)
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || value.bytes().all(|byte| byte == b'0')
        {
            return Err(RegistryError::InvalidCodeRevision);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Nonzero digest of the observation lineage DAG.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LineageHash([u8; 32]);

impl LineageHash {
    pub fn new(value: [u8; 32]) -> Result<Self, RegistryError> {
        ensure_nonzero_digest(value, "lineage_hash").map(Self)
    }

    pub const fn bytes(self) -> [u8; 32] {
        self.0
    }
}

impl Serialize for LineageHash {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&encode_hex(&self.0))
    }
}

/// Construction input for a complete feature observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeatureObservationInput {
    pub feature_id: FeatureId,
    pub feature_version: Version,
    pub entity: FeatureEntity,
    pub window_id: WindowId,
    pub resolution: DurationNanos,
    pub datum: FeatureDatum,
    pub value_type: FeatureValueType,
    pub event_time_start: UnixNanos,
    pub event_time_end: UnixNanos,
    pub as_known_at: UnixNanos,
    pub computed_at: UnixNanos,
    pub watermark: Option<UnixNanos>,
    pub finality_as_known_at: UnixNanos,
    pub finality_state: FinalityState,
    pub revision: ObservationRevision,
    pub source_coverage: SourceCoverage,
    pub quality_score: QualityScore,
    pub normalization_version: Version,
    pub formula_hash: FormulaHash,
    pub code_commit: CodeRevision,
    pub lineage_hash: LineageHash,
}

/// Complete point-in-time observation for replay, training, and inference.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeatureObservation {
    feature_id: FeatureId,
    feature_version: Version,
    entity: FeatureEntity,
    window_id: WindowId,
    resolution: DurationNanos,
    datum: FeatureDatum,
    value_type: FeatureValueType,
    event_time_start: UnixNanos,
    event_time_end: UnixNanos,
    as_known_at: UnixNanos,
    computed_at: UnixNanos,
    watermark: Option<UnixNanos>,
    finality_as_known_at: UnixNanos,
    finality_state: FinalityState,
    revision: ObservationRevision,
    source_coverage: SourceCoverage,
    quality_score: QualityScore,
    normalization_version: Version,
    formula_hash: FormulaHash,
    code_commit: CodeRevision,
    lineage_hash: LineageHash,
}

impl FeatureObservation {
    pub fn try_new(input: FeatureObservationInput) -> Result<Self, RegistryError> {
        validate_version(&input.feature_version, "feature_version")?;
        validate_version(&input.normalization_version, "normalization_version")?;
        validate_times(&input)?;
        if input.finality_state == FinalityState::Corrected && input.revision.value() == 1 {
            return Err(RegistryError::InvalidCorrectionRevision);
        }
        if let FeatureDatum::Present(value) = &input.datum
            && value.value_type() != input.value_type
        {
            return Err(RegistryError::ValueTypeMismatch);
        }
        if input.finality_state == FinalityState::Invalid
            && matches!(input.datum, FeatureDatum::Present(_))
        {
            return Err(RegistryError::InvalidObservationMustBeMissing);
        }
        if input.watermark.is_none()
            && (input.finality_state != FinalityState::Invalid
                || !matches!(input.datum, FeatureDatum::Missing(_)))
        {
            return Err(RegistryError::InvalidTimeOrdering);
        }
        if let FeatureEntity::AssetPair(left, right) = &input.entity
            && (left == right || compare_asset(left, right) != Ordering::Less)
        {
            return Err(RegistryError::InvalidObservationEntity);
        }

        Ok(Self {
            feature_id: input.feature_id,
            feature_version: input.feature_version,
            entity: input.entity,
            window_id: input.window_id,
            resolution: input.resolution,
            datum: input.datum,
            value_type: input.value_type,
            event_time_start: input.event_time_start,
            event_time_end: input.event_time_end,
            as_known_at: input.as_known_at,
            computed_at: input.computed_at,
            watermark: input.watermark,
            finality_as_known_at: input.finality_as_known_at,
            finality_state: input.finality_state,
            revision: input.revision,
            source_coverage: input.source_coverage,
            quality_score: input.quality_score,
            normalization_version: input.normalization_version,
            formula_hash: input.formula_hash,
            code_commit: input.code_commit,
            lineage_hash: input.lineage_hash,
        })
    }

    pub fn into_input(self) -> FeatureObservationInput {
        FeatureObservationInput {
            feature_id: self.feature_id,
            feature_version: self.feature_version,
            entity: self.entity,
            window_id: self.window_id,
            resolution: self.resolution,
            datum: self.datum,
            value_type: self.value_type,
            event_time_start: self.event_time_start,
            event_time_end: self.event_time_end,
            as_known_at: self.as_known_at,
            computed_at: self.computed_at,
            watermark: self.watermark,
            finality_as_known_at: self.finality_as_known_at,
            finality_state: self.finality_state,
            revision: self.revision,
            source_coverage: self.source_coverage,
            quality_score: self.quality_score,
            normalization_version: self.normalization_version,
            formula_hash: self.formula_hash,
            code_commit: self.code_commit,
            lineage_hash: self.lineage_hash,
        }
    }

    pub const fn feature_id(&self) -> &FeatureId {
        &self.feature_id
    }

    pub const fn feature_version(&self) -> &Version {
        &self.feature_version
    }

    pub const fn entity(&self) -> &FeatureEntity {
        &self.entity
    }

    pub const fn window_id(&self) -> &WindowId {
        &self.window_id
    }

    pub const fn resolution(&self) -> DurationNanos {
        self.resolution
    }

    pub const fn datum(&self) -> &FeatureDatum {
        &self.datum
    }

    pub const fn value_type(&self) -> FeatureValueType {
        self.value_type
    }

    pub const fn event_time_end(&self) -> UnixNanos {
        self.event_time_end
    }

    pub const fn event_time_start(&self) -> UnixNanos {
        self.event_time_start
    }

    pub const fn as_known_at(&self) -> UnixNanos {
        self.as_known_at
    }

    pub const fn computed_at(&self) -> UnixNanos {
        self.computed_at
    }

    pub const fn watermark(&self) -> Option<UnixNanos> {
        self.watermark
    }

    pub const fn finality_as_known_at(&self) -> UnixNanos {
        self.finality_as_known_at
    }

    pub const fn finality_state(&self) -> FinalityState {
        self.finality_state
    }

    pub const fn source_coverage(&self) -> &SourceCoverage {
        &self.source_coverage
    }

    pub const fn quality_score(&self) -> QualityScore {
        self.quality_score
    }

    pub const fn normalization_version(&self) -> &Version {
        &self.normalization_version
    }

    pub const fn formula_hash(&self) -> FormulaHash {
        self.formula_hash
    }

    pub const fn revision(&self) -> ObservationRevision {
        self.revision
    }

    pub const fn code_commit(&self) -> &CodeRevision {
        &self.code_commit
    }

    pub const fn lineage_hash(&self) -> LineageHash {
        self.lineage_hash
    }
}

impl Serialize for FeatureObservation {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("FeatureObservation", 22)?;
        state.serialize_field("feature_id", &self.feature_id)?;
        state.serialize_field("feature_version", &self.feature_version)?;
        state.serialize_field("entity_id", &self.entity)?;
        state.serialize_field("window_id", &self.window_id)?;
        state.serialize_field("resolution", &self.resolution)?;
        state.serialize_field("value", &self.datum.value())?;
        state.serialize_field("value_type", &self.value_type)?;
        state.serialize_field("event_time_start", &self.event_time_start)?;
        state.serialize_field("event_time_end", &self.event_time_end)?;
        state.serialize_field("as_known_at", &self.as_known_at)?;
        state.serialize_field("computed_at", &self.computed_at)?;
        state.serialize_field("watermark", &self.watermark)?;
        state.serialize_field("finality_as_known_at", &self.finality_as_known_at)?;
        state.serialize_field("finality_state", &self.finality_state)?;
        state.serialize_field("revision", &self.revision)?;
        state.serialize_field("source_coverage", &self.source_coverage)?;
        state.serialize_field("quality_score", &self.quality_score)?;
        state.serialize_field("missingness_reason", &self.datum.missingness_reason())?;
        state.serialize_field("normalization_version", &self.normalization_version)?;
        state.serialize_field("formula_hash", &self.formula_hash)?;
        state.serialize_field("code_commit", &self.code_commit)?;
        state.serialize_field("lineage_hash", &self.lineage_hash)?;
        state.end()
    }
}

fn validate_times(input: &FeatureObservationInput) -> Result<(), RegistryError> {
    let start = input.event_time_start.value();
    let end = input.event_time_end.value();
    let known = input.as_known_at.value();
    let computed = input.computed_at.value();
    let watermark_invalid = input
        .watermark
        .is_some_and(|watermark| watermark.value() <= 0 || watermark.value() > computed);
    let finality_availability_invalid = input.finality_as_known_at.value() <= 0
        || input.finality_as_known_at > input.as_known_at
        || input.finality_as_known_at > input.computed_at
        || input
            .watermark
            .is_some_and(|watermark| watermark > input.finality_as_known_at);
    if start <= 0
        || start > end
        || end > known
        || known > computed
        || watermark_invalid
        || finality_availability_invalid
        || input.resolution.is_zero()
        || input.resolution.value() > i64::MAX as u64
    {
        Err(RegistryError::InvalidTimeOrdering)
    } else {
        Ok(())
    }
}

fn compare_coverage_entry(left: &SourceCoverageEntry, right: &SourceCoverageEntry) -> Ordering {
    compare_source(&left.source, &right.source)
}

fn compare_source(left: &SourceId, right: &SourceId) -> Ordering {
    (left.kind() as u8)
        .cmp(&(right.kind() as u8))
        .then_with(|| left.name().cmp(right.name()))
        .then_with(|| left.generation().cmp(&right.generation()))
}

fn compare_asset(left: &AssetId, right: &AssetId) -> Ordering {
    (left.namespace() as u8)
        .cmp(&(right.namespace() as u8))
        .then_with(|| left.chain_id().cmp(right.chain_id()))
        .then_with(|| left.contract_or_mint().cmp(right.contract_or_mint()))
        .then_with(|| left.canonical_symbol().cmp(right.canonical_symbol()))
        .then_with(|| left.generation().cmp(&right.generation()))
}
