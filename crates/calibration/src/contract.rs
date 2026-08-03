//! Production calibration boundary: target lineage, validation-only selection,
//! immutable artifacts, and coherent cumulative-horizon application.

use std::collections::{BTreeMap, BTreeSet};

use dataset::OuterFold;
use ensemble::StackerPrediction;
use hazard::HazardHorizonForecast;
use labels::{EventType, LabelDefinition, LabelEvaluation, LabelOutcome};
use serde::{Deserialize, Serialize};

use crate::{
    BetaCalibrator, CalibrationError, CalibrationMethod, CalibrationObservation, CalibrationPeriod,
    CalibrationSet, FitConfig, IsotonicCalibrator, PlattCalibrator, kish_effective_sample_size,
    sigmoid,
};

const KEY_DOMAIN: &[u8] = b"cmti:calibration-key:v1\0";
const LINEAGE_DOMAIN: &[u8] = b"cmti:calibration-lineage:v1\0";
const VALIDATION_DOMAIN: &[u8] = b"cmti:calibration-validation:v1\0";
const ARTIFACT_DOMAIN: &[u8] = b"cmti:calibration-artifact:v1\0";
const OUTPUT_DOMAIN: &[u8] = b"cmti:calibrated-output:v1\0";
const JOINT_DOMAIN: &[u8] = b"cmti:joint-calibration:v1\0";
const NANOS_PER_SECOND: i64 = 1_000_000_000;
const MAXIMUM_ROWS: usize = 1_000_000;
const MAXIMUM_HORIZONS: usize = 64;
const WILSON_Z_95: f64 = 1.959_963_984_540_054;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LiquidityClass(String);

impl LiquidityClass {
    pub fn try_new(value: impl Into<String>) -> Result<Self, CalibrationError> {
        let value = value.into();
        validate_identifier(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationKey {
    schema_version: u32,
    event_id: String,
    event_type: String,
    horizon_seconds: u64,
    liquidity_class: LiquidityClass,
    label_definition_hash: [u8; 32],
    raw_model_hash: [u8; 32],
    raw_model_training_hash: [u8; 32],
    evidence_hash: [u8; 32],
}

impl CalibrationKey {
    pub fn try_from_score(
        definition: &LabelDefinition,
        liquidity_class: LiquidityClass,
        score: &BoundModelScore,
    ) -> Result<Self, CalibrationError> {
        score.validate()?;
        if !definition
            .horizons_seconds()
            .contains(&score.horizon_seconds)
            || definition.definition_hash() == [0; 32]
            || score.target_event_id != definition.id()
            || score.target_event_type != event_type_name(definition.event_type())
            || score.label_definition_hash != definition.definition_hash()
        {
            return Err(CalibrationError::InvalidLineage);
        }
        let event_id = definition.id().to_owned();
        validate_identifier(&event_id)?;
        let event_type = event_type_name(definition.event_type()).to_owned();
        let mut key = Self {
            schema_version: 1,
            event_id,
            event_type,
            horizon_seconds: score.horizon_seconds,
            liquidity_class,
            label_definition_hash: definition.definition_hash(),
            raw_model_hash: score.raw_model_hash,
            raw_model_training_hash: score.raw_model_training_hash,
            evidence_hash: [0; 32],
        };
        key.evidence_hash = key.calculate_hash();
        Ok(key)
    }

    pub fn event_id(&self) -> &str {
        &self.event_id
    }

    pub fn event_type(&self) -> &str {
        &self.event_type
    }

    pub const fn horizon_seconds(&self) -> u64 {
        self.horizon_seconds
    }

    pub const fn label_definition_hash(&self) -> [u8; 32] {
        self.label_definition_hash
    }

    pub const fn raw_model_hash(&self) -> [u8; 32] {
        self.raw_model_hash
    }

    pub const fn raw_model_training_hash(&self) -> [u8; 32] {
        self.raw_model_training_hash
    }

    pub const fn evidence_hash(&self) -> [u8; 32] {
        self.evidence_hash
    }

    fn validate(&self) -> Result<(), CalibrationError> {
        if self.schema_version != 1
            || validate_identifier(&self.event_id).is_err()
            || validate_identifier(&self.event_type).is_err()
            || validate_identifier(self.liquidity_class.as_str()).is_err()
            || self.horizon_seconds == 0
            || self.label_definition_hash == [0; 32]
            || self.raw_model_hash == [0; 32]
            || self.raw_model_training_hash == [0; 32]
            || self.evidence_hash != self.calculate_hash()
        {
            Err(CalibrationError::InvalidLineage)
        } else {
            Ok(())
        }
    }

    fn calculate_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(KEY_DOMAIN);
        hash_u32(&mut hasher, self.schema_version);
        hash_string(&mut hasher, &self.event_id);
        hash_string(&mut hasher, &self.event_type);
        hash_u64(&mut hasher, self.horizon_seconds);
        hash_string(&mut hasher, self.liquidity_class.as_str());
        hasher.update(&self.label_definition_hash);
        hasher.update(&self.raw_model_hash);
        hasher.update(&self.raw_model_training_hash);
        *hasher.finalize().as_bytes()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationLineage {
    schema_version: u32,
    outer_fold_hash: [u8; 32],
    training_period: CalibrationPeriod,
    outer_calibration_period: CalibrationPeriod,
    outer_test_period: CalibrationPeriod,
    purge_embargo_ns: i64,
    model_fitted_at_ns: i64,
    raw_model_training_hash: [u8; 32],
    evidence_hash: [u8; 32],
}

impl CalibrationLineage {
    pub fn try_from_outer_fold(
        fold: &OuterFold,
        score: &BoundModelScore,
    ) -> Result<Self, CalibrationError> {
        score.validate()?;
        let model_fitted_at_ns = score.model_fitted_at_ns;
        let training_period = period_from_range(fold.training())?;
        let outer_calibration_period = period_from_range(fold.calibration())?;
        let outer_test_period = period_from_range(fold.test())?;
        if fold.fold_hash() == [0; 32]
            || score.outer_fold_hash != fold.fold_hash()
            || fold.purge_embargo_ns() <= 0
            || model_fitted_at_ns < training_period.end_ns
            || model_fitted_at_ns >= outer_calibration_period.start_ns
            || score.origin_time_ns < outer_calibration_period.start_ns
            || score.origin_time_ns >= outer_calibration_period.end_ns
            || outer_calibration_period.start_ns - training_period.end_ns < fold.purge_embargo_ns()
            || outer_test_period.start_ns - outer_calibration_period.end_ns
                < fold.purge_embargo_ns()
        {
            return Err(CalibrationError::InvalidLineage);
        }
        let mut lineage = Self {
            schema_version: 1,
            outer_fold_hash: fold.fold_hash(),
            training_period,
            outer_calibration_period,
            outer_test_period,
            purge_embargo_ns: fold.purge_embargo_ns(),
            model_fitted_at_ns,
            raw_model_training_hash: score.raw_model_training_hash,
            evidence_hash: [0; 32],
        };
        lineage.evidence_hash = lineage.calculate_hash();
        Ok(lineage)
    }

    pub const fn outer_fold_hash(&self) -> [u8; 32] {
        self.outer_fold_hash
    }

    pub const fn training_period(&self) -> CalibrationPeriod {
        self.training_period
    }

    pub const fn calibration_period(&self) -> CalibrationPeriod {
        self.outer_calibration_period
    }

    pub const fn test_period(&self) -> CalibrationPeriod {
        self.outer_test_period
    }

    pub const fn purge_embargo_ns(&self) -> i64 {
        self.purge_embargo_ns
    }

    pub const fn raw_model_training_hash(&self) -> [u8; 32] {
        self.raw_model_training_hash
    }

    pub const fn evidence_hash(&self) -> [u8; 32] {
        self.evidence_hash
    }

    fn validate(&self) -> Result<(), CalibrationError> {
        if self.schema_version != 1
            || self.outer_fold_hash == [0; 32]
            || self.purge_embargo_ns <= 0
            || self.model_fitted_at_ns < self.training_period.end_ns
            || self.model_fitted_at_ns >= self.outer_calibration_period.start_ns
            || self.raw_model_training_hash == [0; 32]
            || self.outer_calibration_period.start_ns - self.training_period.end_ns
                < self.purge_embargo_ns
            || self.outer_test_period.start_ns - self.outer_calibration_period.end_ns
                < self.purge_embargo_ns
            || self.evidence_hash != self.calculate_hash()
        {
            Err(CalibrationError::InvalidLineage)
        } else {
            Ok(())
        }
    }

    fn calculate_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(LINEAGE_DOMAIN);
        hash_u32(&mut hasher, self.schema_version);
        hasher.update(&self.outer_fold_hash);
        hash_period(&mut hasher, self.training_period);
        hash_period(&mut hasher, self.outer_calibration_period);
        hash_period(&mut hasher, self.outer_test_period);
        hash_i64(&mut hasher, self.purge_embargo_ns);
        hash_i64(&mut hasher, self.model_fitted_at_ns);
        hasher.update(&self.raw_model_training_hash);
        *hasher.finalize().as_bytes()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawScore {
    logit: f64,
    probability: f64,
}

impl RawScore {
    pub fn try_from_logit(logit: f64) -> Result<Self, CalibrationError> {
        if !logit.is_finite() || logit.abs() > crate::MAXIMUM_ABS_LOGIT {
            return Err(CalibrationError::NonFiniteArithmetic);
        }
        Ok(Self {
            logit,
            probability: sigmoid(logit)?,
        })
    }

    pub const fn logit(self) -> f64 {
        self.logit
    }

    pub const fn probability(self) -> f64 {
        self.probability
    }

    fn validate(self) -> Result<(), CalibrationError> {
        if self.logit.is_finite()
            && self.logit.abs() <= crate::MAXIMUM_ABS_LOGIT
            && sigmoid(self.logit)? == self.probability
        {
            Ok(())
        } else {
            Err(CalibrationError::InvalidProbability)
        }
    }
}

/// A score that can enter the production calibration boundary only after its
/// model, target, prediction time, and evidence lineage have been verified.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundModelScore {
    raw_score: RawScore,
    raw_model_hash: [u8; 32],
    raw_model_training_hash: [u8; 32],
    prediction_evidence_hash: [u8; 32],
    input_evidence_hash: [u8; 32],
    entity_id: String,
    episode_cluster_id: String,
    outer_fold_hash: [u8; 32],
    model_fitted_at_ns: i64,
    curve_evidence_hash: [u8; 32],
    origin_time_ns: i64,
    as_known_at_ns: i64,
    horizon_seconds: u64,
    target_event_id: String,
    target_event_type: String,
    label_definition_hash: [u8; 32],
    evidence_hash: [u8; 32],
}

impl BoundModelScore {
    pub fn try_from_stacker(
        prediction: &StackerPrediction,
        outer_fold: &OuterFold,
    ) -> Result<Self, CalibrationError> {
        if prediction.outer_fold_hash() != outer_fold.fold_hash() {
            return Err(CalibrationError::InvalidLineage);
        }
        let target = prediction.target_schema();
        let raw_score = RawScore::try_from_logit(prediction.logit())?;
        if raw_score.probability != prediction.probability() {
            return Err(CalibrationError::InvalidProbability);
        }
        Self::finish(Self {
            raw_score,
            raw_model_hash: prediction.model_id(),
            raw_model_training_hash: prediction.training_evidence_hash(),
            prediction_evidence_hash: prediction.evidence_hash(),
            input_evidence_hash: prediction.input_evidence_hash(),
            entity_id: prediction.entity_id().to_owned(),
            episode_cluster_id: prediction.episode_cluster_id().to_owned(),
            outer_fold_hash: outer_fold.fold_hash(),
            model_fitted_at_ns: prediction.model_fitted_at_ns(),
            curve_evidence_hash: curve_evidence_hash(
                prediction.entity_id(),
                prediction.origin_time_ns(),
                prediction.input_evidence_hash(),
            ),
            origin_time_ns: prediction.origin_time_ns(),
            as_known_at_ns: prediction.as_known_at_ns(),
            horizon_seconds: target.horizon_seconds(),
            target_event_id: target.event_id().to_owned(),
            target_event_type: event_type_name(target.event_type()).to_owned(),
            label_definition_hash: target.definition_hash(),
            evidence_hash: [0; 32],
        })
    }

    pub fn try_from_hazard(
        forecast: &HazardHorizonForecast,
        definition: &LabelDefinition,
        outer_fold: &OuterFold,
    ) -> Result<Self, CalibrationError> {
        if forecast.outer_fold_hash() != outer_fold.fold_hash() {
            return Err(CalibrationError::InvalidLineage);
        }
        let cause_id = event_type_name(definition.event_type());
        let cause_index = forecast
            .causes()
            .iter()
            .position(|cause| cause.cause_id() == cause_id)
            .ok_or(CalibrationError::IncompatibleArtifact)?;
        if forecast.cause_definition_hashes().get(cause_index).copied()
            != Some(definition.definition_hash())
        {
            return Err(CalibrationError::IncompatibleArtifact);
        }
        let probability = forecast.causes()[cause_index].probability();
        let logit = crate::logit_from_open_probability(probability)?;
        let raw_score = RawScore::try_from_logit(logit)?;
        if !definition
            .horizons_seconds()
            .contains(&forecast.horizon_seconds())
        {
            return Err(CalibrationError::IncompatibleArtifact);
        }
        Self::finish(Self {
            raw_score,
            raw_model_hash: forecast.model_id(),
            raw_model_training_hash: forecast.training_evidence_id(),
            prediction_evidence_hash: forecast.evidence_id(),
            input_evidence_hash: forecast.input_evidence_id(),
            entity_id: forecast.entity_id().to_owned(),
            episode_cluster_id: forecast.episode_cluster_id().to_owned(),
            outer_fold_hash: outer_fold.fold_hash(),
            model_fitted_at_ns: forecast.model_fitted_at_ns(),
            curve_evidence_hash: curve_evidence_hash(
                forecast.entity_id(),
                forecast.origin_time_ns(),
                forecast.input_evidence_id(),
            ),
            origin_time_ns: forecast.origin_time_ns(),
            as_known_at_ns: forecast.as_known_at_ns(),
            horizon_seconds: forecast.horizon_seconds(),
            target_event_id: definition.id().to_owned(),
            target_event_type: cause_id.to_owned(),
            label_definition_hash: definition.definition_hash(),
            evidence_hash: [0; 32],
        })
    }

    fn finish(mut score: Self) -> Result<Self, CalibrationError> {
        score.evidence_hash = score.calculate_hash();
        score.validate()?;
        Ok(score)
    }

    pub const fn raw_score(&self) -> RawScore {
        self.raw_score
    }

    pub const fn origin_time_ns(&self) -> i64 {
        self.origin_time_ns
    }

    pub const fn prediction_evidence_hash(&self) -> [u8; 32] {
        self.prediction_evidence_hash
    }

    pub fn entity_id(&self) -> &str {
        &self.entity_id
    }

    pub fn episode_cluster_id(&self) -> &str {
        &self.episode_cluster_id
    }

    fn validate(&self) -> Result<(), CalibrationError> {
        self.raw_score.validate()?;
        if self.raw_model_hash == [0; 32]
            || self.raw_model_training_hash == [0; 32]
            || self.prediction_evidence_hash == [0; 32]
            || self.input_evidence_hash == [0; 32]
            || validate_identifier(&self.entity_id).is_err()
            || validate_identifier(&self.episode_cluster_id).is_err()
            || self.outer_fold_hash == [0; 32]
            || self.model_fitted_at_ns <= 0
            || self.model_fitted_at_ns > self.as_known_at_ns
            || self.curve_evidence_hash == [0; 32]
            || self.origin_time_ns <= 0
            || self.as_known_at_ns <= 0
            || self.as_known_at_ns > self.origin_time_ns
            || self.horizon_seconds == 0
            || validate_identifier(&self.target_event_id).is_err()
            || validate_identifier(&self.target_event_type).is_err()
            || self.label_definition_hash == [0; 32]
            || self.evidence_hash != self.calculate_hash()
        {
            Err(CalibrationError::InvalidLineage)
        } else {
            Ok(())
        }
    }

    fn calculate_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"cmti:bound-model-score:v1\0");
        hash_f64(&mut hasher, self.raw_score.logit);
        hash_f64(&mut hasher, self.raw_score.probability);
        hasher.update(&self.raw_model_hash);
        hasher.update(&self.raw_model_training_hash);
        hasher.update(&self.prediction_evidence_hash);
        hasher.update(&self.input_evidence_hash);
        hash_string(&mut hasher, &self.entity_id);
        hash_string(&mut hasher, &self.episode_cluster_id);
        hasher.update(&self.outer_fold_hash);
        hash_i64(&mut hasher, self.model_fitted_at_ns);
        hasher.update(&self.curve_evidence_hash);
        hash_i64(&mut hasher, self.origin_time_ns);
        hash_i64(&mut hasher, self.as_known_at_ns);
        hash_u64(&mut hasher, self.horizon_seconds);
        hash_string(&mut hasher, &self.target_event_id);
        hash_string(&mut hasher, &self.target_event_type);
        hasher.update(&self.label_definition_hash);
        *hasher.finalize().as_bytes()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ValidationRow {
    pub(crate) row_id: u64,
    pub(crate) entity_id: String,
    pub(crate) episode_cluster_id: String,
    pub(crate) key_hash: [u8; 32],
    pub(crate) raw_score: RawScore,
    pub(crate) outcome: bool,
    pub(crate) origin_time_ns: i64,
    pub(crate) outcome_known_at_ns: i64,
    pub(crate) weight: f64,
    pub(crate) source_evidence_hash: [u8; 32],
    pub(crate) outer_fold_hash: [u8; 32],
}

impl ValidationRow {
    pub fn try_new(
        key: &CalibrationKey,
        row_id: u64,
        score: &BoundModelScore,
        label: LabelEvaluation,
        weight: f64,
    ) -> Result<Self, CalibrationError> {
        key.validate()?;
        score.validate()?;
        if row_id == 0
            || !label.provenance_bound()
            || label.entity_id() != score.entity_id
            || label.origin_time_ns() != score.origin_time_ns
            || label.episode_cluster_id() != score.episode_cluster_id
            || score.raw_model_hash != key.raw_model_hash
            || score.raw_model_training_hash != key.raw_model_training_hash
            || score.horizon_seconds != key.horizon_seconds
            || score.target_event_id != key.event_id
            || score.target_event_type != key.event_type
            || score.label_definition_hash != key.label_definition_hash
            || label.horizon_seconds() != key.horizon_seconds
            || label.definition_hash() != key.label_definition_hash
            || label.source_evidence_hash() == [0; 32]
            || !weight.is_finite()
            || weight <= 0.0
            || weight > 1.0e12
        {
            return Err(CalibrationError::InvalidRow);
        }
        let origin_time_ns = score.origin_time_ns;
        let horizon_end = add_seconds(origin_time_ns, key.horizon_seconds)?;
        let (binary, earliest_known) = match label.outcome() {
            LabelOutcome::Occurred { offset_seconds }
                if offset_seconds > 0 && offset_seconds <= key.horizon_seconds =>
            {
                (true, add_seconds(origin_time_ns, offset_seconds)?)
            }
            LabelOutcome::NotOccurred => (false, horizon_end),
            LabelOutcome::Occurred { .. } => return Err(CalibrationError::InvalidRow),
            LabelOutcome::Censored { .. } | LabelOutcome::Excluded(_) => {
                return Err(CalibrationError::IneligibleOutcome);
            }
        };
        let outcome_known_at_ns =
            add_seconds(origin_time_ns, label.outcome_known_at_offset_seconds())?;
        if outcome_known_at_ns < earliest_known {
            return Err(CalibrationError::InvalidObservationTime);
        }
        let mut evidence_hasher = blake3::Hasher::new();
        evidence_hasher.update(b"cmti:calibration-row-sources:v1\0");
        evidence_hasher.update(&score.evidence_hash);
        evidence_hasher.update(&label.source_evidence_hash());
        let source_evidence_hash = *evidence_hasher.finalize().as_bytes();
        Ok(Self {
            row_id,
            entity_id: score.entity_id.clone(),
            episode_cluster_id: score.episode_cluster_id.clone(),
            key_hash: key.evidence_hash,
            raw_score: score.raw_score,
            outcome: binary,
            origin_time_ns,
            outcome_known_at_ns,
            weight,
            source_evidence_hash,
            outer_fold_hash: score.outer_fold_hash,
        })
    }

    pub const fn raw_score(&self) -> RawScore {
        self.raw_score
    }

    pub const fn outcome(&self) -> bool {
        self.outcome
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ValidationPartition {
    period: CalibrationPeriod,
    rows: Vec<ValidationRow>,
}

impl ValidationPartition {
    pub fn try_new(
        period: CalibrationPeriod,
        rows: Vec<ValidationRow>,
    ) -> Result<Self, CalibrationError> {
        if rows.len() < 8 || rows.len() > MAXIMUM_ROWS {
            return Err(CalibrationError::ObservationCapacity);
        }
        Ok(Self { period, rows })
    }

    pub const fn period(&self) -> CalibrationPeriod {
        self.period
    }

    pub fn rows(&self) -> &[ValidationRow] {
        &self.rows
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ValidationSet {
    key: CalibrationKey,
    lineage: CalibrationLineage,
    fit: ValidationPartition,
    selector: ValidationPartition,
    evidence_hash: [u8; 32],
}

impl ValidationSet {
    pub fn try_new(
        key: CalibrationKey,
        lineage: CalibrationLineage,
        fit: ValidationPartition,
        selector: ValidationPartition,
    ) -> Result<Self, CalibrationError> {
        key.validate()?;
        lineage.validate()?;
        if key.raw_model_training_hash != lineage.raw_model_training_hash
            || fit.period.start_ns < lineage.outer_calibration_period.start_ns
            || selector.period.end_ns > lineage.outer_calibration_period.end_ns
            || fit.period.end_ns > selector.period.start_ns
        {
            return Err(CalibrationError::ValidationLeakage);
        }
        let fit_clusters = fit
            .rows
            .iter()
            .map(|row| (row.entity_id.as_str(), row.episode_cluster_id.as_str()))
            .collect::<BTreeSet<_>>();
        if selector.rows.iter().any(|row| {
            fit_clusters.contains(&(row.entity_id.as_str(), row.episode_cluster_id.as_str()))
        }) {
            return Err(CalibrationError::ValidationLeakage);
        }
        let mut identities = BTreeSet::new();
        for (partition, rows) in [(fit.period, &fit.rows), (selector.period, &selector.rows)] {
            let mut previous = None;
            for row in rows {
                if row.key_hash != key.evidence_hash
                    || row.outer_fold_hash != lineage.outer_fold_hash
                    || row.origin_time_ns < partition.start_ns
                    || row.origin_time_ns >= partition.end_ns
                    || row.outcome_known_at_ns > partition.end_ns
                {
                    return Err(CalibrationError::RowOutsidePartition);
                }
                if previous.is_some_and(|value| row.origin_time_ns < value) {
                    return Err(CalibrationError::ObservationOrder);
                }
                previous = Some(row.origin_time_ns);
                if !identities.insert((row.row_id, row.entity_id.clone())) {
                    return Err(CalibrationError::DuplicateRow);
                }
            }
        }
        let mut set = Self {
            key,
            lineage,
            fit,
            selector,
            evidence_hash: [0; 32],
        };
        set.evidence_hash = set.calculate_hash();
        Ok(set)
    }

    pub const fn key(&self) -> &CalibrationKey {
        &self.key
    }

    pub const fn lineage(&self) -> &CalibrationLineage {
        &self.lineage
    }

    pub const fn evidence_hash(&self) -> [u8; 32] {
        self.evidence_hash
    }

    fn calculate_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(VALIDATION_DOMAIN);
        hasher.update(&self.key.evidence_hash);
        hasher.update(&self.lineage.evidence_hash);
        for partition in [&self.fit, &self.selector] {
            hash_period(&mut hasher, partition.period);
            hash_u64(&mut hasher, partition.rows.len() as u64);
            for row in &partition.rows {
                hash_u64(&mut hasher, row.row_id);
                hash_string(&mut hasher, &row.entity_id);
                hash_string(&mut hasher, &row.episode_cluster_id);
                hasher.update(&row.outer_fold_hash);
                hash_f64(&mut hasher, row.raw_score.logit);
                hasher.update(&[u8::from(row.outcome)]);
                hash_i64(&mut hasher, row.origin_time_ns);
                hash_i64(&mut hasher, row.outcome_known_at_ns);
                hash_f64(&mut hasher, row.weight);
                hasher.update(&row.source_evidence_hash);
            }
        }
        *hasher.finalize().as_bytes()
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalibrationKind {
    Platt,
    Beta,
    Isotonic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionMetric {
    LogLoss,
    Brier,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionConfig {
    pub fit: FitConfig,
    pub metric: SelectionMetric,
    pub minimum_observations: u64,
    pub minimum_positives: u64,
    pub minimum_negatives: u64,
    pub minimum_effective_sample_size: f64,
    pub isotonic_minimum_effective_sample_size: f64,
}

impl Default for SelectionConfig {
    fn default() -> Self {
        Self {
            fit: FitConfig::default(),
            metric: SelectionMetric::LogLoss,
            minimum_observations: 8,
            minimum_positives: 2,
            minimum_negatives: 2,
            minimum_effective_sample_size: 8.0,
            isotonic_minimum_effective_sample_size: 200.0,
        }
    }
}

impl SelectionConfig {
    fn validate(self) -> Result<(), CalibrationError> {
        self.fit.validate(8)?;
        if self.minimum_observations < 8
            || self.minimum_observations > MAXIMUM_ROWS as u64
            || self.minimum_positives == 0
            || self.minimum_negatives == 0
            || !self.minimum_effective_sample_size.is_finite()
            || self.minimum_effective_sample_size < 2.0
            || !self.isotonic_minimum_effective_sample_size.is_finite()
            || self.isotonic_minimum_effective_sample_size < self.minimum_effective_sample_size
        {
            Err(CalibrationError::InvalidFitConfig)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationSupport {
    pub observations: u64,
    /// Explicit entity/episode block clusters; rows within one block are not
    /// counted as independent support.
    pub independent_clusters: u64,
    pub positives: u64,
    pub negatives: u64,
    pub total_weight: f64,
    pub positive_weight: f64,
    /// Maximum row weight used to normalize `sum_squared_weights`.
    pub weight_scale: f64,
    /// Sum of squared row weights after division by `weight_scale`.
    pub sum_squared_weights: f64,
    /// Maximum aggregated entity/episode-cluster weight used to normalize
    /// `sum_squared_cluster_weights`.
    pub cluster_weight_scale: f64,
    /// Sum of squared cluster weights after division by
    /// `cluster_weight_scale`.
    pub sum_squared_cluster_weights: f64,
    pub effective_sample_size: f64,
}

impl CalibrationSupport {
    fn from_rows(rows: &[ValidationRow]) -> Result<Self, CalibrationError> {
        let total_weight = compensated_sum(rows.iter().map(|row| row.weight))?;
        let (weight_scale, sum_squares) = scaled_sum_of_squares(rows.iter().map(|row| row.weight))?;
        let positive_weight =
            compensated_sum(rows.iter().filter(|row| row.outcome).map(|row| row.weight))?;
        let positives = rows.iter().filter(|row| row.outcome).count();
        let mut cluster_sums = BTreeMap::<(&str, &str), CompensatedSum>::new();
        for row in rows {
            cluster_sums
                .entry((row.entity_id.as_str(), row.episode_cluster_id.as_str()))
                .or_default()
                .add(row.weight)?;
        }
        let cluster_weights = cluster_sums
            .values()
            .map(CompensatedSum::value)
            .collect::<Vec<_>>();
        let independent_clusters = cluster_weights.len();
        let (cluster_weight_scale, sum_squared_cluster_weights) =
            scaled_sum_of_squares(cluster_weights.iter().copied())?;
        let effective_sample_size =
            kish_effective_sample_size(cluster_weights.iter().copied(), independent_clusters)?;
        let support = Self {
            observations: rows.len() as u64,
            independent_clusters: independent_clusters as u64,
            positives: positives as u64,
            negatives: (rows.len() - positives) as u64,
            total_weight,
            positive_weight,
            weight_scale,
            sum_squared_weights: sum_squares,
            cluster_weight_scale,
            sum_squared_cluster_weights,
            effective_sample_size,
        };
        if [
            total_weight,
            positive_weight,
            weight_scale,
            sum_squares,
            cluster_weight_scale,
            sum_squared_cluster_weights,
            effective_sample_size,
        ]
        .into_iter()
        .all(f64::is_finite)
            && total_weight > 0.0
            && effective_sample_size > 0.0
            && independent_clusters > 0
        {
            Ok(support)
        } else {
            Err(CalibrationError::InvalidObservationWeight)
        }
    }

    fn meets(self, config: SelectionConfig) -> bool {
        self.observations >= config.minimum_observations
            && self.positives >= config.minimum_positives
            && self.negatives >= config.minimum_negatives
            && self.effective_sample_size >= config.minimum_effective_sample_size
    }

    fn validate(self) -> Result<(), CalibrationError> {
        let expected_effective = stabilized_kish_from_summaries(
            self.total_weight,
            self.cluster_weight_scale,
            self.sum_squared_cluster_weights,
            self.independent_clusters,
        )?;
        let normalized_row_weight = self.total_weight / self.weight_scale;
        let normalized_cluster_weight = self.total_weight / self.cluster_weight_scale;
        let class_count = self
            .positives
            .checked_add(self.negatives)
            .ok_or(CalibrationError::InvalidObservationWeight)?;
        if self.observations == 0
            || self.independent_clusters == 0
            || self.independent_clusters > self.observations
            || class_count != self.observations
            || !self.total_weight.is_finite()
            || self.total_weight <= 0.0
            || !self.positive_weight.is_finite()
            || self.positive_weight < 0.0
            || self.positive_weight > self.total_weight
            || (self.positives == 0) != (self.positive_weight == 0.0)
            || !self.weight_scale.is_finite()
            || self.weight_scale <= 0.0
            || !self.cluster_weight_scale.is_finite()
            || self.cluster_weight_scale < self.weight_scale
            || !self.sum_squared_weights.is_finite()
            || !normalized_row_weight.is_finite()
            || normalized_row_weight < 1.0
            || normalized_row_weight > self.observations as f64 * (1.0 + 128.0 * f64::EPSILON)
            || self.sum_squared_weights < 1.0
            || self.sum_squared_weights > self.observations as f64 * (1.0 + 128.0 * f64::EPSILON)
            || !normalized_cluster_weight.is_finite()
            || normalized_cluster_weight < 1.0
            || normalized_cluster_weight
                > self.independent_clusters as f64 * (1.0 + 128.0 * f64::EPSILON)
            || self.sum_squared_cluster_weights < 1.0
            || self.sum_squared_cluster_weights
                > self.independent_clusters as f64 * (1.0 + 128.0 * f64::EPSILON)
            || !nearly_equal(self.effective_sample_size, expected_effective)
        {
            Err(CalibrationError::InvalidObservationWeight)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationUncertainty {
    pub lower: f64,
    pub upper: f64,
    pub confidence_level: f64,
    pub method: String,
}

impl CalibrationUncertainty {
    pub fn try_validation_base_rate_wilson_kish(
        probability: f64,
        effective_sample_size: f64,
    ) -> Result<Self, CalibrationError> {
        if !probability.is_finite()
            || !(0.0..=1.0).contains(&probability)
            || !effective_sample_size.is_finite()
            || effective_sample_size <= 0.0
        {
            return Err(CalibrationError::UndefinedMetric);
        }
        let z2 = WILSON_Z_95 * WILSON_Z_95;
        let denominator = 1.0 + z2 / effective_sample_size;
        let center = (probability + z2 / (2.0 * effective_sample_size)) / denominator;
        let radius = WILSON_Z_95
            * ((probability * (1.0 - probability) / effective_sample_size
                + z2 / (4.0 * effective_sample_size * effective_sample_size))
                .sqrt())
            / denominator;
        Ok(Self {
            lower: (center - radius).max(0.0),
            upper: (center + radius).min(1.0),
            confidence_level: 0.95,
            method: "validation-base-rate-wilson-kish-episode-block-approximation-95-v2".to_owned(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateScore {
    pub kind: CalibrationKind,
    pub eligible: bool,
    pub score: Option<f64>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionReport {
    pub config: SelectionConfig,
    pub metric: SelectionMetric,
    pub selected: CalibrationKind,
    pub candidates: Vec<CandidateScore>,
    pub fit_support: CalibrationSupport,
    pub selector_support: CalibrationSupport,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationArtifact {
    schema_version: u32,
    key: CalibrationKey,
    lineage: CalibrationLineage,
    pub(crate) method: CalibrationMethod,
    validation_base_rate: f64,
    support: CalibrationSupport,
    selection_population_support: CalibrationSupport,
    base_rate_uncertainty: CalibrationUncertainty,
    selection: SelectionReport,
    validation_evidence_hash: [u8; 32],
    artifact_id: [u8; 32],
}

impl CalibrationArtifact {
    pub const fn key(&self) -> &CalibrationKey {
        &self.key
    }

    pub const fn lineage(&self) -> &CalibrationLineage {
        &self.lineage
    }

    pub const fn support(&self) -> CalibrationSupport {
        self.support
    }

    pub const fn validation_base_rate(&self) -> f64 {
        self.validation_base_rate
    }

    pub const fn base_rate_uncertainty(&self) -> &CalibrationUncertainty {
        &self.base_rate_uncertainty
    }

    pub fn registry_descriptor(
        &self,
    ) -> Result<model_registry::CalibrationDescriptor, CalibrationError> {
        self.validate()?;
        let effective_sample_size = self.support.effective_sample_size.floor();
        if effective_sample_size < 8.0 || effective_sample_size > u64::MAX as f64 {
            return Err(CalibrationError::InsufficientSupport);
        }
        let validation_period = model_registry::PackagePeriod::new(
            self.lineage.outer_calibration_period.start_ns,
            self.lineage.outer_calibration_period.end_ns,
        )
        .map_err(|_| CalibrationError::InvalidPeriod)?;
        Ok(model_registry::CalibrationDescriptor {
            method: self.method.name().to_owned(),
            version: "calibration_artifact_v1".to_owned(),
            evidence_blake3: self.artifact_id,
            validation_period,
            effective_sample_size: effective_sample_size as u64,
        })
    }

    pub const fn selection_report(&self) -> &SelectionReport {
        &self.selection
    }

    pub const fn artifact_id(&self) -> [u8; 32] {
        self.artifact_id
    }

    pub fn method_name(&self) -> &'static str {
        self.method.name()
    }

    pub fn calibrate(&self, score: &BoundModelScore) -> Result<CalibratedOutput, CalibrationError> {
        self.validate()?;
        score.validate()?;
        if score.raw_model_hash != self.key.raw_model_hash
            || score.raw_model_training_hash != self.key.raw_model_training_hash
            || score.horizon_seconds != self.key.horizon_seconds
            || score.target_event_id != self.key.event_id
            || score.target_event_type != self.key.event_type
            || score.label_definition_hash != self.key.label_definition_hash
            || score.origin_time_ns < self.lineage.model_fitted_at_ns
        {
            return Err(CalibrationError::IncompatibleArtifact);
        }
        let raw_score = score.raw_score;
        let calibrated_probability = self.method.calibrate_logit(raw_score.logit)?;
        // This is explicitly the empirical validation-base-rate interval, not
        // a forecast-level or calibrator-parameter confidence interval.
        let uncertainty = self.base_rate_uncertainty.clone();
        let mut output = CalibratedOutput {
            schema_version: 1,
            key: self.key.clone(),
            raw_score,
            calibrated_probability,
            validation_base_rate: self.validation_base_rate,
            calibration_version: self.method.name().to_owned(),
            artifact_id: self.artifact_id,
            uncertainty,
            uncertainty_scope: "validation_base_rate".to_owned(),
            effective_sample_size: self.support.effective_sample_size,
            input_evidence_hash: score.evidence_hash,
            evidence_hash: [0; 32],
            status: CalibrationStatus::Experimental,
        };
        output.evidence_hash = output.calculate_hash();
        output.verify(self)?;
        Ok(output)
    }

    pub(crate) fn validate(&self) -> Result<(), CalibrationError> {
        self.key.validate()?;
        self.lineage.validate()?;
        self.selection.config.validate()?;
        self.support.validate()?;
        self.selection_population_support.validate()?;
        let selected_kind = match &self.method {
            CalibrationMethod::Platt(_) => CalibrationKind::Platt,
            CalibrationMethod::Beta(_) => CalibrationKind::Beta,
            CalibrationMethod::Isotonic(_) => CalibrationKind::Isotonic,
        };
        let fit = self.selection.fit_support;
        let selector = self.selection.selector_support;
        fit.validate()?;
        selector.validate()?;
        let expected_weight = compensated_sum([fit.total_weight, selector.total_weight])?;
        let expected_positive_weight =
            compensated_sum([fit.positive_weight, selector.positive_weight])?;
        let (expected_weight_scale, expected_squared_weight) = combine_scaled_squares(
            fit.weight_scale,
            fit.sum_squared_weights,
            selector.weight_scale,
            selector.sum_squared_weights,
        )?;
        let expected_observations = fit
            .observations
            .checked_add(selector.observations)
            .ok_or(CalibrationError::InvalidCalibrator)?;
        let expected_clusters = fit
            .independent_clusters
            .checked_add(selector.independent_clusters)
            .ok_or(CalibrationError::InvalidCalibrator)?;
        let expected_positives = fit
            .positives
            .checked_add(selector.positives)
            .ok_or(CalibrationError::InvalidCalibrator)?;
        let expected_negatives = fit
            .negatives
            .checked_add(selector.negatives)
            .ok_or(CalibrationError::InvalidCalibrator)?;
        let (expected_cluster_weight_scale, expected_squared_cluster_weight) =
            combine_scaled_squares(
                fit.cluster_weight_scale,
                fit.sum_squared_cluster_weights,
                selector.cluster_weight_scale,
                selector.sum_squared_cluster_weights,
            )?;
        let expected_effective = stabilized_kish_from_summaries(
            expected_weight,
            expected_cluster_weight_scale,
            expected_squared_cluster_weight,
            expected_clusters,
        )?;
        let expected_uncertainty = CalibrationUncertainty::try_validation_base_rate_wilson_kish(
            self.validation_base_rate,
            self.support.effective_sample_size,
        )?;
        if self.schema_version != 1
            || self.key.raw_model_training_hash != self.lineage.raw_model_training_hash
            || !self.validation_base_rate.is_finite()
            || !(0.0..=1.0).contains(&self.validation_base_rate)
            || self.validation_evidence_hash == [0; 32]
            || self.selection.config.metric != self.selection.metric
            || selected_kind != self.selection.selected
            || self.selection.candidates.len() != 3
            || !self.selection.candidates.iter().any(|candidate| {
                candidate.kind == selected_kind && candidate.eligible && candidate.score.is_some()
            })
            || self.selection_population_support.observations != expected_observations
            || self.selection_population_support.independent_clusters != expected_clusters
            || self.selection_population_support.positives != expected_positives
            || self.selection_population_support.negatives != expected_negatives
            || !nearly_equal(
                self.selection_population_support.total_weight,
                expected_weight,
            )
            || !nearly_equal(
                self.selection_population_support.positive_weight,
                expected_positive_weight,
            )
            || !nearly_equal(
                self.selection_population_support.weight_scale,
                expected_weight_scale,
            )
            || !nearly_equal(
                self.selection_population_support.sum_squared_weights,
                expected_squared_weight,
            )
            || !nearly_equal(
                self.selection_population_support.cluster_weight_scale,
                expected_cluster_weight_scale,
            )
            || !nearly_equal(
                self.selection_population_support
                    .sum_squared_cluster_weights,
                expected_squared_cluster_weight,
            )
            || !nearly_equal(
                self.selection_population_support.effective_sample_size,
                expected_effective,
            )
            || self.validation_base_rate != self.support.positive_weight / self.support.total_weight
            || self.support.independent_clusters == 0
            || self.support.independent_clusters > self.support.observations
            || self.support.effective_sample_size > self.support.independent_clusters as f64
            || self.base_rate_uncertainty != expected_uncertainty
            || self.artifact_id != self.calculate_hash()
            || self.method.calibrate_logit(0.0).is_err()
        {
            Err(CalibrationError::InvalidCalibrator)
        } else {
            Ok(())
        }
    }

    fn calculate_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(ARTIFACT_DOMAIN);
        hasher.update(&self.key.evidence_hash);
        hasher.update(&self.lineage.evidence_hash);
        hash_string(&mut hasher, self.method.name());
        hash_method(&mut hasher, &self.method);
        hash_f64(&mut hasher, self.validation_base_rate);
        hash_support(&mut hasher, self.support);
        hash_support(&mut hasher, self.selection_population_support);
        hash_f64(&mut hasher, self.base_rate_uncertainty.lower);
        hash_f64(&mut hasher, self.base_rate_uncertainty.upper);
        hash_f64(&mut hasher, self.base_rate_uncertainty.confidence_level);
        hash_string(&mut hasher, &self.base_rate_uncertainty.method);
        hash_selection(&mut hasher, &self.selection);
        hasher.update(&self.validation_evidence_hash);
        *hasher.finalize().as_bytes()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalibrationStatus {
    Experimental,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibratedOutput {
    pub schema_version: u32,
    pub key: CalibrationKey,
    pub raw_score: RawScore,
    pub calibrated_probability: f64,
    pub validation_base_rate: f64,
    pub calibration_version: String,
    pub artifact_id: [u8; 32],
    pub uncertainty: CalibrationUncertainty,
    pub uncertainty_scope: String,
    pub effective_sample_size: f64,
    pub input_evidence_hash: [u8; 32],
    pub evidence_hash: [u8; 32],
    pub status: CalibrationStatus,
}

impl CalibratedOutput {
    pub fn verify(&self, artifact: &CalibrationArtifact) -> Result<(), CalibrationError> {
        artifact.validate()?;
        self.raw_score.validate()?;
        let expected_probability = artifact.method.calibrate_logit(self.raw_score.logit)?;
        let expected_uncertainty = &artifact.base_rate_uncertainty;
        if self.schema_version != 1
            || self.key != artifact.key
            || self.artifact_id != artifact.artifact_id
            || self.calibrated_probability != expected_probability
            || self.validation_base_rate != artifact.validation_base_rate
            || self.calibration_version != artifact.method.name()
            || &self.uncertainty != expected_uncertainty
            || self.uncertainty_scope != "validation_base_rate"
            || self.effective_sample_size != artifact.support.effective_sample_size
            || self.input_evidence_hash == [0; 32]
            || self.evidence_hash != self.calculate_hash()
            || self.status != CalibrationStatus::Experimental
        {
            Err(CalibrationError::IncompatibleArtifact)
        } else {
            Ok(())
        }
    }

    fn calculate_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(OUTPUT_DOMAIN);
        hasher.update(&self.key.evidence_hash);
        hash_f64(&mut hasher, self.raw_score.logit);
        hash_f64(&mut hasher, self.raw_score.probability);
        hash_f64(&mut hasher, self.calibrated_probability);
        hash_f64(&mut hasher, self.validation_base_rate);
        hash_string(&mut hasher, &self.calibration_version);
        hasher.update(&self.artifact_id);
        hash_f64(&mut hasher, self.uncertainty.lower);
        hash_f64(&mut hasher, self.uncertainty.upper);
        hash_f64(&mut hasher, self.uncertainty.confidence_level);
        hash_string(&mut hasher, &self.uncertainty.method);
        hash_string(&mut hasher, &self.uncertainty_scope);
        hash_f64(&mut hasher, self.effective_sample_size);
        hasher.update(&self.input_evidence_hash);
        *hasher.finalize().as_bytes()
    }
}

pub fn select_validation_only(
    validation: &ValidationSet,
    config: SelectionConfig,
) -> Result<CalibrationArtifact, CalibrationError> {
    validation.key.validate()?;
    validation.lineage.validate()?;
    if validation.evidence_hash != validation.calculate_hash() {
        return Err(CalibrationError::InvalidLineage);
    }
    select_rows(
        validation.key.clone(),
        validation.lineage.clone(),
        &validation.fit.rows,
        &validation.selector.rows,
        validation.evidence_hash,
        config,
    )
}

fn select_rows(
    key: CalibrationKey,
    lineage: CalibrationLineage,
    fit_rows: &[ValidationRow],
    selector_rows: &[ValidationRow],
    validation_evidence_hash: [u8; 32],
    config: SelectionConfig,
) -> Result<CalibrationArtifact, CalibrationError> {
    config.validate()?;
    let fit_support = CalibrationSupport::from_rows(fit_rows)?;
    let selector_support = CalibrationSupport::from_rows(selector_rows)?;
    if !fit_support.meets(config) || !selector_support.meets(config) {
        return Err(CalibrationError::InsufficientSupport);
    }
    let fit_id = evidence_identifier("calibration_fit", validation_evidence_hash);
    let fit_set = low_level_set(&fit_id, fit_rows)?;
    let mut candidates = Vec::with_capacity(3);
    let mut fitted = Vec::with_capacity(3);
    for kind in [
        CalibrationKind::Platt,
        CalibrationKind::Beta,
        CalibrationKind::Isotonic,
    ] {
        if kind == CalibrationKind::Isotonic
            && fit_support.effective_sample_size < config.isotonic_minimum_effective_sample_size
        {
            candidates.push(CandidateScore {
                kind,
                eligible: false,
                score: None,
                reason: Some("effective_sample_size_below_isotonic_minimum".to_owned()),
            });
            continue;
        }
        let method = match fit_kind(kind, &fit_set, config.fit) {
            Ok(method) => method,
            Err(error) => {
                candidates.push(CandidateScore {
                    kind,
                    eligible: false,
                    score: None,
                    reason: Some(candidate_failure_reason(&error).to_owned()),
                });
                continue;
            }
        };
        let score = match score_rows(&method, selector_rows, config.metric) {
            Ok(score) => score,
            Err(error) => {
                candidates.push(CandidateScore {
                    kind,
                    eligible: false,
                    score: None,
                    reason: Some(candidate_failure_reason(&error).to_owned()),
                });
                continue;
            }
        };
        candidates.push(CandidateScore {
            kind,
            eligible: true,
            score: Some(score),
            reason: None,
        });
        fitted.push((kind, score));
    }
    fitted.sort_by(|left, right| {
        left.1
            .total_cmp(&right.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    if fitted.is_empty() {
        return Err(CalibrationError::NoEligibleCandidate);
    }
    let mut complete_rows = fit_rows.to_vec();
    complete_rows.extend_from_slice(selector_rows);
    complete_rows.sort_by_key(|row| (row.origin_time_ns, row.row_id));
    let complete_id = evidence_identifier("calibration_complete", validation_evidence_hash);
    let complete_set = low_level_set(&complete_id, &complete_rows)?;
    let mut selected_method = None;
    for (kind, _) in &fitted {
        match fit_kind(*kind, &complete_set, config.fit) {
            Ok(method) => {
                selected_method = Some((*kind, method));
                break;
            }
            Err(error) => {
                if let Some(candidate) = candidates.iter_mut().find(|value| value.kind == *kind) {
                    candidate.eligible = false;
                    candidate.score = None;
                    candidate.reason = Some(format!(
                        "complete_refit_{}",
                        candidate_failure_reason(&error)
                    ));
                }
            }
        }
    }
    let (selected, method) = selected_method.ok_or(CalibrationError::NoEligibleCandidate)?;
    let selection = SelectionReport {
        config,
        metric: config.metric,
        selected,
        candidates,
        fit_support,
        selector_support,
    };
    let support = CalibrationSupport::from_rows(&complete_rows)?;
    let validation_base_rate = support.positive_weight / support.total_weight;
    let base_rate_uncertainty = CalibrationUncertainty::try_validation_base_rate_wilson_kish(
        validation_base_rate,
        support.effective_sample_size,
    )?;
    let mut artifact = CalibrationArtifact {
        schema_version: 1,
        key,
        lineage,
        method,
        validation_base_rate,
        support,
        selection_population_support: support,
        base_rate_uncertainty,
        selection,
        validation_evidence_hash,
        artifact_id: [0; 32],
    };
    artifact.artifact_id = artifact.calculate_hash();
    artifact.validate()?;
    Ok(artifact)
}

fn candidate_failure_reason(error: &CalibrationError) -> &'static str {
    match error {
        CalibrationError::NonConverged => "optimizer_nonconvergence",
        CalibrationError::NonFiniteArithmetic => "nonfinite_candidate_arithmetic",
        CalibrationError::UndefinedMetric => "undefined_selector_metric",
        CalibrationError::WorkCapacity => "candidate_work_capacity",
        _ => "invalid_candidate",
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct HorizonScore {
    score: BoundModelScore,
}

impl HorizonScore {
    pub fn try_new(score: BoundModelScore) -> Result<Self, CalibrationError> {
        score.validate()?;
        Ok(Self { score })
    }

    pub const fn score(&self) -> &BoundModelScore {
        &self.score
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct JointCalibrationArtifact {
    artifacts: Vec<CalibrationArtifact>,
    shared_method: CalibrationMethod,
    evidence_hash: [u8; 32],
}

pub fn select_joint_validation_only(
    validations: &[ValidationSet],
    config: SelectionConfig,
) -> Result<JointCalibrationArtifact, CalibrationError> {
    if validations.len() < 2 || validations.len() > MAXIMUM_HORIZONS {
        return Err(CalibrationError::HorizonIncoherence);
    }
    let first = &validations[0];
    let mut horizons = BTreeSet::new();
    let mut fit_rows = Vec::new();
    let mut selector_rows = Vec::new();
    let mut combined_hasher = blake3::Hasher::new();
    combined_hasher.update(JOINT_DOMAIN);
    let mut previous_horizon = None;
    for validation in validations {
        if validation.key.event_id != first.key.event_id
            || validation.key.event_type != first.key.event_type
            || validation.key.liquidity_class != first.key.liquidity_class
            || validation.key.label_definition_hash != first.key.label_definition_hash
            || validation.key.raw_model_hash != first.key.raw_model_hash
            || validation.key.raw_model_training_hash != first.key.raw_model_training_hash
            || validation.lineage != first.lineage
            || !horizons.insert(validation.key.horizon_seconds)
            || previous_horizon.is_some_and(|horizon| validation.key.horizon_seconds <= horizon)
        {
            return Err(CalibrationError::IncompatibleArtifact);
        }
        combined_hasher.update(&validation.evidence_hash);
        fit_rows.extend_from_slice(&validation.fit.rows);
        selector_rows.extend_from_slice(&validation.selector.rows);
        previous_horizon = Some(validation.key.horizon_seconds);
    }
    fit_rows.sort_by_key(|row| (row.origin_time_ns, row.row_id, row.key_hash));
    selector_rows.sort_by_key(|row| (row.origin_time_ns, row.row_id, row.key_hash));
    let combined_evidence = *combined_hasher.finalize().as_bytes();
    let shared = select_rows(
        first.key.clone(),
        first.lineage.clone(),
        &fit_rows,
        &selector_rows,
        combined_evidence,
        config,
    )?;
    let shared_method = shared.method.clone();
    let artifacts = validations
        .iter()
        .map(|validation| {
            let mut artifact = shared.clone();
            let mut horizon_rows = validation.fit.rows.clone();
            horizon_rows.extend_from_slice(&validation.selector.rows);
            horizon_rows.sort_by_key(|row| (row.origin_time_ns, row.row_id));
            let support = CalibrationSupport::from_rows(&horizon_rows)?;
            artifact.key = validation.key.clone();
            artifact.support = support;
            artifact.validation_base_rate = support.positive_weight / support.total_weight;
            artifact.base_rate_uncertainty =
                CalibrationUncertainty::try_validation_base_rate_wilson_kish(
                    artifact.validation_base_rate,
                    support.effective_sample_size,
                )?;
            artifact.validation_evidence_hash = validation.evidence_hash;
            artifact.artifact_id = artifact.calculate_hash();
            artifact.validate()?;
            Ok(artifact)
        })
        .collect::<Result<Vec<_>, CalibrationError>>()?;
    let mut joint = JointCalibrationArtifact {
        artifacts,
        shared_method,
        evidence_hash: [0; 32],
    };
    joint.evidence_hash = joint.calculate_hash();
    joint.validate()?;
    Ok(joint)
}

impl JointCalibrationArtifact {
    pub fn artifacts(&self) -> &[CalibrationArtifact] {
        &self.artifacts
    }

    pub fn calibrate_curve(
        &self,
        scores: &[HorizonScore],
    ) -> Result<Vec<CalibratedOutput>, CalibrationError> {
        self.validate()?;
        if scores.len() != self.artifacts.len() {
            return Err(CalibrationError::HorizonIncoherence);
        }
        let first = scores.first().ok_or(CalibrationError::HorizonIncoherence)?;
        let mut previous_raw = None;
        let mut previous_calibrated = None;
        let mut outputs = Vec::with_capacity(scores.len());
        for (artifact, score) in self.artifacts.iter().zip(scores) {
            if score.score.horizon_seconds != artifact.key.horizon_seconds
                || score.score.curve_evidence_hash != first.score.curve_evidence_hash
                || score.score.origin_time_ns != first.score.origin_time_ns
                || score.score.entity_id != first.score.entity_id
                || previous_raw
                    .is_some_and(|value| score.score.raw_score.probability + 1.0e-15 < value)
            {
                return Err(CalibrationError::HorizonIncoherence);
            }
            let output = artifact.calibrate(&score.score)?;
            if previous_calibrated
                .is_some_and(|value| output.calibrated_probability + 1.0e-15 < value)
            {
                return Err(CalibrationError::HorizonIncoherence);
            }
            previous_raw = Some(score.score.raw_score.probability);
            previous_calibrated = Some(output.calibrated_probability);
            outputs.push(output);
        }
        Ok(outputs)
    }

    fn validate(&self) -> Result<(), CalibrationError> {
        if self.artifacts.len() < 2
            || self.artifacts.len() > MAXIMUM_HORIZONS
            || self.evidence_hash != self.calculate_hash()
            || self.artifacts.iter().any(|artifact| {
                artifact.validate().is_err() || artifact.method != self.shared_method
            })
            || self.artifacts.windows(2).any(|pair| {
                pair[0].key.horizon_seconds >= pair[1].key.horizon_seconds
                    || pair[0].key.event_id != pair[1].key.event_id
                    || pair[0].key.event_type != pair[1].key.event_type
                    || pair[0].key.liquidity_class != pair[1].key.liquidity_class
                    || pair[0].key.raw_model_hash != pair[1].key.raw_model_hash
                    || pair[0].key.raw_model_training_hash != pair[1].key.raw_model_training_hash
                    || pair[0].lineage != pair[1].lineage
            })
        {
            Err(CalibrationError::HorizonIncoherence)
        } else {
            Ok(())
        }
    }

    fn calculate_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(JOINT_DOMAIN);
        hash_string(&mut hasher, self.shared_method.name());
        hash_method(&mut hasher, &self.shared_method);
        for artifact in &self.artifacts {
            hasher.update(&artifact.artifact_id);
        }
        *hasher.finalize().as_bytes()
    }
}

fn low_level_set(name: &str, rows: &[ValidationRow]) -> Result<CalibrationSet, CalibrationError> {
    let start = rows
        .iter()
        .map(|row| row.origin_time_ns)
        .min()
        .ok_or(CalibrationError::ObservationCapacity)?;
    let end = rows
        .iter()
        .map(|row| row.outcome_known_at_ns)
        .max()
        .and_then(|value| value.checked_add(1))
        .ok_or(CalibrationError::ObservationCapacity)?;
    let observations = rows
        .iter()
        .map(|row| {
            CalibrationObservation::from_logit(
                row.raw_score.logit,
                row.outcome,
                row.origin_time_ns,
                row.outcome_known_at_ns,
                row.outcome_known_at_ns,
                row.weight,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    CalibrationSet::new(name, CalibrationPeriod::new(start, end)?, observations)
}

fn fit_kind(
    kind: CalibrationKind,
    set: &CalibrationSet,
    config: FitConfig,
) -> Result<CalibrationMethod, CalibrationError> {
    match kind {
        CalibrationKind::Platt => PlattCalibrator::fit(set, config).map(CalibrationMethod::Platt),
        CalibrationKind::Beta => BetaCalibrator::fit(set, config).map(CalibrationMethod::Beta),
        CalibrationKind::Isotonic => IsotonicCalibrator::fit(set).map(CalibrationMethod::Isotonic),
    }
}

fn score_rows(
    method: &CalibrationMethod,
    rows: &[ValidationRow],
    metric: SelectionMetric,
) -> Result<f64, CalibrationError> {
    let mut total = 0.0;
    let mut weight_sum = 0.0;
    for row in rows {
        let probability = method.calibrate_logit(row.raw_score.logit)?;
        total += row.weight
            * match metric {
                SelectionMetric::LogLoss => {
                    if row.outcome {
                        if probability == 0.0 {
                            return Err(CalibrationError::UndefinedMetric);
                        }
                        -probability.ln()
                    } else {
                        if probability == 1.0 {
                            return Err(CalibrationError::UndefinedMetric);
                        }
                        -(1.0 - probability).ln()
                    }
                }
                SelectionMetric::Brier => {
                    let residual = probability - f64::from(row.outcome);
                    residual * residual
                }
            };
        weight_sum += row.weight;
    }
    let score = total / weight_sum;
    if score.is_finite() {
        Ok(score)
    } else {
        Err(CalibrationError::NonFiniteArithmetic)
    }
}

fn event_type_name(event_type: EventType) -> &'static str {
    match event_type {
        EventType::Downside => "downside",
        EventType::Upside => "upside",
        EventType::VolatilityExplosion => "volatility_explosion",
        EventType::LiquidityVacuum => "liquidity_vacuum",
        EventType::LiquidationCascade => "liquidation_cascade",
    }
}

fn period_from_range(range: dataset::TimeRange) -> Result<CalibrationPeriod, CalibrationError> {
    CalibrationPeriod::new(range.start_ns(), range.end_ns())
}

fn add_seconds(origin_ns: i64, seconds: u64) -> Result<i64, CalibrationError> {
    let nanoseconds = i64::try_from(seconds)
        .ok()
        .and_then(|value| value.checked_mul(NANOS_PER_SECOND))
        .ok_or(CalibrationError::InvalidObservationTime)?;
    origin_ns
        .checked_add(nanoseconds)
        .ok_or(CalibrationError::InvalidObservationTime)
}

fn validate_identifier(value: &str) -> Result<(), CalibrationError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        Err(CalibrationError::InvalidIdentifier)
    } else {
        Ok(())
    }
}

fn hash_method(hasher: &mut blake3::Hasher, method: &CalibrationMethod) {
    match method {
        CalibrationMethod::Platt(value) => {
            hash_u32(hasher, value.schema_version);
            hash_string(hasher, &value.calibration_evidence_id);
            hash_f64(hasher, value.slope);
            hash_f64(hasher, value.intercept);
            hash_u64(hasher, value.observations);
            hash_f64(hasher, value.effective_sample_size);
            hash_f64(hasher, value.convergence_tolerance);
            hash_u32(hasher, value.diagnostics.iterations);
            hash_f64(hasher, value.diagnostics.objective);
            hash_f64(hasher, value.diagnostics.projected_gradient_norm);
            hash_u64(hasher, value.diagnostics.objective_evaluations);
            hash_f64(hasher, value.diagnostics.feature_log_scale_span);
            hasher.update(&[fit_termination_code(value.diagnostics.termination)]);
        }
        CalibrationMethod::Beta(value) => {
            hash_u32(hasher, value.schema_version);
            hash_string(hasher, &value.calibration_evidence_id);
            hash_f64(hasher, value.a);
            hash_f64(hasher, value.b);
            hash_f64(hasher, value.c);
            hash_u64(hasher, value.observations);
            hash_f64(hasher, value.effective_sample_size);
            hash_f64(hasher, value.convergence_tolerance);
            hash_u32(hasher, value.diagnostics.iterations);
            hash_f64(hasher, value.diagnostics.objective);
            hash_f64(hasher, value.diagnostics.projected_gradient_norm);
            hash_u64(hasher, value.diagnostics.objective_evaluations);
            hash_f64(hasher, value.diagnostics.feature_log_scale_span);
            hasher.update(&[fit_termination_code(value.diagnostics.termination)]);
        }
        CalibrationMethod::Isotonic(value) => {
            hash_u32(hasher, value.schema_version);
            hash_string(hasher, &value.calibration_evidence_id);
            hash_u64(hasher, value.upper_logits.len() as u64);
            for (upper, calibrated) in value
                .upper_logits
                .iter()
                .zip(&value.calibrated_probabilities)
            {
                hash_f64(hasher, *upper);
                hash_f64(hasher, *calibrated);
            }
            hash_u64(hasher, value.observations);
            hash_f64(hasher, value.effective_sample_size);
        }
    }
}

fn hash_support(hasher: &mut blake3::Hasher, support: CalibrationSupport) {
    hash_u64(hasher, support.observations);
    hash_u64(hasher, support.independent_clusters);
    hash_u64(hasher, support.positives);
    hash_u64(hasher, support.negatives);
    hash_f64(hasher, support.total_weight);
    hash_f64(hasher, support.positive_weight);
    hash_f64(hasher, support.weight_scale);
    hash_f64(hasher, support.sum_squared_weights);
    hash_f64(hasher, support.cluster_weight_scale);
    hash_f64(hasher, support.sum_squared_cluster_weights);
    hash_f64(hasher, support.effective_sample_size);
}

#[derive(Clone, Copy, Debug, Default)]
struct CompensatedSum {
    sum: f64,
    compensation: f64,
}

impl CompensatedSum {
    fn add(&mut self, value: f64) -> Result<(), CalibrationError> {
        if !value.is_finite() || value < 0.0 {
            return Err(CalibrationError::InvalidObservationWeight);
        }
        let adjusted = value - self.compensation;
        let next = self.sum + adjusted;
        if !next.is_finite() {
            return Err(CalibrationError::InvalidObservationWeight);
        }
        self.compensation = (next - self.sum) - adjusted;
        self.sum = next;
        Ok(())
    }

    const fn value(&self) -> f64 {
        self.sum
    }
}

fn compensated_sum(weights: impl IntoIterator<Item = f64>) -> Result<f64, CalibrationError> {
    let mut accumulator = CompensatedSum::default();
    for weight in weights {
        accumulator.add(weight)?;
    }
    Ok(accumulator.value())
}

fn scaled_sum_of_squares(
    weights: impl IntoIterator<Item = f64>,
) -> Result<(f64, f64), CalibrationError> {
    let weights = weights.into_iter().collect::<Vec<_>>();
    let scale = weights.iter().copied().fold(0.0_f64, f64::max);
    if weights.is_empty()
        || !scale.is_finite()
        || scale <= 0.0
        || weights
            .iter()
            .any(|weight| !weight.is_finite() || *weight <= 0.0)
    {
        return Err(CalibrationError::InvalidObservationWeight);
    }
    let mut sum = 0.0;
    let mut compensation = 0.0;
    for weight in weights {
        let normalized = weight / scale;
        let square = normalized * normalized;
        let adjusted = square - compensation;
        let next = sum + adjusted;
        compensation = (next - sum) - adjusted;
        sum = next;
    }
    if sum.is_finite() && sum > 0.0 {
        Ok((scale, sum))
    } else {
        Err(CalibrationError::InvalidObservationWeight)
    }
}

fn combine_scaled_squares(
    left_scale: f64,
    left_sum: f64,
    right_scale: f64,
    right_sum: f64,
) -> Result<(f64, f64), CalibrationError> {
    if [left_scale, left_sum, right_scale, right_sum]
        .into_iter()
        .any(|value| !value.is_finite() || value <= 0.0)
    {
        return Err(CalibrationError::InvalidObservationWeight);
    }
    let scale = left_scale.max(right_scale);
    let left_ratio = left_scale / scale;
    let right_ratio = right_scale / scale;
    let sum = left_sum * left_ratio * left_ratio + right_sum * right_ratio * right_ratio;
    if sum.is_finite() && sum > 0.0 {
        Ok((scale, sum))
    } else {
        Err(CalibrationError::InvalidObservationWeight)
    }
}

fn stabilized_kish_from_summaries(
    total_weight: f64,
    weight_scale: f64,
    sum_squared_weights: f64,
    observations: u64,
) -> Result<f64, CalibrationError> {
    if !total_weight.is_finite()
        || total_weight <= 0.0
        || !weight_scale.is_finite()
        || weight_scale <= 0.0
        || !sum_squared_weights.is_finite()
        || sum_squared_weights <= 0.0
        || observations == 0
    {
        return Err(CalibrationError::InvalidObservationWeight);
    }
    let normalized_total = total_weight / weight_scale;
    let raw = normalized_total * normalized_total / sum_squared_weights;
    let ceiling = observations as f64;
    if !raw.is_finite() || raw <= 0.0 || raw > ceiling * (1.0 + 128.0 * f64::EPSILON) {
        return Err(CalibrationError::InvalidObservationWeight);
    }
    Ok(raw.min(ceiling))
}

fn nearly_equal(left: f64, right: f64) -> bool {
    left.is_finite()
        && right.is_finite()
        && (left == right
            || (left - right).abs() <= 128.0 * f64::EPSILON * left.abs().max(right.abs()))
}

const fn fit_termination_code(termination: crate::platt::FitTermination) -> u8 {
    match termination {
        crate::platt::FitTermination::ManualReference => 1,
        crate::platt::FitTermination::ProjectedGradientTolerance => 2,
    }
}

fn hash_selection(hasher: &mut blake3::Hasher, selection: &SelectionReport) {
    hash_u32(hasher, selection.config.fit.maximum_iterations);
    hash_u32(hasher, selection.config.fit.maximum_backtracking_steps);
    hash_f64(hasher, selection.config.fit.tolerance);
    hash_f64(hasher, selection.config.fit.l2_penalty);
    hash_u64(hasher, selection.config.minimum_observations);
    hash_u64(hasher, selection.config.minimum_positives);
    hash_u64(hasher, selection.config.minimum_negatives);
    hash_f64(hasher, selection.config.minimum_effective_sample_size);
    hash_f64(
        hasher,
        selection.config.isotonic_minimum_effective_sample_size,
    );
    hasher.update(&[match selection.config.metric {
        SelectionMetric::LogLoss => 1,
        SelectionMetric::Brier => 2,
    }]);
    hasher.update(&[match selection.metric {
        SelectionMetric::LogLoss => 1,
        SelectionMetric::Brier => 2,
    }]);
    hasher.update(&[selection.selected as u8]);
    for candidate in &selection.candidates {
        hasher.update(&[candidate.kind as u8, u8::from(candidate.eligible)]);
        if let Some(score) = candidate.score {
            hasher.update(&[1]);
            hash_f64(hasher, score);
        } else {
            hasher.update(&[0]);
        }
        if let Some(reason) = &candidate.reason {
            hash_string(hasher, reason);
        } else {
            hash_string(hasher, "");
        }
    }
    hash_support(hasher, selection.fit_support);
    hash_support(hasher, selection.selector_support);
}

fn hash_period(hasher: &mut blake3::Hasher, period: CalibrationPeriod) {
    hash_i64(hasher, period.start_ns);
    hash_i64(hasher, period.end_ns);
}

fn hash_string(hasher: &mut blake3::Hasher, value: &str) {
    hash_u64(hasher, value.len() as u64);
    hasher.update(value.as_bytes());
}

fn curve_evidence_hash(
    entity_id: &str,
    origin_time_ns: i64,
    input_evidence_hash: [u8; 32],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"cmti:prediction-curve:v1\0");
    hash_string(&mut hasher, entity_id);
    hash_i64(&mut hasher, origin_time_ns);
    hasher.update(&input_evidence_hash);
    *hasher.finalize().as_bytes()
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

fn evidence_identifier(prefix: &str, digest: [u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(prefix.len() + 1 + 64);
    value.push_str(prefix);
    value.push('_');
    for byte in digest {
        value.push(char::from(HEX[usize::from(byte >> 4)]));
        value.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_equal_weight_support_uses_compensated_summaries() {
        const OBSERVATIONS: usize = 100_000;
        let raw_score = RawScore::try_from_logit(0.0).expect("raw score");
        let rows = (0..OBSERVATIONS)
            .map(|index| ValidationRow {
                row_id: u64::try_from(index + 1).expect("row id"),
                entity_id: "asset".to_owned(),
                episode_cluster_id: "one_episode".to_owned(),
                key_hash: [1; 32],
                raw_score,
                outcome: index % 2 == 0,
                origin_time_ns: i64::try_from(index + 1).expect("origin"),
                outcome_known_at_ns: i64::try_from(index + 2).expect("outcome time"),
                weight: 0.1,
                source_evidence_hash: [2; 32],
                outer_fold_hash: [3; 32],
            })
            .collect::<Vec<_>>();

        let support = CalibrationSupport::from_rows(&rows).expect("support");
        support.validate().expect("revalidated support");
        assert_eq!(support.total_weight / support.weight_scale, 100_000.0);
        assert_eq!(support.positive_weight / support.weight_scale, 50_000.0);
        assert_eq!(support.effective_sample_size, 1.0);
    }
}
