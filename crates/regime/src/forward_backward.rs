//! Log-space forward/backward inference with per-step scaling.

use crate::HmmError;

#[derive(Debug)]
pub(crate) struct InferenceWorkspace {
    pub(crate) filtered: Vec<Vec<f64>>,
    pub(crate) smoothed: Vec<Vec<f64>>,
    pub(crate) expected_transitions: Vec<Vec<f64>>,
    pub(crate) log_likelihood: f64,
}

pub(crate) fn infer(
    initial: &[f64],
    transition: &[Vec<f64>],
    emission_log_likelihoods: &[Vec<f64>],
) -> Result<InferenceWorkspace, HmmError> {
    let state_count = initial.len();
    if state_count == 0
        || transition.len() != state_count
        || transition.iter().any(|row| row.len() != state_count)
        || emission_log_likelihoods.is_empty()
        || emission_log_likelihoods
            .iter()
            .any(|row| row.len() != state_count)
    {
        return Err(HmmError::InvalidParameters);
    }
    validate_probability_vector(initial)?;
    for row in transition {
        validate_probability_vector(row)?;
    }
    if emission_log_likelihoods
        .iter()
        .flatten()
        .any(|value| !value.is_finite())
    {
        return Err(HmmError::NumericalFailure);
    }

    let time_count = emission_log_likelihoods.len();
    let mut log_alpha = vec![vec![0.0; state_count]; time_count];
    let mut log_scales = vec![0.0; time_count];
    for (state, probability) in initial.iter().enumerate() {
        log_alpha[0][state] = probability.ln() + emission_log_likelihoods[0][state];
    }
    log_scales[0] = logsumexp(&log_alpha[0])?;
    for value in &mut log_alpha[0] {
        *value -= log_scales[0];
    }

    let mut branches = vec![0.0; state_count];
    for time in 1..time_count {
        for next_state in 0..state_count {
            for (previous_state, row) in transition.iter().enumerate() {
                branches[previous_state] =
                    log_alpha[time - 1][previous_state] + row[next_state].ln();
            }
            log_alpha[time][next_state] =
                logsumexp(&branches)? + emission_log_likelihoods[time][next_state];
        }
        log_scales[time] = logsumexp(&log_alpha[time])?;
        for value in &mut log_alpha[time] {
            *value -= log_scales[time];
        }
    }
    let log_likelihood = log_scales.iter().sum::<f64>();
    if !log_likelihood.is_finite() {
        return Err(HmmError::NumericalFailure);
    }

    let mut log_beta = vec![vec![0.0; state_count]; time_count];
    for time in (0..time_count - 1).rev() {
        for (state, row) in transition.iter().enumerate() {
            for (next_state, probability) in row.iter().enumerate() {
                branches[next_state] = probability.ln()
                    + emission_log_likelihoods[time + 1][next_state]
                    + log_beta[time + 1][next_state];
            }
            log_beta[time][state] = logsumexp(&branches)? - log_scales[time + 1];
        }
    }

    let filtered = log_alpha
        .iter()
        .map(|row| normalized_exp(row))
        .collect::<Result<Vec<_>, _>>()?;
    let mut smoothed = Vec::with_capacity(time_count);
    for time in 0..time_count {
        let joint = log_alpha[time]
            .iter()
            .zip(&log_beta[time])
            .map(|(alpha, beta)| alpha + beta)
            .collect::<Vec<_>>();
        smoothed.push(normalized_exp(&joint)?);
    }

    let mut expected_transitions = vec![vec![0.0; state_count]; state_count];
    let mut joint = vec![0.0; state_count * state_count];
    for time in 0..time_count - 1 {
        let mut index = 0;
        for (state, row) in transition.iter().enumerate() {
            for (next_state, probability) in row.iter().enumerate() {
                joint[index] = log_alpha[time][state]
                    + probability.ln()
                    + emission_log_likelihoods[time + 1][next_state]
                    + log_beta[time + 1][next_state];
                index += 1;
            }
        }
        let normalization = logsumexp(&joint)?;
        index = 0;
        for row in &mut expected_transitions {
            for value in row {
                let probability = (joint[index] - normalization).exp();
                if !probability.is_finite() {
                    return Err(HmmError::NumericalFailure);
                }
                *value += probability;
                index += 1;
            }
        }
    }

    Ok(InferenceWorkspace {
        filtered,
        smoothed,
        expected_transitions,
        log_likelihood,
    })
}

pub(crate) fn stationary(transition: &[Vec<f64>]) -> Result<Vec<f64>, HmmError> {
    let state_count = transition.len();
    if state_count == 0 || transition.iter().any(|row| row.len() != state_count) {
        return Err(HmmError::InvalidParameters);
    }
    for row in transition {
        validate_probability_vector(row)?;
    }
    let mut current = vec![1.0 / state_count as f64; state_count];
    for _ in 0..10_000 {
        let mut next = vec![0.0; state_count];
        for (state, row) in transition.iter().enumerate() {
            for (next_state, probability) in row.iter().enumerate() {
                next[next_state] += current[state] * probability;
            }
        }
        normalize_probability_vector(&mut next)?;
        let change = current
            .iter()
            .zip(&next)
            .map(|(left, right)| (left - right).abs())
            .fold(0.0_f64, f64::max);
        current = next;
        if change <= 1e-14 {
            return Ok(current);
        }
    }
    Err(HmmError::NumericalFailure)
}

pub(crate) fn normalize_probability_vector(values: &mut [f64]) -> Result<(), HmmError> {
    if values.is_empty()
        || values
            .iter()
            .any(|value| !value.is_finite() || *value < 0.0)
    {
        return Err(HmmError::NumericalFailure);
    }
    let sum = values.iter().sum::<f64>();
    if !sum.is_finite() || sum <= 0.0 {
        return Err(HmmError::NumericalFailure);
    }
    for value in values {
        *value /= sum;
    }
    Ok(())
}

fn validate_probability_vector(values: &[f64]) -> Result<(), HmmError> {
    if values.is_empty()
        || values
            .iter()
            .any(|value| !value.is_finite() || *value <= 0.0 || *value > 1.0)
        || (values.iter().sum::<f64>() - 1.0).abs() > 1e-10
    {
        return Err(HmmError::InvalidParameters);
    }
    Ok(())
}

fn normalized_exp(log_values: &[f64]) -> Result<Vec<f64>, HmmError> {
    let normalization = logsumexp(log_values)?;
    let mut probabilities = log_values
        .iter()
        .map(|value| (value - normalization).exp())
        .collect::<Vec<_>>();
    normalize_probability_vector(&mut probabilities)?;
    Ok(probabilities)
}

fn logsumexp(values: &[f64]) -> Result<f64, HmmError> {
    if values.is_empty() || values.iter().any(|value| !value.is_finite()) {
        return Err(HmmError::NumericalFailure);
    }
    let maximum = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let scaled_sum = values
        .iter()
        .map(|value| (value - maximum).exp())
        .sum::<f64>();
    if !maximum.is_finite() || !scaled_sum.is_finite() || scaled_sum <= 0.0 {
        return Err(HmmError::NumericalFailure);
    }
    let result = maximum + scaled_sum.ln();
    if result.is_finite() {
        Ok(result)
    } else {
        Err(HmmError::NumericalFailure)
    }
}

#[cfg(test)]
mod tests {
    use super::infer;

    #[test]
    fn tiny_reference_likelihood_and_posteriors_match_literal_enumeration() {
        // Two states, two observations. Direct path enumeration gives:
        // 0->0: .6*.5*.7*.4 = .084
        // 0->1: .6*.5*.3*.9 = .081
        // 1->0: .4*.2*.2*.4 = .0064
        // 1->1: .4*.2*.8*.9 = .0576
        // total = .229
        let initial = [0.6, 0.4];
        let transition = vec![vec![0.7, 0.3], vec![0.2, 0.8]];
        let emissions = vec![
            vec![0.5_f64.ln(), 0.2_f64.ln()],
            vec![0.4_f64.ln(), 0.9_f64.ln()],
        ];
        let output = infer(&initial, &transition, &emissions).expect("reference inference");

        assert!((output.log_likelihood - 0.229_f64.ln()).abs() < 1e-12);
        assert!((output.filtered[0][0] - 0.789_473_684_210_526_3).abs() < 1e-12);
        assert!((output.filtered[1][0] - 0.394_759_825_327_510_9).abs() < 1e-12);
        assert!((output.smoothed[0][0] - 0.720_524_017_467_248_9).abs() < 1e-12);
        assert!((output.expected_transitions[0][0] - 0.366_812_227_074_235_8).abs() < 1e-12);
    }
}
