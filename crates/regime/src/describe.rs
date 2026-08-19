//! Post-hoc descriptions derived only from fitted standardized statistics.

use crate::{HmmError, StateId, StudentTHmm};

const NEAR_BASELINE_THRESHOLD: f64 = 0.25;
const DESCRIPTION_HASH_DOMAIN: &[u8] = b"cmti:student-t-hmm:state-description:v1\0";

/// One fitted statistic supporting a separately stored state description.
#[derive(Clone, Debug, PartialEq)]
pub struct StateStatistic {
    pub feature_family: String,
    pub standardized_location: f64,
    pub diagonal_scale: f64,
}

/// Human-readable post-hoc description. It never replaces the neutral ID.
#[derive(Clone, Debug, PartialEq)]
pub struct StateDescription {
    pub state_id: StateId,
    pub description: String,
    pub evidence: Vec<StateStatistic>,
    pub model_id: [u8; 32],
    pub description_id: [u8; 32],
}

pub(crate) fn describe(model: &StudentTHmm) -> Result<Vec<StateDescription>, HmmError> {
    let mut output = Vec::with_capacity(model.emissions.len());
    for (state_index, emission) in model.emissions.iter().enumerate() {
        if emission.location.len() != model.feature_families.len()
            || emission.diagonal_scale.len() != model.feature_families.len()
        {
            return Err(HmmError::DimensionMismatch);
        }
        let evidence = model
            .feature_families
            .iter()
            .zip(&emission.location)
            .zip(&emission.diagonal_scale)
            .map(
                |((feature_family, standardized_location), diagonal_scale)| StateStatistic {
                    feature_family: feature_family.clone(),
                    standardized_location: *standardized_location,
                    diagonal_scale: *diagonal_scale,
                },
            )
            .collect::<Vec<_>>();
        let dominant = evidence
            .iter()
            .max_by(|left, right| {
                left.standardized_location
                    .abs()
                    .total_cmp(&right.standardized_location.abs())
            })
            .ok_or(HmmError::DimensionMismatch)?;
        let description = if dominant.standardized_location.abs() < NEAR_BASELINE_THRESHOLD {
            "near_baseline".to_owned()
        } else if dominant.standardized_location > 0.0 {
            format!("elevated_{}", dominant.feature_family)
        } else {
            format!("depressed_{}", dominant.feature_family)
        };
        let state_id = StateId(state_index);
        let description_id = hash_description(model.model_id, state_id, &description, &evidence);
        output.push(StateDescription {
            state_id,
            description,
            evidence,
            model_id: model.model_id,
            description_id,
        });
    }
    Ok(output)
}

fn hash_description(
    model_id: [u8; 32],
    state_id: StateId,
    description: &str,
    evidence: &[StateStatistic],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(DESCRIPTION_HASH_DOMAIN);
    hasher.update(&NEAR_BASELINE_THRESHOLD.to_bits().to_le_bytes());
    hasher.update(&model_id);
    hasher.update(&(state_id.0 as u64).to_le_bytes());
    hasher.update(&(description.len() as u64).to_le_bytes());
    hasher.update(description.as_bytes());
    for statistic in evidence {
        hasher.update(&(statistic.feature_family.len() as u64).to_le_bytes());
        hasher.update(statistic.feature_family.as_bytes());
        hasher.update(&statistic.standardized_location.to_bits().to_le_bytes());
        hasher.update(&statistic.diagonal_scale.to_bits().to_le_bytes());
    }
    *hasher.finalize().as_bytes()
}
