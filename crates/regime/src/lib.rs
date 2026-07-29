//! Deterministic point-in-time Student-t hidden Markov regime modelling.
//!
//! Inputs are selected and standardized upstream. This crate deliberately does
//! not fill missing values, normalize full history, choose features, or attach
//! economic labels during fitting. HMM states retain neutral [`StateId`] values;
//! optional human-readable descriptions are derived from fitted statistics and
//! stored separately.

mod describe;
mod em;
mod forward_backward;

use std::fmt;

use thiserror::Error;

pub use describe::{StateDescription, StateStatistic};
pub use em::{
    CandidateIneligibility, CandidateScore, FoldScore, SelectedStudentTHmm, SelectionConfig,
    SelectionReport, WalkForwardFold,
};

const MAXIMUM_DIMENSIONS: usize = 16;
const MAXIMUM_OBSERVATIONS: usize = 100_000;
const MAXIMUM_IDENTIFIER_BYTES: usize = 96;
const MODEL_HASH_DOMAIN: &[u8] = b"cmti:student-t-hmm:model:v1\0";
const SEQUENCE_HASH_DOMAIN: &[u8] = b"cmti:student-t-hmm:sequence:v1\0";
const CANDIDATE_HASH_DOMAIN: &[u8] = b"cmti:student-t-hmm:candidate:v1\0";

/// Neutral fitted-state identity. Economic meaning is never encoded here.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StateId(pub usize);

impl fmt::Display for StateId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "state_{}", self.0)
    }
}

/// Availability/quality attached to one point-in-time emission observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmissionQuality {
    Trusted,
    Degraded,
    Unavailable,
}

impl EmissionQuality {
    const fn hash_byte(self) -> u8 {
        match self {
            Self::Trusted => 1,
            Self::Degraded => 2,
            Self::Unavailable => 3,
        }
    }
}

/// Immutable selected standardized emissions at one point-in-time boundary.
#[derive(Clone, Debug, PartialEq)]
pub struct EmissionObservation {
    id: u64,
    event_time_ns: i64,
    as_known_at_ns: i64,
    values: Vec<f64>,
    quality: EmissionQuality,
    lineage_hash: [u8; 32],
}

impl EmissionObservation {
    pub fn try_new(
        id: u64,
        event_time_ns: i64,
        as_known_at_ns: i64,
        values: Vec<f64>,
        quality: EmissionQuality,
        lineage_hash: [u8; 32],
    ) -> Result<Self, HmmError> {
        if id == 0
            || event_time_ns <= 0
            || as_known_at_ns < event_time_ns
            || values.is_empty()
            || values.len() > MAXIMUM_DIMENSIONS
            || lineage_hash == [0; 32]
        {
            return Err(HmmError::InvalidObservation);
        }
        if quality == EmissionQuality::Unavailable {
            return Err(HmmError::UnavailableQuality);
        }
        if values.iter().any(|value| !value.is_finite()) {
            return Err(HmmError::NonFiniteValue);
        }
        Ok(Self {
            id,
            event_time_ns,
            as_known_at_ns,
            values,
            quality,
            lineage_hash,
        })
    }

    pub const fn id(&self) -> u64 {
        self.id
    }

    pub const fn event_time_ns(&self) -> i64 {
        self.event_time_ns
    }

    pub const fn as_known_at_ns(&self) -> i64 {
        self.as_known_at_ns
    }

    pub fn values(&self) -> &[f64] {
        &self.values
    }

    pub const fn quality(&self) -> EmissionQuality {
        self.quality
    }

    pub const fn lineage_hash(&self) -> [u8; 32] {
        self.lineage_hash
    }
}

/// Strictly ordered point-in-time sequence with a frozen emission schema.
#[derive(Clone, Debug, PartialEq)]
pub struct EmissionSequence {
    feature_families: Vec<String>,
    observations: Vec<EmissionObservation>,
    evidence_id: [u8; 32],
}

impl EmissionSequence {
    pub fn try_new(
        feature_families: Vec<String>,
        observations: Vec<EmissionObservation>,
    ) -> Result<Self, HmmError> {
        validate_schema(&feature_families)?;
        if observations.len() < 2 || observations.len() > MAXIMUM_OBSERVATIONS {
            return Err(HmmError::ObservationCapacity);
        }
        let dimension = feature_families.len();
        for observation in &observations {
            if observation.values.len() != dimension {
                return Err(HmmError::DimensionMismatch);
            }
        }
        for pair in observations.windows(2) {
            let previous = &pair[0];
            let current = &pair[1];
            if current.id <= previous.id
                || current.event_time_ns <= previous.event_time_ns
                || current.as_known_at_ns < previous.as_known_at_ns
            {
                return Err(HmmError::NonMonotonicObservation);
            }
        }
        let evidence_id = hash_sequence(&feature_families, &observations);
        Ok(Self {
            feature_families,
            observations,
            evidence_id,
        })
    }

    pub fn feature_families(&self) -> &[String] {
        &self.feature_families
    }

    pub fn observations(&self) -> &[EmissionObservation] {
        &self.observations
    }

    pub const fn evidence_id(&self) -> [u8; 32] {
        self.evidence_id
    }

    pub const fn len(&self) -> usize {
        self.observations.len()
    }

    pub const fn is_empty(&self) -> bool {
        self.observations.is_empty()
    }

    pub fn first_event_time_ns(&self) -> i64 {
        self.observations[0].event_time_ns
    }

    pub fn last_event_time_ns(&self) -> i64 {
        self.observations[self.observations.len() - 1].event_time_ns
    }

    pub fn last_as_known_at_ns(&self) -> i64 {
        self.observations[self.observations.len() - 1].as_known_at_ns
    }
}

/// Bounded fixed-degree fitting configuration.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HmmConfig {
    state_count: usize,
    degrees_of_freedom: f64,
    diagonal_scale_floor: f64,
    probability_pseudocount: f64,
    max_iterations: usize,
    convergence_tolerance: f64,
    seed: u64,
}

impl HmmConfig {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        state_count: usize,
        degrees_of_freedom: f64,
        diagonal_scale_floor: f64,
        probability_pseudocount: f64,
        max_iterations: usize,
        convergence_tolerance: f64,
        seed: u64,
    ) -> Result<Self, HmmError> {
        if !(3..=6).contains(&state_count)
            || !degrees_of_freedom.is_finite()
            || degrees_of_freedom <= 2.0
            || degrees_of_freedom > 1_000.0
            || !diagonal_scale_floor.is_finite()
            || diagonal_scale_floor <= 0.0
            || diagonal_scale_floor > 1.0
            || !probability_pseudocount.is_finite()
            || probability_pseudocount <= 0.0
            || probability_pseudocount > 1_000_000.0
            || !(1..=10_000).contains(&max_iterations)
            || !convergence_tolerance.is_finite()
            || convergence_tolerance <= 0.0
            || convergence_tolerance >= 1.0
        {
            return Err(HmmError::InvalidConfiguration);
        }
        Ok(Self {
            state_count,
            degrees_of_freedom,
            diagonal_scale_floor,
            probability_pseudocount,
            max_iterations,
            convergence_tolerance,
            seed,
        })
    }

    pub const fn state_count(self) -> usize {
        self.state_count
    }

    pub const fn degrees_of_freedom(self) -> f64 {
        self.degrees_of_freedom
    }

    pub const fn diagonal_scale_floor(self) -> f64 {
        self.diagonal_scale_floor
    }

    /// Dirichlet pseudocount applied to initial and transition probabilities.
    pub const fn probability_pseudocount(self) -> f64 {
        self.probability_pseudocount
    }

    pub const fn max_iterations(self) -> usize {
        self.max_iterations
    }

    pub const fn convergence_tolerance(self) -> f64 {
        self.convergence_tolerance
    }

    pub const fn seed(self) -> u64 {
        self.seed
    }
}

/// One fitted multivariate Student-t emission with diagonal scale.
#[derive(Clone, Debug, PartialEq)]
pub struct StudentTEmission {
    location: Vec<f64>,
    diagonal_scale: Vec<f64>,
    degrees_of_freedom: f64,
}

impl StudentTEmission {
    pub fn location(&self) -> &[f64] {
        &self.location
    }

    pub fn diagonal_scale(&self) -> &[f64] {
        &self.diagonal_scale
    }

    pub const fn degrees_of_freedom(&self) -> f64 {
        self.degrees_of_freedom
    }

    fn try_new(
        location: Vec<f64>,
        diagonal_scale: Vec<f64>,
        degrees_of_freedom: f64,
    ) -> Result<Self, HmmError> {
        if location.is_empty()
            || location.len() != diagonal_scale.len()
            || location.len() > MAXIMUM_DIMENSIONS
            || location.iter().any(|value| !value.is_finite())
            || diagonal_scale
                .iter()
                .any(|value| !value.is_finite() || *value <= 0.0)
            || !degrees_of_freedom.is_finite()
            || degrees_of_freedom <= 2.0
        {
            return Err(HmmError::InvalidParameters);
        }
        Ok(Self {
            location,
            diagonal_scale,
            degrees_of_freedom,
        })
    }
}

/// Exact termination evidence for one bounded EM fit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FitDiagnostics {
    pub converged: bool,
    pub iterations: usize,
    pub log_likelihood: f64,
    pub max_parameter_change: f64,
    pub seed: u64,
}

/// Fitted Student-t HMM. A fit can be nonconverged for diagnostic inspection.
#[derive(Clone, Debug, PartialEq)]
pub struct StudentTHmm {
    config: HmmConfig,
    feature_families: Vec<String>,
    initial: Vec<f64>,
    transition: Vec<Vec<f64>>,
    emissions: Vec<StudentTEmission>,
    stationary: Vec<f64>,
    diagnostics: FitDiagnostics,
    training_evidence_id: [u8; 32],
    model_id: [u8; 32],
}

impl StudentTHmm {
    pub fn fit(sequence: &EmissionSequence, config: HmmConfig) -> Result<Self, HmmError> {
        em::fit(sequence, config)
    }

    pub fn select(
        inner_folds: &[WalkForwardFold],
        final_training: &EmissionSequence,
        config: SelectionConfig,
    ) -> Result<SelectedStudentTHmm, HmmError> {
        em::select(inner_folds, final_training, config)
    }

    pub fn infer(&self, sequence: &EmissionSequence) -> Result<InferenceOutput, HmmError> {
        self.ensure_schema(sequence)?;
        let workspace =
            em::infer_parameters(&self.initial, &self.transition, &self.emissions, sequence)?;
        Ok(InferenceOutput {
            filtered: workspace.filtered,
            smoothed: workspace.smoothed,
            log_likelihood: workspace.log_likelihood,
            model_id: self.model_id,
            evidence_id: hash_inference(self.model_id, sequence.evidence_id),
        })
    }

    pub fn filtered_probabilities(
        &self,
        sequence: &EmissionSequence,
    ) -> Result<Vec<Vec<f64>>, HmmError> {
        Ok(self.infer(sequence)?.filtered)
    }

    pub fn smoothed_probabilities(
        &self,
        sequence: &EmissionSequence,
    ) -> Result<Vec<Vec<f64>>, HmmError> {
        Ok(self.infer(sequence)?.smoothed)
    }

    pub fn describe_states(&self) -> Result<Vec<StateDescription>, HmmError> {
        if !self.diagnostics.converged {
            return Err(HmmError::NonConvergedFit);
        }
        describe::describe(self)
    }

    pub const fn config(&self) -> HmmConfig {
        self.config
    }

    pub fn feature_families(&self) -> &[String] {
        &self.feature_families
    }

    pub fn initial_probabilities(&self) -> &[f64] {
        &self.initial
    }

    pub fn transition_matrix(&self) -> &[Vec<f64>] {
        &self.transition
    }

    pub fn stationary_probabilities(&self) -> &[f64] {
        &self.stationary
    }

    pub fn emissions(&self) -> &[StudentTEmission] {
        &self.emissions
    }

    pub const fn diagnostics(&self) -> FitDiagnostics {
        self.diagnostics
    }

    pub const fn training_evidence_id(&self) -> [u8; 32] {
        self.training_evidence_id
    }

    pub const fn model_id(&self) -> [u8; 32] {
        self.model_id
    }

    fn ensure_schema(&self, sequence: &EmissionSequence) -> Result<(), HmmError> {
        if self.feature_families != sequence.feature_families {
            return Err(HmmError::SchemaMismatch);
        }
        Ok(())
    }
}

/// Filtered and smoothed posterior output for one immutable sequence.
#[derive(Clone, Debug, PartialEq)]
pub struct InferenceOutput {
    pub filtered: Vec<Vec<f64>>,
    pub smoothed: Vec<Vec<f64>>,
    pub log_likelihood: f64,
    pub model_id: [u8; 32],
    pub evidence_id: [u8; 32],
}

/// Promotion boundary for a converged fitted model.
#[derive(Clone, Debug, PartialEq)]
pub struct CandidateArtifact {
    model: StudentTHmm,
    artifact_id: [u8; 32],
    selection_id: Option<[u8; 32]>,
}

impl CandidateArtifact {
    pub const fn artifact_id(&self) -> [u8; 32] {
        self.artifact_id
    }

    pub const fn model(&self) -> &StudentTHmm {
        &self.model
    }

    pub const fn selection_id(&self) -> Option<[u8; 32]> {
        self.selection_id
    }

    fn try_new(model: StudentTHmm, selection_id: Option<[u8; 32]>) -> Result<Self, HmmError> {
        if !model.diagnostics.converged {
            return Err(HmmError::NonConvergedFit);
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update(CANDIDATE_HASH_DOMAIN);
        hasher.update(&model.model_id);
        hasher.update(&(model.diagnostics.iterations as u64).to_le_bytes());
        hasher.update(&model.diagnostics.log_likelihood.to_bits().to_le_bytes());
        match selection_id {
            Some(selection_id) => {
                hasher.update(&[1]);
                hasher.update(&selection_id);
            }
            None => {
                hasher.update(&[0]);
            }
        }
        let artifact_id = *hasher.finalize().as_bytes();
        Ok(Self {
            model,
            artifact_id,
            selection_id,
        })
    }
}

impl TryFrom<StudentTHmm> for CandidateArtifact {
    type Error = HmmError;

    fn try_from(model: StudentTHmm) -> Result<Self, Self::Error> {
        Self::try_new(model, None)
    }
}

/// Fail-closed boundary, fitting, inference, and promotion errors.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum HmmError {
    #[error("HMM configuration is invalid")]
    InvalidConfiguration,
    #[error("point-in-time emission observation is invalid")]
    InvalidObservation,
    #[error("emission value must be finite")]
    NonFiniteValue,
    #[error("unavailable quality cannot enter an HMM sequence")]
    UnavailableQuality,
    #[error("emission observation count exceeds its bounded contract")]
    ObservationCapacity,
    #[error("emission feature schema is invalid")]
    InvalidFeatureSchema,
    #[error("emission observations must be strictly ordered")]
    NonMonotonicObservation,
    #[error("emission dimensions do not match the sequence schema")]
    DimensionMismatch,
    #[error("emission sequence schema does not match the model")]
    SchemaMismatch,
    #[error("walk-forward training and validation ranges overlap")]
    OverlappingFold,
    #[error("walk-forward fold count exceeds its bounded contract")]
    FoldCapacity,
    #[error("insufficient observations for the requested state count")]
    InsufficientData,
    #[error("HMM fit exceeds its checked computational work budget")]
    WorkCapacity,
    #[error("HMM parameters are invalid")]
    InvalidParameters,
    #[error("HMM probability or likelihood calculation failed")]
    NumericalFailure,
    #[error("HMM state occupancy collapsed")]
    DegenerateFit,
    #[error("EM likelihood decreased beyond numerical tolerance")]
    LikelihoodDecreased,
    #[error("nonconverged fit cannot become a candidate artifact")]
    NonConvergedFit,
    #[error("no state-count candidate converged across all inner folds")]
    NoEligibleCandidate,
}

fn validate_schema(feature_families: &[String]) -> Result<(), HmmError> {
    if feature_families.is_empty() || feature_families.len() > MAXIMUM_DIMENSIONS {
        return Err(HmmError::InvalidFeatureSchema);
    }
    let mut previous: Option<&str> = None;
    for identifier in feature_families {
        if !valid_identifier(identifier) || previous == Some(identifier.as_str()) {
            return Err(HmmError::InvalidFeatureSchema);
        }
        previous = Some(identifier);
    }
    let mut unique = feature_families.iter().collect::<Vec<_>>();
    unique.sort_unstable();
    unique.dedup();
    if unique.len() != feature_families.len() {
        return Err(HmmError::InvalidFeatureSchema);
    }
    Ok(())
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAXIMUM_IDENTIFIER_BYTES
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':' | b'/')
        })
}

fn hash_sequence(feature_families: &[String], observations: &[EmissionObservation]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(SEQUENCE_HASH_DOMAIN);
    hasher.update(&(feature_families.len() as u64).to_le_bytes());
    for feature in feature_families {
        hash_bytes(&mut hasher, feature.as_bytes());
    }
    hasher.update(&(observations.len() as u64).to_le_bytes());
    for observation in observations {
        hasher.update(&observation.id.to_le_bytes());
        hasher.update(&observation.event_time_ns.to_le_bytes());
        hasher.update(&observation.as_known_at_ns.to_le_bytes());
        hasher.update(&[observation.quality.hash_byte()]);
        hasher.update(&observation.lineage_hash);
        hasher.update(&(observation.values.len() as u64).to_le_bytes());
        for value in &observation.values {
            hasher.update(&value.to_bits().to_le_bytes());
        }
    }
    *hasher.finalize().as_bytes()
}

fn hash_model(model: &StudentTHmm) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(MODEL_HASH_DOMAIN);
    hash_config(&mut hasher, model.config);
    hasher.update(&model.training_evidence_id);
    for feature in &model.feature_families {
        hash_bytes(&mut hasher, feature.as_bytes());
    }
    hash_probability_vector(&mut hasher, &model.initial);
    for row in &model.transition {
        hash_probability_vector(&mut hasher, row);
    }
    for emission in &model.emissions {
        hash_float_vector(&mut hasher, &emission.location);
        hash_float_vector(&mut hasher, &emission.diagonal_scale);
        hasher.update(&emission.degrees_of_freedom.to_bits().to_le_bytes());
    }
    hasher.update(&[u8::from(model.diagnostics.converged)]);
    hasher.update(&(model.diagnostics.iterations as u64).to_le_bytes());
    hasher.update(&model.diagnostics.log_likelihood.to_bits().to_le_bytes());
    hasher.update(
        &model
            .diagnostics
            .max_parameter_change
            .to_bits()
            .to_le_bytes(),
    );
    *hasher.finalize().as_bytes()
}

fn hash_config(hasher: &mut blake3::Hasher, config: HmmConfig) {
    hasher.update(&(config.state_count as u64).to_le_bytes());
    hasher.update(&config.degrees_of_freedom.to_bits().to_le_bytes());
    hasher.update(&config.diagonal_scale_floor.to_bits().to_le_bytes());
    hasher.update(&config.probability_pseudocount.to_bits().to_le_bytes());
    hasher.update(&(config.max_iterations as u64).to_le_bytes());
    hasher.update(&config.convergence_tolerance.to_bits().to_le_bytes());
    hasher.update(&config.seed.to_le_bytes());
}

fn hash_bytes(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn hash_float_vector(hasher: &mut blake3::Hasher, values: &[f64]) {
    hasher.update(&(values.len() as u64).to_le_bytes());
    for value in values {
        hasher.update(&value.to_bits().to_le_bytes());
    }
}

fn hash_probability_vector(hasher: &mut blake3::Hasher, values: &[f64]) {
    hash_float_vector(hasher, values);
}

fn hash_inference(model_id: [u8; 32], evidence_id: [u8; 32]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"cmti:student-t-hmm:inference:v1\0");
    hasher.update(&model_id);
    hasher.update(&evidence_id);
    *hasher.finalize().as_bytes()
}
