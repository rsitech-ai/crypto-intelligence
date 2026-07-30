use serde::Serialize;

use crate::{
    Barrier, Controls, CuspError, EquilibriumRoot, barrier::adjacent_barriers, real_equilibria,
    roots::state_scale,
};

const STABILITY_ROUNDOFF_MULTIPLIER: f64 = 128.0;

/// Stability under the approved gradient-flow potential.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum Stability {
    Stable,
    Unstable,
    NeutralAtTolerance,
}

/// Numerically observed equilibrium topology.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum EquilibriumTopology {
    OneStable,
    ThreeBranches,
    Fold,
    Critical,
}

/// A root with derived stability and restoring-force evidence.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct ClassifiedRoot {
    pub equilibrium: EquilibriumRoot,
    pub stability: Stability,
    /// `V''(y*)` in the original state units.
    pub hessian: f64,
    /// Positive local restoring coefficient, defined only for stable roots.
    pub restoring_force: Option<f64>,
}

/// Complete deterministic equilibrium analysis for one control point.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EquilibriumSet {
    pub controls: Controls,
    pub topology: EquilibriumTopology,
    pub roots: Vec<ClassifiedRoot>,
    pub barriers: Vec<Barrier>,
}

impl EquilibriumSet {
    /// Return the smallest branch-specific barrier when three branches exist.
    pub fn minimum_barrier(&self) -> Option<f64> {
        self.barriers
            .iter()
            .map(|barrier| barrier.height)
            .reduce(f64::min)
    }
}

/// Classify all real equilibria and calculate valid adjacent barriers.
pub fn analyze_equilibria(controls: Controls) -> Result<EquilibriumSet, CuspError> {
    let scale = state_scale(controls)?;
    let roots = real_equilibria(controls)?;
    let normalized_beta = if scale == 0.0 {
        0.0
    } else {
        (controls.beta / scale) / scale
    };
    if !normalized_beta.is_finite() {
        return Err(CuspError::NonFiniteDerivedValue);
    }

    let classified = roots
        .into_iter()
        .map(|root| classify_root(root, scale, normalized_beta))
        .collect::<Result<Vec<_>, _>>()?;
    let topology = determine_topology(&classified)?;
    let barriers = if topology == EquilibriumTopology::ThreeBranches {
        adjacent_barriers(&classified, scale)?
    } else {
        Vec::new()
    };

    Ok(EquilibriumSet {
        controls,
        topology,
        roots: classified,
        barriers,
    })
}

fn classify_root(
    equilibrium: EquilibriumRoot,
    scale: f64,
    normalized_beta: f64,
) -> Result<ClassifiedRoot, CuspError> {
    let normalized_root = if scale == 0.0 {
        0.0
    } else {
        equilibrium.value / scale
    };
    let root_term = 3.0 * normalized_root.powi(2);
    let normalized_hessian = root_term - normalized_beta;
    let tolerance = STABILITY_ROUNDOFF_MULTIPLIER
        * f64::EPSILON
        * root_term.abs().max(normalized_beta.abs()).max(1.0);
    if !normalized_root.is_finite() || !normalized_hessian.is_finite() || !tolerance.is_finite() {
        return Err(CuspError::NonFiniteDerivedValue);
    }

    let stability = if normalized_hessian > tolerance {
        Stability::Stable
    } else if normalized_hessian < -tolerance {
        Stability::Unstable
    } else {
        Stability::NeutralAtTolerance
    };
    let hessian = restore_hessian(normalized_hessian, scale)?;
    let restoring_force = match stability {
        Stability::Stable if hessian > 0.0 => Some(hessian),
        Stability::Stable => return Err(CuspError::InvalidEquilibriumTopology),
        Stability::Unstable | Stability::NeutralAtTolerance => None,
    };

    Ok(ClassifiedRoot {
        equilibrium,
        stability,
        hessian,
        restoring_force,
    })
}

fn restore_hessian(normalized_hessian: f64, scale: f64) -> Result<f64, CuspError> {
    if scale == 0.0 {
        return Ok(0.0);
    }
    let scale_squared = scale * scale;
    let hessian = normalized_hessian * scale_squared;
    if !scale_squared.is_finite()
        || !hessian.is_finite()
        || (normalized_hessian != 0.0 && hessian == 0.0)
    {
        Err(CuspError::NonFiniteDerivedValue)
    } else {
        Ok(hessian)
    }
}

fn determine_topology(roots: &[ClassifiedRoot]) -> Result<EquilibriumTopology, CuspError> {
    match roots {
        [root] if root.equilibrium.multiplicity == 1 && root.stability == Stability::Stable => {
            Ok(EquilibriumTopology::OneStable)
        }
        [root]
            if root.equilibrium.multiplicity == 3
                && root.stability == Stability::NeutralAtTolerance =>
        {
            Ok(EquilibriumTopology::Critical)
        }
        [left, right]
            if left.equilibrium.multiplicity + right.equilibrium.multiplicity == 3
                && [left, right]
                    .iter()
                    .any(|root| root.equilibrium.multiplicity == 2)
                && [left, right]
                    .iter()
                    .filter(|root| root.stability == Stability::Stable)
                    .count()
                    == 1
                && [left, right]
                    .iter()
                    .filter(|root| root.stability == Stability::NeutralAtTolerance)
                    .count()
                    == 1 =>
        {
            Ok(EquilibriumTopology::Fold)
        }
        [left, middle, right]
            if [left, middle, right]
                .iter()
                .all(|root| root.equilibrium.multiplicity == 1)
                && left.stability == Stability::Stable
                && middle.stability == Stability::Unstable
                && right.stability == Stability::Stable =>
        {
            Ok(EquilibriumTopology::ThreeBranches)
        }
        _ => Err(CuspError::InvalidEquilibriumTopology),
    }
}
