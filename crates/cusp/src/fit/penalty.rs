use super::{
    CoefficientSign, CoordinateScope, FitError, Objective, ParameterLayout, PenaltyConfig,
};

pub(super) fn add_smooth_penalty(
    objective: &mut Objective,
    parameters: &[f64],
    layout: &ParameterLayout,
    config: PenaltyConfig,
) -> Result<(), FitError> {
    let l2 = config.lambda() * (1.0 - config.l1_ratio());
    for (index, (parameter, coordinate)) in parameters.iter().zip(&layout.coordinates).enumerate() {
        let weight = penalty_weight(*coordinate, config);
        objective.value += 0.5 * l2 * weight * parameter * parameter;
        objective.gradient[index] += l2 * weight * parameter;
    }
    if objective.value.is_finite() && objective.gradient.iter().all(|value| value.is_finite()) {
        Ok(())
    } else {
        Err(FitError::NonFiniteObjective)
    }
}

pub(super) fn composite_value(
    smooth: f64,
    parameters: &[f64],
    layout: &ParameterLayout,
    config: PenaltyConfig,
) -> Result<f64, FitError> {
    let l1 = config.lambda() * config.l1_ratio();
    let penalty = parameters
        .iter()
        .zip(&layout.coordinates)
        .map(|(parameter, coordinate)| l1 * penalty_weight(*coordinate, config) * parameter.abs())
        .sum::<f64>();
    let value = smooth + penalty;
    if value.is_finite() {
        Ok(value)
    } else {
        Err(FitError::NonFiniteObjective)
    }
}

pub(super) fn proximal_step(
    parameters: &[f64],
    gradient: &[f64],
    step: f64,
    layout: &ParameterLayout,
    config: PenaltyConfig,
) -> Result<Vec<f64>, FitError> {
    if parameters.len() != gradient.len()
        || parameters.len() != layout.coordinates.len()
        || !step.is_finite()
        || step <= 0.0
    {
        return Err(FitError::InvalidParameters);
    }
    let l1 = config.lambda() * config.l1_ratio();
    parameters
        .iter()
        .zip(gradient)
        .zip(&layout.coordinates)
        .map(|((parameter, derivative), coordinate)| {
            let unprojected = parameter - step * derivative;
            let threshold = step * l1 * penalty_weight(*coordinate, config);
            let thresholded = soft_threshold(unprojected, threshold);
            let projected = match coordinate.sign {
                CoefficientSign::Any => thresholded,
                CoefficientSign::NonNegative => thresholded.max(0.0),
                CoefficientSign::NonPositive => thresholded.min(0.0),
            };
            if projected.is_finite() {
                Ok(projected)
            } else {
                Err(FitError::NonFiniteObjective)
            }
        })
        .collect()
}

fn penalty_weight(coordinate: super::Coordinate, config: PenaltyConfig) -> f64 {
    match coordinate.scope {
        CoordinateScope::Intercept => 0.0,
        CoordinateScope::Shared => coordinate.sparse_weight,
        CoordinateScope::Asset(_) => coordinate.sparse_weight * config.asset_multiplier(),
    }
}

fn soft_threshold(value: f64, threshold: f64) -> f64 {
    if value > threshold {
        value - threshold
    } else if value < -threshold {
        value + threshold
    } else {
        0.0
    }
}
