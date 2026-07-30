use serde::{Deserialize, Serialize};

use crate::{
    BetaCalibrator, CalibrationError, EvaluationSet, FitConfig, IsotonicCalibrator,
    PlattCalibrator,
    platt::{fit_logistic, logit},
};

/// A predeclared fitted calibration method.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum CalibrationMethod {
    Platt(PlattCalibrator),
    Beta(BetaCalibrator),
    Isotonic(IsotonicCalibrator),
}

impl CalibrationMethod {
    pub fn calibrate(&self, probability: f64) -> Result<f64, CalibrationError> {
        match self {
            Self::Platt(calibrator) => calibrator.calibrate(probability),
            Self::Beta(calibrator) => calibrator.calibrate(probability),
            Self::Isotonic(calibrator) => calibrator.calibrate(probability),
        }
    }

    pub const fn name(&self) -> &'static str {
        match self {
            Self::Platt(_) => "platt-log-odds-v1",
            Self::Beta(_) => "beta-monotone-v1",
            Self::Isotonic(_) => "pav-isotonic-v1",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReliabilityBin {
    pub lower_bound: f64,
    pub upper_bound: f64,
    pub observations: u64,
    pub mean_probability: f64,
    pub event_rate: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProbabilityMetrics {
    pub schema_version: u32,
    pub evaluation_evidence_id: String,
    pub calibration_evidence_id: Option<String>,
    pub calibration_method: Option<String>,
    pub observations: u64,
    pub positives: u64,
    pub base_rate: f64,
    pub log_loss: f64,
    pub uncalibrated_log_loss: f64,
    pub brier_score: f64,
    pub uncalibrated_brier_score: f64,
    pub brier_skill_score: f64,
    pub calibration_intercept: f64,
    pub calibration_slope: f64,
    pub expected_calibration_error: f64,
    pub adaptive_calibration_error: f64,
    pub reliability: Vec<ReliabilityBin>,
}

impl ProbabilityMetrics {
    pub fn evaluate(
        evaluation: &EvaluationSet,
        calibrator: Option<&CalibrationMethod>,
        bins: usize,
    ) -> Result<Self, CalibrationError> {
        evaluate_probability_metrics(evaluation, calibrator, bins)
    }
}

pub fn evaluate_probability_metrics(
    evaluation: &EvaluationSet,
    calibrator: Option<&CalibrationMethod>,
    bins: usize,
) -> Result<ProbabilityMetrics, CalibrationError> {
    evaluation.validate()?;
    if !(2..=100).contains(&bins) || bins > evaluation.observations().len() {
        return Err(CalibrationError::InvalidBinCount);
    }
    let scored = evaluation
        .observations()
        .iter()
        .map(|observation| {
            let raw = observation.uncalibrated_probability();
            let calibrated = calibrator.map_or(Ok(raw), |value| value.calibrate(raw))?;
            validate_metric_probability(calibrated)?;
            validate_metric_probability(raw)?;
            Ok((calibrated, raw, observation.outcome()))
        })
        .collect::<Result<Vec<_>, CalibrationError>>()?;
    let positives = scored.iter().filter(|(_, _, outcome)| *outcome).count();
    let base_rate = positives as f64 / scored.len() as f64;
    if base_rate == 0.0 || base_rate == 1.0 {
        return Err(CalibrationError::DegenerateOutcomes);
    }
    let log_loss = mean_loss(&scored, false)?;
    let uncalibrated_log_loss = mean_loss(&scored, true)?;
    let brier_score = mean_brier(&scored, false)?;
    let uncalibrated_brier_score = mean_brier(&scored, true)?;
    let baseline_brier = base_rate * (1.0 - base_rate);
    let brier_skill_score = 1.0 - brier_score / baseline_brier;
    let reliability = reliability_bins(&scored, bins);
    let expected_calibration_error = reliability
        .iter()
        .map(|bin| {
            (bin.observations as f64 / scored.len() as f64)
                * (bin.mean_probability - bin.event_rate).abs()
        })
        .sum();
    let adaptive_calibration_error = adaptive_error(&scored, bins);
    let (calibration_slope, calibration_intercept) = calibration_regression(&scored)?;

    let result = ProbabilityMetrics {
        schema_version: 1,
        evaluation_evidence_id: evaluation.evidence_id().to_owned(),
        calibration_evidence_id: calibrator.map(calibration_evidence_id),
        calibration_method: calibrator.map(|value| value.name().to_owned()),
        observations: u64::try_from(scored.len())
            .map_err(|_| CalibrationError::ObservationCapacity)?,
        positives: u64::try_from(positives).map_err(|_| CalibrationError::ObservationCapacity)?,
        base_rate,
        log_loss,
        uncalibrated_log_loss,
        brier_score,
        uncalibrated_brier_score,
        brier_skill_score,
        calibration_intercept,
        calibration_slope,
        expected_calibration_error,
        adaptive_calibration_error,
        reliability,
    };
    if [
        result.base_rate,
        result.log_loss,
        result.uncalibrated_log_loss,
        result.brier_score,
        result.uncalibrated_brier_score,
        result.brier_skill_score,
        result.calibration_intercept,
        result.calibration_slope,
        result.expected_calibration_error,
        result.adaptive_calibration_error,
    ]
    .into_iter()
    .all(f64::is_finite)
    {
        Ok(result)
    } else {
        Err(CalibrationError::NonFiniteArithmetic)
    }
}

fn calibration_evidence_id(calibrator: &CalibrationMethod) -> String {
    match calibrator {
        CalibrationMethod::Platt(value) => value.calibration_evidence_id.clone(),
        CalibrationMethod::Beta(value) => value.calibration_evidence_id.clone(),
        CalibrationMethod::Isotonic(value) => value.calibration_evidence_id.clone(),
    }
}

fn validate_metric_probability(probability: f64) -> Result<(), CalibrationError> {
    if probability.is_finite() && (0.0..=1.0).contains(&probability) {
        Ok(())
    } else {
        Err(CalibrationError::InvalidProbability)
    }
}

fn mean_loss(scored: &[(f64, f64, bool)], raw: bool) -> Result<f64, CalibrationError> {
    let mut total = 0.0;
    for (calibrated, uncalibrated, outcome) in scored {
        let probability = if raw { *uncalibrated } else { *calibrated };
        total += match (probability, outcome) {
            (0.0, false) | (1.0, true) => 0.0,
            (0.0, true) | (1.0, false) => return Err(CalibrationError::UndefinedMetric),
            (_, true) => -probability.ln(),
            (_, false) => -(1.0 - probability).ln(),
        };
    }
    Ok(total / scored.len() as f64)
}

fn mean_brier(scored: &[(f64, f64, bool)], raw: bool) -> Result<f64, CalibrationError> {
    let total = scored.iter().try_fold(0.0, |sum, row| {
        let probability = if raw { row.1 } else { row.0 };
        let residual = probability - f64::from(row.2);
        let next = residual.mul_add(residual, sum);
        next.is_finite()
            .then_some(next)
            .ok_or(CalibrationError::NonFiniteArithmetic)
    })?;
    Ok(total / scored.len() as f64)
}

fn reliability_bins(scored: &[(f64, f64, bool)], bins: usize) -> Vec<ReliabilityBin> {
    let mut counts = vec![0_u64; bins];
    let mut probability_sums = vec![0.0; bins];
    let mut event_sums = vec![0_u64; bins];
    for (probability, _, outcome) in scored {
        let index = ((*probability * bins as f64).floor() as usize).min(bins - 1);
        counts[index] += 1;
        probability_sums[index] += probability;
        event_sums[index] += u64::from(*outcome);
    }
    (0..bins)
        .map(|index| {
            let count = counts[index];
            ReliabilityBin {
                lower_bound: index as f64 / bins as f64,
                upper_bound: (index + 1) as f64 / bins as f64,
                observations: count,
                mean_probability: if count == 0 {
                    0.0
                } else {
                    probability_sums[index] / count as f64
                },
                event_rate: if count == 0 {
                    0.0
                } else {
                    event_sums[index] as f64 / count as f64
                },
            }
        })
        .collect()
}

fn adaptive_error(scored: &[(f64, f64, bool)], bins: usize) -> f64 {
    let mut ordered = scored.to_vec();
    ordered.sort_by(|left, right| left.0.total_cmp(&right.0));
    let mut total = 0.0;
    for index in 0..bins {
        let start = index * ordered.len() / bins;
        let end = (index + 1) * ordered.len() / bins;
        if start == end {
            continue;
        }
        let slice = &ordered[start..end];
        let mean_probability = slice.iter().map(|row| row.0).sum::<f64>() / slice.len() as f64;
        let event_rate = slice.iter().filter(|row| row.2).count() as f64 / slice.len() as f64;
        total +=
            (slice.len() as f64 / ordered.len() as f64) * (mean_probability - event_rate).abs();
    }
    total
}

fn calibration_regression(scored: &[(f64, f64, bool)]) -> Result<(f64, f64), CalibrationError> {
    let rows = scored
        .iter()
        .map(|(probability, _, outcome)| Ok(([logit(*probability)?, 1.0], f64::from(*outcome))))
        .collect::<Result<Vec<_>, CalibrationError>>()?;
    let coefficients = fit_logistic::<2>(
        &rows,
        [1.0, 0.0],
        0,
        FitConfig {
            maximum_iterations: 4_000,
            maximum_backtracking_steps: 32,
            tolerance: 1e-8,
            l2_penalty: 1e-3,
        },
    )?;
    Ok((coefficients[0], coefficients[1]))
}
