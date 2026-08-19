use crate::NumericalError;

const MAXIMUM_GRADIENT_DIMENSION: usize = 4_096;

/// Central finite-difference gradient with a scale-aware step per coordinate.
pub fn central_gradient<F>(
    mut function: F,
    point: &[f64],
    relative_step: f64,
) -> Result<Vec<f64>, NumericalError>
where
    F: FnMut(&[f64]) -> f64,
{
    if point.is_empty() || point.len() > MAXIMUM_GRADIENT_DIMENSION {
        return Err(NumericalError::Dimension);
    }
    if point.iter().any(|value| !value.is_finite()) {
        return Err(NumericalError::NonFiniteInput);
    }
    if !relative_step.is_finite() || relative_step <= 0.0 || relative_step > 1.0 {
        return Err(NumericalError::InvalidTolerance);
    }
    let mut working = point.to_vec();
    let mut gradient = Vec::with_capacity(point.len());
    for (index, coordinate) in point.iter().copied().enumerate() {
        let step = relative_step * coordinate.abs().max(1.0);
        let upper = coordinate + step;
        let lower = coordinate - step;
        if !upper.is_finite() || !lower.is_finite() || upper == lower {
            return Err(NumericalError::NonFiniteInput);
        }
        working[index] = upper;
        let upper_value = function(&working);
        working[index] = lower;
        let lower_value = function(&working);
        working[index] = coordinate;
        if !upper_value.is_finite() || !lower_value.is_finite() {
            return Err(NumericalError::NonFiniteEvaluation);
        }
        let derivative = (upper_value - lower_value) / (upper - lower);
        if !derivative.is_finite() {
            return Err(NumericalError::NonFiniteEvaluation);
        }
        gradient.push(derivative);
    }
    Ok(gradient)
}
