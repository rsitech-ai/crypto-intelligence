//! Exact conversion from conditional competing hazards to cumulative incidence.

use crate::HazardError;

const PROBABILITY_TOLERANCE: f64 = 1.0e-12;
const MAXIMUM_CAUSES: usize = 32;
const MAXIMUM_BUCKETS: usize = 4_096;

/// Conditional cause and survival probabilities for one at-risk bucket.
#[derive(Clone, Debug, PartialEq)]
pub struct BucketProbability {
    causes: Vec<f64>,
    survival: f64,
}

impl BucketProbability {
    pub fn try_new(causes: Vec<f64>, survival: f64) -> Result<Self, HazardError> {
        let probability = Self { causes, survival };
        probability.validate(None)?;
        Ok(probability)
    }

    #[must_use]
    pub fn causes(&self) -> &[f64] {
        &self.causes
    }

    #[must_use]
    pub const fn survival(&self) -> f64 {
        self.survival
    }

    pub(crate) fn validate(&self, cause_count: Option<usize>) -> Result<(), HazardError> {
        if self.causes.is_empty()
            || self.causes.len() > MAXIMUM_CAUSES
            || cause_count.is_some_and(|expected| self.causes.len() != expected)
            || !self.survival.is_finite()
            || !(0.0..=1.0).contains(&self.survival)
            || self
                .causes
                .iter()
                .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
        {
            return Err(HazardError::InvalidBucketProbability);
        }
        let total = self.causes.iter().sum::<f64>() + self.survival;
        if !total.is_finite() || (total - 1.0).abs() > PROBABILITY_TOLERANCE {
            return Err(HazardError::InvalidBucketProbability);
        }
        Ok(())
    }
}

/// Cumulative cause incidence and survival after every bucket.
#[derive(Clone, Debug, PartialEq)]
pub struct CumulativeIncidence {
    by_cause: Vec<Vec<f64>>,
    total_survival: Vec<f64>,
}

impl CumulativeIncidence {
    #[must_use]
    pub fn by_cause(&self) -> &[Vec<f64>] {
        &self.by_cause
    }

    #[must_use]
    pub fn total_survival(&self) -> &[f64] {
        &self.total_survival
    }
}

/// Converts conditional bucket hazards into unconditional cumulative incidence.
pub fn cumulative_incidence(
    buckets: &[BucketProbability],
) -> Result<CumulativeIncidence, HazardError> {
    let first = buckets.first().ok_or(HazardError::EmptyBuckets)?;
    if buckets.len() > MAXIMUM_BUCKETS {
        return Err(HazardError::InvalidBucketProbability);
    }
    let cause_count = first.causes.len();
    let mut by_cause = vec![Vec::with_capacity(buckets.len()); cause_count];
    let mut totals = vec![0.0; cause_count];
    let mut survival = 1.0;
    let mut total_survival = Vec::with_capacity(buckets.len());

    for bucket in buckets {
        bucket
            .validate(Some(cause_count))
            .map_err(|error| match error {
                HazardError::InvalidBucketProbability if bucket.causes.len() != cause_count => {
                    HazardError::CauseDimensionMismatch
                }
                other => other,
            })?;
        for (cause_index, conditional_probability) in bucket.causes.iter().enumerate() {
            totals[cause_index] += survival * conditional_probability;
            if !totals[cause_index].is_finite() {
                return Err(HazardError::NonFiniteProbability);
            }
            by_cause[cause_index].push(totals[cause_index]);
        }
        survival *= bucket.survival;
        if !survival.is_finite() {
            return Err(HazardError::NonFiniteProbability);
        }
        total_survival.push(survival);
    }

    Ok(CumulativeIncidence {
        by_cause,
        total_survival,
    })
}
