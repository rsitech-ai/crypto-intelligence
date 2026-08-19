//! Approved mathematical convention and validated state for the cusp model.

pub mod potential;
pub mod types;

pub use potential::Potential;
pub use types::{AlternativeControls, Controls, CuspState};

use thiserror::Error;

/// Compatibility name for later inactive cusp consumers.
pub type ControlPoint = Controls;

/// Compatibility wrapper for the approved product potential.
pub fn potential(state: f64, controls: Controls) -> f64 {
    Potential::value(state, controls)
}

/// Compatibility wrapper for the approved product potential gradient.
pub fn gradient(state: f64, controls: Controls) -> f64 {
    Potential::gradient(state, controls)
}

/// Compatibility wrapper for the approved product potential Hessian.
pub fn curvature(state: f64, controls: Controls) -> f64 {
    Potential::hessian(state, controls)
}

#[derive(Clone, Copy, Debug, Error, PartialEq)]
pub enum CuspError {
    #[error("cusp input is not finite")]
    NonFiniteInput,
    #[error("cusp calculation produced a nonfinite value")]
    NonFiniteDerivedValue,
    #[error("serialized cusp state is inconsistent with the approved convention")]
    InconsistentState,
    #[error("unsupported cusp state schema version {found}")]
    UnsupportedSchema { found: u32 },
}
