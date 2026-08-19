use serde::Serialize;

use crate::{ClassifiedRoot, CuspError, Stability};

/// Escape barrier from a stable root to its adjacent unstable root.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Barrier {
    pub stable_root: f64,
    pub unstable_root: f64,
    /// `V(unstable_root) - V(stable_root)` in original potential units.
    pub height: f64,
}

pub(crate) fn adjacent_barriers(
    roots: &[ClassifiedRoot],
    scale: f64,
) -> Result<Vec<Barrier>, CuspError> {
    let [left, middle, right] = roots else {
        return Err(CuspError::InvalidEquilibriumTopology);
    };
    if left.stability != Stability::Stable
        || middle.stability != Stability::Unstable
        || right.stability != Stability::Stable
        || scale <= 0.0
        || !scale.is_finite()
    {
        return Err(CuspError::InvalidEquilibriumTopology);
    }

    let normalized_left = left.equilibrium.value / scale;
    let normalized_middle = middle.equilibrium.value / scale;
    let normalized_right = right.equilibrium.value / scale;
    if !normalized_left.is_finite()
        || !normalized_middle.is_finite()
        || !normalized_right.is_finite()
        || normalized_left >= normalized_middle
        || normalized_middle >= normalized_right
    {
        return Err(CuspError::InvalidEquilibriumTopology);
    }

    let left_gap = normalized_middle - normalized_left;
    let right_gap = normalized_right - normalized_middle;
    // For sorted stationary roots a < b < c with a + b + c = 0, integrating
    // (z-a)(z-b)(z-c) gives V(b)-V(a)=c(b-a)^3/4 and
    // V(b)-V(c)=-a(c-b)^3/4. This avoids subtracting near-equal potentials.
    let left_height = 0.25 * normalized_right * left_gap.powi(3);
    let right_height = -0.25 * normalized_left * right_gap.powi(3);
    Ok(vec![
        Barrier {
            stable_root: left.equilibrium.value,
            unstable_root: middle.equilibrium.value,
            height: restore_height(left_height, scale)?,
        },
        Barrier {
            stable_root: right.equilibrium.value,
            unstable_root: middle.equilibrium.value,
            height: restore_height(right_height, scale)?,
        },
    ])
}

fn restore_height(normalized_height: f64, scale: f64) -> Result<f64, CuspError> {
    let scale_squared = scale * scale;
    let height = (normalized_height * scale_squared) * scale_squared;
    if !normalized_height.is_finite()
        || normalized_height <= 0.0
        || !scale_squared.is_finite()
        || !height.is_finite()
        || height <= 0.0
    {
        Err(CuspError::NonFiniteDerivedValue)
    } else {
        Ok(height)
    }
}
