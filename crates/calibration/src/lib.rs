//! Deterministic, point-in-time probability calibration and evaluation.
//!
//! Calibration is intentionally fitted on a typed validation period. An
//! [`EvaluationSet`] must start strictly after that period, preventing the
//! outer evaluation segment from being reused through this API.

mod beta;
mod contract;
mod isotonic;
mod platt;
mod select;

pub use beta::BetaCalibrator;
pub use contract::{
    BoundModelScore, CalibratedOutput, CalibrationArtifact, CalibrationKey, CalibrationKind,
    CalibrationLineage, CalibrationStatus, CalibrationSupport, CalibrationUncertainty,
    CandidateScore, HorizonScore, JointCalibrationArtifact, LiquidityClass, RawScore,
    SelectionConfig, SelectionMetric, SelectionReport, ValidationPartition, ValidationRow,
    ValidationSet, select_joint_validation_only, select_validation_only,
};
pub use isotonic::IsotonicCalibrator;
pub use platt::{FitDiagnostics, FitTermination, PlattCalibrator};
pub use select::{
    BaselineForecastSet, BaselinePrediction, CalibrationMethod, ProbabilityMetrics, ReliabilityBin,
    evaluate_probability_metrics,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

const MINIMUM_OBSERVATIONS: usize = 8;
const MAXIMUM_OBSERVATIONS: usize = 1_000_000;
pub(crate) const MAXIMUM_ABS_LOGIT: f64 = 1_000_000.0;

/// A closed-open time period `[start_ns, end_ns)`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CalibrationPeriod {
    pub start_ns: i64,
    pub end_ns: i64,
}

impl CalibrationPeriod {
    pub fn new(start_ns: i64, end_ns: i64) -> Result<Self, CalibrationError> {
        let period = Self { start_ns, end_ns };
        validate_period(period)?;
        Ok(period)
    }
}

/// One binary outcome and the probability emitted before that outcome.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalibrationObservation {
    raw_logit: f64,
    uncalibrated_probability: f64,
    weight: f64,
    outcome: bool,
    origin_time_ns: i64,
    outcome_time_ns: i64,
    as_known_at_ns: i64,
}

impl CalibrationObservation {
    pub fn new(
        uncalibrated_probability: f64,
        outcome: bool,
        origin_time_ns: i64,
        outcome_time_ns: i64,
        as_known_at_ns: i64,
    ) -> Result<Self, CalibrationError> {
        let raw_logit = logit_from_open_probability(uncalibrated_probability)?;
        if origin_time_ns <= 0
            || outcome_time_ns <= origin_time_ns
            || as_known_at_ns < outcome_time_ns
        {
            return Err(CalibrationError::InvalidObservationTime);
        }
        Ok(Self {
            raw_logit,
            uncalibrated_probability,
            weight: 1.0,
            outcome,
            origin_time_ns,
            outcome_time_ns,
            as_known_at_ns,
        })
    }

    /// Builds one observation from the model's finite raw logit. This is the
    /// authoritative path because it remains defined when sigmoid rounds to
    /// an exact probability endpoint.
    pub fn from_logit(
        raw_logit: f64,
        outcome: bool,
        origin_time_ns: i64,
        outcome_time_ns: i64,
        as_known_at_ns: i64,
        weight: f64,
    ) -> Result<Self, CalibrationError> {
        if !raw_logit.is_finite() || raw_logit.abs() > MAXIMUM_ABS_LOGIT {
            return Err(CalibrationError::NonFiniteArithmetic);
        }
        if !weight.is_finite() || weight <= 0.0 || weight > 1.0e12 {
            return Err(CalibrationError::InvalidObservationWeight);
        }
        if origin_time_ns <= 0
            || outcome_time_ns <= origin_time_ns
            || as_known_at_ns < outcome_time_ns
        {
            return Err(CalibrationError::InvalidObservationTime);
        }
        Ok(Self {
            raw_logit,
            uncalibrated_probability: sigmoid(raw_logit)?,
            weight,
            outcome,
            origin_time_ns,
            outcome_time_ns,
            as_known_at_ns,
        })
    }

    pub const fn raw_logit(self) -> f64 {
        self.raw_logit
    }

    pub const fn uncalibrated_probability(self) -> f64 {
        self.uncalibrated_probability
    }

    pub const fn outcome(self) -> bool {
        self.outcome
    }

    pub const fn weight(self) -> f64 {
        self.weight
    }

    pub const fn origin_time_ns(self) -> i64 {
        self.origin_time_ns
    }

    pub const fn outcome_time_ns(self) -> i64 {
        self.outcome_time_ns
    }

    pub const fn as_known_at_ns(self) -> i64 {
        self.as_known_at_ns
    }
}

/// Later validation observations used only to fit a calibrator.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalibrationSet {
    evidence_id: String,
    period: CalibrationPeriod,
    observations: Vec<CalibrationObservation>,
}

impl CalibrationSet {
    pub fn new(
        evidence_id: impl Into<String>,
        period: CalibrationPeriod,
        observations: Vec<CalibrationObservation>,
    ) -> Result<Self, CalibrationError> {
        let evidence_id = evidence_id.into();
        validate_identifier(&evidence_id)?;
        validate_observations(period, &observations)?;
        Ok(Self {
            evidence_id,
            period,
            observations,
        })
    }

    pub(crate) fn validate(&self) -> Result<(), CalibrationError> {
        validate_identifier(&self.evidence_id)?;
        validate_period(self.period)?;
        validate_observations(self.period, &self.observations)
    }

    pub fn evidence_id(&self) -> &str {
        &self.evidence_id
    }

    pub const fn period(&self) -> CalibrationPeriod {
        self.period
    }

    pub fn observations(&self) -> &[CalibrationObservation] {
        &self.observations
    }
}

/// Untouched observations used after calibrator fitting and selection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvaluationSet {
    evidence_id: String,
    period: CalibrationPeriod,
    calibration_period: CalibrationPeriod,
    artifact_id: [u8; 32],
    key_hash: [u8; 32],
    raw_model_hash: [u8; 32],
    raw_model_training_hash: [u8; 32],
    outer_fold_hash: [u8; 32],
    observations: Vec<CalibrationObservation>,
    row_evidence: Vec<EvaluationRowEvidence>,
    evidence_hash: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvaluationRowEvidence {
    row_id: u64,
    entity_id: String,
    source_evidence_hash: [u8; 32],
}

impl EvaluationSet {
    pub fn new(
        artifact: &CalibrationArtifact,
        evidence_id: impl Into<String>,
        rows: Vec<ValidationRow>,
    ) -> Result<Self, CalibrationError> {
        artifact.validate()?;
        let evidence_id = evidence_id.into();
        let period = artifact.lineage().test_period();
        let row_evidence = rows
            .iter()
            .map(|row| EvaluationRowEvidence {
                row_id: row.row_id,
                entity_id: row.entity_id.clone(),
                source_evidence_hash: row.source_evidence_hash,
            })
            .collect();
        let observations = rows
            .into_iter()
            .map(|row| {
                if row.key_hash != artifact.key().evidence_hash()
                    || row.origin_time_ns < period.start_ns
                    || row.origin_time_ns >= period.end_ns
                    || row.outcome_known_at_ns > period.end_ns
                    || row.source_evidence_hash == [0; 32]
                {
                    return Err(CalibrationError::RowOutsidePartition);
                }
                CalibrationObservation::from_logit(
                    row.raw_score.logit(),
                    row.outcome,
                    row.origin_time_ns,
                    row.outcome_known_at_ns,
                    row.outcome_known_at_ns,
                    row.weight,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut evaluation = Self {
            evidence_id,
            period,
            calibration_period: artifact.lineage().calibration_period(),
            artifact_id: artifact.artifact_id(),
            key_hash: artifact.key().evidence_hash(),
            raw_model_hash: artifact.key().raw_model_hash(),
            raw_model_training_hash: artifact.key().raw_model_training_hash(),
            outer_fold_hash: artifact.lineage().outer_fold_hash(),
            observations,
            row_evidence,
            evidence_hash: [0; 32],
        };
        evaluation.evidence_hash = evaluation.calculate_hash();
        evaluation.validate()?;
        Ok(evaluation)
    }

    pub(crate) fn validate(&self) -> Result<(), CalibrationError> {
        validate_identifier(&self.evidence_id)?;
        validate_period(self.period)?;
        validate_period(self.calibration_period)?;
        let mut identities = std::collections::BTreeSet::new();
        if self.period.start_ns < self.calibration_period.end_ns
            || self.artifact_id == [0; 32]
            || self.key_hash == [0; 32]
            || self.raw_model_hash == [0; 32]
            || self.raw_model_training_hash == [0; 32]
            || self.outer_fold_hash == [0; 32]
            || self.observations.len() != self.row_evidence.len()
            || self.row_evidence.iter().any(|row| {
                row.row_id == 0
                    || row.entity_id.is_empty()
                    || row.entity_id.len() > 128
                    || !row.entity_id.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'
                    })
                    || row.source_evidence_hash == [0; 32]
                    || !identities.insert((row.row_id, row.entity_id.clone()))
            })
            || self.evidence_hash != self.calculate_hash()
        {
            return Err(CalibrationError::InvalidLineage);
        }
        validate_observations(self.period, &self.observations)
    }

    pub(crate) fn validate_for_artifact(
        &self,
        artifact: &CalibrationArtifact,
    ) -> Result<(), CalibrationError> {
        self.validate()?;
        artifact.validate()?;
        if self.artifact_id != artifact.artifact_id()
            || self.key_hash != artifact.key().evidence_hash()
            || self.raw_model_hash != artifact.key().raw_model_hash()
            || self.raw_model_training_hash != artifact.key().raw_model_training_hash()
            || self.outer_fold_hash != artifact.lineage().outer_fold_hash()
            || self.calibration_period != artifact.lineage().calibration_period()
            || self.period != artifact.lineage().test_period()
        {
            return Err(CalibrationError::IncompatibleArtifact);
        }
        Ok(())
    }

    pub fn evidence_id(&self) -> &str {
        &self.evidence_id
    }

    pub const fn period(&self) -> CalibrationPeriod {
        self.period
    }

    pub const fn calibration_period(&self) -> CalibrationPeriod {
        self.calibration_period
    }

    pub fn observations(&self) -> &[CalibrationObservation] {
        &self.observations
    }

    pub const fn evidence_hash(&self) -> [u8; 32] {
        self.evidence_hash
    }

    pub(crate) fn row_evidence(&self) -> &[EvaluationRowEvidence] {
        &self.row_evidence
    }

    fn calculate_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"cmti:artifact-evaluation-set:v1\0");
        hasher.update(&(self.evidence_id.len() as u64).to_le_bytes());
        hasher.update(self.evidence_id.as_bytes());
        hasher.update(&self.period.start_ns.to_le_bytes());
        hasher.update(&self.period.end_ns.to_le_bytes());
        hasher.update(&self.calibration_period.start_ns.to_le_bytes());
        hasher.update(&self.calibration_period.end_ns.to_le_bytes());
        hasher.update(&self.artifact_id);
        hasher.update(&self.key_hash);
        hasher.update(&self.raw_model_hash);
        hasher.update(&self.raw_model_training_hash);
        hasher.update(&self.outer_fold_hash);
        hasher.update(&(self.observations.len() as u64).to_le_bytes());
        for (observation, evidence) in self.observations.iter().zip(&self.row_evidence) {
            hasher.update(&evidence.row_id.to_le_bytes());
            hasher.update(&(evidence.entity_id.len() as u64).to_le_bytes());
            hasher.update(evidence.entity_id.as_bytes());
            hasher.update(&evidence.source_evidence_hash);
            hasher.update(&observation.raw_logit.to_bits().to_le_bytes());
            hasher.update(&[u8::from(observation.outcome)]);
            hasher.update(&observation.origin_time_ns.to_le_bytes());
            hasher.update(&observation.outcome_time_ns.to_le_bytes());
            hasher.update(&observation.as_known_at_ns.to_le_bytes());
            hasher.update(&observation.weight.to_bits().to_le_bytes());
        }
        *hasher.finalize().as_bytes()
    }
}

/// Bounded deterministic optimizer configuration.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FitConfig {
    pub maximum_iterations: u32,
    pub maximum_backtracking_steps: u32,
    pub tolerance: f64,
    pub l2_penalty: f64,
}

impl Default for FitConfig {
    fn default() -> Self {
        Self {
            maximum_iterations: 4_000,
            maximum_backtracking_steps: 32,
            tolerance: 1e-6,
            l2_penalty: 1e-3,
        }
    }
}

impl FitConfig {
    pub(crate) fn validate(self, observations: usize) -> Result<(), CalibrationError> {
        if self.maximum_iterations == 0
            || self.maximum_iterations > 100_000
            || self.maximum_backtracking_steps == 0
            || self.maximum_backtracking_steps > 128
            || !self.tolerance.is_finite()
            || !(0.0..=1e-2).contains(&self.tolerance)
            || self.tolerance == 0.0
            || !self.l2_penalty.is_finite()
            || !(0.0..=1.0).contains(&self.l2_penalty)
        {
            return Err(CalibrationError::InvalidFitConfig);
        }
        let work = usize::try_from(self.maximum_iterations)
            .ok()
            .and_then(|iterations| iterations.checked_mul(observations))
            .and_then(|work| {
                usize::try_from(self.maximum_backtracking_steps)
                    .ok()
                    .and_then(|steps| work.checked_mul(steps))
            })
            .ok_or(CalibrationError::WorkCapacity)?;
        if work > 16_000_000_000 {
            return Err(CalibrationError::WorkCapacity);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum CalibrationError {
    #[error("calibration period is invalid")]
    InvalidPeriod,
    #[error("calibration evidence identifier is invalid")]
    InvalidIdentifier,
    #[error("probability is not finite or outside [0, 1]")]
    InvalidProbability,
    #[error("exact endpoint probability requires an explicit upstream policy")]
    EndpointProbability,
    #[error("observation time or knowledge boundary is invalid")]
    InvalidObservationTime,
    #[error("observation weight is not finite, positive, or bounded")]
    InvalidObservationWeight,
    #[error("calibration target or lineage is invalid")]
    InvalidLineage,
    #[error("calibration row identity or evidence is invalid")]
    InvalidRow,
    #[error("calibration validation partitions overlap or violate the embargo")]
    ValidationLeakage,
    #[error("calibration rows do not match their declared partition")]
    RowOutsidePartition,
    #[error("calibration row identity is duplicated")]
    DuplicateRow,
    #[error("calibration outcome is censored or excluded")]
    IneligibleOutcome,
    #[error("calibration sample support is below the declared minimum")]
    InsufficientSupport,
    #[error("calibration selector has no eligible candidate")]
    NoEligibleCandidate,
    #[error("calibration artifact, score, or evaluation context is incompatible")]
    IncompatibleArtifact,
    #[error("cumulative horizon scores are not strictly keyed and monotone")]
    HorizonIncoherence,
    #[error("observation does not belong to the declared period")]
    ObservationOutsidePeriod,
    #[error("observations are not in strict point-in-time order")]
    ObservationOrder,
    #[error("calibration set size is outside its bounded contract")]
    ObservationCapacity,
    #[error("both outcome classes are required")]
    DegenerateOutcomes,
    #[error("evaluation overlaps the calibration period")]
    EvaluationOverlap,
    #[error("fit configuration is invalid")]
    InvalidFitConfig,
    #[error("calibration work exceeds its declared capacity")]
    WorkCapacity,
    #[error("calibration optimization failed to converge")]
    NonConverged,
    #[error("fitted calibrator state is invalid")]
    InvalidCalibrator,
    #[error("calibration arithmetic is not finite")]
    NonFiniteArithmetic,
    #[error("reliability bin count is invalid")]
    InvalidBinCount,
    #[error("probability metric is undefined for the supplied data")]
    UndefinedMetric,
}

pub(crate) fn require_open_probability(probability: f64) -> Result<f64, CalibrationError> {
    if !probability.is_finite() || !(0.0..=1.0).contains(&probability) {
        return Err(CalibrationError::InvalidProbability);
    }
    if probability == 0.0 || probability == 1.0 {
        return Err(CalibrationError::EndpointProbability);
    }
    Ok(probability)
}

fn logit_from_open_probability(probability: f64) -> Result<f64, CalibrationError> {
    let probability = require_open_probability(probability)?;
    let result = (probability / (1.0 - probability)).ln();
    if result.is_finite() {
        Ok(result)
    } else {
        Err(CalibrationError::NonFiniteArithmetic)
    }
}

pub(crate) fn sigmoid(value: f64) -> Result<f64, CalibrationError> {
    if !value.is_finite() {
        return Err(CalibrationError::NonFiniteArithmetic);
    }
    let probability = if value >= 0.0 {
        1.0 / (1.0 + (-value).exp())
    } else {
        let exponential = value.exp();
        exponential / (1.0 + exponential)
    };
    if probability.is_finite() {
        Ok(probability)
    } else {
        Err(CalibrationError::NonFiniteArithmetic)
    }
}

pub(crate) fn kish_effective_sample_size(
    weights: impl IntoIterator<Item = f64>,
    observations: usize,
) -> Result<f64, CalibrationError> {
    if observations == 0 {
        return Err(CalibrationError::InvalidObservationWeight);
    }
    let weights = weights.into_iter().collect::<Vec<_>>();
    if weights.len() != observations
        || weights
            .iter()
            .any(|weight| !weight.is_finite() || *weight <= 0.0)
    {
        return Err(CalibrationError::InvalidObservationWeight);
    }
    let scale = weights.iter().copied().fold(0.0_f64, f64::max);
    let mut sum = 0.0;
    let mut sum_compensation = 0.0;
    let mut sum_squares = 0.0;
    let mut squares_compensation = 0.0;
    for weight in weights {
        let normalized = weight / scale;
        compensated_add(&mut sum, &mut sum_compensation, normalized);
        compensated_add(
            &mut sum_squares,
            &mut squares_compensation,
            normalized * normalized,
        );
    }
    let effective = sum * sum / sum_squares;
    let upper = observations as f64;
    let tolerance = upper * 128.0 * f64::EPSILON;
    if !effective.is_finite() || effective <= 0.0 || effective > upper + tolerance {
        return Err(CalibrationError::InvalidObservationWeight);
    }
    Ok(effective.min(upper))
}

pub(crate) fn valid_effective_sample_size(effective: f64, observations: u64) -> bool {
    effective.is_finite()
        && effective > 0.0
        && observations > 0
        && effective <= observations as f64 * (1.0 + 128.0 * f64::EPSILON)
}

fn compensated_add(sum: &mut f64, compensation: &mut f64, value: f64) {
    let adjusted = value - *compensation;
    let next = *sum + adjusted;
    *compensation = (next - *sum) - adjusted;
    *sum = next;
}

pub(crate) fn validate_identifier(identifier: &str) -> Result<(), CalibrationError> {
    if identifier.is_empty()
        || identifier.len() > 256
        || identifier
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(CalibrationError::InvalidIdentifier);
    }
    Ok(())
}

fn validate_period(period: CalibrationPeriod) -> Result<(), CalibrationError> {
    if period.start_ns <= 0 || period.end_ns <= period.start_ns {
        Err(CalibrationError::InvalidPeriod)
    } else {
        Ok(())
    }
}

fn validate_observations(
    period: CalibrationPeriod,
    observations: &[CalibrationObservation],
) -> Result<(), CalibrationError> {
    validate_period(period)?;
    if !(MINIMUM_OBSERVATIONS..=MAXIMUM_OBSERVATIONS).contains(&observations.len()) {
        return Err(CalibrationError::ObservationCapacity);
    }
    let mut positive = false;
    let mut negative = false;
    let mut previous_origin = None;
    for observation in observations {
        if !observation.uncalibrated_probability.is_finite()
            || !(0.0..=1.0).contains(&observation.uncalibrated_probability)
            || !observation.raw_logit.is_finite()
            || observation.raw_logit.abs() > MAXIMUM_ABS_LOGIT
            || !observation.weight.is_finite()
            || observation.weight <= 0.0
            || observation.weight > 1.0e12
        {
            return Err(CalibrationError::InvalidObservationWeight);
        }
        let reconstructed = sigmoid(observation.raw_logit)?;
        if (reconstructed - observation.uncalibrated_probability).abs()
            > 8.0 * f64::EPSILON * (1.0 + observation.uncalibrated_probability.abs())
        {
            return Err(CalibrationError::InvalidProbability);
        }
        if observation.origin_time_ns <= 0
            || observation.outcome_time_ns <= observation.origin_time_ns
            || observation.as_known_at_ns < observation.outcome_time_ns
        {
            return Err(CalibrationError::InvalidObservationTime);
        }
        if observation.origin_time_ns < period.start_ns
            || observation.origin_time_ns >= period.end_ns
            || observation.outcome_time_ns > period.end_ns
            || observation.as_known_at_ns > period.end_ns
        {
            return Err(CalibrationError::ObservationOutsidePeriod);
        }
        if previous_origin.is_some_and(|previous| observation.origin_time_ns < previous) {
            return Err(CalibrationError::ObservationOrder);
        }
        previous_origin = Some(observation.origin_time_ns);
        positive |= observation.outcome;
        negative |= !observation.outcome;
    }
    if !positive || !negative {
        return Err(CalibrationError::DegenerateOutcomes);
    }
    Ok(())
}
