use serde::{Deserialize, Serialize};

use crate::{
    BetaCalibrator, CalibrationArtifact, CalibrationError, EvaluationSet, FitConfig,
    IsotonicCalibrator, PlattCalibrator, kish_effective_sample_size, platt::fit_logistic,
};

const BASELINE_DOMAIN: &[u8] = b"cmti:evaluation-baseline:v1\0";

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

    pub fn calibrate_logit(&self, raw_logit: f64) -> Result<f64, CalibrationError> {
        match self {
            Self::Platt(calibrator) => calibrator.calibrate_logit(raw_logit),
            Self::Beta(calibrator) => calibrator.calibrate_logit(raw_logit),
            Self::Isotonic(calibrator) => calibrator.calibrate_logit(raw_logit),
        }
    }

    pub(crate) fn calibrated_logit(&self, raw_logit: f64) -> Result<f64, CalibrationError> {
        match self {
            Self::Platt(calibrator) => calibrator.linear_predictor(raw_logit),
            Self::Beta(calibrator) => calibrator.linear_predictor(raw_logit),
            Self::Isotonic(calibrator) => calibrator.calibrated_logit(raw_logit),
        }
    }

    pub const fn name(&self) -> &'static str {
        match self {
            Self::Platt(_) => "platt-log-odds-v1",
            Self::Beta(_) => "beta-monotone-v1",
            Self::Isotonic(_) => "pav-isotonic-global-ess-boundary-v3",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReliabilityBin {
    pub lower_bound: f64,
    pub upper_bound: f64,
    pub observations: u64,
    pub total_weight: f64,
    pub effective_sample_size: f64,
    pub mean_probability: f64,
    pub event_rate: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BaselinePrediction {
    pub row_id: u64,
    pub entity_id: String,
    pub probability: f64,
    pub source_evidence_hash: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BaselineForecastSet {
    baseline_id: String,
    evaluation_evidence_hash: [u8; 32],
    predictions: Vec<BaselinePrediction>,
    evidence_hash: [u8; 32],
}

impl BaselineForecastSet {
    pub fn try_new(
        evaluation: &EvaluationSet,
        baseline_id: impl Into<String>,
        predictions: Vec<BaselinePrediction>,
    ) -> Result<Self, CalibrationError> {
        evaluation.validate()?;
        let mut value = Self {
            baseline_id: baseline_id.into(),
            evaluation_evidence_hash: evaluation.evidence_hash(),
            predictions,
            evidence_hash: [0; 32],
        };
        value.evidence_hash = value.calculate_hash();
        value.validate_for(evaluation)?;
        Ok(value)
    }

    fn validate_for(&self, evaluation: &EvaluationSet) -> Result<(), CalibrationError> {
        if self.baseline_id.is_empty()
            || self.baseline_id.len() > 128
            || !self
                .baseline_id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            || self.evaluation_evidence_hash != evaluation.evidence_hash()
            || self.predictions.len() != evaluation.observations().len()
            || self
                .predictions
                .iter()
                .zip(evaluation.row_evidence())
                .any(|(prediction, row)| {
                    prediction.row_id != row.row_id
                        || prediction.entity_id != row.entity_id
                        || prediction.source_evidence_hash == [0; 32]
                        || !prediction.probability.is_finite()
                        || !(0.0..=1.0).contains(&prediction.probability)
                })
            || self.evidence_hash != self.calculate_hash()
        {
            Err(CalibrationError::UndefinedMetric)
        } else {
            Ok(())
        }
    }

    fn brier_score(&self, evaluation: &EvaluationSet) -> Result<f64, CalibrationError> {
        self.validate_for(evaluation)?;
        let mut weighted_loss = 0.0;
        let mut total_weight = 0.0;
        for (prediction, observation) in self.predictions.iter().zip(evaluation.observations()) {
            let residual = prediction.probability - f64::from(observation.outcome());
            weighted_loss += observation.weight() * residual * residual;
            total_weight += observation.weight();
        }
        let score = weighted_loss / total_weight;
        score
            .is_finite()
            .then_some(score)
            .ok_or(CalibrationError::NonFiniteArithmetic)
    }

    fn calculate_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(BASELINE_DOMAIN);
        hash_string(&mut hasher, &self.baseline_id);
        hasher.update(&self.evaluation_evidence_hash);
        hasher.update(&(self.predictions.len() as u64).to_le_bytes());
        for prediction in &self.predictions {
            hasher.update(&prediction.row_id.to_le_bytes());
            hash_string(&mut hasher, &prediction.entity_id);
            hasher.update(&prediction.probability.to_bits().to_le_bytes());
            hasher.update(&prediction.source_evidence_hash);
        }
        *hasher.finalize().as_bytes()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProbabilityMetrics {
    pub schema_version: u32,
    pub evaluation_evidence_id: String,
    pub evaluation_evidence_hash: [u8; 32],
    pub weighting_policy: String,
    pub calibration_evidence_id: Option<String>,
    pub calibration_method: Option<String>,
    pub observations: u64,
    pub positives: u64,
    pub total_weight: f64,
    pub effective_sample_size: f64,
    pub base_rate: f64,
    pub log_loss: f64,
    pub uncalibrated_log_loss: f64,
    pub brier_score: f64,
    pub uncalibrated_brier_score: f64,
    pub strongest_baseline_id: String,
    pub strongest_baseline_evidence_hash: [u8; 32],
    pub strongest_baseline_brier_score: f64,
    pub brier_skill_score: f64,
    pub calibration_intercept: Option<f64>,
    pub calibration_slope: Option<f64>,
    pub calibration_regression_status: String,
    pub calibration_regression_l2_penalty: f64,
    pub expected_calibration_error: f64,
    pub adaptive_calibration_error: f64,
    pub reliability: Vec<ReliabilityBin>,
}

impl ProbabilityMetrics {
    pub fn evaluate(
        evaluation: &EvaluationSet,
        artifact: &CalibrationArtifact,
        bins: usize,
        eligible_baselines: &[BaselineForecastSet],
    ) -> Result<Self, CalibrationError> {
        evaluate_probability_metrics(evaluation, artifact, bins, eligible_baselines)
    }
}

pub fn evaluate_probability_metrics(
    evaluation: &EvaluationSet,
    artifact: &CalibrationArtifact,
    bins: usize,
    eligible_baselines: &[BaselineForecastSet],
) -> Result<ProbabilityMetrics, CalibrationError> {
    evaluation.validate_for_artifact(artifact)?;
    if !(2..=100).contains(&bins) || bins > evaluation.observations().len() {
        return Err(CalibrationError::InvalidBinCount);
    }
    let scored = evaluation
        .observations()
        .iter()
        .map(|observation| {
            let raw = observation.uncalibrated_probability();
            let calibrated = artifact.method.calibrate_logit(observation.raw_logit())?;
            validate_metric_probability(calibrated)?;
            validate_metric_probability(raw)?;
            Ok(ScoredRow {
                calibrated,
                calibrated_logit: artifact.method.calibrated_logit(observation.raw_logit())?,
                raw,
                outcome: observation.outcome(),
                weight: observation.weight(),
            })
        })
        .collect::<Result<Vec<_>, CalibrationError>>()?;
    let positives = scored.iter().filter(|row| row.outcome).count();
    let total_weight = scored.iter().map(|row| row.weight).sum::<f64>();
    let positive_weight = scored
        .iter()
        .filter(|row| row.outcome)
        .map(|row| row.weight)
        .sum::<f64>();
    let base_rate = positive_weight / total_weight;
    if base_rate == 0.0 || base_rate == 1.0 {
        return Err(CalibrationError::DegenerateOutcomes);
    }
    let log_loss = mean_loss(&scored, false)?;
    let uncalibrated_log_loss = mean_loss(&scored, true)?;
    let brier_score = mean_brier(&scored, false)?;
    let uncalibrated_brier_score = mean_brier(&scored, true)?;
    let (strongest_baseline, strongest_baseline_brier_score) = eligible_baselines
        .iter()
        .map(|baseline| Ok((baseline, baseline.brier_score(evaluation)?)))
        .collect::<Result<Vec<_>, CalibrationError>>()?
        .into_iter()
        .min_by(|left, right| {
            left.1
                .total_cmp(&right.1)
                .then_with(|| left.0.baseline_id.cmp(&right.0.baseline_id))
        })
        .ok_or(CalibrationError::UndefinedMetric)?;
    let brier_skill_score = 1.0 - brier_score / strongest_baseline_brier_score;
    let reliability = reliability_bins(&scored, bins);
    let expected_calibration_error = reliability
        .iter()
        .map(|bin| {
            (bin.total_weight / total_weight) * (bin.mean_probability - bin.event_rate).abs()
        })
        .sum();
    let adaptive_calibration_error = adaptive_error(&scored, bins);
    let (calibration_slope, calibration_intercept, calibration_regression_status) =
        match calibration_regression(&scored) {
            Ok((slope, intercept)) => (
                Some(slope),
                Some(intercept),
                "estimable_unpenalized_logistic_v1".to_owned(),
            ),
            Err(CalibrationError::DegenerateOutcomes) => {
                (None, None, "not_estimable_rank_deficient".to_owned())
            }
            Err(CalibrationError::NonConverged) => {
                (None, None, "not_estimable_nonconvergence".to_owned())
            }
            Err(error) => return Err(error),
        };

    let result = ProbabilityMetrics {
        schema_version: 1,
        evaluation_evidence_id: evaluation.evidence_id().to_owned(),
        evaluation_evidence_hash: evaluation.evidence_hash(),
        weighting_policy: "importance_weighted_v1".to_owned(),
        calibration_evidence_id: Some(digest_identifier(artifact.artifact_id())),
        calibration_method: Some(artifact.method.name().to_owned()),
        observations: u64::try_from(scored.len())
            .map_err(|_| CalibrationError::ObservationCapacity)?,
        positives: u64::try_from(positives).map_err(|_| CalibrationError::ObservationCapacity)?,
        total_weight,
        effective_sample_size: kish_effective_sample_size(
            scored.iter().map(|row| row.weight),
            scored.len(),
        )?,
        base_rate,
        log_loss,
        uncalibrated_log_loss,
        brier_score,
        uncalibrated_brier_score,
        strongest_baseline_id: strongest_baseline.baseline_id.clone(),
        strongest_baseline_evidence_hash: strongest_baseline.evidence_hash,
        strongest_baseline_brier_score,
        brier_skill_score,
        calibration_intercept,
        calibration_slope,
        calibration_regression_status,
        calibration_regression_l2_penalty: 0.0,
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
        result.strongest_baseline_brier_score,
        result.brier_skill_score,
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

fn digest_identifier(digest: [u8; 32]) -> String {
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn validate_metric_probability(probability: f64) -> Result<(), CalibrationError> {
    if probability.is_finite() && (0.0..=1.0).contains(&probability) {
        Ok(())
    } else {
        Err(CalibrationError::InvalidProbability)
    }
}

#[derive(Clone, Copy, Debug)]
struct ScoredRow {
    calibrated: f64,
    calibrated_logit: f64,
    raw: f64,
    outcome: bool,
    weight: f64,
}

fn mean_loss(scored: &[ScoredRow], raw: bool) -> Result<f64, CalibrationError> {
    let mut total = 0.0;
    let mut total_weight = 0.0;
    for row in scored {
        let probability = if raw { row.raw } else { row.calibrated };
        total += row.weight
            * match (probability, row.outcome) {
                (0.0, false) | (1.0, true) => 0.0,
                (0.0, true) | (1.0, false) => return Err(CalibrationError::UndefinedMetric),
                (_, true) => -probability.ln(),
                (_, false) => -(1.0 - probability).ln(),
            };
        total_weight += row.weight;
    }
    Ok(total / total_weight)
}

fn mean_brier(scored: &[ScoredRow], raw: bool) -> Result<f64, CalibrationError> {
    let mut total = 0.0;
    let mut total_weight = 0.0;
    for row in scored {
        let probability = if raw { row.raw } else { row.calibrated };
        let residual = probability - f64::from(row.outcome);
        total += row.weight * residual * residual;
        total_weight += row.weight;
    }
    let result = total / total_weight;
    result
        .is_finite()
        .then_some(result)
        .ok_or(CalibrationError::NonFiniteArithmetic)
}

fn reliability_bins(scored: &[ScoredRow], bins: usize) -> Vec<ReliabilityBin> {
    let mut counts = vec![0_u64; bins];
    let mut probability_sums = vec![0.0; bins];
    let mut event_sums = vec![0.0; bins];
    let mut weight_sums = vec![0.0; bins];
    let mut squared_weight_sums = vec![0.0; bins];
    for row in scored {
        let index = ((row.calibrated * bins as f64).floor() as usize).min(bins - 1);
        counts[index] += 1;
        probability_sums[index] += row.weight * row.calibrated;
        event_sums[index] += row.weight * f64::from(row.outcome);
        weight_sums[index] += row.weight;
        squared_weight_sums[index] += row.weight * row.weight;
    }
    (0..bins)
        .map(|index| {
            let count = counts[index];
            ReliabilityBin {
                lower_bound: index as f64 / bins as f64,
                upper_bound: (index + 1) as f64 / bins as f64,
                observations: count,
                total_weight: weight_sums[index],
                effective_sample_size: if count == 0 {
                    0.0
                } else {
                    weight_sums[index] * weight_sums[index] / squared_weight_sums[index]
                },
                mean_probability: if weight_sums[index] == 0.0 {
                    0.0
                } else {
                    probability_sums[index] / weight_sums[index]
                },
                event_rate: if weight_sums[index] == 0.0 {
                    0.0
                } else {
                    event_sums[index] / weight_sums[index]
                },
            }
        })
        .collect()
}

fn adaptive_error(scored: &[ScoredRow], bins: usize) -> f64 {
    let mut ordered = scored.to_vec();
    ordered.sort_by(|left, right| left.calibrated.total_cmp(&right.calibrated));
    let mut total = 0.0;
    let total_weight = ordered.iter().map(|row| row.weight).sum::<f64>();
    let target = total_weight / bins as f64;
    let mut start = 0;
    while start < ordered.len() {
        let mut end = start;
        let mut accumulated_weight = 0.0;
        while end < ordered.len() {
            let probability = ordered[end].calibrated;
            while end < ordered.len() && ordered[end].calibrated == probability {
                accumulated_weight += ordered[end].weight;
                end += 1;
            }
            if accumulated_weight >= target {
                break;
            }
        }
        let slice = &ordered[start..end];
        let weight = slice.iter().map(|row| row.weight).sum::<f64>();
        let mean_probability = slice
            .iter()
            .map(|row| row.weight * row.calibrated)
            .sum::<f64>()
            / weight;
        let event_rate = slice
            .iter()
            .map(|row| row.weight * f64::from(row.outcome))
            .sum::<f64>()
            / weight;
        total += (weight / total_weight) * (mean_probability - event_rate).abs();
        start = end;
    }
    total
}

fn calibration_regression(scored: &[ScoredRow]) -> Result<(f64, f64), CalibrationError> {
    let total_weight = scored.iter().map(|row| row.weight).sum::<f64>();
    let weighted_mean = scored
        .iter()
        .map(|row| row.weight * row.calibrated_logit)
        .sum::<f64>()
        / total_weight;
    let weighted_variance = scored
        .iter()
        .map(|row| {
            let centered = row.calibrated_logit - weighted_mean;
            row.weight * centered * centered
        })
        .sum::<f64>()
        / total_weight;
    let magnitude = scored
        .iter()
        .map(|row| row.calibrated_logit.abs())
        .fold(1.0_f64, f64::max);
    if !weighted_variance.is_finite()
        || weighted_variance <= 256.0 * f64::EPSILON * magnitude * magnitude
    {
        return Err(CalibrationError::DegenerateOutcomes);
    }
    let rows = scored
        .iter()
        .map(|row| {
            (
                [row.calibrated_logit, 1.0],
                f64::from(row.outcome),
                row.weight,
            )
        })
        .collect::<Vec<_>>();
    let fit = fit_logistic::<2>(
        &rows,
        [1.0, 0.0],
        0,
        FitConfig {
            maximum_iterations: 20_000,
            maximum_backtracking_steps: 32,
            tolerance: 1e-6,
            l2_penalty: 0.0,
        },
    )?;
    Ok((fit.coefficients[0], fit.coefficients[1]))
}

fn hash_string(hasher: &mut blake3::Hasher, value: &str) {
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value.as_bytes());
}
