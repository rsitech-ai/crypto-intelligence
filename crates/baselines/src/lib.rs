//! Auditable historical, seasonal, regime-conditioned, and rolling event baselines.

pub mod base_rate;

pub use base_rate::{
    BaseRateConfig, BaseRateEstimate, BaseRateModel, BaseRateQuery, BaselineError,
    BetaBinomialEstimate, EstimateKind, Observation, ObservationInput, beta_binomial_estimate,
};
