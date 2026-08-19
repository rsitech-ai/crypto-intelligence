//! Stable multinomial softmax with survival as the reference category.

use crate::{BucketProbability, HazardError};

/// Converts cause logits to conditional cause probabilities plus reference survival.
///
/// Survival has an exact reference logit of zero. The maximum includes that reference
/// before exponentiation, so finite inputs cannot overflow.
pub fn softmax_with_survival(logits: &[f64]) -> Result<BucketProbability, HazardError> {
    if logits.is_empty() || logits.iter().any(|value| !value.is_finite()) {
        return Err(HazardError::InvalidLogits);
    }

    let maximum = logits
        .iter()
        .copied()
        .fold(0.0_f64, |current, value| current.max(value));
    let survival_weight = (-maximum).exp();
    let cause_weights = logits
        .iter()
        .map(|logit| (*logit - maximum).exp())
        .collect::<Vec<_>>();
    let denominator = cause_weights.iter().sum::<f64>() + survival_weight;
    if !denominator.is_finite() || denominator <= 0.0 {
        return Err(HazardError::NonFiniteProbability);
    }

    let causes = cause_weights
        .into_iter()
        .map(|weight| weight / denominator)
        .collect();
    BucketProbability::try_new(causes, survival_weight / denominator)
}
