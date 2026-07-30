use statrs::function::gamma::ln_gamma;

use super::{FitError, FitProblem, Objective};

pub(super) fn objective(problem: &FitProblem, parameters: &[f64]) -> Result<Objective, FitError> {
    let degrees = problem.config.degrees_of_freedom;
    let constant = student_t_log_normalizer(degrees)?;
    let total_weight = problem.rows.iter().map(|row| row.weight).sum::<f64>();
    if !total_weight.is_finite() || total_weight <= 0.0 {
        return Err(FitError::InvalidRow);
    }

    let mut value = 0.0;
    let mut gradient = vec![0.0; parameters.len()];
    for row in &problem.rows {
        let normalized_weight = row.weight / total_weight;
        let (alpha, beta) = problem.controls(parameters, row)?;
        let drift = alpha + beta * row.state - row.state.powi(3);
        let innovation_scale = row.innovation_scale * row.delta_time.sqrt();
        let residual = row.delta_state - drift * row.delta_time;
        let standardized = residual / innovation_scale;
        if !drift.is_finite()
            || !innovation_scale.is_finite()
            || innovation_scale <= 0.0
            || !standardized.is_finite()
        {
            return Err(FitError::NonFiniteObjective);
        }
        let contribution = constant
            + innovation_scale.ln()
            + 0.5 * (degrees + 1.0) * (standardized * standardized / degrees).ln_1p();
        value += normalized_weight * contribution;
        let score = (degrees + 1.0) * standardized / (degrees + standardized * standardized);
        let residual_factor = -row.delta_time / innovation_scale;
        for (index, derivative) in gradient.iter_mut().enumerate() {
            let (alpha_derivative, beta_derivative) = problem.control_derivative(index, row)?;
            let drift_derivative = beta_derivative.mul_add(row.state, alpha_derivative);
            *derivative += normalized_weight * score * residual_factor * drift_derivative;
        }
    }
    Ok(Objective { value, gradient })
}

pub(super) fn hessian(problem: &FitProblem, parameters: &[f64]) -> Result<Vec<Vec<f64>>, FitError> {
    let degrees = problem.config.degrees_of_freedom;
    let total_weight = problem.rows.iter().map(|row| row.weight).sum::<f64>();
    if !total_weight.is_finite() || total_weight <= 0.0 {
        return Err(FitError::InvalidRow);
    }
    let dimension = parameters.len();
    let mut hessian = vec![vec![0.0; dimension]; dimension];
    for row in &problem.rows {
        let normalized_weight = row.weight / total_weight;
        let (alpha, beta) = problem.controls(parameters, row)?;
        let drift = alpha + beta * row.state - row.state.powi(3);
        let innovation_scale = row.innovation_scale * row.delta_time.sqrt();
        let standardized = (row.delta_state - drift * row.delta_time) / innovation_scale;
        if !drift.is_finite()
            || !innovation_scale.is_finite()
            || innovation_scale <= 0.0
            || !standardized.is_finite()
        {
            return Err(FitError::NonFiniteObjective);
        }
        let denominator = degrees + standardized * standardized;
        let score_derivative =
            (degrees + 1.0) * (degrees - standardized * standardized) / denominator.powi(2);
        let residual_factor = -row.delta_time / innovation_scale;
        let curvature = normalized_weight * score_derivative * residual_factor.powi(2);
        if !curvature.is_finite() {
            return Err(FitError::NonFiniteObjective);
        }
        let design = (0..dimension)
            .map(|index| {
                let (alpha_derivative, beta_derivative) = problem.control_derivative(index, row)?;
                let drift_derivative = beta_derivative.mul_add(row.state, alpha_derivative);
                if drift_derivative.is_finite() {
                    Ok(drift_derivative)
                } else {
                    Err(FitError::NonFiniteObjective)
                }
            })
            .collect::<Result<Vec<_>, FitError>>()?;
        for left in 0..dimension {
            for right in 0..=left {
                let contribution = curvature * design[left] * design[right];
                hessian[left][right] += contribution;
                if left != right {
                    hessian[right][left] += contribution;
                }
            }
        }
    }
    Ok(hessian)
}

fn student_t_log_normalizer(degrees: f64) -> Result<f64, FitError> {
    let value = ln_gamma(degrees / 2.0) - ln_gamma((degrees + 1.0) / 2.0)
        + 0.5 * (degrees * std::f64::consts::PI).ln();
    if value.is_finite() {
        Ok(value)
    } else {
        Err(FitError::NonFiniteObjective)
    }
}

#[cfg(test)]
mod tests {
    use super::student_t_log_normalizer;

    #[test]
    fn student_t_normalizer_matches_literal_nu_five_reference() {
        let degrees = 5.0;
        let actual = student_t_log_normalizer(degrees).expect("normalizer");
        let expected = 0.968_619_589_054_724_2;
        assert!((actual - expected).abs() < 1.0e-14);
    }
}
