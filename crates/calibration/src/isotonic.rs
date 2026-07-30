use serde::{Deserialize, Serialize};

use crate::{CalibrationError, CalibrationSet, require_open_probability, validate_identifier};

#[derive(Clone, Copy, Debug)]
struct Block {
    upper_probability: f64,
    positives: u64,
    count: u64,
}

impl Block {
    fn mean(self) -> f64 {
        self.positives as f64 / self.count as f64
    }
}

/// Pool-adjacent-violators isotonic calibration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IsotonicCalibrator {
    pub schema_version: u32,
    pub calibration_evidence_id: String,
    pub upper_probabilities: Vec<f64>,
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
                require_open_probability(observation.uncalibrated_probability())
                    .map(|probability| (probability, u64::from(observation.outcome())))
            })
            .collect::<Result<Vec<_>, CalibrationError>>()?;
        ordered.sort_by(|left, right| left.0.total_cmp(&right.0));

        let mut blocks: Vec<Block> = Vec::with_capacity(ordered.len());
        for (probability, outcome) in ordered {
            if let Some(last) = blocks.last_mut()
                && last.upper_probability == probability
            {
                last.positives += outcome;
                last.count += 1;
            } else {
                blocks.push(Block {
                    upper_probability: probability,
                    positives: outcome,
                    count: 1,
                });
            }
            while blocks.len() >= 2 {
                let right = blocks[blocks.len() - 1];
                let left = blocks[blocks.len() - 2];
                if left.mean() <= right.mean() {
                    break;
                }
                blocks.pop();
                blocks.pop();
                blocks.push(Block {
                    upper_probability: right.upper_probability,
                    positives: left.positives + right.positives,
                    count: left.count + right.count,
                });
            }
        }
        let upper_probabilities = blocks.iter().map(|block| block.upper_probability).collect();
        // A single global finite-sample boundary transform preserves the PAV
        // ordering while keeping downstream log-loss/logit metrics defined.
        let epsilon = 0.5 / (calibration.observations().len() as f64 + 1.0);
        let calibrated_probabilities = blocks
            .iter()
            .copied()
            .map(Block::mean)
            .map(|probability| epsilon + (1.0 - 2.0 * epsilon) * probability)
            .collect();
        Ok(Self {
            schema_version: 1,
            calibration_evidence_id: calibration.evidence_id().to_owned(),
            upper_probabilities,
            calibrated_probabilities,
            observations: u64::try_from(calibration.observations().len())
                .map_err(|_| CalibrationError::ObservationCapacity)?,
            effective_sample_size: calibration.observations().len() as f64,
        })
    }

    pub fn calibrate(&self, probability: f64) -> Result<f64, CalibrationError> {
        self.validate()?;
        let probability = require_open_probability(probability)?;
        let index = self
            .upper_probabilities
            .partition_point(|upper| *upper < probability)
            .min(self.calibrated_probabilities.len().saturating_sub(1));
        self.calibrated_probabilities
            .get(index)
            .copied()
            .ok_or(CalibrationError::UndefinedMetric)
    }

    fn validate(&self) -> Result<(), CalibrationError> {
        let valid = self.schema_version == 1
            && validate_identifier(&self.calibration_evidence_id).is_ok()
            && self.observations >= 8
            && self.observations <= 1_000_000
            && self.effective_sample_size.is_finite()
            && self.effective_sample_size == self.observations as f64
            && !self.upper_probabilities.is_empty()
            && self.upper_probabilities.len() == self.calibrated_probabilities.len()
            && self.upper_probabilities.len() <= self.observations as usize
            && self
                .upper_probabilities
                .iter()
                .all(|probability| require_open_probability(*probability).is_ok())
            && self
                .upper_probabilities
                .windows(2)
                .all(|pair| pair[0] < pair[1])
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
