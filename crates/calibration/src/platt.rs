use serde::{Deserialize, Serialize};

use crate::{
    CalibrationError, CalibrationSet, FitConfig, kish_effective_sample_size,
    require_open_probability, sigmoid, valid_effective_sample_size, validate_identifier,
};

/// Inspectable proof that a bounded logistic calibration fit converged.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FitTermination {
    ManualReference,
    ProjectedGradientTolerance,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FitDiagnostics {
    pub iterations: u32,
    pub objective: f64,
    pub projected_gradient_norm: f64,
    pub objective_evaluations: u64,
    /// Natural-log span between the largest and smallest nonzero feature
    /// magnitudes. The logarithmic representation remains finite for valid
    /// subnormal inputs and is descriptive rather than a rank claim.
    pub feature_log_scale_span: f64,
    pub termination: FitTermination,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LogisticFit<const N: usize> {
    pub coefficients: [f64; N],
    pub diagnostics: FitDiagnostics,
}

/// Logistic calibration on the log-odds of an uncalibrated probability.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlattCalibrator {
    pub schema_version: u32,
    pub calibration_evidence_id: String,
    pub slope: f64,
    pub intercept: f64,
    pub observations: u64,
    pub effective_sample_size: f64,
    pub convergence_tolerance: f64,
    pub diagnostics: FitDiagnostics,
}

impl PlattCalibrator {
    /// Constructs a low-level monotone reference transform. Production code
    /// should use [`crate::select_validation_only`] so lineage is frozen in a
    /// [`crate::CalibrationArtifact`].
    pub fn new(slope: f64, intercept: f64) -> Result<Self, CalibrationError> {
        if !slope.is_finite() || slope < 0.0 || !intercept.is_finite() {
            return Err(CalibrationError::InvalidCalibrator);
        }
        Ok(Self {
            schema_version: 1,
            calibration_evidence_id: "manual_reference_v1".to_owned(),
            slope,
            intercept,
            observations: 8,
            effective_sample_size: 8.0,
            convergence_tolerance: 0.0,
            diagnostics: FitDiagnostics {
                iterations: 0,
                objective: 0.0,
                projected_gradient_norm: 0.0,
                objective_evaluations: 1,
                feature_log_scale_span: 0.0,
                termination: FitTermination::ManualReference,
            },
        })
    }

    pub fn fit(calibration: &CalibrationSet, config: FitConfig) -> Result<Self, CalibrationError> {
        calibration.validate()?;
        config.validate(calibration.observations().len())?;
        let rows = calibration
            .observations()
            .iter()
            .map(|observation| {
                Ok((
                    [observation.raw_logit(), 1.0],
                    f64::from(observation.outcome()),
                    observation.weight(),
                ))
            })
            .collect::<Result<Vec<_>, CalibrationError>>()?;
        let initial =
            rank_deficient_intercept(calibration).map_or([1.0, 0.0], |intercept| [0.0, intercept]);
        let fit = fit_logistic::<2>(&rows, initial, 1, config)?;
        let calibrator = Self {
            schema_version: 1,
            calibration_evidence_id: calibration.evidence_id().to_owned(),
            slope: fit.coefficients[0],
            intercept: fit.coefficients[1],
            observations: u64::try_from(rows.len())
                .map_err(|_| CalibrationError::ObservationCapacity)?,
            effective_sample_size: effective_sample_size(&rows)?,
            convergence_tolerance: config.tolerance,
            diagnostics: fit.diagnostics,
        };
        calibrator.validate()?;
        Ok(calibrator)
    }

    pub fn calibrate(&self, probability: f64) -> Result<f64, CalibrationError> {
        self.validate()?;
        let probability = require_open_probability(probability)?;
        self.calibrate_logit(logit(probability)?)
    }

    pub fn calibrate_logit(&self, raw_logit: f64) -> Result<f64, CalibrationError> {
        sigmoid(self.linear_predictor(raw_logit)?)
    }

    pub(crate) fn linear_predictor(&self, raw_logit: f64) -> Result<f64, CalibrationError> {
        self.validate()?;
        if !raw_logit.is_finite() {
            return Err(CalibrationError::NonFiniteArithmetic);
        }
        let linear = self.slope.mul_add(raw_logit, self.intercept);
        linear
            .is_finite()
            .then_some(linear)
            .ok_or(CalibrationError::NonFiniteArithmetic)
    }

    fn validate(&self) -> Result<(), CalibrationError> {
        let valid = self.schema_version == 1
            && validate_identifier(&self.calibration_evidence_id).is_ok()
            && self.slope.is_finite()
            && self.slope >= 0.0
            && self.intercept.is_finite()
            && self.observations >= 8
            && self.observations <= 1_000_000
            && valid_effective_sample_size(self.effective_sample_size, self.observations)
            && self.convergence_tolerance.is_finite()
            && self.convergence_tolerance >= 0.0
            && self.diagnostics.projected_gradient_norm <= self.convergence_tolerance
            && valid_diagnostics(self.diagnostics)
            && match self.diagnostics.termination {
                FitTermination::ManualReference => {
                    self.calibration_evidence_id == "manual_reference_v1"
                        && self.convergence_tolerance == 0.0
                        && self.diagnostics.iterations == 0
                }
                FitTermination::ProjectedGradientTolerance => {
                    self.calibration_evidence_id != "manual_reference_v1"
                        && self.convergence_tolerance > 0.0
                }
            };
        if valid {
            Ok(())
        } else {
            Err(CalibrationError::InvalidCalibrator)
        }
    }
}

pub(crate) fn logit(probability: f64) -> Result<f64, CalibrationError> {
    let probability = require_open_probability(probability)?;
    let result = (probability / (1.0 - probability)).ln();
    if result.is_finite() {
        Ok(result)
    } else {
        Err(CalibrationError::NonFiniteArithmetic)
    }
}

pub(crate) fn fit_logistic<const N: usize>(
    rows: &[([f64; N], f64, f64)],
    mut coefficients: [f64; N],
    monotone_prefix: usize,
    config: FitConfig,
) -> Result<LogisticFit<N>, CalibrationError> {
    config.validate(rows.len())?;
    let feature_log_scale_span = feature_log_scale_span(rows)?;
    let mut objective = logistic_objective(rows, &coefficients, config.l2_penalty)?;
    let mut evaluations = 1_u64;
    for iteration in 0..config.maximum_iterations {
        let gradient = logistic_gradient(rows, &coefficients, config.l2_penalty)?;
        let projected_norm = projected_gradient_norm(&coefficients, &gradient, monotone_prefix);
        if projected_norm <= config.tolerance {
            return Ok(LogisticFit {
                coefficients,
                diagnostics: FitDiagnostics {
                    iterations: iteration,
                    objective,
                    projected_gradient_norm: projected_norm,
                    objective_evaluations: evaluations,
                    feature_log_scale_span,
                    termination: FitTermination::ProjectedGradientTolerance,
                },
            });
        }

        let mut step = 1.0;
        let mut accepted = None;
        for _ in 0..config.maximum_backtracking_steps {
            let mut candidate = coefficients;
            for index in 0..N {
                candidate[index] -= step * gradient[index];
                if index < monotone_prefix {
                    candidate[index] = candidate[index].max(0.0);
                }
            }
            let candidate_objective = logistic_objective(rows, &candidate, config.l2_penalty)?;
            evaluations = evaluations
                .checked_add(1)
                .ok_or(CalibrationError::WorkCapacity)?;
            let displacement_dot_gradient = candidate
                .iter()
                .zip(&coefficients)
                .zip(&gradient)
                .map(|((candidate, current), gradient)| (candidate - current) * gradient)
                .sum::<f64>();
            if candidate_objective <= objective + 1.0e-4 * displacement_dot_gradient {
                accepted = Some((candidate, candidate_objective));
                break;
            }
            step *= 0.5;
        }
        let (candidate, candidate_objective) = accepted.ok_or(CalibrationError::NonConverged)?;
        coefficients = candidate;
        objective = candidate_objective;
    }
    Err(CalibrationError::NonConverged)
}

fn logistic_gradient<const N: usize>(
    rows: &[([f64; N], f64, f64)],
    coefficients: &[f64; N],
    l2_penalty: f64,
) -> Result<[f64; N], CalibrationError> {
    let weight_sum = rows.iter().map(|row| row.2).sum::<f64>();
    if !weight_sum.is_finite() || weight_sum <= 0.0 {
        return Err(CalibrationError::InvalidObservationWeight);
    }
    let mut gradient = [0.0; N];
    for (features, outcome, weight) in rows {
        let residual = weight * (sigmoid(dot(features, coefficients)?)? - outcome);
        for (index, feature) in features.iter().enumerate() {
            gradient[index] = residual.mul_add(*feature, gradient[index]);
        }
    }
    for (index, value) in gradient.iter_mut().enumerate() {
        *value = (*value / weight_sum)
            + if index + 1 == N {
                0.0
            } else {
                l2_penalty * coefficients[index]
            };
    }
    if gradient.iter().all(|value| value.is_finite()) {
        Ok(gradient)
    } else {
        Err(CalibrationError::NonFiniteArithmetic)
    }
}

fn projected_gradient_norm<const N: usize>(
    coefficients: &[f64; N],
    gradient: &[f64; N],
    monotone_prefix: usize,
) -> f64 {
    coefficients
        .iter()
        .zip(gradient)
        .enumerate()
        .map(|(index, (coefficient, gradient))| {
            let projected = if index < monotone_prefix {
                (coefficient - gradient).max(0.0)
            } else {
                coefficient - gradient
            };
            (coefficient - projected).abs()
        })
        .fold(0.0, f64::max)
}

fn logistic_objective<const N: usize>(
    rows: &[([f64; N], f64, f64)],
    coefficients: &[f64; N],
    l2_penalty: f64,
) -> Result<f64, CalibrationError> {
    let mut total = 0.0;
    let weight_sum = rows.iter().map(|row| row.2).sum::<f64>();
    if !weight_sum.is_finite() || weight_sum <= 0.0 {
        return Err(CalibrationError::InvalidObservationWeight);
    }
    for (features, outcome, weight) in rows {
        let linear = dot(features, coefficients)?;
        total += weight * (softplus(linear)? - outcome * linear);
    }
    total /= weight_sum;
    for coefficient in &coefficients[..N.saturating_sub(1)] {
        total += 0.5 * l2_penalty * coefficient * coefficient;
    }
    if total.is_finite() {
        Ok(total)
    } else {
        Err(CalibrationError::NonFiniteArithmetic)
    }
}

pub(crate) fn effective_sample_size<const N: usize>(
    rows: &[([f64; N], f64, f64)],
) -> Result<f64, CalibrationError> {
    kish_effective_sample_size(rows.iter().map(|row| row.2), rows.len())
}

pub(crate) fn rank_deficient_intercept(calibration: &CalibrationSet) -> Option<f64> {
    let minimum = calibration
        .observations()
        .iter()
        .map(|row| row.raw_logit())
        .fold(f64::INFINITY, f64::min);
    let maximum = calibration
        .observations()
        .iter()
        .map(|row| row.raw_logit())
        .fold(f64::NEG_INFINITY, f64::max);
    let scale = minimum.abs().max(maximum.abs()).max(1.0);
    if maximum - minimum > 256.0 * f64::EPSILON * scale {
        return None;
    }
    let total_weight = calibration
        .observations()
        .iter()
        .map(|row| row.weight())
        .sum::<f64>();
    let positive_weight = calibration
        .observations()
        .iter()
        .filter(|row| row.outcome())
        .map(|row| row.weight())
        .sum::<f64>();
    let base_rate = positive_weight / total_weight;
    Some((base_rate / (1.0 - base_rate)).ln())
}

fn valid_diagnostics(diagnostics: FitDiagnostics) -> bool {
    diagnostics.objective.is_finite()
        && diagnostics.projected_gradient_norm.is_finite()
        && diagnostics.projected_gradient_norm >= 0.0
        && diagnostics.objective_evaluations > 0
        && diagnostics.feature_log_scale_span.is_finite()
        && diagnostics.feature_log_scale_span >= 0.0
}

fn feature_log_scale_span<const N: usize>(
    rows: &[([f64; N], f64, f64)],
) -> Result<f64, CalibrationError> {
    let mut minimum = f64::INFINITY;
    let mut maximum = 0.0_f64;
    for (features, _, _) in rows {
        for feature in features {
            let magnitude = feature.abs();
            if !magnitude.is_finite() {
                return Err(CalibrationError::NonFiniteArithmetic);
            }
            if magnitude > 0.0 {
                minimum = minimum.min(magnitude);
                maximum = maximum.max(magnitude);
            }
        }
    }
    let span = maximum.ln() - minimum.ln();
    if span.is_finite() && span >= 0.0 {
        Ok(span)
    } else {
        Err(CalibrationError::NonFiniteArithmetic)
    }
}

fn dot<const N: usize>(
    features: &[f64; N],
    coefficients: &[f64; N],
) -> Result<f64, CalibrationError> {
    let result = features
        .iter()
        .zip(coefficients)
        .fold(0.0, |sum, (feature, coefficient)| {
            coefficient.mul_add(*feature, sum)
        });
    if result.is_finite() {
        Ok(result)
    } else {
        Err(CalibrationError::NonFiniteArithmetic)
    }
}

fn softplus(value: f64) -> Result<f64, CalibrationError> {
    if !value.is_finite() {
        return Err(CalibrationError::NonFiniteArithmetic);
    }
    Ok(if value > 0.0 {
        value + (-value).exp().ln_1p()
    } else {
        value.exp().ln_1p()
    })
}

#[cfg(test)]
mod tests {
    use super::{fit_logistic, logistic_gradient, logistic_objective, projected_gradient_norm};
    use crate::FitConfig;

    #[test]
    fn weighted_logistic_gradient_matches_finite_difference() {
        let rows = [
            ([-2.0, 1.0], 0.0, 0.5),
            ([-0.5, 1.0], 0.0, 1.0),
            ([0.75, 1.0], 1.0, 0.25),
            ([2.5, 1.0], 1.0, 1.5),
        ];
        let coefficients = [0.3, -0.2];
        let penalty = 0.01;
        let analytic = logistic_gradient(&rows, &coefficients, penalty).expect("analytic gradient");
        let epsilon = 1.0e-6;
        for index in 0..2 {
            let mut left = coefficients;
            let mut right = coefficients;
            left[index] -= epsilon;
            right[index] += epsilon;
            let numerical = (logistic_objective(&rows, &right, penalty).expect("right objective")
                - logistic_objective(&rows, &left, penalty).expect("left objective"))
                / (2.0 * epsilon);
            assert!((analytic[index] - numerical).abs() < 1.0e-8);
        }
    }

    #[test]
    fn projected_kkt_norm_respects_an_active_monotone_boundary() {
        assert_eq!(projected_gradient_norm(&[0.0, 0.0], &[1.0, 0.0], 1), 0.0);
        assert_eq!(projected_gradient_norm(&[0.0, 0.0], &[-1.0, 0.0], 1), 1.0);
    }

    #[test]
    fn successful_fit_carries_terminal_kkt_evidence() {
        let rows = [
            ([-1.0, 1.0], 0.0, 1.0),
            ([-0.5, 1.0], 0.0, 1.0),
            ([0.5, 1.0], 1.0, 1.0),
            ([1.0, 1.0], 1.0, 1.0),
            ([-0.2, 1.0], 0.0, 1.0),
            ([0.2, 1.0], 1.0, 1.0),
            ([-0.8, 1.0], 0.0, 1.0),
            ([0.8, 1.0], 1.0, 1.0),
        ];
        let config = FitConfig::default();
        let fit = fit_logistic(&rows, [1.0, 0.0], 1, config).expect("converged fit");
        assert!(fit.diagnostics.projected_gradient_norm <= config.tolerance);
        assert!(fit.diagnostics.objective_evaluations > 0);
        assert!(fit.diagnostics.objective.is_finite());
    }
}
