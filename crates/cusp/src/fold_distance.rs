use numerics::{OptimizationDiagnostics, brent_minimize, brent_root};
use serde::{Deserialize, Serialize};

use crate::{Controls, CuspError, roots::state_scale};

const SEARCH_GRID_INTERVALS: usize = 256;
const BRENT_TOLERANCE: f64 = 1e-12;
const BRENT_MAXIMUM_ITERATIONS: u32 = 256;

/// Conditioning evidence for the covariance used to whiten control space.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct CovarianceConditioning {
    pub minimum_eigenvalue: f64,
    pub maximum_eigenvalue: f64,
    pub condition_number: f64,
}

/// Validated two-control covariance whitening.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawControlWhitening", into = "RawControlWhitening")]
pub struct ControlWhitening {
    alpha_variance: f64,
    alpha_beta_covariance: f64,
    beta_variance: f64,
    alpha_standard_deviation: f64,
    beta_standard_deviation: f64,
    correlation: f64,
    decorrelation_scale: f64,
    pub conditioning: CovarianceConditioning,
}

impl ControlWhitening {
    pub fn identity() -> Self {
        Self {
            alpha_variance: 1.0,
            alpha_beta_covariance: 0.0,
            beta_variance: 1.0,
            alpha_standard_deviation: 1.0,
            beta_standard_deviation: 1.0,
            correlation: 0.0,
            decorrelation_scale: 1.0,
            conditioning: CovarianceConditioning {
                minimum_eigenvalue: 1.0,
                maximum_eigenvalue: 1.0,
                condition_number: 1.0,
            },
        }
    }

    pub fn diagonal(alpha_variance: f64, beta_variance: f64) -> Result<Self, CuspError> {
        Self::from_covariance(alpha_variance, 0.0, beta_variance)
    }

    pub fn from_covariance(
        alpha_variance: f64,
        alpha_beta_covariance: f64,
        beta_variance: f64,
    ) -> Result<Self, CuspError> {
        if !alpha_variance.is_finite()
            || !alpha_beta_covariance.is_finite()
            || !beta_variance.is_finite()
            || alpha_variance <= 0.0
            || beta_variance <= 0.0
        {
            return Err(CuspError::InvalidWhitening);
        }
        let scale = alpha_variance
            .max(alpha_beta_covariance.abs())
            .max(beta_variance);
        let normalized_alpha = alpha_variance / scale;
        let normalized_covariance = alpha_beta_covariance / scale;
        let normalized_beta = beta_variance / scale;
        let determinant = normalized_alpha * normalized_beta - normalized_covariance.powi(2);
        let maximum_eigenvalue_normalized = 0.5
            * (normalized_alpha
                + normalized_beta
                + (normalized_alpha - normalized_beta).hypot(2.0 * normalized_covariance));
        let minimum_eigenvalue_normalized = determinant / maximum_eigenvalue_normalized;
        let minimum_eigenvalue = minimum_eigenvalue_normalized * scale;
        let maximum_eigenvalue = maximum_eigenvalue_normalized * scale;
        let condition_number = maximum_eigenvalue_normalized / minimum_eigenvalue_normalized;
        let correlation = normalized_covariance / (normalized_alpha * normalized_beta).sqrt();
        let decorrelation_scale = (-correlation).mul_add(correlation, 1.0).sqrt();
        if determinant <= 0.0
            || minimum_eigenvalue_normalized <= 0.0
            || !minimum_eigenvalue.is_finite()
            || minimum_eigenvalue <= 0.0
            || !maximum_eigenvalue.is_finite()
            || maximum_eigenvalue <= 0.0
            || !condition_number.is_finite()
            || !correlation.is_finite()
            || !decorrelation_scale.is_finite()
            || decorrelation_scale <= 0.0
        {
            return Err(CuspError::InvalidWhitening);
        }
        Ok(Self {
            alpha_variance,
            alpha_beta_covariance,
            beta_variance,
            alpha_standard_deviation: alpha_variance.sqrt(),
            beta_standard_deviation: beta_variance.sqrt(),
            correlation,
            decorrelation_scale,
            conditioning: CovarianceConditioning {
                minimum_eigenvalue,
                maximum_eigenvalue,
                condition_number,
            },
        })
    }

    /// Mahalanobis distance between two control points.
    pub fn distance(self, left: Controls, right: Controls) -> Result<f64, CuspError> {
        left.validate()?;
        right.validate()?;
        let alpha_residual = (left.alpha - right.alpha) / self.alpha_standard_deviation;
        let beta_residual = (left.beta - right.beta) / self.beta_standard_deviation;
        let decorrelated_beta =
            (beta_residual - self.correlation * alpha_residual) / self.decorrelation_scale;
        let distance = alpha_residual.hypot(decorrelated_beta);
        if distance.is_finite() {
            Ok(distance)
        } else {
            Err(CuspError::NonFiniteDerivedValue)
        }
    }

    fn fold_objective_derivative(
        self,
        controls: Controls,
        parameter: f64,
    ) -> Result<f64, CuspError> {
        let fold = fold_point(parameter)?;
        let alpha_residual = (controls.alpha - fold.alpha) / self.alpha_standard_deviation;
        let beta_residual = (controls.beta - fold.beta) / self.beta_standard_deviation;
        let alpha_derivative = 6.0 * parameter.powi(2) / self.alpha_standard_deviation;
        let beta_derivative = -6.0 * parameter / self.beta_standard_deviation;
        let decorrelated_beta =
            (beta_residual - self.correlation * alpha_residual) / self.decorrelation_scale;
        let decorrelated_derivative =
            (beta_derivative - self.correlation * alpha_derivative) / self.decorrelation_scale;
        let derivative =
            2.0 * (alpha_residual * alpha_derivative + decorrelated_beta * decorrelated_derivative);
        if derivative.is_finite() {
            Ok(derivative)
        } else {
            Err(CuspError::NonFiniteDerivedValue)
        }
    }
}

/// Signed nearest-fold distance and reproducible optimizer evidence.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FoldDistance {
    pub distance: f64,
    pub nearest: Controls,
    pub fold_parameter: f64,
    pub diagnostics: OptimizationDiagnostics,
    pub covariance_conditioning: CovarianceConditioning,
}

/// Evaluate the approved fold parameterization `(-2s^3, 3s^2)`.
pub fn fold_point(parameter: f64) -> Result<Controls, CuspError> {
    if !parameter.is_finite() {
        return Err(CuspError::NonFiniteInput);
    }
    let controls = Controls {
        alpha: -2.0 * parameter.powi(3),
        beta: 3.0 * parameter.powi(2),
    };
    controls
        .validate()
        .map(|()| controls)
        .map_err(|_| CuspError::NonFiniteDerivedValue)
}

/// Find the nearest fold point in validated whitened control space.
pub fn nearest_fold(
    controls: Controls,
    whitening: &ControlWhitening,
) -> Result<FoldDistance, CuspError> {
    controls.validate()?;
    let bound = fold_search_bound(controls, *whitening)?;
    if bound == 0.0 {
        return zero_fold_result(*whitening);
    }

    let mut grid = Vec::with_capacity(SEARCH_GRID_INTERVALS + 1);
    for index in 0..=SEARCH_GRID_INTERVALS {
        let unit_parameter = -1.0 + 2.0 * index as f64 / SEARCH_GRID_INTERVALS as f64;
        let parameter = unit_parameter * bound;
        let distance = whitening.distance(controls, fold_point(parameter)?)?;
        grid.push((unit_parameter, distance));
    }
    let mut candidate_indices = Vec::new();
    for index in 0..=SEARCH_GRID_INTERVALS {
        let left_ok = index == 0 || grid[index].1 <= grid[index - 1].1;
        let right_ok = index == SEARCH_GRID_INTERVALS || grid[index].1 <= grid[index + 1].1;
        if left_ok && right_ok {
            candidate_indices.push(index);
        }
    }

    let mut best = None;
    let mut total_iterations = 0_u32;
    let mut total_evaluations =
        u32::try_from(grid.len()).map_err(|_| CuspError::FoldOptimization)?;
    for index in candidate_indices {
        let left_index = index.saturating_sub(1);
        let right_index = (index + 1).min(SEARCH_GRID_INTERVALS);
        if left_index == right_index {
            continue;
        }
        let mut optimum = brent_minimize(
            |unit_parameter| {
                fold_point(unit_parameter * bound)
                    .and_then(|fold| whitening.distance(controls, fold))
                    .unwrap_or(f64::NAN)
            },
            grid[left_index].0,
            grid[right_index].0,
            BRENT_TOLERANCE,
            BRENT_MAXIMUM_ITERATIONS,
        )
        .map_err(|_| CuspError::FoldOptimization)?;
        let left_derivative =
            whitening.fold_objective_derivative(controls, grid[left_index].0 * bound)? * bound;
        let right_derivative =
            whitening.fold_objective_derivative(controls, grid[right_index].0 * bound)? * bound;
        total_evaluations = total_evaluations
            .checked_add(2)
            .ok_or(CuspError::FoldOptimization)?;
        if left_derivative == 0.0
            || right_derivative == 0.0
            || left_derivative.is_sign_positive() != right_derivative.is_sign_positive()
        {
            let polished = brent_root(
                |unit_parameter| {
                    whitening
                        .fold_objective_derivative(controls, unit_parameter * bound)
                        .map(|derivative| derivative * bound)
                        .unwrap_or(f64::NAN)
                },
                grid[left_index].0,
                grid[right_index].0,
                BRENT_TOLERANCE,
                BRENT_MAXIMUM_ITERATIONS,
            )
            .map_err(|_| CuspError::FoldOptimization)?;
            optimum.argmin = polished.root;
            optimum.value = whitening.distance(controls, fold_point(polished.root * bound)?)?;
            total_iterations = total_iterations
                .checked_add(polished.diagnostics.iterations)
                .ok_or(CuspError::FoldOptimization)?;
            total_evaluations = total_evaluations
                .checked_add(polished.diagnostics.evaluations)
                .ok_or(CuspError::FoldOptimization)?;
        }
        total_iterations = total_iterations
            .checked_add(optimum.diagnostics.iterations)
            .ok_or(CuspError::FoldOptimization)?;
        total_evaluations = total_evaluations
            .checked_add(optimum.diagnostics.evaluations)
            .ok_or(CuspError::FoldOptimization)?;
        if best
            .as_ref()
            .is_none_or(|current: &numerics::ScalarOptimum| optimum.value < current.value)
        {
            best = Some(optimum);
        }
    }
    let optimum = best.ok_or(CuspError::FoldOptimization)?;
    let fold_parameter = optimum.argmin * bound;
    let nearest = fold_point(fold_parameter)?;
    let magnitude = whitening.distance(controls, nearest)?;
    let distance = if inside_cusp_scaled(controls)? {
        -magnitude
    } else {
        magnitude
    };
    Ok(FoldDistance {
        distance,
        nearest,
        fold_parameter,
        diagnostics: OptimizationDiagnostics {
            schema_version: 1,
            method: "fold-grid-brent-v1".to_owned(),
            converged: true,
            iterations: total_iterations,
            evaluations: total_evaluations,
            objective: magnitude,
            gradient_norm: None,
            condition_number: Some(whitening.conditioning.condition_number),
            requested_tolerance: BRENT_TOLERANCE,
            termination: "all_grid_local_basins_refined".to_owned(),
            deterministic_seed: None,
        },
        covariance_conditioning: whitening.conditioning,
    })
}

fn fold_search_bound(controls: Controls, whitening: ControlWhitening) -> Result<f64, CuspError> {
    let distance_at_origin = whitening.distance(controls, Controls::default())?;
    let alpha_radius =
        finite_product_or_max(distance_at_origin, whitening.alpha_standard_deviation);
    let beta_radius = finite_product_or_max(distance_at_origin, whitening.beta_standard_deviation);
    let alpha_bound = positive_sum_root(controls.alpha.abs(), alpha_radius, 2.0, 3.0)?;
    let beta_bound = positive_sum_root(controls.beta.abs(), beta_radius, 3.0, 2.0)?;
    let bound = alpha_bound.max(beta_bound);
    if bound.is_finite() {
        Ok(bound)
    } else {
        Err(CuspError::NonFiniteDerivedValue)
    }
}

fn positive_sum_root(left: f64, right: f64, divisor: f64, root: f64) -> Result<f64, CuspError> {
    let scale = left.max(right);
    if scale == 0.0 {
        return Ok(0.0);
    }
    let normalized_sum = left / scale + right / scale;
    let scaled_sum = (scale / divisor) * normalized_sum;
    let result = if root == 3.0 {
        scaled_sum.cbrt()
    } else {
        scaled_sum.sqrt()
    };
    if result.is_finite() {
        Ok(result)
    } else {
        Err(CuspError::NonFiniteDerivedValue)
    }
}

fn finite_product_or_max(left: f64, right: f64) -> f64 {
    let product = left * right;
    if product.is_finite() {
        product
    } else {
        f64::MAX
    }
}

fn inside_cusp_scaled(controls: Controls) -> Result<bool, CuspError> {
    let scale = state_scale(controls)?;
    if scale == 0.0 || controls.beta <= 0.0 {
        return Ok(false);
    }
    let normalized_beta = (controls.beta / scale) / scale;
    let normalized_alpha = ((controls.alpha / scale) / scale) / scale;
    let discriminant = 4.0 * normalized_beta.powi(3) - 27.0 * normalized_alpha.powi(2);
    if discriminant.is_finite() {
        Ok(discriminant > 0.0)
    } else {
        Err(CuspError::NonFiniteDerivedValue)
    }
}

fn zero_fold_result(whitening: ControlWhitening) -> Result<FoldDistance, CuspError> {
    Ok(FoldDistance {
        distance: 0.0,
        nearest: fold_point(0.0)?,
        fold_parameter: 0.0,
        diagnostics: OptimizationDiagnostics {
            schema_version: 1,
            method: "fold-grid-brent-v1".to_owned(),
            converged: true,
            iterations: 0,
            evaluations: 1,
            objective: 0.0,
            gradient_norm: None,
            condition_number: Some(whitening.conditioning.condition_number),
            requested_tolerance: BRENT_TOLERANCE,
            termination: "exact_cusp_point".to_owned(),
            deterministic_seed: None,
        },
        covariance_conditioning: whitening.conditioning,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawControlWhitening {
    alpha_variance: f64,
    alpha_beta_covariance: f64,
    beta_variance: f64,
}

impl TryFrom<RawControlWhitening> for ControlWhitening {
    type Error = CuspError;

    fn try_from(raw: RawControlWhitening) -> Result<Self, Self::Error> {
        Self::from_covariance(
            raw.alpha_variance,
            raw.alpha_beta_covariance,
            raw.beta_variance,
        )
    }
}

impl From<ControlWhitening> for RawControlWhitening {
    fn from(whitening: ControlWhitening) -> Self {
        Self {
            alpha_variance: whitening.alpha_variance,
            alpha_beta_covariance: whitening.alpha_beta_covariance,
            beta_variance: whitening.beta_variance,
        }
    }
}
