//! Exponentially weighted volatility with a frozen first-observation rule.

use crate::ForecastError;

const NORMAL_INTERVAL_MULTIPLIER: f64 = 1.96;

/// Recursive EWMA state.
///
/// The first finite return sets variance to its square. Decay starts with the
/// second return, preventing an implicit zero prior or double weighting.
#[derive(Clone, Debug, PartialEq)]
pub struct EwmaVolatility {
    lambda: f64,
    variance: Option<f64>,
    squared_residual_sum: f64,
    residual_count: u64,
    observation_count: u64,
}

impl EwmaVolatility {
    pub fn try_new(lambda: f64) -> Result<Self, ForecastError> {
        if !lambda.is_finite() || lambda <= 0.0 || lambda >= 1.0 {
            return Err(ForecastError::InvalidLambda);
        }
        Ok(Self {
            lambda,
            variance: None,
            squared_residual_sum: 0.0,
            residual_count: 0,
            observation_count: 0,
        })
    }

    pub const fn lambda(&self) -> f64 {
        self.lambda
    }

    pub const fn observation_count(&self) -> u64 {
        self.observation_count
    }

    pub const fn variance(&self) -> Option<f64> {
        self.variance
    }

    pub fn update(&mut self, return_value: f64) -> Result<(), ForecastError> {
        if !return_value.is_finite() {
            return Err(ForecastError::NonFinite);
        }
        let squared = return_value * return_value;
        if !squared.is_finite() {
            return Err(ForecastError::NumericalFailure);
        }
        let next_count = self
            .observation_count
            .checked_add(1)
            .ok_or(ForecastError::CapacityExceeded)?;
        let (next_variance, next_residual_sum, next_residual_count) =
            if let Some(previous) = self.variance {
                let residual = squared - previous;
                let residual_square = residual * residual;
                let next_residual_sum = self.squared_residual_sum + residual_square;
                let next_variance = self.lambda * previous + (1.0 - self.lambda) * squared;
                if !residual_square.is_finite()
                    || !next_residual_sum.is_finite()
                    || !next_variance.is_finite()
                    || next_variance < 0.0
                {
                    return Err(ForecastError::NumericalFailure);
                }
                (
                    next_variance,
                    next_residual_sum,
                    self.residual_count
                        .checked_add(1)
                        .ok_or(ForecastError::CapacityExceeded)?,
                )
            } else {
                (squared, 0.0, 0)
            };

        self.variance = Some(canonical_zero(next_variance));
        self.squared_residual_sum = canonical_zero(next_residual_sum);
        self.residual_count = next_residual_count;
        self.observation_count = next_count;
        Ok(())
    }

    pub fn forecast(&self) -> Result<EwmaForecast, ForecastError> {
        let variance = self.variance.ok_or(ForecastError::InsufficientHistory)?;
        if self.residual_count == 0 {
            return Err(ForecastError::InsufficientHistory);
        }
        let variance_error = (self.squared_residual_sum / self.residual_count as f64).sqrt();
        let lower_variance = (variance - NORMAL_INTERVAL_MULTIPLIER * variance_error).max(0.0);
        let upper_variance = variance + NORMAL_INTERVAL_MULTIPLIER * variance_error;
        let volatility = variance.sqrt();
        let lower = lower_variance.sqrt();
        let upper = upper_variance.sqrt();
        if [variance, volatility, lower, upper]
            .iter()
            .any(|value| !value.is_finite())
        {
            return Err(ForecastError::NumericalFailure);
        }
        Ok(EwmaForecast {
            variance: canonical_zero(variance),
            volatility: canonical_zero(volatility),
            lower: canonical_zero(lower),
            upper: canonical_zero(upper),
            observation_count: self.observation_count,
        })
    }
}

/// One-step EWMA forecast with empirical squared-return residual uncertainty.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EwmaForecast {
    pub variance: f64,
    pub volatility: f64,
    pub lower: f64,
    pub upper: f64,
    pub observation_count: u64,
}

const fn canonical_zero(value: f64) -> f64 {
    if value == 0.0 { 0.0 } else { value }
}
