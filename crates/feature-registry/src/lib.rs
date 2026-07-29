//! Deterministic, versioned point-in-time feature contracts.

mod definition;
mod observation;

use std::collections::BTreeMap;

use semver::Version;
use serde::Serialize;
use thiserror::Error;

pub use definition::{
    DurationNanos, EntityScope, EventTimePolicy, FeatureDefinition, FeatureDefinitionInput,
    FeatureDocumentation, FeatureId, FeatureStatus, FeatureValueType, FormulaHash,
    InputRequirement, MissingnessPolicy, NormalizationKind, NormalizationPolicy,
    QualityRequirement, QualityScore, WindowDefinition, WindowId, WindowKind, WindowParameter,
};
pub use observation::{
    CodeRevision, FeatureDatum, FeatureEntity, FeatureObservation, FeatureObservationInput,
    FeatureValue, FinalityState, FiniteF64, LineageHash, MissingnessReason, ObservationRevision,
    SourceCoverage, SourceCoverageEntry,
};

const MAX_REGISTRY_DEFINITIONS: usize = 4_096;
const SNAPSHOT_SCHEMA_VERSION: u32 = 1;
const SNAPSHOT_HASH_DOMAIN: &[u8] = b"crypto-intelligence/feature-registry-snapshot/v1";

/// Fail-closed feature-contract errors.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum RegistryError {
    #[error("invalid identifier for {field}")]
    InvalidIdentifier { field: &'static str },
    #[error("invalid feature documentation reference")]
    InvalidDocumentation,
    #[error("feature documentation anchor does not match its feature ID")]
    DocumentationFeatureMismatch,
    #[error("invalid source code revision")]
    InvalidCodeRevision,
    #[error("invalid or unbounded semantic version for {field}")]
    InvalidVersion { field: &'static str },
    #[error("{field} must be a nonzero digest")]
    ZeroDigest { field: &'static str },
    #[error("invalid definition field {field}")]
    InvalidDefinition { field: &'static str },
    #[error("feature definition contains a duplicate input")]
    DuplicateInput,
    #[error("feature definition contains a duplicate window")]
    DuplicateWindow,
    #[error("invalid window declaration")]
    InvalidWindow,
    #[error("quality score must be in inclusive range 0..=1_000_000")]
    InvalidQualityScore,
    #[error("quality gate must allow at least one unique source state")]
    InvalidQualityGate,
    #[error("feature definition ID and version already exist")]
    DuplicateDefinition,
    #[error("feature registry has reached its bounded capacity")]
    RegistryCapacityExceeded,
    #[error("feature registry capacity must be in inclusive range 1..=4_096")]
    InvalidRegistryCapacity,
    #[error("feature registry snapshot could not be serialized")]
    SnapshotSerialization,
    #[error("feature observation has invalid point-in-time ordering")]
    InvalidTimeOrdering,
    #[error("feature observation correction revision must be greater than one")]
    InvalidCorrectionRevision,
    #[error("feature observation revision must be nonzero")]
    InvalidObservationRevision,
    #[error("feature value is not finite")]
    NonFiniteValue,
    #[error("feature value type does not match its declared type")]
    ValueTypeMismatch,
    #[error("invalid feature observation must carry explicit missingness")]
    InvalidObservationMustBeMissing,
    #[error("source coverage is empty or exceeds its bound")]
    InvalidSourceCoverage,
    #[error("source coverage contains a duplicate source")]
    DuplicateSource,
    #[error("observed source is absent from expected source coverage")]
    UnexpectedSource,
    #[error("feature observation has no registered definition")]
    UnknownDefinition,
    #[error("feature observation entity does not match its definition")]
    EntityScopeMismatch,
    #[error("feature observation entity identity is invalid")]
    InvalidObservationEntity,
    #[error("feature observation window does not match its definition")]
    WindowMismatch,
    #[error("feature observation resolution does not match its definition")]
    ResolutionMismatch,
    #[error("feature observation normalization version does not match its definition")]
    NormalizationVersionMismatch,
    #[error("feature observation formula hash does not match its definition")]
    FormulaHashMismatch,
    #[error("feature observation quality is below its definition's gate")]
    QualityBelowRequirement,
    #[error("feature observation source coverage is below its definition's gate")]
    CoverageBelowRequirement,
    #[error("feature observation contains a source state rejected by its definition")]
    SourceHealthRejected,
    #[error("final or corrected observation watermark has not passed allowed lateness")]
    FinalityBeforeWatermark,
    #[error("feature observation predates its finalization evidence")]
    FinalityKnowledgeMismatch,
}

/// Bounded registry keyed by exact feature identity and semantic version.
#[derive(Clone, Debug)]
pub struct FeatureRegistry {
    definitions: BTreeMap<FeatureId, BTreeMap<Version, FeatureDefinition>>,
    definition_count: usize,
    capacity: usize,
}

impl Default for FeatureRegistry {
    fn default() -> Self {
        Self {
            definitions: BTreeMap::new(),
            definition_count: 0,
            capacity: MAX_REGISTRY_DEFINITIONS,
        }
    }
}

impl FeatureRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(capacity: usize) -> Result<Self, RegistryError> {
        if capacity == 0 || capacity > MAX_REGISTRY_DEFINITIONS {
            return Err(RegistryError::InvalidRegistryCapacity);
        }
        Ok(Self {
            definitions: BTreeMap::new(),
            definition_count: 0,
            capacity,
        })
    }

    pub fn len(&self) -> usize {
        self.definition_count
    }

    pub fn is_empty(&self) -> bool {
        self.definition_count == 0
    }

    pub fn contains(&self, id: &FeatureId, version: &Version) -> bool {
        if definition::validate_version(version, "feature_version").is_err() {
            return false;
        }
        self.definitions
            .get(id)
            .is_some_and(|versions| versions.contains_key(version))
    }

    pub fn get(&self, id: &FeatureId, version: &Version) -> Option<&FeatureDefinition> {
        definition::validate_version(version, "feature_version")
            .ok()
            .and_then(|()| self.definitions.get(id))
            .and_then(|versions| versions.get(version))
    }

    pub fn register(&mut self, definition: FeatureDefinition) -> Result<(), RegistryError> {
        if self.contains(definition.id(), definition.version()) {
            return Err(RegistryError::DuplicateDefinition);
        }
        if self.definition_count == self.capacity {
            return Err(RegistryError::RegistryCapacityExceeded);
        }
        let id = definition.id().clone();
        let version = definition.version().clone();
        self.definitions
            .entry(id)
            .or_default()
            .insert(version, definition);
        self.definition_count += 1;
        Ok(())
    }

    pub fn validate_observation(
        &self,
        observation: &FeatureObservation,
    ) -> Result<(), RegistryError> {
        let definition = self
            .get(observation.feature_id(), observation.feature_version())
            .ok_or(RegistryError::UnknownDefinition)?;
        if !entity_matches(definition.entities(), observation.entity()) {
            return Err(RegistryError::EntityScopeMismatch);
        }
        let Some(window) = definition.window(observation.window_id()) else {
            return Err(RegistryError::WindowMismatch);
        };
        if !window_geometry_matches(window, definition.output_resolution(), observation) {
            return Err(RegistryError::WindowMismatch);
        }
        if definition.output_resolution() != observation.resolution() {
            return Err(RegistryError::ResolutionMismatch);
        }
        if definition.value_type() != observation.value_type() {
            return Err(RegistryError::ValueTypeMismatch);
        }
        if let FeatureDatum::Present(value) = observation.datum()
            && value.value_type() != definition.value_type()
        {
            return Err(RegistryError::ValueTypeMismatch);
        }
        if definition.normalization().version() != observation.normalization_version() {
            return Err(RegistryError::NormalizationVersionMismatch);
        }
        if definition.formula_hash() != observation.formula_hash() {
            return Err(RegistryError::FormulaHashMismatch);
        }

        if observation.finality_state() != FinalityState::Invalid {
            if observation.quality_score() < definition.quality_gate().minimum_score() {
                return Err(RegistryError::QualityBelowRequirement);
            }
            let coverage =
                QualityScore::from_millionths(observation.source_coverage().coverage_millionths())
                    .expect("source coverage constructs an in-range ratio");
            if coverage < definition.quality_gate().minimum_coverage() {
                return Err(RegistryError::CoverageBelowRequirement);
            }
            if observation
                .source_coverage()
                .entries()
                .iter()
                .any(|entry| !definition.quality_gate().allows(entry.health()))
            {
                return Err(RegistryError::SourceHealthRejected);
            }
        }

        if matches!(
            observation.finality_state(),
            FinalityState::Final | FinalityState::Corrected
        ) && observation.finality_as_known_at() > observation.as_known_at()
        {
            return Err(RegistryError::FinalityKnowledgeMismatch);
        }
        if matches!(
            observation.finality_state(),
            FinalityState::Final | FinalityState::Corrected
        ) {
            let lateness = i64::try_from(definition.allowed_lateness().value()).map_err(|_| {
                RegistryError::InvalidDefinition {
                    field: "allowed_lateness",
                }
            })?;
            let final_watermark = observation
                .event_time_end()
                .value()
                .checked_add(lateness)
                .ok_or(RegistryError::FinalityBeforeWatermark)?;
            if observation
                .watermark()
                .is_none_or(|watermark| watermark.value() < final_watermark)
            {
                return Err(RegistryError::FinalityBeforeWatermark);
            }
        }
        Ok(())
    }

    /// Stable JSON snapshot in canonical registry-key order.
    pub fn canonical_snapshot(&self) -> Result<Vec<u8>, RegistryError> {
        #[derive(Serialize)]
        struct Snapshot<'a> {
            schema_version: u32,
            definitions: Vec<&'a FeatureDefinition>,
        }

        serde_json::to_vec(&Snapshot {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            definitions: self
                .definitions
                .values()
                .flat_map(BTreeMap::values)
                .collect(),
        })
        .map_err(|_| RegistryError::SnapshotSerialization)
    }

    /// Domain-separated BLAKE3 identity of the canonical snapshot.
    pub fn snapshot_digest(&self) -> Result<[u8; 32], RegistryError> {
        let snapshot = self.canonical_snapshot()?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(SNAPSHOT_HASH_DOMAIN);
        hasher.update(
            &u64::try_from(snapshot.len())
                .map_err(|_| RegistryError::SnapshotSerialization)?
                .to_le_bytes(),
        );
        hasher.update(&snapshot);
        Ok(*hasher.finalize().as_bytes())
    }
}

fn window_geometry_matches(
    window: &WindowDefinition,
    resolution: DurationNanos,
    observation: &FeatureObservation,
) -> bool {
    let start = observation.event_time_start().value();
    let end = observation.event_time_end().value();
    let Some(extent) = end
        .checked_sub(start)
        .and_then(|value| u64::try_from(value).ok())
    else {
        return false;
    };
    match window.parameter() {
        WindowParameter::Time {
            extent: declared,
            advance,
        } => {
            extent == declared.value()
                && match (window.kind(), advance) {
                    (WindowKind::Sliding, Some(advance)) => i64::try_from(advance.value())
                        .is_ok_and(|advance| start.rem_euclid(advance) == 0),
                    (WindowKind::Tumbling, None) => i64::try_from(declared.value())
                        .is_ok_and(|declared| start.rem_euclid(declared) == 0),
                    _ => false,
                }
        }
        WindowParameter::SessionAligned {
            extent: declared,
            utc_anchor,
        } => {
            extent == declared.value()
                && i64::try_from(declared.value()).is_ok_and(|declared| {
                    start
                        .checked_sub(utc_anchor.value())
                        .is_some_and(|delta| delta.rem_euclid(declared) == 0)
                })
        }
        WindowParameter::ExponentiallyWeighted { lookback, .. } => {
            extent == lookback.value()
                && i64::try_from(resolution.value())
                    .is_ok_and(|resolution| start.rem_euclid(resolution) == 0)
        }
        WindowParameter::EventCount { .. } | WindowParameter::Threshold { .. } => true,
    }
}

fn entity_matches(scope: EntityScope, entity: &FeatureEntity) -> bool {
    matches!(
        (scope, entity),
        (EntityScope::Instrument, FeatureEntity::Instrument(_))
            | (EntityScope::Asset, FeatureEntity::Asset(_))
            | (EntityScope::AssetPair, FeatureEntity::AssetPair(_, _))
            | (EntityScope::Venue, FeatureEntity::Venue(_))
            | (EntityScope::Source, FeatureEntity::Source(_))
            | (EntityScope::Global, FeatureEntity::Global)
    )
}
