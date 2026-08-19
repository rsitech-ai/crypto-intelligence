use serde::{Deserialize, Serialize};

use crate::{
    CalibrationError, CalibrationSet, kish_effective_sample_size, require_open_probability,
    valid_effective_sample_size, validate_identifier,
};

#[derive(Clone, Copy, Debug)]
struct Block {
    upper_logit: f64,
    positive_weight: f64,
    weight: f64,
}

/// Pool-adjacent-violators isotonic calibration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IsotonicCalibrator {
    pub schema_version: u32,
    pub calibration_evidence_id: String,
    pub upper_logits: Vec<f64>,
    pub calibrated_probabilities: Vec<f64>,
    pub observations: u64,
    pub effective_sample_size: f64,
}

impl IsotonicCalibrator {
    pub fn fit(calibration: &CalibrationSet) -> Result<Self, CalibrationError> {
        calibration.validate()?;
        let mut ordered = calibration
            .observations()
            .iter()
            .map(|observation| {
                Ok((
                    observation.raw_logit(),
                    f64::from(observation.outcome()) * observation.weight(),
                    observation.weight(),
                ))
            })
            .collect::<Result<Vec<_>, CalibrationError>>()?;
        ordered.sort_by(|left, right| left.0.total_cmp(&right.0));

        let mut blocks: Vec<Block> = Vec::with_capacity(ordered.len());
        for (raw_logit, positive_weight, weight) in ordered {
            if let Some(last) = blocks.last_mut()
                && last.upper_logit == raw_logit
            {
                last.positive_weight += positive_weight;
                last.weight += weight;
            } else {
                blocks.push(Block {
                    upper_logit: raw_logit,
                    positive_weight,
                    weight,
                });
            }
            while blocks.len() >= 2 {
                let right = blocks[blocks.len() - 1];
                let left = blocks[blocks.len() - 2];
                if left.positive_weight / left.weight <= right.positive_weight / right.weight {
                    break;
                }
                blocks.pop();
                blocks.pop();
                blocks.push(Block {
                    upper_logit: right.upper_logit,
                    positive_weight: left.positive_weight + right.positive_weight,
                    weight: left.weight + right.weight,
                });
            }
        }
        let upper_logits = blocks.iter().map(|block| block.upper_logit).collect();
        // A global Jeffreys boundary transform preserves the exact PAV order
        // while keeping downstream log-loss/logit metrics defined. Per-block
        // smoothing is deliberately avoided because unequal block weights can
        // invert adjacent fitted values.
        let effective_sample_size = effective_sample_size(calibration)?;
        let epsilon = 0.5 / (effective_sample_size + 1.0);
        let calibrated_probabilities = blocks
            .iter()
            .copied()
            .map(|block| block.positive_weight / block.weight)
            .map(|probability| epsilon + (1.0 - 2.0 * epsilon) * probability)
            .collect();
        let calibrator = Self {
            schema_version: 1,
            calibration_evidence_id: calibration.evidence_id().to_owned(),
            upper_logits,
            calibrated_probabilities,
            observations: u64::try_from(calibration.observations().len())
                .map_err(|_| CalibrationError::ObservationCapacity)?,
            effective_sample_size,
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
        self.validate()?;
        if !raw_logit.is_finite() {
            return Err(CalibrationError::NonFiniteArithmetic);
        }
        let index = self
            .upper_logits
            .partition_point(|upper| *upper < raw_logit)
            .min(self.calibrated_probabilities.len().saturating_sub(1));
        self.calibrated_probabilities
            .get(index)
            .copied()
            .ok_or(CalibrationError::UndefinedMetric)
    }

    pub(crate) fn calibrated_logit(&self, raw_logit: f64) -> Result<f64, CalibrationError> {
        crate::platt::logit(self.calibrate_logit(raw_logit)?)
    }

    fn validate(&self) -> Result<(), CalibrationError> {
        let valid = self.schema_version == 1
            && validate_identifier(&self.calibration_evidence_id).is_ok()
            && self.observations >= 8
            && self.observations <= 1_000_000
            && valid_effective_sample_size(self.effective_sample_size, self.observations)
            && !self.upper_logits.is_empty()
            && self.upper_logits.len() == self.calibrated_probabilities.len()
            && self.upper_logits.len() <= self.observations as usize
            && self
                .upper_logits
                .iter()
                .all(|raw_logit| raw_logit.is_finite())
            && self.upper_logits.windows(2).all(|pair| pair[0] < pair[1])
            && self.calibrated_probabilities.iter().all(|probability| {
                probability.is_finite() && *probability > 0.0 && *probability < 1.0
            })
            && self
                .calibrated_probabilities
                .windows(2)
                .all(|pair| pair[0] <= pair[1]);
        if valid {
            Ok(())
        } else {
            Err(CalibrationError::InvalidCalibrator)
        }
    }
}

fn effective_sample_size(calibration: &CalibrationSet) -> Result<f64, CalibrationError> {
    kish_effective_sample_size(
        calibration.observations().iter().map(|row| row.weight()),
        calibration.observations().len(),
    )
}
