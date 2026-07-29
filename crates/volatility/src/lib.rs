//! Deterministic bounded volatility measures and walk-forward forecasts.
//!
//! EWMA initializes from the first squared return and applies decay only from
//! the second observation. HAR-RV normalization is fitted only on the supplied
//! training rows. Model selection accepts inner folds only, so untouched outer
//! test outcomes have no selection API.

mod ewma;
mod forecast;
mod har;
mod measures;

pub use ewma::{EwmaForecast, EwmaVolatility};
pub use forecast::{
    CandidateModel, CandidateScore, ForecastError, ModelFamily, SelectionConfig,
    SelectionObservation, SelectionReport, select_volatility_model, volatility_normalized_state,
};
pub use har::{HarRvModel, HarRvObservation, PredictorNormalization, VolatilityForecast};
pub use measures::{
    MAX_MEASURE_OBSERVATIONS, MeasureError, OhlcBar, SeasonalBaseline, bipower_variation,
    downside_semivariance, forecast_residual, garman_klass_variance, jump_variation,
    parkinson_variance, realized_correlation, realized_covariance, realized_variance,
    realized_volatility, sample_correlation, sample_covariance, seasonality_adjusted_volatility,
    upside_semivariance, volatility_of_volatility, volatility_term_ratio,
};
