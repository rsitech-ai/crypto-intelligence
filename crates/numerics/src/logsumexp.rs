use crate::NumericalError;

const MAXIMUM_LOGITS: usize = 1_000_000;

/// Stable `ln(sum(exp(values)))` using a maximum shift.
pub fn logsumexp(values: &[f64]) -> Result<f64, NumericalError> {
    if values.is_empty() {
        return Err(NumericalError::EmptyInput);
    }
    if values.len() > MAXIMUM_LOGITS {
        return Err(NumericalError::Dimension);
    }
    if values.iter().any(|value| value.is_nan()) {
        return Err(NumericalError::NonFiniteInput);
    }
    if values.contains(&f64::INFINITY) {
        return Ok(f64::INFINITY);
    }
    let maximum = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if maximum == f64::NEG_INFINITY {
        return Ok(f64::NEG_INFINITY);
    }
    let exponential_sum = values
        .iter()
        .map(|value| (*value - maximum).exp())
        .sum::<f64>();
    let result = maximum + exponential_sum.ln();
    if result.is_finite() {
        Ok(result)
    } else {
        Err(NumericalError::NonFiniteEvaluation)
    }
}
