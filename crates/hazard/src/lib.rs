//! Deterministic discrete-time competing-risk hazard models.

pub mod buckets;
pub mod design;
pub mod elastic_net;
pub mod incidence;
pub mod softmax;

pub use buckets::{BucketBoundaryConvention, BucketSpec, HorizonInterpolationPolicy};
pub use design::{
    BucketTarget, DesignRow, FeatureSchema, HazardOutcome, HazardSampleInput, HazardTrainingSet,
    HazardTrainingSetInput, InputQuality, augment_design,
};
pub use elastic_net::{
    CandidateHazardArtifact, CompetingRiskHazard, FitDiagnostics, HazardConfig, HazardConfigInput,
    HazardHorizonForecast, HazardPrediction, HazardPredictionInput, NormalizationStats,
    fit_regularization_path,
};
pub use incidence::{
    BucketProbability, CauseHorizonProbability, CumulativeIncidence, HorizonIncidence,
    cumulative_incidence, cumulative_incidence_for_spec,
};
pub use softmax::softmax_with_survival;

use thiserror::Error;

/// Fail-closed errors at the hazard model boundary.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum HazardError {
    #[error("bucket probability is invalid")]
    InvalidBucketProbability,
    #[error("bucket sequence is empty")]
    EmptyBuckets,
    #[error("bucket cause dimensions do not match")]
    CauseDimensionMismatch,
    #[error("cumulative-incidence curve does not match its bucket specification")]
    CurveDimensionMismatch,
    #[error("cumulative-incidence curve is not bound to a bucket specification")]
    UnboundBucketSpec,
    #[error("forecast horizon is not an exact supported bucket edge")]
    UnsupportedHorizon,
    #[error("bucket logits are invalid")]
    InvalidLogits,
    #[error("probability arithmetic is not finite")]
    NonFiniteProbability,
    #[error("cumulative probability mass drifted outside tolerance")]
    ProbabilityMassDrift,
    #[error("feature schema is invalid")]
    InvalidFeatureSchema,
    #[error("cause schema is invalid")]
    InvalidCauseSchema,
    #[error("training set is invalid")]
    InvalidTrainingSet,
    #[error("training sample is invalid")]
    InvalidSample,
    #[error("training sample identity is duplicated")]
    DuplicateSample,
    #[error("hazard outcome is invalid")]
    InvalidOutcome,
    #[error("outcome knowledge precedes its observed boundary")]
    OutcomeKnownTooEarly,
    #[error("time arithmetic overflowed")]
    TimeOverflow,
    #[error("expanded design exceeds its declared capacity")]
    DesignCapacity,
    #[error("hazard configuration is invalid")]
    InvalidConfig,
    #[error("fit or path exceeds its declared work capacity")]
    WorkCapacity,
    #[error("one or more hazard buckets lack the declared at-risk support")]
    InsufficientBucketSupport,
    #[error("optimization arithmetic is not finite")]
    NonFiniteOptimization,
    #[error("optimizer could not find a valid descent step")]
    OptimizationFailure,
    #[error("optimizer objective regressed")]
    ObjectiveRegression,
    #[error("prediction input is invalid")]
    InvalidPrediction,
    #[error("regularization path is invalid")]
    InvalidRegularizationPath,
    #[error("a nonconverged model cannot become a candidate")]
    NonConvergedCandidate,
}
