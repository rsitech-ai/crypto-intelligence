//! Deterministic bounded Bayesian online changepoint detection.
//!
//! The detector implements the causal Adams-MacKay run-length recurrence
//! using a Normal-Inverse-Gamma conjugate observation model. Posterior
//! arithmetic remains in log space, and hard tail truncation is disclosed in
//! every output rather than silently folded into a false conjugate state.

mod bocpd;
mod nig;

pub use bocpd::{Bocpd, BocpdConfig, BocpdObservation, BocpdOutput, QualityState, ResetPolicy};
pub use nig::NormalInverseGamma;
use thiserror::Error;

/// Fail-closed configuration, observation, and numerical errors.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum BocpdError {
    #[error("Normal-Inverse-Gamma prior parameters are invalid")]
    InvalidPrior,
    #[error("BOCPD configuration is invalid")]
    InvalidConfiguration,
    #[error("observation must be finite")]
    NonFiniteObservation,
    #[error("point-in-time BOCPD observation is invalid")]
    InvalidObservation,
    #[error("BOCPD observations must be strictly ordered")]
    NonMonotonicObservation,
    #[error("unavailable quality cannot update model state")]
    UnavailableQuality,
    #[error("BOCPD state exceeds its bounded capacity")]
    CapacityExceeded,
    #[error("BOCPD calculation cannot be represented finitely")]
    NumericalFailure,
    #[error("BOCPD probability underflow would silently erase a hypothesis")]
    ProbabilityUnderflow,
}
