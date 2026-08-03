//! Canonical empirical summaries for bounded simulated paths.

use serde::{Deserialize, Serialize};

use crate::{CrossingDirection, EventThreshold, ScenarioError};

const SUMMARY_QUANTILES: [f64; 5] = [0.01, 0.05, 0.50, 0.95, 0.99];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimulationLabel {
    Simulated,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuantileValue {
    pub probability: f64,
    pub value: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThresholdProbability {
    pub threshold_id: String,
    pub direction: CrossingDirection,
    pub threshold_return: f64,
    pub crossing_count: u32,
    pub probability: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RangeSummary {
    pub minimum: f64,
    pub lower: f64,
    pub median: f64,
    pub upper: f64,
    pub maximum: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SweepCostSummary {
    pub notional_usd: f64,
    pub expected_bps: f64,
    pub tail_bps: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HorizonSummary {
    pub horizon_seconds: u64,
    pub return_quantiles: Vec<QuantileValue>,
    pub volatility_quantiles: Vec<QuantileValue>,
    pub threshold_probabilities: Vec<ThresholdProbability>,
    pub sweep_costs: Vec<SweepCostSummary>,
    pub liquidation_notional_range: RangeSummary,
    pub open_interest_range: RangeSummary,
}

impl HorizonSummary {
    pub fn return_quantile(&self, probability: f64) -> Option<f64> {
        self.return_quantiles
            .iter()
            .find(|value| value.probability.to_bits() == probability.to_bits())
            .map(|value| value.value)
    }

    pub fn sweep_cost(&self, notional_usd: f64) -> Option<&SweepCostSummary> {
        self.sweep_costs
            .iter()
            .find(|value| value.notional_usd.to_bits() == notional_usd.to_bits())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioPathPoint {
    pub elapsed_seconds: u64,
    pub simulated_price: f64,
    pub price_return: f64,
    pub volatility_annualized: f64,
    pub cumulative_liquidation_usd: f64,
    pub open_interest_usd: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepresentativePath {
    pub label: SimulationLabel,
    pub selection_quantile: f64,
    pub source_path_index: u32,
    pub points: Vec<ScenarioPathPoint>,
}

#[derive(Clone, Debug)]
pub(crate) struct RawHorizonValue {
    pub price_return: f64,
    pub volatility: f64,
    pub threshold_crossed: Vec<bool>,
    pub sweep_costs: Vec<f64>,
    pub cumulative_liquidation: f64,
    pub open_interest: f64,
}

#[derive(Clone, Debug)]
pub(crate) struct RawPath {
    pub points: Vec<ScenarioPathPoint>,
    pub horizons: Vec<RawHorizonValue>,
}

pub(crate) fn summarize_horizons(
    paths: &[RawPath],
    horizons: &[u64],
    thresholds: &[EventThreshold],
    notionals: &[f64],
) -> Result<Vec<HorizonSummary>, ScenarioError> {
    if paths.is_empty() || horizons.is_empty() {
        return Err(ScenarioError::InvalidSummary);
    }
    let mut summaries = Vec::with_capacity(horizons.len());
    for (horizon_index, horizon_seconds) in horizons.iter().copied().enumerate() {
        let values: Vec<&RawHorizonValue> = paths
            .iter()
            .map(|path| {
                path.horizons
                    .get(horizon_index)
                    .ok_or(ScenarioError::InvalidSummary)
            })
            .collect::<Result<_, _>>()?;
        let returns: Vec<f64> = values.iter().map(|value| value.price_return).collect();
        let volatilities: Vec<f64> = values.iter().map(|value| value.volatility).collect();
        let liquidations: Vec<f64> = values
            .iter()
            .map(|value| value.cumulative_liquidation)
            .collect();
        let open_interest: Vec<f64> = values.iter().map(|value| value.open_interest).collect();
        let threshold_probabilities = thresholds
            .iter()
            .enumerate()
            .map(|(index, threshold)| {
                let count = values
                    .iter()
                    .filter(|value| value.threshold_crossed.get(index) == Some(&true))
                    .count();
                let crossing_count =
                    u32::try_from(count).map_err(|_| ScenarioError::InvalidSummary)?;
                Ok(ThresholdProbability {
                    threshold_id: threshold.id().to_owned(),
                    direction: threshold.direction(),
                    threshold_return: threshold.return_fraction(),
                    crossing_count,
                    probability: count as f64 / paths.len() as f64,
                })
            })
            .collect::<Result<_, ScenarioError>>()?;
        let sweep_costs = notionals
            .iter()
            .copied()
            .enumerate()
            .map(|(index, notional_usd)| {
                let costs: Vec<f64> = values
                    .iter()
                    .map(|value| {
                        value
                            .sweep_costs
                            .get(index)
                            .copied()
                            .ok_or(ScenarioError::InvalidSummary)
                    })
                    .collect::<Result<_, _>>()?;
                Ok(SweepCostSummary {
                    notional_usd,
                    expected_bps: mean(&costs)?,
                    tail_bps: empirical_quantile(&costs, 0.95)?,
                })
            })
            .collect::<Result<_, ScenarioError>>()?;
        summaries.push(HorizonSummary {
            horizon_seconds,
            return_quantiles: quantiles(&returns)?,
            volatility_quantiles: quantiles(&volatilities)?,
            threshold_probabilities,
            sweep_costs,
            liquidation_notional_range: range(&liquidations)?,
            open_interest_range: range(&open_interest)?,
        });
    }
    Ok(summaries)
}

pub(crate) fn representative_paths(
    paths: &[RawPath],
    probabilities: &[f64],
) -> Result<Vec<RepresentativePath>, ScenarioError> {
    let terminal_returns: Vec<f64> = paths
        .iter()
        .map(|path| {
            path.horizons
                .last()
                .map(|value| value.price_return)
                .ok_or(ScenarioError::InvalidSummary)
        })
        .collect::<Result<_, _>>()?;
    let mut selected = Vec::with_capacity(probabilities.len());
    for probability in probabilities {
        let target = empirical_quantile(&terminal_returns, *probability)?;
        let (index, _) = terminal_returns
            .iter()
            .enumerate()
            .min_by(|(left_index, left), (right_index, right)| {
                (**left - target)
                    .abs()
                    .total_cmp(&(**right - target).abs())
                    .then_with(|| left_index.cmp(right_index))
            })
            .ok_or(ScenarioError::InvalidSummary)?;
        selected.push(RepresentativePath {
            label: SimulationLabel::Simulated,
            selection_quantile: *probability,
            source_path_index: u32::try_from(index).map_err(|_| ScenarioError::InvalidSummary)?,
            points: paths[index].points.clone(),
        });
    }
    Ok(selected)
}

fn quantiles(values: &[f64]) -> Result<Vec<QuantileValue>, ScenarioError> {
    SUMMARY_QUANTILES
        .into_iter()
        .map(|probability| {
            Ok(QuantileValue {
                probability,
                value: empirical_quantile(values, probability)?,
            })
        })
        .collect()
}

fn range(values: &[f64]) -> Result<RangeSummary, ScenarioError> {
    let ordered = finite_sorted(values)?;
    let minimum = *ordered.first().ok_or(ScenarioError::InvalidSummary)?;
    let maximum = *ordered.last().ok_or(ScenarioError::InvalidSummary)?;
    Ok(RangeSummary {
        minimum,
        lower: empirical_quantile_sorted(&ordered, 0.05)?,
        median: empirical_quantile_sorted(&ordered, 0.50)?,
        upper: empirical_quantile_sorted(&ordered, 0.95)?,
        maximum,
    })
}

pub(crate) fn empirical_quantile(values: &[f64], probability: f64) -> Result<f64, ScenarioError> {
    let ordered = finite_sorted(values)?;
    empirical_quantile_sorted(&ordered, probability)
}

fn empirical_quantile_sorted(ordered: &[f64], probability: f64) -> Result<f64, ScenarioError> {
    if ordered.is_empty() || !probability.is_finite() || !(0.0..=1.0).contains(&probability) {
        return Err(ScenarioError::InvalidSummary);
    }
    if ordered.len() == 1 {
        return Ok(ordered[0]);
    }
    let position = ordered.len() as f64 * probability - 0.5;
    if position <= 0.0 {
        return Ok(ordered[0]);
    }
    let last = ordered.len() - 1;
    if position >= last as f64 {
        return Ok(ordered[last]);
    }
    let lower = position.floor() as usize;
    let fraction = position - lower as f64;
    let value = ordered[lower] * (1.0 - fraction) + ordered[lower + 1] * fraction;
    value
        .is_finite()
        .then_some(value)
        .ok_or(ScenarioError::NonFiniteArithmetic)
}

fn finite_sorted(values: &[f64]) -> Result<Vec<f64>, ScenarioError> {
    if values.is_empty() || !values.iter().all(|value| value.is_finite()) {
        return Err(ScenarioError::InvalidSummary);
    }
    let mut ordered = values.to_vec();
    ordered.sort_by(f64::total_cmp);
    Ok(ordered)
}

fn mean(values: &[f64]) -> Result<f64, ScenarioError> {
    if values.is_empty() || !values.iter().all(|value| value.is_finite()) {
        return Err(ScenarioError::InvalidSummary);
    }
    let mut sum = 0.0;
    let mut correction = 0.0;
    for value in values {
        let adjusted = value - correction;
        let next = sum + adjusted;
        correction = (next - sum) - adjusted;
        sum = next;
    }
    let result = sum / values.len() as f64;
    result
        .is_finite()
        .then_some(result)
        .ok_or(ScenarioError::NonFiniteArithmetic)
}

#[cfg(test)]
mod tests {
    use super::empirical_quantile;
    use crate::ScenarioError;

    #[test]
    fn nist_empirical_linear_reference_values_are_exact() {
        let values = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];
        assert_eq!(empirical_quantile(&values, 0.0).unwrap(), 1.0);
        assert_eq!(empirical_quantile(&values, 0.25).unwrap(), 3.0);
        assert_eq!(empirical_quantile(&values, 0.50).unwrap(), 5.5);
        assert_eq!(empirical_quantile(&values, 0.75).unwrap(), 8.0);
        assert_eq!(empirical_quantile(&values, 1.0).unwrap(), 10.0);
    }

    #[test]
    fn quantiles_reject_empty_nonfinite_and_invalid_probability_inputs() {
        assert_eq!(
            empirical_quantile(&[], 0.5),
            Err(ScenarioError::InvalidSummary)
        );
        assert_eq!(
            empirical_quantile(&[1.0, f64::NAN], 0.5),
            Err(ScenarioError::InvalidSummary)
        );
        assert_eq!(
            empirical_quantile(&[1.0], 1.1),
            Err(ScenarioError::InvalidSummary)
        );
    }
}
