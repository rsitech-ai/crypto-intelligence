use serde::{Deserialize, Serialize};

use crate::{
    CalibrationError, CalibrationSet, FitConfig, platt::fit_logistic, require_open_probability,
    sigmoid, validate_identifier,
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
}

impl BetaCalibrator {
    pub fn fit(calibration: &CalibrationSet, config: FitConfig) -> Result<Self, CalibrationError> {
        calibration.validate()?;
        config.validate(calibration.observations().len())?;
        let rows = calibration
            .observations()
            .iter()
            .map(|observation| {
                let probability = require_open_probability(observation.uncalibrated_probability())?;
                Ok((
                    [probability.ln(), -(1.0 - probability).ln(), 1.0],
                    f64::from(observation.outcome()),
                ))
            })
            .collect::<Result<Vec<_>, CalibrationError>>()?;
        let coefficients = fit_logistic::<3>(&rows, [1.0, 1.0, 0.0], 2, config)?;
        Ok(Self {
            schema_version: 1,
            calibration_evidence_id: calibration.evidence_id().to_owned(),
            a: coefficients[0],
            b: coefficients[1],
            c: coefficients[2],
            observations: u64::try_from(rows.len())
                .map_err(|_| CalibrationError::ObservationCapacity)?,
            effective_sample_size: rows.len() as f64,
        })
    }

    pub fn calibrate(&self, probability: f64) -> Result<f64, CalibrationError> {
        self.validate()?;
        let probability = require_open_probability(probability)?;
        let linear = self.a.mul_add(
            probability.ln(),
            self.b.mul_add(-(1.0 - probability).ln(), self.c),
        );
        sigmoid(linear)
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
            && self.effective_sample_size.is_finite()
            && self.effective_sample_size == self.observations as f64;
        if valid {
            Ok(())
        } else {
            Err(CalibrationError::InvalidCalibrator)
        }
    }
}
