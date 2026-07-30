//! Deterministic discrete-time competing-risk hazard models.

pub mod buckets;
pub mod design;
pub mod elastic_net;
pub mod incidence;
pub mod softmax;

pub use buckets::BucketSpec;
pub use design::{
    BucketTarget, DesignRow, FeatureSchema, HazardOutcome, HazardSampleInput, HazardTrainingSet,
    HazardTrainingSetInput, InputQuality, augment_design,
};
pub use elastic_net::{
    CandidateHazardArtifact, CompetingRiskHazard, FitDiagnostics, HazardConfig, HazardConfigInput,
    HazardPrediction, HazardPredictionInput, NormalizationStats, fit_regularization_path,
};
pub use incidence::{BucketProbability, CumulativeIncidence, cumulative_incidence};
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
    #[error("bucket logits are invalid")]
    InvalidLogits,
    #[error("probability arithmetic is not finite")]
    NonFiniteProbability,
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
