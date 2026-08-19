//! Exact conversion from conditional competing hazards to cumulative incidence.

use std::collections::BTreeSet;

use crate::{BucketSpec, HazardError};

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
    log_total_survival: Vec<f64>,
    bucket_spec: Option<BucketSpec>,
    cause_ids: Option<Vec<String>>,
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

    /// Log survival is retained even after its linear representation
    /// underflows to zero. An absorbing zero-survival bucket is represented by
    /// negative infinity.
    #[must_use]
    pub fn log_total_survival(&self) -> &[f64] {
        &self.log_total_survival
    }

    /// Returns the immutable bucket specification bound during construction.
    /// Generic arithmetic-only curves intentionally return `None`.
    #[must_use]
    pub const fn bucket_spec(&self) -> Option<BucketSpec> {
        self.bucket_spec
    }

    /// Extracts an exact supported product horizon without assuming a hazard
    /// shape inside any bucket.
    pub fn at_horizon(&self, horizon_seconds: u64) -> Result<HorizonIncidence, HazardError> {
        let bucket_spec = self.bucket_spec.ok_or(HazardError::UnboundBucketSpec)?;
        let cause_ids = self
            .cause_ids
            .as_ref()
            .ok_or(HazardError::InvalidCauseSchema)?;
        if self.total_survival.len() != bucket_spec.len()
            || self.log_total_survival.len() != bucket_spec.len()
            || self
                .by_cause
                .iter()
                .any(|curve| curve.len() != bucket_spec.len())
            || cause_ids.len() != self.by_cause.len()
        {
            return Err(HazardError::CurveDimensionMismatch);
        }
        let index = bucket_spec.bucket_index_for_horizon(horizon_seconds)?;
        Ok(HorizonIncidence {
            horizon_seconds,
            bucket_spec,
            causes: cause_ids
                .iter()
                .zip(&self.by_cause)
                .map(|(cause_id, curve)| CauseHorizonProbability {
                    cause_id: cause_id.clone(),
                    probability: curve[index],
                })
                .collect(),
            survival: self.total_survival[index],
        })
    }
}

/// One cause-keyed cumulative probability at an exact horizon.
#[derive(Clone, Debug, PartialEq)]
pub struct CauseHorizonProbability {
    cause_id: String,
    probability: f64,
}

impl CauseHorizonProbability {
    #[must_use]
    pub fn cause_id(&self) -> &str {
        &self.cause_id
    }

    #[must_use]
    pub const fn probability(&self) -> f64 {
        self.probability
    }
}

/// Cause incidence and survival at one exact supported horizon.
#[derive(Clone, Debug, PartialEq)]
pub struct HorizonIncidence {
    horizon_seconds: u64,
    bucket_spec: BucketSpec,
    causes: Vec<CauseHorizonProbability>,
    survival: f64,
}

impl HorizonIncidence {
    #[must_use]
    pub const fn horizon_seconds(&self) -> u64 {
        self.horizon_seconds
    }

    #[must_use]
    pub const fn bucket_spec(&self) -> BucketSpec {
        self.bucket_spec
    }

    #[must_use]
    pub fn causes(&self) -> &[CauseHorizonProbability] {
        &self.causes
    }

    #[must_use]
    pub const fn survival(&self) -> f64 {
        self.survival
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct CompensatedSum {
    total: f64,
    compensation: f64,
}

impl CompensatedSum {
    fn add(&mut self, value: f64) -> Result<f64, HazardError> {
        let corrected = value - self.compensation;
        let next = self.total + corrected;
        self.compensation = (next - self.total) - corrected;
        if !next.is_finite() {
            return Err(HazardError::NonFiniteProbability);
        }
        self.total = next;
        Ok(next)
    }
}

/// Converts conditional bucket hazards into unconditional cumulative incidence.
pub fn cumulative_incidence(
    buckets: &[BucketProbability],
) -> Result<CumulativeIncidence, HazardError> {
    cumulative_incidence_with_spec(None, None, buckets)
}

/// Converts conditional hazards into a curve bound to an exact bucket
/// specification. Dimension mismatches fail before any probability arithmetic.
pub fn cumulative_incidence_for_spec(
    bucket_spec: BucketSpec,
    cause_ids: &[String],
    buckets: &[BucketProbability],
) -> Result<CumulativeIncidence, HazardError> {
    if buckets.len() != bucket_spec.len() {
        return Err(HazardError::CurveDimensionMismatch);
    }
    let cause_count = buckets
        .first()
        .ok_or(HazardError::EmptyBuckets)?
        .causes
        .len();
    if cause_ids.len() != cause_count || !valid_cause_ids(cause_ids) {
        return Err(HazardError::InvalidCauseSchema);
    }
    cumulative_incidence_with_spec(Some(bucket_spec), Some(cause_ids.to_vec()), buckets)
}

fn cumulative_incidence_with_spec(
    bucket_spec: Option<BucketSpec>,
    cause_ids: Option<Vec<String>>,
    buckets: &[BucketProbability],
) -> Result<CumulativeIncidence, HazardError> {
    let first = buckets.first().ok_or(HazardError::EmptyBuckets)?;
    if buckets.len() > MAXIMUM_BUCKETS {
        return Err(HazardError::InvalidBucketProbability);
    }
    let cause_count = first.causes.len();
    let mut by_cause = vec![Vec::with_capacity(buckets.len()); cause_count];
    let mut totals = vec![CompensatedSum::default(); cause_count];
    let mut log_survival = 0.0_f64;
    let mut total_survival = Vec::with_capacity(buckets.len());
    let mut log_total_survival = Vec::with_capacity(buckets.len());

    for bucket in buckets {
        bucket
            .validate(Some(cause_count))
            .map_err(|error| match error {
                HazardError::InvalidBucketProbability if bucket.causes.len() != cause_count => {
                    HazardError::CauseDimensionMismatch
                }
                other => other,
            })?;
        let survival_before = log_survival.exp();
        for (cause_index, conditional_probability) in bucket.causes.iter().enumerate() {
            let total = totals[cause_index].add(survival_before * conditional_probability)?;
            by_cause[cause_index].push(total);
        }
        if bucket.survival == 0.0 || log_survival == f64::NEG_INFINITY {
            log_survival = f64::NEG_INFINITY;
        } else {
            log_survival += bucket.survival.ln();
            if !log_survival.is_finite() {
                return Err(HazardError::NonFiniteProbability);
            }
        }
        log_total_survival.push(log_survival);
        let linear_survival = log_survival.exp();
        let mut mass = CompensatedSum::default();
        for total in &totals {
            mass.add(total.total)?;
        }
        let global_mass = mass.add(linear_survival)?;
        if (global_mass - 1.0).abs() > PROBABILITY_TOLERANCE {
            return Err(HazardError::ProbabilityMassDrift);
        }
        total_survival.push(linear_survival);
    }

    Ok(CumulativeIncidence {
        by_cause,
        total_survival,
        log_total_survival,
        bucket_spec,
        cause_ids,
    })
}

fn valid_cause_ids(cause_ids: &[String]) -> bool {
    let mut unique = BTreeSet::new();
    cause_ids.iter().all(|identifier| {
        !identifier.is_empty()
            && identifier.len() <= 128
            && identifier.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':' | b'/')
            })
            && unique.insert(identifier.as_str())
    })
}
