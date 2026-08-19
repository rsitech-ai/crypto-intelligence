//! Deterministic, bounded numerical primitives shared by model fitting.

pub mod brent;
pub mod finite_difference;
pub mod logsumexp;

pub use brent::{ScalarOptimum, ScalarRoot, brent_minimize, brent_root};
pub use finite_difference::central_gradient;
pub use logsumexp::logsumexp;

use serde::{Deserialize, Serialize};
use thiserror::Error;

const MAXIMUM_MATRIX_DIMENSION: usize = 256;

/// Convergence evidence returned only with a successful numerical result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptimizationDiagnostics {
    pub schema_version: u32,
    pub method: String,
    pub converged: bool,
    pub iterations: u32,
    pub evaluations: u32,
    pub objective: f64,
    pub gradient_norm: Option<f64>,
    pub condition_number: Option<f64>,
    pub requested_tolerance: f64,
    pub termination: String,
    pub deterministic_seed: Option<u64>,
}

/// Real root of `x^3 + p*x + q`, retained for later cusp consumers.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Root {
    pub value: f64,
    pub residual: f64,
    pub multiplicity_hint: u8,
}

pub fn real_cubic_roots(p: f64, q: f64, tolerance: f64) -> Result<Vec<Root>, NumericalError> {
    validate_tolerance(tolerance)?;
    if !p.is_finite() || !q.is_finite() {
        return Err(NumericalError::NonFiniteInput);
    }
    let discriminant = (q / 2.0).powi(2) + (p / 3.0).powi(3);
    if !discriminant.is_finite() {
        return Err(NumericalError::NonFiniteEvaluation);
    }
    let mut roots = if discriminant > tolerance {
        let square_root = discriminant.sqrt();
        vec![(-q / 2.0 + square_root).cbrt() + (-q / 2.0 - square_root).cbrt()]
    } else if discriminant.abs() <= tolerance {
        let repeated = (-q / 2.0).cbrt();
        vec![2.0 * repeated, -repeated]
    } else {
        if p >= 0.0 {
            return Err(NumericalError::NonFiniteEvaluation);
        }
        let radius = 2.0 * (-p / 3.0).sqrt();
        let argument = ((3.0 * q / (2.0 * p)) * (-3.0 / p).sqrt()).clamp(-1.0, 1.0);
        let theta = argument.acos() / 3.0;
        vec![
            radius * theta.cos(),
            radius * (theta - 2.0 * std::f64::consts::PI / 3.0).cos(),
            radius * (theta - 4.0 * std::f64::consts::PI / 3.0).cos(),
        ]
    };
    if roots.iter().any(|root| !root.is_finite()) {
        return Err(NumericalError::NonFiniteEvaluation);
    }
    roots.sort_by(f64::total_cmp);
    roots.dedup_by(|left, right| (*left - *right).abs() <= tolerance);
    roots
        .into_iter()
        .map(|value| {
            let residual = value.powi(3) + p * value + q;
            if !residual.is_finite() {
                return Err(NumericalError::NonFiniteEvaluation);
            }
            Ok(Root {
                value,
                residual,
                multiplicity_hint: if (3.0 * value * value + p).abs() <= tolerance {
                    2
                } else {
                    1
                },
            })
        })
        .collect()
}

pub fn symmetric_eigenvalues_2x2(a: f64, b: f64, d: f64) -> Result<[f64; 2], NumericalError> {
    if [a, b, d].iter().any(|value| !value.is_finite()) {
        return Err(NumericalError::NonFiniteInput);
    }
    let trace = a + d;
    let radius = (a - d).hypot(2.0 * b);
    let values = [(trace - radius) / 2.0, (trace + radius) / 2.0];
    if values.iter().all(|value| value.is_finite()) {
        Ok(values)
    } else {
        Err(NumericalError::NonFiniteEvaluation)
    }
}

pub fn invert_matrix(mut matrix: Vec<Vec<f64>>) -> Result<Vec<Vec<f64>>, NumericalError> {
    let dimension = matrix.len();
    validate_square_matrix(&matrix)?;
    let mut inverse = vec![vec![0.0; dimension]; dimension];
    for (index, row) in inverse.iter_mut().enumerate() {
        row[index] = 1.0;
    }
    for column in 0..dimension {
        let pivot = (column..dimension)
            .max_by(|left, right| {
                matrix[*left][column]
                    .abs()
                    .total_cmp(&matrix[*right][column].abs())
            })
            .ok_or(NumericalError::Singular)?;
        if matrix[pivot][column].abs() < 1e-14 {
            return Err(NumericalError::Singular);
        }
        matrix.swap(column, pivot);
        inverse.swap(column, pivot);
        let scale = matrix[column][column];
        for index in 0..dimension {
            matrix[column][index] /= scale;
            inverse[column][index] /= scale;
        }
        for row in 0..dimension {
            if row == column {
                continue;
            }
            let factor = matrix[row][column];
            for index in 0..dimension {
                matrix[row][index] -= factor * matrix[column][index];
                inverse[row][index] -= factor * inverse[column][index];
            }
        }
    }
    if inverse.iter().flatten().all(|value| value.is_finite()) {
        Ok(inverse)
    } else {
        Err(NumericalError::NonFiniteEvaluation)
    }
}

/// Test strict positive definiteness with deterministic Cholesky pivots.
pub fn is_positive_definite(matrix: &[Vec<f64>], tolerance: f64) -> Result<bool, NumericalError> {
    validate_tolerance(tolerance)?;
    validate_square_matrix(matrix)?;
    let dimension = matrix.len();
    for (row, values) in matrix.iter().enumerate() {
        for (column, value) in values.iter().enumerate().take(row) {
            let scale = 1.0_f64.max(value.abs()).max(matrix[column][row].abs());
            if (*value - matrix[column][row]).abs() > tolerance * scale {
                return Ok(false);
            }
        }
    }
    let mut lower = vec![vec![0.0; dimension]; dimension];
    for row in 0..dimension {
        for column in 0..=row {
            let dot = (0..column)
                .map(|index| lower[row][index] * lower[column][index])
                .sum::<f64>();
            let residual = matrix[row][column] - dot;
            if !residual.is_finite() {
                return Err(NumericalError::NonFiniteEvaluation);
            }
            if row == column {
                if residual <= tolerance {
                    return Ok(false);
                }
                lower[row][column] = residual.sqrt();
            } else {
                lower[row][column] = residual / lower[column][column];
            }
        }
    }
    Ok(true)
}

pub(crate) fn validate_tolerance(tolerance: f64) -> Result<(), NumericalError> {
    if tolerance.is_finite() && tolerance > 0.0 && tolerance <= 1.0 {
        Ok(())
    } else {
        Err(NumericalError::InvalidTolerance)
    }
}

fn validate_square_matrix(matrix: &[Vec<f64>]) -> Result<(), NumericalError> {
    let dimension = matrix.len();
    if dimension == 0 || dimension > MAXIMUM_MATRIX_DIMENSION {
        return Err(NumericalError::Dimension);
    }
    if matrix.iter().any(|row| row.len() != dimension) {
        return Err(NumericalError::Dimension);
    }
    if matrix.iter().flatten().any(|value| !value.is_finite()) {
        return Err(NumericalError::NonFiniteInput);
    }
    Ok(())
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum NumericalError {
    #[error("numerical input is not finite")]
    NonFiniteInput,
    #[error("numerical callback or derived value is not finite")]
    NonFiniteEvaluation,
    #[error("input collection is empty")]
    EmptyInput,
    #[error("dimension or aggregate work is outside the bounded contract")]
    Dimension,
    #[error("search interval is invalid")]
    InvalidInterval,
    #[error("root interval does not bracket a sign change")]
    InvalidBracket,
    #[error("requested tolerance is invalid")]
    InvalidTolerance,
    #[error("iteration limit is invalid")]
    InvalidIterationLimit,
    #[error("bounded numerical algorithm did not converge")]
    NonConverged,
    #[error("matrix is singular")]
    Singular,
    #[error("invalid numerical input")]
    Input,
}
