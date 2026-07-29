//! Normal-Inverse-Gamma sufficient statistics and Student-t prediction.

use statrs::function::gamma::ln_gamma;

use crate::BocpdError;

/// Conjugate posterior for a Gaussian with unknown mean and variance.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NormalInverseGamma {
    mu: f64,
    kappa: f64,
    alpha: f64,
    beta: f64,
}

impl NormalInverseGamma {
    pub fn try_new(mu: f64, kappa: f64, alpha: f64, beta: f64) -> Result<Self, BocpdError> {
        if !mu.is_finite()
            || !kappa.is_finite()
            || kappa <= 0.0
            || !alpha.is_finite()
            || alpha <= 0.0
            || !beta.is_finite()
            || beta <= 0.0
        {
            return Err(BocpdError::InvalidPrior);
        }
        Ok(Self {
            mu: canonical_zero(mu),
            kappa,
            alpha,
            beta,
        })
    }

    pub const fn mu(self) -> f64 {
        self.mu
    }

    pub const fn kappa(self) -> f64 {
        self.kappa
    }

    pub const fn alpha(self) -> f64 {
        self.alpha
    }

    pub const fn beta(self) -> f64 {
        self.beta
    }

    /// Log density of the conjugate Student-t posterior predictive.
    pub fn log_predictive_density(self, value: f64) -> Result<f64, BocpdError> {
        if !value.is_finite() {
            return Err(BocpdError::NonFiniteObservation);
        }
        let degrees_of_freedom = finite(2.0 * self.alpha)?;
        let scale_squared =
            finite(self.beta * finite(self.kappa + 1.0)? / finite(self.alpha * self.kappa)?)?;
        if degrees_of_freedom <= 0.0 || scale_squared <= 0.0 {
            return Err(BocpdError::NumericalFailure);
        }
        let deviation = finite(value - self.mu)?;
        let standardized_square = finite(deviation * deviation / scale_squared)?;
        let log_density = ln_gamma(finite((degrees_of_freedom + 1.0) / 2.0)?)
            - ln_gamma(degrees_of_freedom / 2.0)
            - 0.5 * finite((degrees_of_freedom * std::f64::consts::PI).ln())?
            - 0.5 * finite(scale_squared.ln())?
            - finite((degrees_of_freedom + 1.0) / 2.0)?
                * finite((1.0 + standardized_square / degrees_of_freedom).ln())?;
        finite(log_density)
    }

    /// Returns the posterior after one observation without mutating this state.
    pub fn updated(self, value: f64) -> Result<Self, BocpdError> {
        if !value.is_finite() {
            return Err(BocpdError::NonFiniteObservation);
        }
        let next_kappa = finite(self.kappa + 1.0)?;
        let weighted_mean = finite(self.kappa * self.mu)?;
        let next_mu = finite(finite(weighted_mean + value)? / next_kappa)?;
        let next_alpha = finite(self.alpha + 0.5)?;
        let deviation = finite(value - self.mu)?;
        let deviation_square = finite(deviation * deviation)?;
        let beta_increment = finite(self.kappa * deviation_square / finite(2.0 * next_kappa)?)?;
        let next_beta = finite(self.beta + beta_increment)?;
        Self::try_new(next_mu, next_kappa, next_alpha, next_beta)
            .map_err(|_| BocpdError::NumericalFailure)
    }
}

fn finite(value: f64) -> Result<f64, BocpdError> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(BocpdError::NumericalFailure)
    }
}

const fn canonical_zero(value: f64) -> f64 {
    if value == 0.0 { 0.0 } else { value }
}
