//! Evidence-bound applicability, out-of-distribution, and abstention policy.

mod mahalanobis;
mod novelty;
mod policy;

pub use mahalanobis::RobustDistribution;
pub use novelty::{CategoricalSupport, FeatureRange, NoveltyDiagnostics, NoveltyProfile};
pub use policy::{
    ApplicabilityPolicy, ApplicabilityThresholds, Availability, PolicyDecision, PolicySignals,
};

use calibration::{CalibratedOutput, CalibrationArtifact, CalibrationStatus};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use thiserror::Error;

use novelty::identifier_is_valid;

const MODEL_DOMAIN: &[u8] = b"cmti:applicability-model:v1\0";
const TRAINING_DOMAIN: &[u8] = b"cmti:applicability-training:v1\0";
const CONTEXT_DOMAIN: &[u8] = b"cmti:applicability-context:v1\0";
const DECISION_DOMAIN: &[u8] = b"cmti:applicability-decision:v1\0";
const MAXIMUM_TRAINING_ROWS: usize = 4_096;
const MAXIMUM_SOURCES: usize = 64;
const MAXIMUM_CATEGORIES: usize = 256;
const MAXIMUM_FEATURES: usize = 256;
const MAXIMUM_REASONS: usize = 1_024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrainingObservation {
    id: String,
    observed_at_ns: i64,
    as_known_at_ns: i64,
    features: Vec<f64>,
    evidence_hash: [u8; 32],
}

impl TrainingObservation {
    pub fn try_new(
        id: impl Into<String>,
        observed_at_ns: i64,
        as_known_at_ns: i64,
        features: Vec<f64>,
        evidence_hash: [u8; 32],
    ) -> Result<Self, ApplicabilityError> {
        let value = Self {
            id: id.into(),
            observed_at_ns,
            as_known_at_ns,
            features,
            evidence_hash,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), ApplicabilityError> {
        if identifier_is_valid(&self.id)
            && self.observed_at_ns > 0
            && self.as_known_at_ns >= self.observed_at_ns
            && !self.features.is_empty()
            && self.features.iter().all(|value| value.is_finite())
            && self.evidence_hash != [0; 32]
        {
            Ok(())
        } else {
            Err(ApplicabilityError::InvalidTrainingObservation)
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRequirement {
    id: String,
    required: bool,
}

impl SourceRequirement {
    pub fn try_new(id: impl Into<String>, required: bool) -> Result<Self, ApplicabilityError> {
        let value = Self {
            id: id.into(),
            required,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), ApplicabilityError> {
        if identifier_is_valid(&self.id) {
            Ok(())
        } else {
            Err(ApplicabilityError::InvalidSourceRequirements)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicabilityModel {
    schema_version: u32,
    calibration_artifact_id: [u8; 32],
    calibration_key_hash: [u8; 32],
    raw_model_hash: [u8; 32],
    raw_model_training_hash: [u8; 32],
    outer_fold_hash: [u8; 32],
    feature_schema_hash: [u8; 32],
    trained_through_ns: i64,
    model_fitted_at_ns: i64,
    training_evidence_hash: [u8; 32],
    distribution: RobustDistribution,
    novelty: NoveltyProfile,
    source_requirements: Vec<SourceRequirement>,
    policy: ApplicabilityPolicy,
    artifact_id: [u8; 32],
}

impl ApplicabilityModel {
    pub fn fit_for_calibration(
        calibration: &CalibrationArtifact,
        novelty: NoveltyProfile,
        mut source_requirements: Vec<SourceRequirement>,
        mut observations: Vec<TrainingObservation>,
        thresholds: ApplicabilityThresholds,
        covariance_shrinkage: f64,
    ) -> Result<Self, ApplicabilityError> {
        calibration
            .registry_descriptor()
            .map_err(|_| ApplicabilityError::IncompatibleCalibration)?;
        novelty.validate()?;
        ApplicabilityPolicy::try_new(thresholds)?;
        source_requirements.sort_by(|left, right| left.id.cmp(&right.id));
        if source_requirements.is_empty()
            || source_requirements.len() > MAXIMUM_SOURCES
            || source_requirements
                .iter()
                .any(|value| value.validate().is_err())
            || source_requirements
                .windows(2)
                .any(|pair| pair[0].id >= pair[1].id)
            || !source_requirements.iter().any(|value| value.required)
        {
            return Err(ApplicabilityError::InvalidSourceRequirements);
        }
        if !(3..=MAXIMUM_TRAINING_ROWS).contains(&observations.len()) {
            return Err(ApplicabilityError::InvalidTrainingDistribution);
        }
        observations.sort_by(|left, right| {
            left.observed_at_ns
                .cmp(&right.observed_at_ns)
                .then_with(|| left.id.cmp(&right.id))
        });
        let unique_ids: BTreeSet<&str> = observations.iter().map(|row| row.id.as_str()).collect();
        if observations.iter().any(|row| {
            row.validate().is_err()
                || row.features.len() != novelty.features().len()
                || row.as_known_at_ns > calibration.lineage().training_period().end_ns
        }) || unique_ids.len() != observations.len()
        {
            return Err(ApplicabilityError::InvalidTrainingObservation);
        }
        let training_evidence_hash = hash_training(&observations);
        let rows: Vec<Vec<f64>> = observations
            .iter()
            .map(|row| row.features.clone())
            .collect();
        let distribution = RobustDistribution::fit(&rows, covariance_shrinkage)?;
        let policy = ApplicabilityPolicy::try_new(thresholds)?;
        let mut model = Self {
            schema_version: 1,
            calibration_artifact_id: calibration.artifact_id(),
            calibration_key_hash: calibration.key().evidence_hash(),
            raw_model_hash: calibration.key().raw_model_hash(),
            raw_model_training_hash: calibration.key().raw_model_training_hash(),
            outer_fold_hash: calibration.lineage().outer_fold_hash(),
            feature_schema_hash: novelty.evidence_hash(),
            trained_through_ns: calibration.lineage().training_period().end_ns,
            model_fitted_at_ns: calibration.lineage().model_fitted_at_ns(),
            training_evidence_hash,
            distribution,
            novelty,
            source_requirements,
            policy,
            artifact_id: [0; 32],
        };
        model.artifact_id = model.calculate_hash();
        model.validate()?;
        Ok(model)
    }

    pub const fn artifact_id(&self) -> [u8; 32] {
        self.artifact_id
    }

    pub const fn feature_schema_hash(&self) -> [u8; 32] {
        self.feature_schema_hash
    }

    pub const fn training_evidence_hash(&self) -> [u8; 32] {
        self.training_evidence_hash
    }

    pub const fn policy(&self) -> ApplicabilityPolicy {
        self.policy
    }

    pub fn distribution(&self) -> &RobustDistribution {
        &self.distribution
    }

    pub fn novelty_profile(&self) -> &NoveltyProfile {
        &self.novelty
    }

    pub fn evaluate(
        &self,
        forecast: &VerifiedCalibratedInput,
        context: &RuntimeContext,
    ) -> Result<ApplicabilityDecision, ApplicabilityError> {
        let decision = self.evaluate_unchecked(forecast, context)?;
        decision.validate_identity(self, forecast, context)?;
        Ok(decision)
    }

    fn evaluate_unchecked(
        &self,
        forecast: &VerifiedCalibratedInput,
        context: &RuntimeContext,
    ) -> Result<ApplicabilityDecision, ApplicabilityError> {
        self.validate()?;
        context.validate()?;
        forecast.validate()?;

        let model_incompatible = forecast.calibration_artifact_id != self.calibration_artifact_id
            || forecast.calibration_key_hash != self.calibration_key_hash
            || forecast.raw_model_hash != self.raw_model_hash
            || forecast.raw_model_training_hash != self.raw_model_training_hash
            || forecast.outer_fold_hash != self.outer_fold_hash
            || context.feature_schema_hash != self.feature_schema_hash
            || context.input_evidence_hash != forecast.output.input_evidence_hash;

        let category_refs: Vec<(&str, &str)> = context
            .categories
            .iter()
            .map(|value| (value.field.as_str(), value.value.as_str()))
            .collect();
        let novelty = self.novelty.evaluate(
            &context.features,
            &category_refs,
            &context.venue,
            &context.product,
        )?;
        let imputed: Vec<f64> = context
            .features
            .iter()
            .zip(self.distribution.center())
            .map(|(value, center)| value.unwrap_or(*center))
            .collect();
        let (mahalanobis_squared, nearest_neighbor_distance) =
            self.distribution.distances_validated(&imputed)?;
        let thresholds = self.policy.thresholds();
        let model_age_ns = context
            .evaluated_at_ns
            .checked_sub(self.model_fitted_at_ns)
            .ok_or(ApplicabilityError::InvalidRuntimeContext)?;
        if model_age_ns < 0 {
            return Err(ApplicabilityError::InvalidRuntimeContext);
        }

        let required_source_unhealthy = self.source_requirements.iter().any(|requirement| {
            requirement.required
                && context
                    .sources
                    .iter()
                    .find(|source| source.id == requirement.id)
                    .is_none_or(|source| source.health == SourceHealth::Unhealthy)
        });
        let degraded_source = self.source_requirements.iter().any(|requirement| {
            context
                .sources
                .iter()
                .find(|source| source.id == requirement.id)
                .map_or(!requirement.required, |source| {
                    source.health == SourceHealth::Degraded
                        || (!requirement.required && source.health == SourceHealth::Unhealthy)
                })
        });
        let residual = context.residual.map(|value| value.absolute_standardized);
        let drift = context.coefficient_drift.map(|value| value.relative_l2);
        let quality_unhealthy = context.quality.overall < thresholds.quality_degraded
            || context.quality.minimum_critical < thresholds.critical_quality_degraded;
        let quality_degraded = context.quality.overall < thresholds.quality_available
            || context.quality.minimum_critical < thresholds.critical_quality_available;
        let signals = PolicySignals {
            model_incompatible: model_incompatible
                || model_age_ns > thresholds.model_age_incompatible_ns
                || drift.is_some_and(|value| value > thresholds.coefficient_drift_incompatible),
            source_unhealthy: required_source_unhealthy || quality_unhealthy,
            insufficient_data: novelty.required_missing,
            out_of_distribution: novelty.out_of_range
                || novelty.categorical_novelty
                || novelty.unsupported_venue
                || novelty.unsupported_product
                || mahalanobis_squared > thresholds.mahalanobis_ood
                || nearest_neighbor_distance > thresholds.nearest_neighbor_ood
                || residual.is_some_and(|value| value > thresholds.residual_ood),
            experimental: forecast.output.status == CalibrationStatus::Experimental
                || model_age_ns > thresholds.model_age_experimental_ns
                || drift.is_none()
                || drift.is_some_and(|value| value > thresholds.coefficient_drift_experimental)
                || residual.is_none()
                || residual.is_some_and(|value| value > thresholds.residual_experimental),
            degraded: novelty.optional_missing
                || degraded_source
                || quality_degraded
                || mahalanobis_squared > thresholds.mahalanobis_degraded
                || nearest_neighbor_distance > thresholds.nearest_neighbor_degraded,
        };
        let policy_decision = self.policy.decide(signals);
        let mut reasons = novelty.reasons.clone();
        append_context_reasons(
            &mut reasons,
            signals,
            context,
            model_age_ns,
            mahalanobis_squared,
            nearest_neighbor_distance,
            thresholds,
        );
        reasons.sort();
        reasons.dedup();

        let score = applicability_score(
            mahalanobis_squared,
            nearest_neighbor_distance,
            residual,
            thresholds,
        )?;
        let mut decision = ApplicabilityDecision {
            schema_version: 1,
            applicability_model_id: self.artifact_id,
            calibration_artifact_id: forecast.calibration_artifact_id,
            calibration_output_evidence_hash: forecast.output.evidence_hash,
            input_evidence_hash: forecast.output.input_evidence_hash,
            feature_schema_hash: context.feature_schema_hash,
            source_health_evidence_hash: context.source_health_evidence_hash(),
            quality_evidence_hash: context.quality.evidence_hash,
            horizon_seconds: forecast.output.key.horizon_seconds(),
            calibrated_probability: forecast.output.calibrated_probability,
            availability: policy_decision.availability,
            applicability_score: score,
            diagnostics: ApplicabilityDiagnostics {
                mahalanobis_squared,
                nearest_neighbor_distance,
                model_age_ns,
                coefficient_drift: drift,
                absolute_standardized_residual: residual,
                quality_overall: context.quality.overall,
                minimum_critical_quality: context.quality.minimum_critical,
                novelty,
            },
            reasons,
            evaluated_at_ns: context.evaluated_at_ns,
            context_evidence_hash: context.evidence_hash,
            evidence_hash: [0; 32],
        };
        decision.evidence_hash = decision.calculate_hash();
        Ok(decision)
    }

    pub fn validate(&self) -> Result<(), ApplicabilityError> {
        if self.schema_version != 1
            || self.calibration_artifact_id == [0; 32]
            || self.calibration_key_hash == [0; 32]
            || self.raw_model_hash == [0; 32]
            || self.raw_model_training_hash == [0; 32]
            || self.outer_fold_hash == [0; 32]
            || self.feature_schema_hash != self.novelty.evidence_hash()
            || self.training_evidence_hash == [0; 32]
            || self.trained_through_ns <= 0
            || self.model_fitted_at_ns < self.trained_through_ns
            || self.distribution.feature_count() != self.novelty.features().len()
            || self.source_requirements.is_empty()
            || self.source_requirements.len() > MAXIMUM_SOURCES
            || !self.source_requirements.iter().any(|value| value.required)
            || self
                .source_requirements
                .iter()
                .any(|value| value.validate().is_err())
            || self
                .source_requirements
                .windows(2)
                .any(|pair| pair[0].id >= pair[1].id)
            || self.distribution.validate().is_err()
            || self.novelty.validate().is_err()
            || self.policy.thresholds().validate().is_err()
            || self.artifact_id != self.calculate_hash()
        {
            Err(ApplicabilityError::InvalidArtifact)
        } else {
            Ok(())
        }
    }

    fn calculate_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(MODEL_DOMAIN);
        hash_u32(&mut hasher, self.schema_version);
        for value in [
            self.calibration_artifact_id,
            self.calibration_key_hash,
            self.raw_model_hash,
            self.raw_model_training_hash,
            self.outer_fold_hash,
            self.feature_schema_hash,
            self.training_evidence_hash,
            self.distribution.evidence_hash(),
            self.novelty.evidence_hash(),
        ] {
            hasher.update(&value);
        }
        hash_i64(&mut hasher, self.trained_through_ns);
        hash_i64(&mut hasher, self.model_fitted_at_ns);
        hash_u64(&mut hasher, self.source_requirements.len() as u64);
        for source in &self.source_requirements {
            hash_string(&mut hasher, &source.id);
            hasher.update(&[u8::from(source.required)]);
        }
        hash_thresholds(&mut hasher, self.policy.thresholds());
        *hasher.finalize().as_bytes()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct VerifiedCalibratedInput {
    output: CalibratedOutput,
    calibration_artifact_id: [u8; 32],
    calibration_key_hash: [u8; 32],
    raw_model_hash: [u8; 32],
    raw_model_training_hash: [u8; 32],
    outer_fold_hash: [u8; 32],
}

impl VerifiedCalibratedInput {
    pub fn try_new(
        artifact: &CalibrationArtifact,
        output: CalibratedOutput,
    ) -> Result<Self, ApplicabilityError> {
        output
            .verify(artifact)
            .map_err(|_| ApplicabilityError::IncompatibleCalibration)?;
        Ok(Self {
            calibration_artifact_id: artifact.artifact_id(),
            calibration_key_hash: artifact.key().evidence_hash(),
            raw_model_hash: artifact.key().raw_model_hash(),
            raw_model_training_hash: artifact.key().raw_model_training_hash(),
            outer_fold_hash: artifact.lineage().outer_fold_hash(),
            output,
        })
    }

    pub const fn output(&self) -> &CalibratedOutput {
        &self.output
    }

    fn validate(&self) -> Result<(), ApplicabilityError> {
        if self.calibration_artifact_id == [0; 32]
            || self.calibration_key_hash != self.output.key.evidence_hash()
            || self.raw_model_hash != self.output.key.raw_model_hash()
            || self.raw_model_training_hash != self.output.key.raw_model_training_hash()
            || self.outer_fold_hash == [0; 32]
            || self.calibration_artifact_id != self.output.artifact_id
            || self.output.evidence_hash == [0; 32]
        {
            Err(ApplicabilityError::IncompatibleCalibration)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceHealth {
    Healthy,
    Degraded,
    Unhealthy,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceEvidence {
    id: String,
    health: SourceHealth,
    evidence_hash: [u8; 32],
}

impl SourceEvidence {
    pub fn try_new(
        id: impl Into<String>,
        health: SourceHealth,
        evidence_hash: [u8; 32],
    ) -> Result<Self, ApplicabilityError> {
        let value = Self {
            id: id.into(),
            health,
            evidence_hash,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), ApplicabilityError> {
        if identifier_is_valid(&self.id) && self.evidence_hash != [0; 32] {
            Ok(())
        } else {
            Err(ApplicabilityError::InvalidRuntimeContext)
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CategoryValue {
    field: String,
    value: String,
}

impl CategoryValue {
    pub fn try_new(
        field: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<Self, ApplicabilityError> {
        let value = Self {
            field: field.into(),
            value: value.into(),
        };
        if identifier_is_valid(&value.field) && identifier_is_valid(&value.value) {
            Ok(value)
        } else {
            Err(ApplicabilityError::InvalidRuntimeContext)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResidualDiagnostic {
    pub absolute_standardized: f64,
    pub as_known_at_ns: i64,
    pub evidence_hash: [u8; 32],
}

impl ResidualDiagnostic {
    pub fn try_new(
        absolute_standardized: f64,
        as_known_at_ns: i64,
        evidence_hash: [u8; 32],
    ) -> Result<Self, ApplicabilityError> {
        let value = Self {
            absolute_standardized,
            as_known_at_ns,
            evidence_hash,
        };
        if value.absolute_standardized.is_finite()
            && value.absolute_standardized >= 0.0
            && value.as_known_at_ns > 0
            && value.evidence_hash != [0; 32]
        {
            Ok(value)
        } else {
            Err(ApplicabilityError::InvalidRuntimeContext)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriftDiagnostic {
    pub relative_l2: f64,
    pub as_known_at_ns: i64,
    pub evidence_hash: [u8; 32],
}

impl DriftDiagnostic {
    pub fn try_new(
        relative_l2: f64,
        as_known_at_ns: i64,
        evidence_hash: [u8; 32],
    ) -> Result<Self, ApplicabilityError> {
        let value = Self {
            relative_l2,
            as_known_at_ns,
            evidence_hash,
        };
        if value.relative_l2.is_finite()
            && value.relative_l2 >= 0.0
            && value.as_known_at_ns > 0
            && value.evidence_hash != [0; 32]
        {
            Ok(value)
        } else {
            Err(ApplicabilityError::InvalidRuntimeContext)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualitySnapshot {
    pub overall: f64,
    pub minimum_critical: f64,
    pub as_known_at_ns: i64,
    pub evidence_hash: [u8; 32],
}

impl QualitySnapshot {
    pub fn try_new(
        overall: f64,
        minimum_critical: f64,
        as_known_at_ns: i64,
        evidence_hash: [u8; 32],
    ) -> Result<Self, ApplicabilityError> {
        let value = Self {
            overall,
            minimum_critical,
            as_known_at_ns,
            evidence_hash,
        };
        if value.overall.is_finite()
            && (0.0..=1.0).contains(&value.overall)
            && value.minimum_critical.is_finite()
            && (0.0..=1.0).contains(&value.minimum_critical)
            && value.minimum_critical <= value.overall
            && value.as_known_at_ns > 0
            && value.evidence_hash != [0; 32]
        {
            Ok(value)
        } else {
            Err(ApplicabilityError::InvalidRuntimeContext)
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeContextInput {
    pub issued_at_ns: i64,
    pub evaluated_at_ns: i64,
    pub feature_as_known_at_ns: i64,
    pub feature_schema_hash: [u8; 32],
    pub input_evidence_hash: [u8; 32],
    pub features: Vec<Option<f64>>,
    pub categories: Vec<CategoryValue>,
    pub venue: String,
    pub product: String,
    pub sources: Vec<SourceEvidence>,
    pub coefficient_drift: Option<DriftDiagnostic>,
    pub residual: Option<ResidualDiagnostic>,
    pub quality: QualitySnapshot,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeContext {
    issued_at_ns: i64,
    evaluated_at_ns: i64,
    feature_as_known_at_ns: i64,
    feature_schema_hash: [u8; 32],
    input_evidence_hash: [u8; 32],
    features: Vec<Option<f64>>,
    categories: Vec<CategoryValue>,
    venue: String,
    product: String,
    sources: Vec<SourceEvidence>,
    coefficient_drift: Option<DriftDiagnostic>,
    residual: Option<ResidualDiagnostic>,
    quality: QualitySnapshot,
    evidence_hash: [u8; 32],
}

impl RuntimeContext {
    pub fn try_new(mut input: RuntimeContextInput) -> Result<Self, ApplicabilityError> {
        input
            .categories
            .sort_by(|left, right| left.field.cmp(&right.field));
        input.sources.sort_by(|left, right| left.id.cmp(&right.id));
        let mut value = Self {
            issued_at_ns: input.issued_at_ns,
            evaluated_at_ns: input.evaluated_at_ns,
            feature_as_known_at_ns: input.feature_as_known_at_ns,
            feature_schema_hash: input.feature_schema_hash,
            input_evidence_hash: input.input_evidence_hash,
            features: input.features,
            categories: input.categories,
            venue: input.venue,
            product: input.product,
            sources: input.sources,
            coefficient_drift: input.coefficient_drift,
            residual: input.residual,
            quality: input.quality,
            evidence_hash: [0; 32],
        };
        value.validate_without_hash()?;
        value.evidence_hash = value.calculate_hash();
        Ok(value)
    }

    pub const fn evidence_hash(&self) -> [u8; 32] {
        self.evidence_hash
    }

    pub fn source_health_evidence_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"cmti:applicability-source-health:v1\0");
        hash_u64(&mut hasher, self.sources.len() as u64);
        for source in &self.sources {
            hash_string(&mut hasher, &source.id);
            hasher.update(&[source.health as u8]);
            hasher.update(&source.evidence_hash);
        }
        *hasher.finalize().as_bytes()
    }

    pub fn validate(&self) -> Result<(), ApplicabilityError> {
        self.validate_without_hash()?;
        if self.evidence_hash == self.calculate_hash() {
            Ok(())
        } else {
            Err(ApplicabilityError::InvalidRuntimeContext)
        }
    }

    fn validate_without_hash(&self) -> Result<(), ApplicabilityError> {
        let unique_categories: BTreeSet<&str> = self
            .categories
            .iter()
            .map(|value| value.field.as_str())
            .collect();
        let unique_sources: BTreeSet<&str> =
            self.sources.iter().map(|value| value.id.as_str()).collect();
        if self.feature_as_known_at_ns <= 0
            || self.feature_as_known_at_ns > self.issued_at_ns
            || self.issued_at_ns > self.evaluated_at_ns
            || self.feature_schema_hash == [0; 32]
            || self.input_evidence_hash == [0; 32]
            || self.features.is_empty()
            || self.features.len() > MAXIMUM_FEATURES
            || self
                .features
                .iter()
                .flatten()
                .any(|value| !value.is_finite())
            || self.categories.len() > MAXIMUM_CATEGORIES
            || unique_categories.len() != self.categories.len()
            || self
                .categories
                .windows(2)
                .any(|pair| pair[0].field >= pair[1].field)
            || self.categories.iter().any(|value| {
                !identifier_is_valid(&value.field) || !identifier_is_valid(&value.value)
            })
            || !identifier_is_valid(&self.venue)
            || !identifier_is_valid(&self.product)
            || self.sources.is_empty()
            || self.sources.len() > MAXIMUM_SOURCES
            || unique_sources.len() != self.sources.len()
            || self.sources.windows(2).any(|pair| pair[0].id >= pair[1].id)
            || self.sources.iter().any(|value| value.validate().is_err())
            || self.coefficient_drift.is_some_and(|value| {
                !value.relative_l2.is_finite()
                    || value.relative_l2 < 0.0
                    || value.as_known_at_ns <= 0
                    || value.as_known_at_ns > self.issued_at_ns
                    || value.evidence_hash == [0; 32]
            })
            || self.residual.is_some_and(|value| {
                !value.absolute_standardized.is_finite()
                    || value.absolute_standardized < 0.0
                    || value.as_known_at_ns <= 0
                    || value.as_known_at_ns > self.issued_at_ns
                    || value.evidence_hash == [0; 32]
            })
            || !self.quality.overall.is_finite()
            || !(0.0..=1.0).contains(&self.quality.overall)
            || !self.quality.minimum_critical.is_finite()
            || !(0.0..=1.0).contains(&self.quality.minimum_critical)
            || self.quality.minimum_critical > self.quality.overall
            || self.quality.as_known_at_ns <= 0
            || self.quality.as_known_at_ns > self.issued_at_ns
            || self.quality.evidence_hash == [0; 32]
        {
            Err(ApplicabilityError::InvalidRuntimeContext)
        } else {
            Ok(())
        }
    }

    fn calculate_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(CONTEXT_DOMAIN);
        hash_i64(&mut hasher, self.issued_at_ns);
        hash_i64(&mut hasher, self.evaluated_at_ns);
        hash_i64(&mut hasher, self.feature_as_known_at_ns);
        hasher.update(&self.feature_schema_hash);
        hasher.update(&self.input_evidence_hash);
        hash_u64(&mut hasher, self.features.len() as u64);
        for value in &self.features {
            match value {
                Some(value) => {
                    hasher.update(&[1]);
                    hash_f64(&mut hasher, *value);
                }
                None => {
                    hasher.update(&[0]);
                }
            }
        }
        hash_u64(&mut hasher, self.categories.len() as u64);
        for category in &self.categories {
            hash_string(&mut hasher, &category.field);
            hash_string(&mut hasher, &category.value);
        }
        hash_string(&mut hasher, &self.venue);
        hash_string(&mut hasher, &self.product);
        hash_u64(&mut hasher, self.sources.len() as u64);
        for source in &self.sources {
            hash_string(&mut hasher, &source.id);
            hasher.update(&[source.health as u8]);
            hasher.update(&source.evidence_hash);
        }
        if let Some(drift) = self.coefficient_drift {
            hasher.update(&[1]);
            hash_f64(&mut hasher, drift.relative_l2);
            hash_i64(&mut hasher, drift.as_known_at_ns);
            hasher.update(&drift.evidence_hash);
        } else {
            hasher.update(&[0]);
        }
        if let Some(residual) = self.residual {
            hasher.update(&[1]);
            hash_f64(&mut hasher, residual.absolute_standardized);
            hash_i64(&mut hasher, residual.as_known_at_ns);
            hasher.update(&residual.evidence_hash);
        } else {
            hasher.update(&[0]);
        }
        hash_f64(&mut hasher, self.quality.overall);
        hash_f64(&mut hasher, self.quality.minimum_critical);
        hash_i64(&mut hasher, self.quality.as_known_at_ns);
        hasher.update(&self.quality.evidence_hash);
        *hasher.finalize().as_bytes()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicabilityDiagnostics {
    pub mahalanobis_squared: f64,
    pub nearest_neighbor_distance: f64,
    pub model_age_ns: i64,
    pub coefficient_drift: Option<f64>,
    pub absolute_standardized_residual: Option<f64>,
    pub quality_overall: f64,
    pub minimum_critical_quality: f64,
    pub novelty: NoveltyDiagnostics,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicabilityDecision {
    pub schema_version: u32,
    pub applicability_model_id: [u8; 32],
    pub calibration_artifact_id: [u8; 32],
    pub calibration_output_evidence_hash: [u8; 32],
    pub input_evidence_hash: [u8; 32],
    pub feature_schema_hash: [u8; 32],
    pub source_health_evidence_hash: [u8; 32],
    pub quality_evidence_hash: [u8; 32],
    pub horizon_seconds: u64,
    pub calibrated_probability: f64,
    pub availability: Availability,
    pub applicability_score: f64,
    pub diagnostics: ApplicabilityDiagnostics,
    pub reasons: Vec<String>,
    pub evaluated_at_ns: i64,
    pub context_evidence_hash: [u8; 32],
    pub evidence_hash: [u8; 32],
}

impl ApplicabilityDecision {
    pub fn verify(
        &self,
        model: &ApplicabilityModel,
        forecast: &VerifiedCalibratedInput,
        context: &RuntimeContext,
    ) -> Result<(), ApplicabilityError> {
        self.validate_identity(model, forecast, context)?;
        let expected = model.evaluate_unchecked(forecast, context)?;
        if self == &expected {
            Ok(())
        } else {
            Err(ApplicabilityError::InvalidDecision)
        }
    }

    fn validate_identity(
        &self,
        model: &ApplicabilityModel,
        forecast: &VerifiedCalibratedInput,
        context: &RuntimeContext,
    ) -> Result<(), ApplicabilityError> {
        if self.schema_version != 1
            || self.applicability_model_id != model.artifact_id
            || self.calibration_artifact_id != forecast.calibration_artifact_id
            || self.calibration_output_evidence_hash != forecast.output.evidence_hash
            || self.input_evidence_hash != forecast.output.input_evidence_hash
            || self.feature_schema_hash != context.feature_schema_hash
            || self.source_health_evidence_hash != context.source_health_evidence_hash()
            || self.quality_evidence_hash != context.quality.evidence_hash
            || self.horizon_seconds != forecast.output.key.horizon_seconds()
            || self.calibrated_probability != forecast.output.calibrated_probability
            || !self.calibrated_probability.is_finite()
            || !(0.0..=1.0).contains(&self.calibrated_probability)
            || !self.applicability_score.is_finite()
            || !(0.0..=1.0).contains(&self.applicability_score)
            || !self.diagnostics.mahalanobis_squared.is_finite()
            || self.diagnostics.mahalanobis_squared < 0.0
            || !self.diagnostics.nearest_neighbor_distance.is_finite()
            || self.diagnostics.nearest_neighbor_distance < 0.0
            || self.diagnostics.model_age_ns < 0
            || self
                .diagnostics
                .coefficient_drift
                .is_some_and(|value| !value.is_finite() || value < 0.0)
            || self
                .diagnostics
                .absolute_standardized_residual
                .is_some_and(|value| !value.is_finite() || value < 0.0)
            || !self.diagnostics.quality_overall.is_finite()
            || !(0.0..=1.0).contains(&self.diagnostics.quality_overall)
            || !self.diagnostics.minimum_critical_quality.is_finite()
            || !(0.0..=1.0).contains(&self.diagnostics.minimum_critical_quality)
            || self.reasons.len() > MAXIMUM_REASONS
            || self
                .reasons
                .iter()
                .any(|reason| !identifier_is_valid(reason))
            || self.reasons.windows(2).any(|pair| pair[0] >= pair[1])
            || self.evaluated_at_ns <= 0
            || self.context_evidence_hash == [0; 32]
            || self.evidence_hash != self.calculate_hash()
        {
            Err(ApplicabilityError::InvalidDecision)
        } else {
            Ok(())
        }
    }

    fn calculate_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(DECISION_DOMAIN);
        hash_u32(&mut hasher, self.schema_version);
        for value in [
            self.applicability_model_id,
            self.calibration_artifact_id,
            self.calibration_output_evidence_hash,
            self.input_evidence_hash,
            self.feature_schema_hash,
            self.source_health_evidence_hash,
            self.quality_evidence_hash,
            self.context_evidence_hash,
        ] {
            hasher.update(&value);
        }
        hash_u64(&mut hasher, self.horizon_seconds);
        hash_f64(&mut hasher, self.calibrated_probability);
        hasher.update(&[self.availability as u8]);
        hash_f64(&mut hasher, self.applicability_score);
        hash_f64(&mut hasher, self.diagnostics.mahalanobis_squared);
        hash_f64(&mut hasher, self.diagnostics.nearest_neighbor_distance);
        hash_i64(&mut hasher, self.diagnostics.model_age_ns);
        hash_optional_f64(&mut hasher, self.diagnostics.coefficient_drift);
        hash_optional_f64(&mut hasher, self.diagnostics.absolute_standardized_residual);
        hash_f64(&mut hasher, self.diagnostics.quality_overall);
        hash_f64(&mut hasher, self.diagnostics.minimum_critical_quality);
        for value in [
            self.diagnostics.novelty.required_missing,
            self.diagnostics.novelty.optional_missing,
            self.diagnostics.novelty.out_of_range,
            self.diagnostics.novelty.categorical_novelty,
            self.diagnostics.novelty.unsupported_venue,
            self.diagnostics.novelty.unsupported_product,
        ] {
            hasher.update(&[u8::from(value)]);
        }
        hash_u64(&mut hasher, self.diagnostics.novelty.reasons.len() as u64);
        for reason in &self.diagnostics.novelty.reasons {
            hash_string(&mut hasher, reason);
        }
        hash_u64(&mut hasher, self.reasons.len() as u64);
        for reason in &self.reasons {
            hash_string(&mut hasher, reason);
        }
        hash_i64(&mut hasher, self.evaluated_at_ns);
        *hasher.finalize().as_bytes()
    }
}

fn hash_training(rows: &[TrainingObservation]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(TRAINING_DOMAIN);
    hash_u64(&mut hasher, rows.len() as u64);
    for row in rows {
        hash_string(&mut hasher, &row.id);
        hash_i64(&mut hasher, row.observed_at_ns);
        hash_i64(&mut hasher, row.as_known_at_ns);
        for value in &row.features {
            hash_f64(&mut hasher, *value);
        }
        hasher.update(&row.evidence_hash);
    }
    *hasher.finalize().as_bytes()
}

fn append_context_reasons(
    reasons: &mut Vec<String>,
    signals: PolicySignals,
    context: &RuntimeContext,
    model_age_ns: i64,
    mahalanobis_squared: f64,
    nearest_neighbor_distance: f64,
    thresholds: ApplicabilityThresholds,
) {
    if signals.model_incompatible {
        reasons.push("model_incompatible".to_owned());
    }
    if signals.source_unhealthy {
        reasons.push("required_source_or_quality_unhealthy".to_owned());
    }
    if signals.insufficient_data {
        reasons.push("insufficient_data".to_owned());
    }
    if signals.out_of_distribution {
        reasons.push("out_of_distribution".to_owned());
    }
    if signals.degraded {
        reasons.push("degraded_evidence".to_owned());
    }
    for source in &context.sources {
        if source.health != SourceHealth::Healthy {
            reasons.push(format!("source_{}:{}", source.health.as_str(), source.id));
        }
    }
    if model_age_ns > thresholds.model_age_experimental_ns {
        reasons.push("model_age_exceeds_experimental_threshold".to_owned());
    }
    if mahalanobis_squared > thresholds.mahalanobis_degraded {
        reasons.push("mahalanobis_distance_elevated".to_owned());
    }
    if nearest_neighbor_distance > thresholds.nearest_neighbor_degraded {
        reasons.push("nearest_neighbor_distance_elevated".to_owned());
    }
    match context.coefficient_drift {
        None => reasons.push("coefficient_drift_unavailable".to_owned()),
        Some(value) if value.relative_l2 > thresholds.coefficient_drift_experimental => {
            reasons.push("coefficient_drift_elevated".to_owned());
        }
        Some(_) => {}
    }
    if context.quality.overall < thresholds.quality_available {
        reasons.push("overall_quality_below_available_threshold".to_owned());
    }
    if context.quality.minimum_critical < thresholds.critical_quality_available {
        reasons.push("critical_quality_below_available_threshold".to_owned());
    }
    match context.residual {
        None => reasons.push("residual_diagnostic_unavailable".to_owned()),
        Some(value) if value.absolute_standardized > thresholds.residual_experimental => {
            reasons.push("residual_diagnostic_elevated".to_owned());
        }
        Some(_) => {}
    }
    if signals.experimental {
        reasons.push("experimental_evidence".to_owned());
    }
}

impl SourceHealth {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Unhealthy => "unhealthy",
        }
    }
}

fn applicability_score(
    mahalanobis_squared: f64,
    nearest_neighbor_distance: f64,
    residual: Option<f64>,
    thresholds: ApplicabilityThresholds,
) -> Result<f64, ApplicabilityError> {
    let penalty = (mahalanobis_squared / thresholds.mahalanobis_ood)
        .max(nearest_neighbor_distance / thresholds.nearest_neighbor_ood)
        .max(residual.unwrap_or(thresholds.residual_experimental) / thresholds.residual_ood);
    let score = (-penalty).exp();
    if score.is_finite() {
        Ok(score.clamp(0.0, 1.0))
    } else {
        Err(ApplicabilityError::NonFiniteArithmetic)
    }
}

fn hash_thresholds(hasher: &mut blake3::Hasher, value: ApplicabilityThresholds) {
    for field in [
        value.mahalanobis_degraded,
        value.mahalanobis_ood,
        value.nearest_neighbor_degraded,
        value.nearest_neighbor_ood,
        value.residual_experimental,
        value.residual_ood,
        value.coefficient_drift_experimental,
        value.coefficient_drift_incompatible,
        value.quality_available,
        value.quality_degraded,
        value.critical_quality_available,
        value.critical_quality_degraded,
    ] {
        hash_f64(hasher, field);
    }
    hash_i64(hasher, value.model_age_experimental_ns);
    hash_i64(hasher, value.model_age_incompatible_ns);
}

fn hash_optional_f64(hasher: &mut blake3::Hasher, value: Option<f64>) {
    match value {
        Some(value) => {
            hasher.update(&[1]);
            hash_f64(hasher, value);
        }
        None => {
            hasher.update(&[0]);
        }
    }
}

fn hash_string(hasher: &mut blake3::Hasher, value: &str) {
    hash_u64(hasher, value.len() as u64);
    hasher.update(value.as_bytes());
}

fn hash_u32(hasher: &mut blake3::Hasher, value: u32) {
    hasher.update(&value.to_le_bytes());
}

fn hash_u64(hasher: &mut blake3::Hasher, value: u64) {
    hasher.update(&value.to_le_bytes());
}

fn hash_i64(hasher: &mut blake3::Hasher, value: i64) {
    hasher.update(&value.to_le_bytes());
}

fn hash_f64(hasher: &mut blake3::Hasher, value: f64) {
    hasher.update(&value.to_bits().to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture_model() -> ApplicabilityModel {
        let novelty = NoveltyProfile::try_new(
            vec![
                FeatureRange::try_new("spread", true, -5.0, 5.0).unwrap(),
                FeatureRange::try_new("flow", false, -5.0, 5.0).unwrap(),
            ],
            vec![CategoricalSupport::try_new("regime", ["calm", "stress"]).unwrap()],
            ["kraken"],
            ["spot"],
        )
        .unwrap();
        let distribution = RobustDistribution::fit(
            &[
                vec![-2.0, -1.0],
                vec![-1.0, -2.0],
                vec![1.0, 2.0],
                vec![2.0, 1.0],
            ],
            0.1,
        )
        .unwrap();
        let mut model = ApplicabilityModel {
            schema_version: 1,
            calibration_artifact_id: [2; 32],
            calibration_key_hash: [6; 32],
            raw_model_hash: [3; 32],
            raw_model_training_hash: [4; 32],
            outer_fold_hash: [5; 32],
            feature_schema_hash: novelty.evidence_hash(),
            trained_through_ns: 50,
            model_fitted_at_ns: 100,
            training_evidence_hash: [9; 32],
            distribution,
            novelty,
            source_requirements: vec![SourceRequirement::try_new("l2", true).unwrap()],
            policy: ApplicabilityPolicy::try_new(ApplicabilityThresholds::default()).unwrap(),
            artifact_id: [0; 32],
        };
        model.artifact_id = model.calculate_hash();
        model.validate().unwrap();
        model
    }

    fn fixture_forecast() -> VerifiedCalibratedInput {
        let output: CalibratedOutput = serde_json::from_value(json!({
            "schema_version": 1,
            "key": {
                "schema_version": 1,
                "event_id": "downside",
                "event_type": "downside",
                "horizon_seconds": 900,
                "liquidity_class": "high",
                "label_definition_hash": vec![1u8; 32],
                "raw_model_hash": vec![3u8; 32],
                "raw_model_training_hash": vec![4u8; 32],
                "evidence_hash": vec![6u8; 32]
            },
            "raw_score": {"logit": 0.0, "probability": 0.5},
            "calibrated_probability": 0.42,
            "validation_base_rate": 0.20,
            "calibration_version": "platt",
            "artifact_id": vec![2u8; 32],
            "uncertainty": {
                "lower": 0.1,
                "upper": 0.3,
                "confidence_level": 0.95,
                "method": "validation-base-rate-only"
            },
            "uncertainty_scope": "validation_base_rate",
            "effective_sample_size": 100.0,
            "input_evidence_hash": vec![7u8; 32],
            "evidence_hash": vec![8u8; 32],
            "status": "experimental"
        }))
        .unwrap();
        VerifiedCalibratedInput {
            output,
            calibration_artifact_id: [2; 32],
            calibration_key_hash: [6; 32],
            raw_model_hash: [3; 32],
            raw_model_training_hash: [4; 32],
            outer_fold_hash: [5; 32],
        }
    }

    fn fixture_context(model: &ApplicabilityModel) -> RuntimeContext {
        RuntimeContext::try_new(RuntimeContextInput {
            issued_at_ns: 200,
            evaluated_at_ns: 250,
            feature_as_known_at_ns: 190,
            feature_schema_hash: model.feature_schema_hash(),
            input_evidence_hash: [7; 32],
            features: vec![Some(0.0), Some(0.0)],
            categories: vec![CategoryValue::try_new("regime", "calm").unwrap()],
            venue: "kraken".to_owned(),
            product: "spot".to_owned(),
            sources: vec![SourceEvidence::try_new("l2", SourceHealth::Healthy, [10; 32]).unwrap()],
            coefficient_drift: Some(DriftDiagnostic::try_new(0.01, 190, [15; 32]).unwrap()),
            residual: Some(ResidualDiagnostic::try_new(0.5, 190, [11; 32]).unwrap()),
            quality: QualitySnapshot::try_new(0.95, 0.95, 190, [16; 32]).unwrap(),
        })
        .unwrap()
    }

    #[test]
    fn decision_preserves_exact_probability_horizon_and_experimental_calibration() {
        let model = fixture_model();
        let forecast = fixture_forecast();
        let context = fixture_context(&model);
        let decision = model.evaluate(&forecast, &context).unwrap();
        assert_eq!(decision.availability, Availability::Experimental);
        assert_eq!(
            decision.calibrated_probability.to_bits(),
            0.42_f64.to_bits()
        );
        assert_eq!(decision.horizon_seconds, 900);
        assert_eq!(decision.input_evidence_hash, [7; 32]);
        decision.verify(&model, &forecast, &context).unwrap();
        assert!(
            !serde_json::to_string(&decision)
                .unwrap()
                .contains("replacement")
        );
    }

    #[test]
    fn exact_precedence_survives_conflicting_runtime_failures() {
        let model = fixture_model();
        let forecast = fixture_forecast();
        let mut context = fixture_context(&model);
        context.features = vec![Some(100.0), Some(-100.0)];
        context.sources[0].health = SourceHealth::Unhealthy;
        context.evidence_hash = context.calculate_hash();
        assert_eq!(
            model.evaluate(&forecast, &context).unwrap().availability,
            Availability::SourceUnhealthy
        );

        context.sources =
            vec![SourceEvidence::try_new("trades", SourceHealth::Healthy, [12; 32]).unwrap()];
        context.evidence_hash = context.calculate_hash();
        assert_eq!(
            model.evaluate(&forecast, &context).unwrap().availability,
            Availability::SourceUnhealthy
        );

        context.sources =
            vec![SourceEvidence::try_new("l2", SourceHealth::Healthy, [10; 32]).unwrap()];
        context.quality.overall = 0.5;
        context.quality.minimum_critical = 0.5;
        context.evidence_hash = context.calculate_hash();
        assert_eq!(
            model.evaluate(&forecast, &context).unwrap().availability,
            Availability::SourceUnhealthy
        );

        context.feature_schema_hash = [99; 32];
        context.evidence_hash = context.calculate_hash();
        assert_eq!(
            model.evaluate(&forecast, &context).unwrap().availability,
            Availability::ModelIncompatible
        );
    }

    #[test]
    fn missing_required_feature_and_input_evidence_mismatch_fail_closed() {
        let model = fixture_model();
        let forecast = fixture_forecast();
        let mut context = fixture_context(&model);
        context.features[0] = None;
        context.evidence_hash = context.calculate_hash();
        assert_eq!(
            model.evaluate(&forecast, &context).unwrap().availability,
            Availability::InsufficientData
        );

        context.features[0] = Some(0.0);
        context.input_evidence_hash = [42; 32];
        context.evidence_hash = context.calculate_hash();
        assert_eq!(
            model.evaluate(&forecast, &context).unwrap().availability,
            Availability::ModelIncompatible
        );
    }

    #[test]
    fn future_known_residual_and_serialized_mutations_are_rejected() {
        let model = fixture_model();
        let forecast = fixture_forecast();
        let mut input = RuntimeContextInput {
            issued_at_ns: 200,
            evaluated_at_ns: 250,
            feature_as_known_at_ns: 190,
            feature_schema_hash: model.feature_schema_hash(),
            input_evidence_hash: [7; 32],
            features: vec![Some(0.0), Some(0.0)],
            categories: vec![CategoryValue::try_new("regime", "calm").unwrap()],
            venue: "kraken".to_owned(),
            product: "spot".to_owned(),
            sources: vec![SourceEvidence::try_new("l2", SourceHealth::Healthy, [10; 32]).unwrap()],
            coefficient_drift: Some(DriftDiagnostic::try_new(0.01, 201, [15; 32]).unwrap()),
            residual: Some(ResidualDiagnostic::try_new(0.5, 190, [11; 32]).unwrap()),
            quality: QualitySnapshot::try_new(0.95, 0.95, 190, [16; 32]).unwrap(),
        };
        assert!(RuntimeContext::try_new(input.clone()).is_err());
        input.coefficient_drift = Some(DriftDiagnostic::try_new(0.01, 190, [15; 32]).unwrap());
        input.residual = Some(ResidualDiagnostic::try_new(0.5, 201, [11; 32]).unwrap());
        assert!(RuntimeContext::try_new(input.clone()).is_err());
        input.residual = Some(ResidualDiagnostic::try_new(0.5, 190, [11; 32]).unwrap());
        let context = RuntimeContext::try_new(input).unwrap();
        let decision = model.evaluate(&forecast, &context).unwrap();

        let mut model_json = serde_json::to_value(&model).unwrap();
        model_json["artifact_id"] = json!(vec![77u8; 32]);
        let forged_model: ApplicabilityModel = serde_json::from_value(model_json).unwrap();
        assert_eq!(
            forged_model.validate(),
            Err(ApplicabilityError::InvalidArtifact)
        );

        let mut decision_json = serde_json::to_value(&decision).unwrap();
        decision_json["calibrated_probability"] = json!(0.99);
        let forged_decision: ApplicabilityDecision = serde_json::from_value(decision_json).unwrap();
        assert_eq!(
            forged_decision.verify(&model, &forecast, &context),
            Err(ApplicabilityError::InvalidDecision)
        );

        let mut semantically_forged = decision;
        semantically_forged.availability = Availability::Available;
        semantically_forged.evidence_hash = semantically_forged.calculate_hash();
        assert_eq!(
            semantically_forged.verify(&model, &forecast, &context),
            Err(ApplicabilityError::InvalidDecision)
        );

        let mut novelty_forged = semantically_forged;
        let prior_hash = novelty_forged.calculate_hash();
        novelty_forged.diagnostics.novelty.optional_missing =
            !novelty_forged.diagnostics.novelty.optional_missing;
        assert_ne!(novelty_forged.calculate_hash(), prior_hash);
    }
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum ApplicabilityError {
    #[error("training distribution is invalid or outside capacity")]
    InvalidTrainingDistribution,
    #[error("training observation is invalid, duplicated, or future-known")]
    InvalidTrainingObservation,
    #[error("frozen source requirements are invalid")]
    InvalidSourceRequirements,
    #[error("feature is degenerate under robust normalization")]
    DegenerateFeature,
    #[error("regularized covariance is singular or its inverse is invalid")]
    SingularCovariance,
    #[error("feature schema does not match the frozen applicability model")]
    FeatureSchemaMismatch,
    #[error("novelty profile is invalid")]
    InvalidNoveltyProfile,
    #[error("applicability policy is invalid")]
    InvalidPolicy,
    #[error("runtime applicability context is invalid")]
    InvalidRuntimeContext,
    #[error("applicability artifact identity is invalid")]
    InvalidArtifact,
    #[error("calibration artifact or output is incompatible")]
    IncompatibleCalibration,
    #[error("applicability decision identity is invalid")]
    InvalidDecision,
    #[error("applicability arithmetic is not finite")]
    NonFiniteArithmetic,
}
