use crate::{Controls, CuspError};

/// Product cusp potential using the repository's approved sign convention.
pub struct Potential;

impl Potential {
    /// Evaluate `y^4 / 4 - beta * y^2 / 2 - alpha * y`.
    pub fn value(state: f64, controls: Controls) -> f64 {
        0.25 * state.powi(4) - 0.5 * controls.beta * state.powi(2) - controls.alpha * state
    }

    /// Evaluate `dV/dy = y^3 - beta * y - alpha`.
    pub fn gradient(state: f64, controls: Controls) -> f64 {
        state.powi(3) - controls.beta * state - controls.alpha
    }

    /// Evaluate `d2V/dy2 = 3 * y^2 - beta`.
    pub fn hessian(state: f64, controls: Controls) -> f64 {
        3.0 * state.powi(2) - controls.beta
    }

    /// Checked potential evaluation for model and I/O boundaries.
    pub fn checked_value(state: f64, controls: Controls) -> Result<f64, CuspError> {
        checked_evaluation(state, controls, Self::value)
    }

    /// Checked gradient evaluation for model and I/O boundaries.
    pub fn checked_gradient(state: f64, controls: Controls) -> Result<f64, CuspError> {
        checked_evaluation(state, controls, Self::gradient)
    }

    /// Checked Hessian evaluation for model and I/O boundaries.
    pub fn checked_hessian(state: f64, controls: Controls) -> Result<f64, CuspError> {
        checked_evaluation(state, controls, Self::hessian)
    }
}

fn checked_evaluation(
    state: f64,
    controls: Controls,
    evaluate: fn(f64, Controls) -> f64,
) -> Result<f64, CuspError> {
    if !state.is_finite() {
        return Err(CuspError::NonFiniteInput);
    }
    controls.validate()?;
    let value = evaluate(state, controls);
    if value.is_finite() {
        Ok(value)
    } else {
        Err(CuspError::NonFiniteDerivedValue)
    }
}
