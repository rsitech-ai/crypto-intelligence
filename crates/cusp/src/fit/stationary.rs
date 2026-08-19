use super::{FitError, FitProblem, Objective};

const MAX_BOUNDARY_MASS_RATIO: f64 = 1.0e-8;

pub(super) fn objective(problem: &FitProblem, parameters: &[f64]) -> Result<Objective, FitError> {
    let total_weight = problem.rows.iter().map(|row| row.weight).sum::<f64>();
    if !total_weight.is_finite() || total_weight <= 0.0 {
        return Err(FitError::InvalidRow);
    }
    let mut value = 0.0;
    let mut gradient = vec![0.0; parameters.len()];
    for row in &problem.rows {
        let normalized_weight = row.weight / total_weight;
        let (alpha, beta) = problem.controls(parameters, row)?;
        let inverse_temperature = 2.0 / row.innovation_scale.powi(2);
        let moments = quadrature_moments(
            alpha,
            beta,
            inverse_temperature,
            problem.config.integration_bound,
            problem.config.integration_intervals,
        )?;
        let observed_potential = potential(row.state, alpha, beta)?;
        let contribution = inverse_temperature * observed_potential + moments.log_normalizer;
        value += normalized_weight * contribution;

        let alpha_score = inverse_temperature * (moments.mean - row.state);
        let beta_score = 0.5 * inverse_temperature * (moments.second_moment - row.state.powi(2));
        for (index, derivative) in gradient.iter_mut().enumerate() {
            let (alpha_derivative, beta_derivative) = problem.control_derivative(index, row)?;
            *derivative += normalized_weight
                * beta_score.mul_add(beta_derivative, alpha_score * alpha_derivative);
        }
    }
    Ok(Objective { value, gradient })
}

#[derive(Clone, Copy, Debug)]
struct QuadratureMoments {
    log_normalizer: f64,
    mean: f64,
    second_moment: f64,
}

fn quadrature_moments(
    alpha: f64,
    beta: f64,
    inverse_temperature: f64,
    bound: f64,
    intervals: u32,
) -> Result<QuadratureMoments, FitError> {
    if [alpha, beta, inverse_temperature, bound]
        .iter()
        .any(|value| !value.is_finite())
        || inverse_temperature <= 0.0
        || bound <= 0.0
        || intervals == 0
        || !intervals.is_multiple_of(2)
    {
        return Err(FitError::InvalidConfig);
    }
    let step = 2.0 * bound / f64::from(intervals);
    let mut log_weights = Vec::with_capacity(intervals as usize + 1);
    for index in 0..=intervals {
        let state = -bound + f64::from(index) * step;
        let simpson_weight: f64 = if index == 0 || index == intervals {
            1.0
        } else if index % 2 == 0 {
            2.0
        } else {
            4.0
        };
        let log_weight =
            -inverse_temperature * potential(state, alpha, beta)? + simpson_weight.ln();
        if !log_weight.is_finite() {
            return Err(FitError::NonFiniteObjective);
        }
        log_weights.push((state, log_weight));
    }
    let maximum = log_weights
        .iter()
        .map(|(_, weight)| *weight)
        .fold(f64::NEG_INFINITY, f64::max);
    let boundary_ratio = (log_weights[0].1 - maximum)
        .exp()
        .max((log_weights[log_weights.len() - 1].1 - maximum).exp());
    if !boundary_ratio.is_finite() || boundary_ratio > MAX_BOUNDARY_MASS_RATIO {
        return Err(FitError::IntegrationBoundary);
    }

    let mut denominator = 0.0;
    let mut first = 0.0;
    let mut second = 0.0;
    for (state, log_weight) in log_weights {
        let weight = (log_weight - maximum).exp();
        denominator += weight;
        first = weight.mul_add(state, first);
        second = weight.mul_add(state * state, second);
    }
    if !denominator.is_finite() || denominator <= 0.0 {
        return Err(FitError::NonFiniteObjective);
    }
    let log_normalizer = maximum + denominator.ln() + (step / 3.0).ln();
    let mean = first / denominator;
    let second_moment = second / denominator;
    if [log_normalizer, mean, second_moment]
        .iter()
        .all(|value| value.is_finite())
    {
        Ok(QuadratureMoments {
            log_normalizer,
            mean,
            second_moment,
        })
    } else {
        Err(FitError::NonFiniteObjective)
    }
}

fn potential(state: f64, alpha: f64, beta: f64) -> Result<f64, FitError> {
    let value = 0.25 * state.powi(4) - 0.5 * beta * state.powi(2) - alpha * state;
    if value.is_finite() {
        Ok(value)
    } else {
        Err(FitError::NonFiniteObjective)
    }
}

#[cfg(test)]
mod tests {
    use super::quadrature_moments;

    #[test]
    fn symmetric_stationary_density_has_zero_mean() {
        let moments = quadrature_moments(0.0, 0.5, 2.0, 6.0, 128).expect("quadrature");
        assert!(moments.mean.abs() < 1.0e-15);
        assert!(moments.second_moment > 0.0);
        assert!(moments.log_normalizer.is_finite());
    }
}
