use serde::{Deserialize, Serialize};

use crate::{
    CalibrationError, CalibrationSet, FitConfig,
    platt::{
        FitDiagnostics, FitTermination, effective_sample_size, fit_logistic,
        rank_deficient_intercept,
    },
    require_open_probability, sigmoid, valid_effective_sample_size, validate_identifier,
};

/// Monotone beta calibration:
/// `sigmoid(a * ln(p) - b * ln(1-p) + c)`, with `a,b >= 0`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BetaCalibrator {
    pub schema_version: u32,
    pub calibration_evidence_id: String,
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub observations: u64,
    pub effective_sample_size: f64,
    pub convergence_tolerance: f64,
    pub diagnostics: FitDiagnostics,
}

impl BetaCalibrator {
    pub fn fit(calibration: &CalibrationSet, config: FitConfig) -> Result<Self, CalibrationError> {
        calibration.validate()?;
        config.validate(calibration.observations().len())?;
        let rows = calibration
            .observations()
            .iter()
            .map(|observation| {
                let raw_logit = observation.raw_logit();
                Ok((
                    [-softplus(-raw_logit)?, softplus(raw_logit)?, 1.0],
                    f64::from(observation.outcome()),
                    observation.weight(),
                ))
            })
            .collect::<Result<Vec<_>, CalibrationError>>()?;
        let initial = rank_deficient_intercept(calibration)
            .map_or([1.0, 1.0, 0.0], |intercept| [0.0, 0.0, intercept]);
        let fit = fit_logistic::<3>(&rows, initial, 2, config)?;
        let calibrator = Self {
            schema_version: 1,
            calibration_evidence_id: calibration.evidence_id().to_owned(),
            a: fit.coefficients[0],
            b: fit.coefficients[1],
            c: fit.coefficients[2],
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
        self.calibrate_logit((probability / (1.0 - probability)).ln())
    }

    pub fn calibrate_logit(&self, raw_logit: f64) -> Result<f64, CalibrationError> {
        sigmoid(self.linear_predictor(raw_logit)?)
    }

    pub(crate) fn linear_predictor(&self, raw_logit: f64) -> Result<f64, CalibrationError> {
        self.validate()?;
        if !raw_logit.is_finite() {
            return Err(CalibrationError::NonFiniteArithmetic);
        }
        let linear = self.a.mul_add(
            -softplus(-raw_logit)?,
            self.b.mul_add(softplus(raw_logit)?, self.c),
        );
        linear
            .is_finite()
            .then_some(linear)
            .ok_or(CalibrationError::NonFiniteArithmetic)
    }

    fn validate(&self) -> Result<(), CalibrationError> {
        let valid = self.schema_version == 1
            && validate_identifier(&self.calibration_evidence_id).is_ok()
            && self.a.is_finite()
            && self.a >= 0.0
            && self.b.is_finite()
            && self.b >= 0.0
            && self.c.is_finite()
            && self.observations >= 8
            && self.observations <= 1_000_000
            && valid_effective_sample_size(self.effective_sample_size, self.observations)
            && self.convergence_tolerance.is_finite()
            && self.convergence_tolerance > 0.0
            && self.diagnostics.projected_gradient_norm <= self.convergence_tolerance
            && self.diagnostics.objective.is_finite()
            && self.diagnostics.projected_gradient_norm.is_finite()
            && self.diagnostics.objective_evaluations > 0
            && self.diagnostics.feature_log_scale_span.is_finite()
            && self.diagnostics.feature_log_scale_span >= 0.0
            && self.diagnostics.termination == FitTermination::ProjectedGradientTolerance;
        if valid {
            Ok(())
        } else {
            Err(CalibrationError::InvalidCalibrator)
        }
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
