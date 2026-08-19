use serde::{Deserialize, Serialize};

use crate::{
    CalibrationError, CalibrationSet, FitConfig, require_open_probability, sigmoid,
    validate_identifier,
};

/// Logistic calibration on the log-odds of an uncalibrated probability.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlattCalibrator {
    pub schema_version: u32,
    pub calibration_evidence_id: String,
    pub slope: f64,
    pub intercept: f64,
    pub observations: u64,
    pub effective_sample_size: f64,
}

impl PlattCalibrator {
    pub fn fit(calibration: &CalibrationSet, config: FitConfig) -> Result<Self, CalibrationError> {
        calibration.validate()?;
        config.validate(calibration.observations().len())?;
        let rows = calibration
            .observations()
            .iter()
            .map(|observation| {
                let probability = require_open_probability(observation.uncalibrated_probability())?;
                Ok(([logit(probability)?, 1.0], f64::from(observation.outcome())))
            })
            .collect::<Result<Vec<_>, CalibrationError>>()?;
        let coefficients = fit_logistic::<2>(&rows, [1.0, 0.0], 1, config)?;
        Ok(Self {
            schema_version: 1,
            calibration_evidence_id: calibration.evidence_id().to_owned(),
            slope: coefficients[0],
            intercept: coefficients[1],
            observations: u64::try_from(rows.len())
                .map_err(|_| CalibrationError::ObservationCapacity)?,
            effective_sample_size: rows.len() as f64,
        })
    }

    pub fn calibrate(&self, probability: f64) -> Result<f64, CalibrationError> {
        self.validate()?;
        let probability = require_open_probability(probability)?;
        sigmoid(self.slope.mul_add(logit(probability)?, self.intercept))
    }

    fn validate(&self) -> Result<(), CalibrationError> {
        let valid = self.schema_version == 1
            && validate_identifier(&self.calibration_evidence_id).is_ok()
            && self.slope.is_finite()
            && self.slope >= 0.0
            && self.intercept.is_finite()
            && self.observations >= 8
            && self.observations <= 1_000_000
            && self.effective_sample_size.is_finite()
            && self.effective_sample_size == self.observations as f64;
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
    rows: &[([f64; N], f64)],
    mut coefficients: [f64; N],
    monotone_prefix: usize,
    config: FitConfig,
) -> Result<[f64; N], CalibrationError> {
    config.validate(rows.len())?;
    let mut objective = logistic_objective(rows, &coefficients, config.l2_penalty)?;
    let scale = rows.len() as f64;
    for _ in 0..config.maximum_iterations {
        let mut gradient = [0.0; N];
        for (features, outcome) in rows {
            let linear = dot(features, &coefficients)?;
            let residual = sigmoid(linear)? - outcome;
            for (index, feature) in features.iter().enumerate() {
                gradient[index] = residual.mul_add(*feature, gradient[index]);
            }
        }
        for (index, value) in gradient.iter_mut().enumerate() {
            *value = (*value / scale)
                + if index + 1 == N {
                    0.0
                } else {
                    config.l2_penalty * coefficients[index]
                };
        }
        if gradient.iter().map(|value| value.abs()).fold(0.0, f64::max) <= config.tolerance {
            return Ok(coefficients);
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
            if candidate_objective <= objective {
                accepted = Some((candidate, candidate_objective));
                break;
            }
            step *= 0.5;
        }
        let (candidate, candidate_objective) = accepted.ok_or(CalibrationError::NonConverged)?;
        let improvement = objective - candidate_objective;
        coefficients = candidate;
        objective = candidate_objective;
        if improvement <= config.tolerance * (1.0 + objective.abs()) {
            return Ok(coefficients);
        }
    }
    Err(CalibrationError::NonConverged)
}

fn logistic_objective<const N: usize>(
    rows: &[([f64; N], f64)],
    coefficients: &[f64; N],
    l2_penalty: f64,
) -> Result<f64, CalibrationError> {
    let mut total = 0.0;
    for (features, outcome) in rows {
        let linear = dot(features, coefficients)?;
        total += softplus(linear)? - outcome * linear;
    }
    total /= rows.len() as f64;
    for coefficient in &coefficients[..N.saturating_sub(1)] {
        total += 0.5 * l2_penalty * coefficient * coefficient;
    }
    if total.is_finite() {
        Ok(total)
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
