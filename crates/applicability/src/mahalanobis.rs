//! Deterministic robust feature normalization and distance diagnostics.

use nalgebra::DMatrix;
use serde::{Deserialize, Serialize};

use crate::ApplicabilityError;

const DISTRIBUTION_DOMAIN: &[u8] = b"cmti:applicability-distribution:v1\0";
const MAXIMUM_FEATURES: usize = 256;
const MAXIMUM_ROWS: usize = 4_096;
const MAD_NORMAL_SCALE: f64 = 1.482_602_218_505_602;
const ROBUST_WINSOR_LIMIT: f64 = 6.0;

/// A frozen robust training distribution used by applicability diagnostics.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RobustDistribution {
    schema_version: u32,
    center: Vec<f64>,
    scale: Vec<f64>,
    regularized_covariance: Vec<f64>,
    inverse_covariance: Vec<f64>,
    normalized_rows: Vec<Vec<f64>>,
    shrinkage: f64,
    evidence_hash: [u8; 32],
}

impl RobustDistribution {
    /// Fits median/MAD normalization and an explicitly shrunk covariance.
    pub fn fit(rows: &[Vec<f64>], shrinkage: f64) -> Result<Self, ApplicabilityError> {
        let row_count = rows.len();
        let feature_count = rows.first().map(Vec::len).unwrap_or(0);
        if !(3..=MAXIMUM_ROWS).contains(&row_count)
            || !(1..=MAXIMUM_FEATURES).contains(&feature_count)
            || !shrinkage.is_finite()
            || !(0.0..=1.0).contains(&shrinkage)
            || rows
                .iter()
                .any(|row| row.len() != feature_count || !row.iter().all(|value| value.is_finite()))
        {
            return Err(ApplicabilityError::InvalidTrainingDistribution);
        }

        let mut center = Vec::with_capacity(feature_count);
        let mut scale = Vec::with_capacity(feature_count);
        for column in 0..feature_count {
            let values: Vec<f64> = rows.iter().map(|row| row[column]).collect();
            let column_center = median(values.clone())?;
            let absolute_deviations = values
                .into_iter()
                .map(|value| (value - column_center).abs())
                .collect();
            let column_scale = median(absolute_deviations)? * MAD_NORMAL_SCALE;
            if !column_scale.is_finite() || column_scale <= f64::EPSILON {
                return Err(ApplicabilityError::DegenerateFeature);
            }
            center.push(column_center);
            scale.push(column_scale);
        }

        let normalized_rows: Vec<Vec<f64>> = rows
            .iter()
            .map(|row| normalize(row, &center, &scale))
            .collect::<Result<_, _>>()?;
        let denominator = (row_count - 1) as f64;
        let mut covariance = DMatrix::<f64>::zeros(feature_count, feature_count);
        for row in &normalized_rows {
            for left in 0..feature_count {
                for right in 0..feature_count {
                    let robust_left = row[left].clamp(-ROBUST_WINSOR_LIMIT, ROBUST_WINSOR_LIMIT);
                    let robust_right = row[right].clamp(-ROBUST_WINSOR_LIMIT, ROBUST_WINSOR_LIMIT);
                    covariance[(left, right)] += robust_left * robust_right / denominator;
                }
            }
        }
        let average_variance = (0..feature_count)
            .map(|index| covariance[(index, index)])
            .sum::<f64>()
            / feature_count as f64;
        if !average_variance.is_finite() || average_variance <= 0.0 {
            return Err(ApplicabilityError::DegenerateFeature);
        }
        for left in 0..feature_count {
            for right in 0..feature_count {
                let target = if left == right { average_variance } else { 0.0 };
                covariance[(left, right)] =
                    (1.0 - shrinkage) * covariance[(left, right)] + shrinkage * target;
            }
        }
        if !covariance.iter().all(|value| value.is_finite()) {
            return Err(ApplicabilityError::NonFiniteArithmetic);
        }
        let inverse = covariance
            .clone()
            .try_inverse()
            .ok_or(ApplicabilityError::SingularCovariance)?;
        validate_inverse(&covariance, &inverse)?;

        let mut fitted = Self {
            schema_version: 1,
            center,
            scale,
            regularized_covariance: covariance.iter().copied().collect(),
            inverse_covariance: inverse.iter().copied().collect(),
            normalized_rows,
            shrinkage,
            evidence_hash: [0; 32],
        };
        fitted.evidence_hash = fitted.calculate_hash();
        fitted.validate()?;
        Ok(fitted)
    }

    pub const fn feature_count(&self) -> usize {
        self.center.len()
    }

    pub const fn row_count(&self) -> usize {
        self.normalized_rows.len()
    }

    pub const fn evidence_hash(&self) -> [u8; 32] {
        self.evidence_hash
    }

    pub fn center(&self) -> &[f64] {
        &self.center
    }

    pub fn scale(&self) -> &[f64] {
        &self.scale
    }

    pub fn mahalanobis_squared(&self, features: &[f64]) -> Result<f64, ApplicabilityError> {
        self.validate()?;
        self.mahalanobis_squared_validated(features)
    }

    pub(crate) fn distances_validated(
        &self,
        features: &[f64],
    ) -> Result<(f64, f64), ApplicabilityError> {
        Ok((
            self.mahalanobis_squared_validated(features)?,
            self.nearest_neighbor_distance_validated(features)?,
        ))
    }

    fn mahalanobis_squared_validated(&self, features: &[f64]) -> Result<f64, ApplicabilityError> {
        let normalized = normalize(features, &self.center, &self.scale)?;
        let dimension = self.feature_count();
        let inverse = DMatrix::from_column_slice(dimension, dimension, &self.inverse_covariance);
        let mut distance = 0.0;
        for left in 0..dimension {
            for right in 0..dimension {
                distance += normalized[left] * inverse[(left, right)] * normalized[right];
            }
        }
        if distance.is_finite() && distance >= -1.0e-10 {
            Ok(distance.max(0.0))
        } else {
            Err(ApplicabilityError::NonFiniteArithmetic)
        }
    }

    pub fn nearest_neighbor_distance(&self, features: &[f64]) -> Result<f64, ApplicabilityError> {
        self.validate()?;
        self.nearest_neighbor_distance_validated(features)
    }

    fn nearest_neighbor_distance_validated(
        &self,
        features: &[f64],
    ) -> Result<f64, ApplicabilityError> {
        let normalized = normalize(features, &self.center, &self.scale)?;
        let mut best = f64::INFINITY;
        for training in &self.normalized_rows {
            let squared = normalized
                .iter()
                .zip(training)
                .try_fold(0.0, |sum, (left, right)| {
                    let delta = left - right;
                    let next = sum + delta * delta;
                    next.is_finite()
                        .then_some(next)
                        .ok_or(ApplicabilityError::NonFiniteArithmetic)
                })?;
            best = best.min(squared.sqrt());
        }
        best.is_finite()
            .then_some(best)
            .ok_or(ApplicabilityError::NonFiniteArithmetic)
    }

    pub fn validate(&self) -> Result<(), ApplicabilityError> {
        let dimension = self.center.len();
        let matrix_elements = dimension
            .checked_mul(dimension)
            .ok_or(ApplicabilityError::InvalidTrainingDistribution)?;
        if self.schema_version != 1
            || !(1..=MAXIMUM_FEATURES).contains(&dimension)
            || !(3..=MAXIMUM_ROWS).contains(&self.normalized_rows.len())
            || self.scale.len() != dimension
            || self.regularized_covariance.len() != matrix_elements
            || self.inverse_covariance.len() != matrix_elements
            || !self.shrinkage.is_finite()
            || !(0.0..=1.0).contains(&self.shrinkage)
            || !self.center.iter().all(|value| value.is_finite())
            || !self
                .scale
                .iter()
                .all(|value| value.is_finite() && *value > f64::EPSILON)
            || self
                .normalized_rows
                .iter()
                .any(|row| row.len() != dimension || !row.iter().all(|value| value.is_finite()))
            || !self
                .regularized_covariance
                .iter()
                .chain(&self.inverse_covariance)
                .all(|value| value.is_finite())
        {
            return Err(ApplicabilityError::InvalidTrainingDistribution);
        }
        let covariance =
            DMatrix::from_column_slice(dimension, dimension, &self.regularized_covariance);
        let inverse = DMatrix::from_column_slice(dimension, dimension, &self.inverse_covariance);
        validate_inverse(&covariance, &inverse)?;
        if self.evidence_hash == self.calculate_hash() {
            Ok(())
        } else {
            Err(ApplicabilityError::InvalidArtifact)
        }
    }

    fn calculate_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(DISTRIBUTION_DOMAIN);
        hash_u32(&mut hasher, self.schema_version);
        hash_f64(&mut hasher, self.shrinkage);
        hash_f64_slice(&mut hasher, &self.center);
        hash_f64_slice(&mut hasher, &self.scale);
        hash_f64_slice(&mut hasher, &self.regularized_covariance);
        hash_f64_slice(&mut hasher, &self.inverse_covariance);
        hash_u64(&mut hasher, self.normalized_rows.len() as u64);
        for row in &self.normalized_rows {
            hash_f64_slice(&mut hasher, row);
        }
        *hasher.finalize().as_bytes()
    }
}

fn normalize(
    values: &[f64],
    center: &[f64],
    scale: &[f64],
) -> Result<Vec<f64>, ApplicabilityError> {
    if values.len() != center.len() || scale.len() != center.len() {
        return Err(ApplicabilityError::FeatureSchemaMismatch);
    }
    values
        .iter()
        .zip(center)
        .zip(scale)
        .map(|((value, center), scale)| {
            let normalized = (value - center) / scale;
            (value.is_finite() && normalized.is_finite())
                .then_some(normalized)
                .ok_or(ApplicabilityError::NonFiniteArithmetic)
        })
        .collect()
}

fn median(mut values: Vec<f64>) -> Result<f64, ApplicabilityError> {
    if values.is_empty() || !values.iter().all(|value| value.is_finite()) {
        return Err(ApplicabilityError::InvalidTrainingDistribution);
    }
    values.sort_by(f64::total_cmp);
    let midpoint = values.len() / 2;
    let result = if values.len().is_multiple_of(2) {
        (values[midpoint - 1] / 2.0) + (values[midpoint] / 2.0)
    } else {
        values[midpoint]
    };
    result
        .is_finite()
        .then_some(result)
        .ok_or(ApplicabilityError::NonFiniteArithmetic)
}

fn validate_inverse(
    covariance: &DMatrix<f64>,
    inverse: &DMatrix<f64>,
) -> Result<(), ApplicabilityError> {
    if covariance.shape() != inverse.shape() || covariance.nrows() != covariance.ncols() {
        return Err(ApplicabilityError::SingularCovariance);
    }
    let dimension = covariance.nrows();
    for row in 0..dimension {
        if covariance[(row, row)] <= 0.0 || inverse[(row, row)] <= 0.0 {
            return Err(ApplicabilityError::SingularCovariance);
        }
        for column in 0..dimension {
            let covariance_scale = 1.0 + covariance[(row, column)].abs();
            let inverse_scale = 1.0 + inverse[(row, column)].abs();
            if (covariance[(row, column)] - covariance[(column, row)]).abs()
                > 1.0e-12 * covariance_scale
                || (inverse[(row, column)] - inverse[(column, row)]).abs() > 1.0e-12 * inverse_scale
            {
                return Err(ApplicabilityError::SingularCovariance);
            }
        }
    }
    if covariance.clone().cholesky().is_none() || inverse.clone().cholesky().is_none() {
        return Err(ApplicabilityError::SingularCovariance);
    }
    let residual = covariance * inverse - DMatrix::identity(covariance.nrows(), covariance.ncols());
    let maximum = residual
        .iter()
        .fold(0.0_f64, |value, item| value.max(item.abs()));
    if maximum.is_finite() && maximum <= 1.0e-8 {
        Ok(())
    } else {
        Err(ApplicabilityError::SingularCovariance)
    }
}

fn hash_u32(hasher: &mut blake3::Hasher, value: u32) {
    hasher.update(&value.to_le_bytes());
}

fn hash_u64(hasher: &mut blake3::Hasher, value: u64) {
    hasher.update(&value.to_le_bytes());
}

fn hash_f64(hasher: &mut blake3::Hasher, value: f64) {
    hasher.update(&value.to_bits().to_le_bytes());
}

fn hash_f64_slice(hasher: &mut blake3::Hasher, values: &[f64]) {
    hash_u64(hasher, values.len() as u64);
    for value in values {
        hash_f64(hasher, *value);
    }
}
