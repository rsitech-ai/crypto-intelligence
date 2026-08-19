//! Bounded analytical volatility and jump measures.

use thiserror::Error;

/// Maximum observations accepted by one pure analytical call.
pub const MAX_MEASURE_OBSERVATIONS: usize = 65_536;

/// Typed analytical failure; production callers convert this to missingness.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum MeasureError {
    #[error("analytical input is empty, nonfinite, negative where prohibited, or overflows")]
    InvalidInput,
    #[error("analytical input does not contain the minimum required history")]
    InsufficientHistory,
    #[error("analytical input exceeds the bounded observation capacity")]
    CapacityExceeded,
    #[error("OHLC input violates positive ordered bar invariants")]
    InvalidOhlc,
    #[error("analytical input lengths do not match")]
    LengthMismatch,
    #[error("analytical parameter is outside its supported domain")]
    InvalidParameter,
    #[error("sample variance is zero")]
    ZeroVariance,
    #[error("validated analytical calculation cannot be represented finitely")]
    AnalyticalUnavailable,
}

/// Positive internally consistent OHLC observation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OhlcBar {
    open: f64,
    high: f64,
    low: f64,
    close: f64,
}

impl OhlcBar {
    pub fn try_new(open: f64, high: f64, low: f64, close: f64) -> Result<Self, MeasureError> {
        if ![open, high, low, close]
            .iter()
            .all(|value| value.is_finite())
            || open <= 0.0
            || high <= 0.0
            || low <= 0.0
            || close <= 0.0
            || high < open.max(close)
            || low > open.min(close)
            || low > high
        {
            return Err(MeasureError::InvalidOhlc);
        }
        Ok(Self {
            open,
            high,
            low,
            close,
        })
    }

    pub const fn open(self) -> f64 {
        self.open
    }

    pub const fn high(self) -> f64 {
        self.high
    }

    pub const fn low(self) -> f64 {
        self.low
    }

    pub const fn close(self) -> f64 {
        self.close
    }
}

/// Sum of squared returns.
pub fn realized_variance(returns: &[f64]) -> Result<f64, MeasureError> {
    checked_sum_squares(returns)
}

/// Sum of negative squared returns.
pub fn downside_semivariance(returns: &[f64]) -> Result<f64, MeasureError> {
    checked_semivariance(returns, |value| value < 0.0)
}

/// Sum of positive squared returns.
pub fn upside_semivariance(returns: &[f64]) -> Result<f64, MeasureError> {
    checked_semivariance(returns, |value| value > 0.0)
}

/// Square root of realized variance for an unannualized return window.
pub fn realized_volatility(returns: &[f64]) -> Result<f64, MeasureError> {
    validate_history(returns, 2)?;
    finite_result(checked_sum_squares(returns)?.sqrt())
}

/// Unannualized realized bipower variation using the frozen
/// `(pi / 2) * sum(|r[i-1]| * |r[i]|)` estimator.
pub fn bipower_variation(returns: &[f64]) -> Result<f64, MeasureError> {
    validate_history(returns, 2)?;
    let mut total = 0.0;
    for pair in returns.windows(2) {
        total += pair[0].abs() * pair[1].abs();
        if !total.is_finite() {
            return Err(MeasureError::AnalyticalUnavailable);
        }
    }
    finite_result((std::f64::consts::PI / 2.0) * total)
}

/// Nonnegative jump proxy `max(realized variance - bipower variation, 0)`.
pub fn jump_variation(returns: &[f64]) -> Result<f64, MeasureError> {
    let realized = checked_sum_squares(returns)?;
    let bipower = bipower_variation(returns)?;
    finite_result((realized - bipower).max(0.0))
}

/// Mean Parkinson range variance over positive valid bars.
pub fn parkinson_variance(bars: &[OhlcBar]) -> Result<f64, MeasureError> {
    validate_bars(bars)?;
    let total = bars.iter().try_fold(0.0, |total, bar| {
        finite_result(total + (bar.high / bar.low).ln().powi(2))
    })?;
    finite_result(total / (4.0 * bars.len() as f64 * std::f64::consts::LN_2))
}

/// Mean Garman-Klass range variance over positive valid bars.
pub fn garman_klass_variance(bars: &[OhlcBar]) -> Result<f64, MeasureError> {
    validate_bars(bars)?;
    let coefficient = 2.0 * std::f64::consts::LN_2 - 1.0;
    let total = bars.iter().try_fold(0.0, |total, bar| {
        let high_low = (bar.high / bar.low).ln();
        let close_open = (bar.close / bar.open).ln();
        finite_result(total + 0.5 * high_low.powi(2) - coefficient * close_open.powi(2))
    })?;
    let mean = finite_result(total / bars.len() as f64)?;
    if mean < -f64::EPSILON {
        Err(MeasureError::InvalidInput)
    } else {
        Ok(mean.max(0.0))
    }
}

/// Versioned intraday seasonal factor fitted and made available before use.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SeasonalBaseline {
    factor: f64,
    fitted_through_nanos: i64,
    as_known_at_nanos: i64,
    version_hash: [u8; 32],
}

impl SeasonalBaseline {
    pub fn try_new(
        factor: f64,
        fitted_through_nanos: i64,
        as_known_at_nanos: i64,
        version_hash: [u8; 32],
    ) -> Result<Self, MeasureError> {
        if !factor.is_finite()
            || factor <= 0.0
            || fitted_through_nanos <= 0
            || as_known_at_nanos < fitted_through_nanos
            || version_hash == [0; 32]
        {
            return Err(MeasureError::InvalidParameter);
        }
        Ok(Self {
            factor,
            fitted_through_nanos,
            as_known_at_nanos,
            version_hash,
        })
    }

    pub const fn factor(self) -> f64 {
        self.factor
    }

    pub const fn fitted_through_nanos(self) -> i64 {
        self.fitted_through_nanos
    }

    pub const fn as_known_at_nanos(self) -> i64 {
        self.as_known_at_nanos
    }

    pub const fn version_hash(self) -> [u8; 32] {
        self.version_hash
    }
}

/// Divide realized volatility by a versioned factor that was knowable before
/// the evaluated window began.
pub fn seasonality_adjusted_volatility(
    volatility: f64,
    baseline: SeasonalBaseline,
    window_start_nanos: i64,
) -> Result<f64, MeasureError> {
    if !volatility.is_finite()
        || volatility < 0.0
        || window_start_nanos <= 0
        || baseline.fitted_through_nanos() > window_start_nanos
        || baseline.as_known_at_nanos() > window_start_nanos
    {
        return Err(MeasureError::InvalidParameter);
    }
    finite_result(volatility / baseline.factor())
}

/// Sample covariance with denominator `n - 1`.
pub fn sample_covariance(left: &[f64], right: &[f64]) -> Result<f64, MeasureError> {
    if left.len() != right.len() {
        return Err(MeasureError::LengthMismatch);
    }
    validate_history(left, 2)?;
    validate_history(right, 2)?;
    let left_mean = finite_mean(left)?;
    let right_mean = finite_mean(right)?;
    let total = left
        .iter()
        .zip(right)
        .try_fold(0.0, |total, (left, right)| {
            finite_result(total + (left - left_mean) * (right - right_mean))
        })?;
    finite_result(total / (left.len() - 1) as f64)
}

/// Sample Pearson correlation.
pub fn sample_correlation(left: &[f64], right: &[f64]) -> Result<f64, MeasureError> {
    let covariance = sample_covariance(left, right)?;
    let left_variance = sample_covariance(left, left)?;
    let right_variance = sample_covariance(right, right)?;
    if left_variance <= 0.0 || right_variance <= 0.0 {
        return Err(MeasureError::ZeroVariance);
    }
    finite_result(covariance / (left_variance * right_variance).sqrt())
}

/// Realized covariation: the uncentered sum of aligned return products.
pub fn realized_covariance(left: &[f64], right: &[f64]) -> Result<f64, MeasureError> {
    if left.len() != right.len() {
        return Err(MeasureError::LengthMismatch);
    }
    validate_history(left, 2)?;
    validate_history(right, 2)?;
    left.iter()
        .zip(right)
        .try_fold(0.0, |total, (left, right)| {
            finite_result(total + left * right)
        })
}

/// Realized correlation: realized covariation normalized by realized variance.
pub fn realized_correlation(left: &[f64], right: &[f64]) -> Result<f64, MeasureError> {
    let covariance = realized_covariance(left, right)?;
    let left_variance = checked_sum_squares(left)?;
    let right_variance = checked_sum_squares(right)?;
    if left_variance <= 0.0 || right_variance <= 0.0 {
        return Err(MeasureError::ZeroVariance);
    }
    finite_result(covariance / (left_variance * right_variance).sqrt())
}

/// Sample standard deviation of a realized-volatility series.
pub fn volatility_of_volatility(values: &[f64]) -> Result<f64, MeasureError> {
    let variance = sample_covariance(values, values)?;
    finite_result(variance.sqrt())
}

/// Positive long-horizon denominator ratio.
pub fn volatility_term_ratio(short: f64, long: f64) -> Result<f64, MeasureError> {
    if !short.is_finite() || short < 0.0 || !long.is_finite() || long <= 0.0 {
        return Err(MeasureError::InvalidParameter);
    }
    finite_result(short / long)
}

/// Signed realized-minus-forecast volatility residual.
pub fn forecast_residual(realized: f64, forecast: f64) -> Result<f64, MeasureError> {
    if !realized.is_finite() || realized < 0.0 || !forecast.is_finite() || forecast < 0.0 {
        return Err(MeasureError::InvalidParameter);
    }
    finite_result(realized - forecast)
}

fn checked_sum_squares(values: &[f64]) -> Result<f64, MeasureError> {
    validate_history(values, 1)?;
    values
        .iter()
        .try_fold(0.0, |total, value| finite_result(total + value * value))
}

fn checked_semivariance(
    values: &[f64],
    include: impl Fn(f64) -> bool,
) -> Result<f64, MeasureError> {
    validate_history(values, 1)?;
    values.iter().try_fold(0.0, |total, value| {
        if include(*value) {
            finite_result(total + value * value)
        } else {
            Ok(total)
        }
    })
}

fn validate_history(values: &[f64], minimum: usize) -> Result<(), MeasureError> {
    if values.len() > MAX_MEASURE_OBSERVATIONS {
        return Err(MeasureError::CapacityExceeded);
    }
    if values.len() < minimum {
        return Err(MeasureError::InsufficientHistory);
    }
    if values.iter().any(|value| !value.is_finite()) {
        return Err(MeasureError::InvalidInput);
    }
    Ok(())
}

fn validate_bars(bars: &[OhlcBar]) -> Result<(), MeasureError> {
    if bars.len() > MAX_MEASURE_OBSERVATIONS {
        Err(MeasureError::CapacityExceeded)
    } else if bars.is_empty() {
        Err(MeasureError::InsufficientHistory)
    } else {
        Ok(())
    }
}

fn finite_mean(values: &[f64]) -> Result<f64, MeasureError> {
    let total = values
        .iter()
        .try_fold(0.0, |total, value| finite_result(total + value))?;
    finite_result(total / values.len() as f64)
}

fn finite_result(value: f64) -> Result<f64, MeasureError> {
    if value.is_finite() {
        Ok(if value == 0.0 { 0.0 } else { value })
    } else {
        Err(MeasureError::AnalyticalUnavailable)
    }
}
