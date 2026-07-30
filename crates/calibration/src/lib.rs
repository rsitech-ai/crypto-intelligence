//! Deterministic, point-in-time probability calibration and evaluation.
//!
//! Calibration is intentionally fitted on a typed validation period. An
//! [`EvaluationSet`] must start strictly after that period, preventing the
//! outer evaluation segment from being reused through this API.

mod beta;
mod isotonic;
mod platt;
mod select;

pub use beta::BetaCalibrator;
pub use isotonic::IsotonicCalibrator;
pub use platt::PlattCalibrator;
pub use select::{
    CalibrationMethod, ProbabilityMetrics, ReliabilityBin, evaluate_probability_metrics,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

const MINIMUM_OBSERVATIONS: usize = 8;
const MAXIMUM_OBSERVATIONS: usize = 1_000_000;

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
    uncalibrated_probability: f64,
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
        if !uncalibrated_probability.is_finite() || !(0.0..=1.0).contains(&uncalibrated_probability)
        {
            return Err(CalibrationError::InvalidProbability);
        }
        if origin_time_ns <= 0
            || outcome_time_ns <= origin_time_ns
            || as_known_at_ns < outcome_time_ns
        {
            return Err(CalibrationError::InvalidObservationTime);
        }
        Ok(Self {
            uncalibrated_probability,
            outcome,
            origin_time_ns,
            outcome_time_ns,
            as_known_at_ns,
        })
    }

    pub const fn uncalibrated_probability(self) -> f64 {
        self.uncalibrated_probability
    }

    pub const fn outcome(self) -> bool {
        self.outcome
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
    observations: Vec<CalibrationObservation>,
}

impl EvaluationSet {
    pub fn new(
        evidence_id: impl Into<String>,
        period: CalibrationPeriod,
        calibration_period: CalibrationPeriod,
        observations: Vec<CalibrationObservation>,
    ) -> Result<Self, CalibrationError> {
        let evidence_id = evidence_id.into();
        let evaluation = Self {
            evidence_id,
            period,
            calibration_period,
            observations,
        };
        evaluation.validate()?;
        Ok(evaluation)
    }

    pub(crate) fn validate(&self) -> Result<(), CalibrationError> {
        validate_identifier(&self.evidence_id)?;
        validate_period(self.period)?;
        validate_period(self.calibration_period)?;
        if self.period.start_ns <= self.calibration_period.end_ns {
            return Err(CalibrationError::EvaluationOverlap);
        }
        validate_observations(self.period, &self.observations)
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
            tolerance: 1e-8,
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
        if previous_origin.is_some_and(|previous| observation.origin_time_ns <= previous) {
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
