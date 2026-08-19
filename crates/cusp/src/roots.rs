use serde::Serialize;

use crate::{Controls, CuspError};

const DISCRIMINANT_ROUNDOFF_MULTIPLIER: f64 = 128.0;
const TRIGONOMETRIC_DOMAIN_ROUNDOFF_MULTIPLIER: f64 = 64.0;
const CONDITION_DERIVATIVE_FLOOR_MULTIPLIER: f64 = 64.0;
const NEWTON_POLISH_STEPS: usize = 3;

/// A unique real equilibrium with numerical multiplicity and conditioning.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct EquilibriumRoot {
    pub value: f64,
    pub multiplicity: u8,
    /// Absolute residual of the scale-normalized depressed cubic.
    pub residual: f64,
    /// Bounded inverse derivative of the scale-normalized cubic.
    pub condition_proxy: f64,
}

/// Return the sorted unique real roots of `y^3 - beta*y - alpha = 0`.
pub fn real_equilibria(controls: Controls) -> Result<Vec<EquilibriumRoot>, CuspError> {
    controls.validate()?;
    let scale = controls.beta.abs().sqrt().max(controls.alpha.abs().cbrt());
    if scale == 0.0 {
        return Ok(vec![root_metadata(0.0, 3, 1.0, 0.0, 0.0)?]);
    }
    if !scale.is_finite() {
        return Err(CuspError::RootCalculation);
    }

    let normalized_beta = (controls.beta / scale) / scale;
    let normalized_alpha = ((controls.alpha / scale) / scale) / scale;
    if !normalized_alpha.is_finite() || !normalized_beta.is_finite() {
        return Err(CuspError::RootCalculation);
    }

    let beta_term = 4.0 * normalized_beta.powi(3);
    let alpha_term = 27.0 * normalized_alpha.powi(2);
    let discriminant = beta_term - alpha_term;
    let discriminant_tolerance =
        DISCRIMINANT_ROUNDOFF_MULTIPLIER * f64::EPSILON * beta_term.abs().max(alpha_term.abs());
    if !discriminant.is_finite() || !discriminant_tolerance.is_finite() {
        return Err(CuspError::RootCalculation);
    }

    let candidates = if discriminant > discriminant_tolerance {
        three_distinct_roots(normalized_alpha, normalized_beta)?
    } else if discriminant < -discriminant_tolerance {
        vec![RootCandidate {
            value: one_real_root(normalized_alpha, normalized_beta)?,
            multiplicity: 1,
        }]
    } else {
        repeated_roots(normalized_alpha)
    };
    finalize_roots(candidates, scale, normalized_alpha, normalized_beta)
}

#[derive(Clone, Copy, Debug)]
struct RootCandidate {
    value: f64,
    multiplicity: u8,
}

fn three_distinct_roots(
    normalized_alpha: f64,
    normalized_beta: f64,
) -> Result<Vec<RootCandidate>, CuspError> {
    if normalized_beta <= 0.0 {
        return Err(CuspError::RootDomain);
    }
    let base = (normalized_beta / 3.0).sqrt();
    let denominator = 2.0 * base.powi(3);
    if !base.is_finite() || denominator <= 0.0 || !denominator.is_finite() {
        return Err(CuspError::RootCalculation);
    }
    let argument = normalized_alpha / denominator;
    let domain_tolerance = TRIGONOMETRIC_DOMAIN_ROUNDOFF_MULTIPLIER * f64::EPSILON;
    if !argument.is_finite() || argument.abs() > 1.0 + domain_tolerance {
        return Err(CuspError::RootDomain);
    }
    let angle = argument.clamp(-1.0, 1.0).acos() / 3.0;
    let radius = 2.0 * base;
    Ok((0..3)
        .map(|index| RootCandidate {
            value: radius * (angle - 2.0 * std::f64::consts::PI * f64::from(index) / 3.0).cos(),
            multiplicity: 1,
        })
        .collect())
}

fn one_real_root(normalized_alpha: f64, normalized_beta: f64) -> Result<f64, CuspError> {
    if normalized_alpha == 0.0 {
        return Ok(0.0);
    }
    let half_alpha = 0.5 * normalized_alpha;
    let cardano_discriminant = half_alpha.powi(2) - (normalized_beta / 3.0).powi(3);
    if !cardano_discriminant.is_finite() || cardano_discriminant < 0.0 {
        return Err(CuspError::RootDomain);
    }
    let dominant_radicand = half_alpha + cardano_discriminant.sqrt().copysign(half_alpha);
    let dominant_term = dominant_radicand.cbrt();
    let root = if dominant_term == 0.0 {
        normalized_alpha.cbrt()
    } else {
        dominant_term + normalized_beta / (3.0 * dominant_term)
    };
    if root.is_finite() {
        Ok(root)
    } else {
        Err(CuspError::RootCalculation)
    }
}

fn repeated_roots(normalized_alpha: f64) -> Vec<RootCandidate> {
    let base = (0.5 * normalized_alpha).cbrt();
    let mut roots = vec![
        RootCandidate {
            value: 2.0 * base,
            multiplicity: 1,
        },
        RootCandidate {
            value: -base,
            multiplicity: 2,
        },
    ];
    roots.sort_by(|left, right| left.value.total_cmp(&right.value));
    roots
}

fn finalize_roots(
    mut roots: Vec<RootCandidate>,
    scale: f64,
    normalized_alpha: f64,
    normalized_beta: f64,
) -> Result<Vec<EquilibriumRoot>, CuspError> {
    roots.sort_by(|left, right| left.value.total_cmp(&right.value));
    let initial_values: Vec<f64> = roots.iter().map(|root| root.value).collect();
    for (index, root) in roots.iter_mut().enumerate() {
        if root.multiplicity > 1 {
            continue;
        }
        let lower = index.checked_sub(1).map_or(f64::NEG_INFINITY, |neighbor| {
            0.5 * (initial_values[neighbor] + initial_values[index])
        });
        let upper = initial_values
            .get(index + 1)
            .map_or(f64::INFINITY, |neighbor| {
                0.5 * (initial_values[index] + *neighbor)
            });
        root.value = polish_root(root.value, lower, upper, normalized_alpha, normalized_beta);
    }

    roots
        .into_iter()
        .map(|root| {
            root_metadata(
                root.value,
                root.multiplicity,
                scale,
                normalized_alpha,
                normalized_beta,
            )
        })
        .collect()
}

fn polish_root(
    mut root: f64,
    lower: f64,
    upper: f64,
    normalized_alpha: f64,
    normalized_beta: f64,
) -> f64 {
    for _ in 0..NEWTON_POLISH_STEPS {
        let residual = polynomial(root, normalized_alpha, normalized_beta);
        let derivative = derivative(root, normalized_beta);
        if !residual.is_finite()
            || !derivative.is_finite()
            || derivative.abs() <= CONDITION_DERIVATIVE_FLOOR_MULTIPLIER * f64::EPSILON
        {
            break;
        }
        let candidate = root - residual / derivative;
        if !candidate.is_finite() || candidate <= lower || candidate >= upper {
            break;
        }
        root = candidate;
    }
    root
}

fn root_metadata(
    normalized_root: f64,
    multiplicity: u8,
    scale: f64,
    normalized_alpha: f64,
    normalized_beta: f64,
) -> Result<EquilibriumRoot, CuspError> {
    let value = normalized_root * scale;
    let residual = polynomial(normalized_root, normalized_alpha, normalized_beta).abs();
    let derivative = derivative(normalized_root, normalized_beta).abs();
    let condition_proxy =
        1.0 / derivative.max(CONDITION_DERIVATIVE_FLOOR_MULTIPLIER * f64::EPSILON);
    if value.is_finite() && residual.is_finite() && condition_proxy.is_finite() {
        Ok(EquilibriumRoot {
            value,
            multiplicity,
            residual,
            condition_proxy,
        })
    } else {
        Err(CuspError::RootCalculation)
    }
}

fn polynomial(root: f64, normalized_alpha: f64, normalized_beta: f64) -> f64 {
    root.mul_add(root.mul_add(root, -normalized_beta), -normalized_alpha)
}

fn derivative(root: f64, normalized_beta: f64) -> f64 {
    3.0 * root.powi(2) - normalized_beta
}
