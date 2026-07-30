use serde::{Deserialize, Serialize};

use crate::{CuspError, Potential};

const CUSP_STATE_SCHEMA_VERSION: u32 = 1;

/// Product cusp controls in the approved `(alpha, beta)` sign convention.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawControls", into = "RawControls")]
pub struct Controls {
    pub alpha: f64,
    pub beta: f64,
}

impl Controls {
    pub fn try_new(alpha: f64, beta: f64) -> Result<Self, CuspError> {
        let controls = Self { alpha, beta };
        controls.validate()?;
        Ok(controls)
    }

    pub fn validate(self) -> Result<(), CuspError> {
        if self.alpha.is_finite() && self.beta.is_finite() {
            Ok(())
        } else {
            Err(CuspError::NonFiniteInput)
        }
    }

    /// Evaluate `4 * beta^3 - 27 * alpha^2` without boundary validation.
    pub fn discriminant(self) -> f64 {
        4.0 * self.beta.powi(3) - 27.0 * self.alpha.powi(2)
    }

    pub fn checked_discriminant(self) -> Result<f64, CuspError> {
        self.validate()?;
        let discriminant = self.discriminant();
        if discriminant.is_finite() {
            Ok(discriminant)
        } else {
            Err(CuspError::NonFiniteDerivedValue)
        }
    }

    /// The strict three-equilibrium region excludes the fold boundary.
    pub fn inside_cusp(self) -> bool {
        self.beta > 0.0
            && self
                .checked_discriminant()
                .is_ok_and(|discriminant| discriminant > 0.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawControls {
    alpha: f64,
    beta: f64,
}

impl TryFrom<RawControls> for Controls {
    type Error = CuspError;

    fn try_from(raw: RawControls) -> Result<Self, Self::Error> {
        Self::try_new(raw.alpha, raw.beta)
    }
}

impl From<Controls> for RawControls {
    fn from(controls: Controls) -> Self {
        Self {
            alpha: controls.alpha,
            beta: controls.beta,
        }
    }
}

/// Research convention `z^4/4 + a*z^2/2 + b*z`, accepted only via mapping.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawAlternativeControls", into = "RawAlternativeControls")]
pub struct AlternativeControls {
    pub a: f64,
    pub b: f64,
}

impl AlternativeControls {
    pub fn try_new(a: f64, b: f64) -> Result<Self, CuspError> {
        if a.is_finite() && b.is_finite() {
            Ok(Self { a, b })
        } else {
            Err(CuspError::NonFiniteInput)
        }
    }
}

impl From<Controls> for AlternativeControls {
    fn from(controls: Controls) -> Self {
        Self {
            a: -controls.beta,
            b: -controls.alpha,
        }
    }
}

impl From<AlternativeControls> for Controls {
    fn from(controls: AlternativeControls) -> Self {
        Self {
            alpha: -controls.b,
            beta: -controls.a,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAlternativeControls {
    a: f64,
    b: f64,
}

impl TryFrom<RawAlternativeControls> for AlternativeControls {
    type Error = CuspError;

    fn try_from(raw: RawAlternativeControls) -> Result<Self, Self::Error> {
        Self::try_new(raw.a, raw.b)
    }
}

impl From<AlternativeControls> for RawAlternativeControls {
    fn from(controls: AlternativeControls) -> Self {
        Self {
            a: controls.a,
            b: controls.b,
        }
    }
}

/// Self-consistent mathematical state derived from a normalized observation.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawCuspState", into = "RawCuspState")]
pub struct CuspState {
    pub schema_version: u32,
    pub normalized_state: f64,
    pub controls: Controls,
    pub potential: f64,
    pub gradient: f64,
    pub hessian: f64,
    pub discriminant: f64,
    pub inside_cusp: bool,
}

impl CuspState {
    pub fn evaluate(normalized_state: f64, controls: Controls) -> Result<Self, CuspError> {
        Ok(Self {
            schema_version: CUSP_STATE_SCHEMA_VERSION,
            normalized_state,
            controls,
            potential: Potential::checked_value(normalized_state, controls)?,
            gradient: Potential::checked_gradient(normalized_state, controls)?,
            hessian: Potential::checked_hessian(normalized_state, controls)?,
            discriminant: controls.checked_discriminant()?,
            inside_cusp: controls.inside_cusp(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCuspState {
    schema_version: u32,
    normalized_state: f64,
    controls: Controls,
    potential: f64,
    gradient: f64,
    hessian: f64,
    discriminant: f64,
    inside_cusp: bool,
}

impl TryFrom<RawCuspState> for CuspState {
    type Error = CuspError;

    fn try_from(raw: RawCuspState) -> Result<Self, Self::Error> {
        if raw.schema_version != CUSP_STATE_SCHEMA_VERSION {
            return Err(CuspError::UnsupportedSchema {
                found: raw.schema_version,
            });
        }
        let derived = Self::evaluate(raw.normalized_state, raw.controls)?;
        if raw.potential != derived.potential
            || raw.gradient != derived.gradient
            || raw.hessian != derived.hessian
            || raw.discriminant != derived.discriminant
            || raw.inside_cusp != derived.inside_cusp
        {
            return Err(CuspError::InconsistentState);
        }
        Ok(derived)
    }
}

impl From<CuspState> for RawCuspState {
    fn from(state: CuspState) -> Self {
        Self {
            schema_version: state.schema_version,
            normalized_state: state.normalized_state,
            controls: state.controls,
            potential: state.potential,
            gradient: state.gradient,
            hessian: state.hessian,
            discriminant: state.discriminant,
            inside_cusp: state.inside_cusp,
        }
    }
}
