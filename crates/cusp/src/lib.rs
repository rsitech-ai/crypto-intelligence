//! Approved mathematical convention and validated state for the cusp model.

pub mod barrier;
pub mod control_schema;
pub mod controls;
pub mod equilibria;
pub mod fit;
pub mod fold_distance;
pub mod potential;
pub mod roots;
pub mod types;

pub use barrier::Barrier;
pub use control_schema::{
    CoefficientSign, ControlFeature, ControlFeatureKey, ControlSchema, ControlSchemaInput,
    ControlTarget, FeatureGroup, MAX_CONTROL_FEATURES, MAX_CONTROL_GROUPS,
};
pub use controls::{
    ControlCovariance, ControlDatum, ControlError, ControlEvaluation, ControlMap, ControlMapInput,
    ControlMissingReason, ControlValue, ControlVector, FeatureCoefficient, FeatureCovariance,
    FeatureSensitivity, LinearControl, MissingControlFeature,
};
pub use equilibria::{
    ClassifiedRoot, EquilibriumSet, EquilibriumTopology, Stability, analyze_equilibria,
};
pub use fold_distance::{
    ControlWhitening, CovarianceConditioning, FoldDistance, fold_point, nearest_fold,
};
pub use potential::Potential;
pub use roots::{EquilibriumRoot, real_equilibria};
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
    #[error("cusp root formula left its mathematically valid domain")]
    RootDomain,
    #[error("cusp root calculation produced an invalid value")]
    RootCalculation,
    #[error("equilibrium roots do not form a valid cusp topology")]
    InvalidEquilibriumTopology,
    #[error("control covariance is not finite and strictly positive definite")]
    InvalidWhitening,
    #[error("nearest-fold optimization failed its bounded numerical contract")]
    FoldOptimization,
    #[error("unsupported cusp state schema version {found}")]
    UnsupportedSchema { found: u32 },
}
