//! Bounded robust Baum-Welch fitting and inner-fold state-count selection.

use std::cmp::Ordering;

use statrs::function::gamma::ln_gamma;

use crate::forward_backward::{self, InferenceWorkspace};
use crate::{
    CandidateArtifact, EmissionSequence, FitDiagnostics, HmmConfig, HmmError, StudentTEmission,
    StudentTHmm, hash_config, hash_model,
};

const MAXIMUM_INNER_FOLDS: usize = 32;
const MAXIMUM_FIT_WORK_UNITS: usize = 250_000_000;
const MAXIMUM_SELECTION_WORK_UNITS: usize = 500_000_000;
const OCCUPANCY_FLOOR: f64 = 1e-12;
const LIKELIHOOD_DECREASE_MULTIPLIER: f64 = 1e-6;
const SELECTION_HASH_DOMAIN: &[u8] = b"cmti:student-t-hmm:selection:v1\0";
type ModelParameters = (Vec<f64>, Vec<Vec<f64>>, Vec<StudentTEmission>);

/// One explicit time-ordered inner training/validation pair.
#[derive(Clone, Debug, PartialEq)]
pub struct WalkForwardFold {
    training: EmissionSequence,
    validation: EmissionSequence,
}

impl WalkForwardFold {
    pub fn try_new(
        training: EmissionSequence,
        validation: EmissionSequence,
    ) -> Result<Self, HmmError> {
        if training.feature_families() != validation.feature_families() {
            return Err(HmmError::SchemaMismatch);
        }
        if training.last_event_time_ns() >= validation.first_event_time_ns()
            || training.last_as_known_at_ns() >= validation.first_event_time_ns()
        {
            return Err(HmmError::OverlappingFold);
        }
        Ok(Self {
            training,
            validation,
        })
    }

    pub const fn training(&self) -> &EmissionSequence {
        &self.training
    }

    pub const fn validation(&self) -> &EmissionSequence {
        &self.validation
    }
}

/// Bounded, predeclared controls for three-to-six-state inner-fold selection.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SelectionConfig {
    minimum_states: usize,
    maximum_states: usize,
    degrees_of_freedom: f64,
    diagonal_scale_floor: f64,
    probability_pseudocount: f64,
    max_iterations: usize,
    convergence_tolerance: f64,
    seed: u64,
    stability_weight: f64,
}

impl SelectionConfig {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        minimum_states: usize,
        maximum_states: usize,
        degrees_of_freedom: f64,
        diagonal_scale_floor: f64,
        probability_pseudocount: f64,
        max_iterations: usize,
        convergence_tolerance: f64,
        seed: u64,
        stability_weight: f64,
    ) -> Result<Self, HmmError> {
        if !(3..=6).contains(&minimum_states)
            || !(3..=6).contains(&maximum_states)
            || minimum_states > maximum_states
            || !stability_weight.is_finite()
            || !(0.0..=1_000_000.0).contains(&stability_weight)
        {
            return Err(HmmError::InvalidConfiguration);
        }
        HmmConfig::try_new(
            minimum_states,
            degrees_of_freedom,
            diagonal_scale_floor,
            probability_pseudocount,
            max_iterations,
            convergence_tolerance,
            seed,
        )?;
        Ok(Self {
            minimum_states,
            maximum_states,
            degrees_of_freedom,
            diagonal_scale_floor,
            probability_pseudocount,
            max_iterations,
            convergence_tolerance,
            seed,
            stability_weight,
        })
    }

    pub const fn minimum_states(self) -> usize {
        self.minimum_states
    }

    pub const fn maximum_states(self) -> usize {
        self.maximum_states
    }

    pub const fn stability_weight(self) -> f64 {
        self.stability_weight
    }

    fn fit_config(self, state_count: usize, seed: u64) -> Result<HmmConfig, HmmError> {
        HmmConfig::try_new(
            state_count,
            self.degrees_of_freedom,
            self.diagonal_scale_floor,
            self.probability_pseudocount,
            self.max_iterations,
            self.convergence_tolerance,
            seed,
        )
    }
}

/// One validation-fold score for an eligible state-count candidate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FoldScore {
    pub training_evidence_id: [u8; 32],
    pub validation_evidence_id: [u8; 32],
    pub fitted_model_id: [u8; 32],
    pub diagnostics: FitDiagnostics,
    pub validation_negative_log_likelihood: f64,
    pub stationary_occupancy_distance: f64,
    pub observation_count: usize,
}

/// Complete auditable score for one candidate state count.
#[derive(Clone, Debug, PartialEq)]
pub struct CandidateScore {
    pub state_count: usize,
    pub eligible: bool,
    pub ineligibility: Option<CandidateIneligibility>,
    pub failed_fold_index: Option<usize>,
    pub mean_validation_negative_log_likelihood: Option<f64>,
    pub mean_stationary_occupancy_distance: Option<f64>,
    pub composite_score: Option<f64>,
    pub fold_scores: Vec<FoldScore>,
}

/// Auditable reason one state count was excluded without aborting other candidates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateIneligibility {
    InsufficientData,
    DegenerateFit,
    LikelihoodDecreased,
    NumericalFailure,
    NonConverged,
}

/// Inner-fold selection evidence. Lower composite score wins; ties prefer fewer states.
#[derive(Clone, Debug, PartialEq)]
pub struct SelectionReport {
    pub selected_state_count: usize,
    pub candidates: Vec<CandidateScore>,
}

/// Converged final-training model and its immutable inner-fold selection evidence.
#[derive(Clone, Debug, PartialEq)]
pub struct SelectedStudentTHmm {
    model: StudentTHmm,
    report: SelectionReport,
    selection_id: [u8; 32],
}

impl SelectedStudentTHmm {
    pub const fn model(&self) -> &StudentTHmm {
        &self.model
    }

    pub const fn report(&self) -> &SelectionReport {
        &self.report
    }

    pub const fn selection_id(&self) -> [u8; 32] {
        self.selection_id
    }

    pub fn into_candidate(self) -> Result<CandidateArtifact, HmmError> {
        CandidateArtifact::try_new(self.model, Some(self.selection_id))
    }
}

pub(crate) fn fit(sequence: &EmissionSequence, config: HmmConfig) -> Result<StudentTHmm, HmmError> {
    if sequence.len() < config.state_count().saturating_mul(4) {
        return Err(HmmError::InsufficientData);
    }
    checked_fit_work(sequence.len(), sequence.feature_families().len(), config)?;
    let (mut initial, mut transition, mut emissions) = initialize(sequence, config)?;
    let mut workspace = infer_parameters(&initial, &transition, &emissions, sequence)?;
    let mut diagnostics = FitDiagnostics {
        converged: false,
        iterations: 0,
        log_likelihood: workspace.log_likelihood,
        max_parameter_change: f64::INFINITY,
        seed: config.seed(),
    };

    for iteration in 1..=config.max_iterations() {
        let (next_initial, next_transition, next_emissions) =
            maximize(sequence, config, &emissions, &workspace)?;
        let parameter_change = maximum_parameter_change(
            &initial,
            &transition,
            &emissions,
            &next_initial,
            &next_transition,
            &next_emissions,
        );
        let next_workspace =
            infer_parameters(&next_initial, &next_transition, &next_emissions, sequence)?;
        let likelihood_change = next_workspace.log_likelihood - workspace.log_likelihood;
        let decrease_tolerance =
            LIKELIHOOD_DECREASE_MULTIPLIER * (1.0 + workspace.log_likelihood.abs());
        if likelihood_change < -decrease_tolerance {
            return Err(HmmError::LikelihoodDecreased);
        }
        let likelihood_converged = likelihood_change.abs()
            <= config.convergence_tolerance() * (1.0 + workspace.log_likelihood.abs());
        let parameters_converged = parameter_change <= config.convergence_tolerance().sqrt();

        initial = next_initial;
        transition = next_transition;
        emissions = next_emissions;
        workspace = next_workspace;
        diagnostics = FitDiagnostics {
            converged: iteration > 1 && likelihood_converged && parameters_converged,
            iterations: iteration,
            log_likelihood: workspace.log_likelihood,
            max_parameter_change: parameter_change,
            seed: config.seed(),
        };
        if diagnostics.converged {
            break;
        }
    }

    canonicalize_states(&mut initial, &mut transition, &mut emissions);
    let stationary = forward_backward::stationary(&transition)?;
    let mut model = StudentTHmm {
        config,
        feature_families: sequence.feature_families().to_vec(),
        initial,
        transition,
        emissions,
        stationary,
        diagnostics,
        training_evidence_id: sequence.evidence_id(),
        model_id: [0; 32],
    };
    model.model_id = hash_model(&model);
    Ok(model)
}

pub(crate) fn select(
    inner_folds: &[WalkForwardFold],
    final_training: &EmissionSequence,
    config: SelectionConfig,
) -> Result<SelectedStudentTHmm, HmmError> {
    if inner_folds.is_empty() || inner_folds.len() > MAXIMUM_INNER_FOLDS {
        return Err(HmmError::FoldCapacity);
    }
    for fold in inner_folds {
        if fold.training.feature_families() != final_training.feature_families()
            || fold.validation.feature_families() != final_training.feature_families()
        {
            return Err(HmmError::SchemaMismatch);
        }
        if fold.training.last_event_time_ns() > final_training.last_event_time_ns()
            || fold.validation.last_event_time_ns() > final_training.last_event_time_ns()
        {
            return Err(HmmError::OverlappingFold);
        }
    }
    checked_selection_work(inner_folds, final_training, config)?;

    let mut candidates = Vec::new();
    for state_count in config.minimum_states..=config.maximum_states {
        let mut fold_scores = Vec::with_capacity(inner_folds.len());
        let mut eligible = true;
        let mut ineligibility = None;
        let mut failed_fold_index = None;
        for (fold_index, fold) in inner_folds.iter().enumerate() {
            let seed = derived_seed(config.seed, state_count, fold_index);
            let fold_config = config.fit_config(state_count, seed)?;
            let model = match fit(&fold.training, fold_config) {
                Ok(model) => model,
                Err(error) => {
                    ineligibility = candidate_ineligibility(error);
                    if ineligibility.is_some() {
                        eligible = false;
                        failed_fold_index = Some(fold_index);
                        break;
                    }
                    return Err(error);
                }
            };
            if !model.diagnostics.converged {
                eligible = false;
                ineligibility = Some(CandidateIneligibility::NonConverged);
                failed_fold_index = Some(fold_index);
                break;
            }
            let inference = match model.infer(&fold.validation) {
                Ok(inference) => inference,
                Err(HmmError::NumericalFailure) => {
                    eligible = false;
                    ineligibility = Some(CandidateIneligibility::NumericalFailure);
                    failed_fold_index = Some(fold_index);
                    break;
                }
                Err(error) => return Err(error),
            };
            let observation_count = fold.validation.len();
            let negative_log_likelihood = -inference.log_likelihood / observation_count as f64;
            let occupancy = mean_occupancy(&inference.filtered, state_count)?;
            let stability = total_variation(&occupancy, model.stationary_probabilities())?;
            if !negative_log_likelihood.is_finite() || !stability.is_finite() {
                return Err(HmmError::NumericalFailure);
            }
            fold_scores.push(FoldScore {
                training_evidence_id: fold.training.evidence_id(),
                validation_evidence_id: fold.validation.evidence_id(),
                fitted_model_id: model.model_id,
                diagnostics: model.diagnostics,
                validation_negative_log_likelihood: negative_log_likelihood,
                stationary_occupancy_distance: stability,
                observation_count,
            });
        }
        let (mean_nll, mean_stability, composite) =
            if eligible && fold_scores.len() == inner_folds.len() {
                let fold_count = fold_scores.len() as f64;
                let mean_nll = fold_scores
                    .iter()
                    .map(|score| score.validation_negative_log_likelihood)
                    .sum::<f64>()
                    / fold_count;
                let mean_stability = fold_scores
                    .iter()
                    .map(|score| score.stationary_occupancy_distance)
                    .sum::<f64>()
                    / fold_count;
                let composite = mean_nll + config.stability_weight * mean_stability;
                if !composite.is_finite() {
                    return Err(HmmError::NumericalFailure);
                }
                (Some(mean_nll), Some(mean_stability), Some(composite))
            } else {
                (None, None, None)
            };
        candidates.push(CandidateScore {
            state_count,
            eligible: composite.is_some(),
            ineligibility,
            failed_fold_index,
            mean_validation_negative_log_likelihood: mean_nll,
            mean_stationary_occupancy_distance: mean_stability,
            composite_score: composite,
            fold_scores,
        });
    }

    let selected_state_count = candidates
        .iter()
        .filter_map(|candidate| {
            candidate
                .composite_score
                .map(|score| (candidate.state_count, score))
        })
        .min_by(|left, right| {
            left.1
                .total_cmp(&right.1)
                .then_with(|| left.0.cmp(&right.0))
        })
        .map(|candidate| candidate.0)
        .ok_or(HmmError::NoEligibleCandidate)?;
    let final_seed = derived_seed(config.seed, selected_state_count, inner_folds.len());
    let model = fit(
        final_training,
        config.fit_config(selected_state_count, final_seed)?,
    )?;
    if !model.diagnostics.converged {
        return Err(HmmError::NonConvergedFit);
    }
    let report = SelectionReport {
        selected_state_count,
        candidates,
    };
    let selection_id = hash_selection(inner_folds, final_training, config, &report, model.model_id);
    Ok(SelectedStudentTHmm {
        model,
        report,
        selection_id,
    })
}

pub(crate) fn infer_parameters(
    initial: &[f64],
    transition: &[Vec<f64>],
    emissions: &[StudentTEmission],
    sequence: &EmissionSequence,
) -> Result<InferenceWorkspace, HmmError> {
    if emissions.len() != initial.len()
        || emissions
            .iter()
            .any(|emission| emission.location.len() != sequence.feature_families().len())
    {
        return Err(HmmError::DimensionMismatch);
    }
    let mut emission_logs = Vec::with_capacity(sequence.len());
    for observation in sequence.observations() {
        let mut row = Vec::with_capacity(emissions.len());
        for emission in emissions {
            row.push(log_student_t_density(emission, observation.values())?);
        }
        emission_logs.push(row);
    }
    forward_backward::infer(initial, transition, &emission_logs)
}

fn initialize(sequence: &EmissionSequence, config: HmmConfig) -> Result<ModelParameters, HmmError> {
    let state_count = config.state_count();
    let observations = sequence.observations();
    let dimension = sequence.feature_families().len();
    let mut global_location = vec![0.0; dimension];
    for observation in observations {
        for (target, value) in global_location.iter_mut().zip(observation.values()) {
            *target += *value;
        }
    }
    for value in &mut global_location {
        *value /= observations.len() as f64;
    }
    let mut global_scale = vec![0.0; dimension];
    for observation in observations {
        for ((target, value), mean) in global_scale
            .iter_mut()
            .zip(observation.values())
            .zip(&global_location)
        {
            *target += (*value - *mean).powi(2);
        }
    }
    for value in &mut global_scale {
        *value = (*value / observations.len() as f64).max(config.diagonal_scale_floor());
    }

    let mut center_indices = Vec::with_capacity(state_count);
    center_indices.push((splitmix64(config.seed()) as usize) % observations.len());
    while center_indices.len() < state_count {
        let mut best: Option<(usize, f64)> = None;
        for (index, observation) in observations.iter().enumerate() {
            if center_indices.contains(&index) {
                continue;
            }
            let nearest = center_indices
                .iter()
                .map(|center| {
                    squared_scaled_distance(
                        observation.values(),
                        observations[*center].values(),
                        &global_scale,
                    )
                })
                .fold(f64::INFINITY, f64::min);
            if best.is_none_or(|(_, distance)| nearest > distance) {
                best = Some((index, nearest));
            }
        }
        let Some((index, distance)) = best else {
            return Err(HmmError::DegenerateFit);
        };
        if !distance.is_finite() || distance <= f64::EPSILON {
            return Err(HmmError::DegenerateFit);
        }
        center_indices.push(index);
    }

    let initial = vec![1.0 / state_count as f64; state_count];
    let diagonal_probability = 0.85;
    let off_diagonal_probability =
        (1.0 - diagonal_probability) / (state_count.saturating_sub(1)) as f64;
    let transition = (0..state_count)
        .map(|row| {
            (0..state_count)
                .map(|column| {
                    if row == column {
                        diagonal_probability
                    } else {
                        off_diagonal_probability
                    }
                })
                .collect()
        })
        .collect();
    let emissions = center_indices
        .into_iter()
        .map(|index| {
            StudentTEmission::try_new(
                observations[index].values().to_vec(),
                global_scale.clone(),
                config.degrees_of_freedom(),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((initial, transition, emissions))
}

fn maximize(
    sequence: &EmissionSequence,
    config: HmmConfig,
    current_emissions: &[StudentTEmission],
    workspace: &InferenceWorkspace,
) -> Result<ModelParameters, HmmError> {
    let state_count = config.state_count();
    let dimension = sequence.feature_families().len();
    let pseudocount = config.probability_pseudocount();
    let mut initial = workspace.smoothed[0]
        .iter()
        .map(|probability| probability + pseudocount)
        .collect::<Vec<_>>();
    forward_backward::normalize_probability_vector(&mut initial)?;

    let mut transition = workspace.expected_transitions.clone();
    for row in &mut transition {
        for value in row.iter_mut() {
            *value += pseudocount;
        }
        forward_backward::normalize_probability_vector(row)?;
    }

    let mut emissions = Vec::with_capacity(state_count);
    for state in 0..state_count {
        let occupancy = workspace
            .smoothed
            .iter()
            .map(|posterior| posterior[state])
            .sum::<f64>();
        if !occupancy.is_finite() || occupancy <= OCCUPANCY_FLOOR {
            return Err(HmmError::DegenerateFit);
        }

        let mut robust_weights = Vec::with_capacity(sequence.len());
        let mut weighted_occupancy = 0.0;
        for (observation, posterior) in sequence.observations().iter().zip(&workspace.smoothed) {
            let latent_weight =
                student_t_latent_weight(&current_emissions[state], observation.values())?;
            let weight = posterior[state] * latent_weight;
            robust_weights.push(weight);
            weighted_occupancy += weight;
        }
        if !weighted_occupancy.is_finite() || weighted_occupancy <= OCCUPANCY_FLOOR {
            return Err(HmmError::DegenerateFit);
        }

        let mut location = vec![0.0; dimension];
        for (observation, weight) in sequence.observations().iter().zip(&robust_weights) {
            for (target, value) in location.iter_mut().zip(observation.values()) {
                *target += *weight * *value;
            }
        }
        for value in &mut location {
            *value /= weighted_occupancy;
        }

        let mut diagonal_scale = vec![0.0; dimension];
        for (observation, weight) in sequence.observations().iter().zip(&robust_weights) {
            for ((target, value), mean) in diagonal_scale
                .iter_mut()
                .zip(observation.values())
                .zip(&location)
            {
                *target += *weight * (*value - *mean).powi(2);
            }
        }
        for value in &mut diagonal_scale {
            *value = (*value / occupancy).max(config.diagonal_scale_floor());
        }
        emissions.push(StudentTEmission::try_new(
            location,
            diagonal_scale,
            config.degrees_of_freedom(),
        )?);
    }
    Ok((initial, transition, emissions))
}

fn log_student_t_density(emission: &StudentTEmission, values: &[f64]) -> Result<f64, HmmError> {
    if values.len() != emission.location.len() {
        return Err(HmmError::DimensionMismatch);
    }
    let dimension = values.len() as f64;
    let degrees = emission.degrees_of_freedom;
    let distance = mahalanobis_squared(emission, values)?;
    let log_determinant = emission
        .diagonal_scale
        .iter()
        .map(|value| value.ln())
        .sum::<f64>();
    let result = ln_gamma((degrees + dimension) / 2.0)
        - ln_gamma(degrees / 2.0)
        - 0.5 * dimension * (degrees * std::f64::consts::PI).ln()
        - 0.5 * log_determinant
        - 0.5 * (degrees + dimension) * (distance / degrees).ln_1p();
    if result.is_finite() {
        Ok(result)
    } else {
        Err(HmmError::NumericalFailure)
    }
}

fn student_t_latent_weight(emission: &StudentTEmission, values: &[f64]) -> Result<f64, HmmError> {
    let dimension = values.len() as f64;
    let distance = mahalanobis_squared(emission, values)?;
    let result =
        (emission.degrees_of_freedom + dimension) / (emission.degrees_of_freedom + distance);
    if result.is_finite() && result > 0.0 {
        Ok(result)
    } else {
        Err(HmmError::NumericalFailure)
    }
}

fn mahalanobis_squared(emission: &StudentTEmission, values: &[f64]) -> Result<f64, HmmError> {
    if values.len() != emission.location.len() {
        return Err(HmmError::DimensionMismatch);
    }
    let result = values
        .iter()
        .zip(&emission.location)
        .zip(&emission.diagonal_scale)
        .map(|((value, mean), scale)| (*value - *mean).powi(2) / *scale)
        .sum::<f64>();
    if result.is_finite() && result >= 0.0 {
        Ok(result)
    } else {
        Err(HmmError::NumericalFailure)
    }
}

fn squared_scaled_distance(left: &[f64], right: &[f64], scale: &[f64]) -> f64 {
    left.iter()
        .zip(right)
        .zip(scale)
        .map(|((left, right), scale)| (*left - *right).powi(2) / *scale)
        .sum()
}

fn maximum_parameter_change(
    initial: &[f64],
    transition: &[Vec<f64>],
    emissions: &[StudentTEmission],
    next_initial: &[f64],
    next_transition: &[Vec<f64>],
    next_emissions: &[StudentTEmission],
) -> f64 {
    let initial_change = initial
        .iter()
        .zip(next_initial)
        .map(|(left, right)| (*left - *right).abs());
    let transition_change = transition
        .iter()
        .flatten()
        .zip(next_transition.iter().flatten())
        .map(|(left, right)| (*left - *right).abs());
    let emission_change = emissions
        .iter()
        .zip(next_emissions)
        .flat_map(|(left, right)| {
            left.location
                .iter()
                .zip(&right.location)
                .chain(left.diagonal_scale.iter().zip(&right.diagonal_scale))
                .map(|(left, right)| (*left - *right).abs())
        });
    initial_change
        .chain(transition_change)
        .chain(emission_change)
        .fold(0.0_f64, f64::max)
}

fn canonicalize_states(
    initial: &mut Vec<f64>,
    transition: &mut Vec<Vec<f64>>,
    emissions: &mut Vec<StudentTEmission>,
) {
    let mut order = (0..emissions.len()).collect::<Vec<_>>();
    order.sort_by(|left, right| compare_emissions(&emissions[*left], &emissions[*right]));
    if order
        .iter()
        .enumerate()
        .all(|(index, value)| index == *value)
    {
        return;
    }
    let old_initial = initial.clone();
    let old_transition = transition.clone();
    let old_emissions = emissions.clone();
    *initial = order.iter().map(|index| old_initial[*index]).collect();
    *transition = order
        .iter()
        .map(|row| {
            order
                .iter()
                .map(|column| old_transition[*row][*column])
                .collect()
        })
        .collect();
    *emissions = order
        .iter()
        .map(|index| old_emissions[*index].clone())
        .collect();
}

fn compare_emissions(left: &StudentTEmission, right: &StudentTEmission) -> Ordering {
    for (left_value, right_value) in left.location.iter().zip(&right.location) {
        let ordering = left_value.total_cmp(right_value);
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

fn mean_occupancy(posteriors: &[Vec<f64>], state_count: usize) -> Result<Vec<f64>, HmmError> {
    if posteriors.is_empty() || posteriors.iter().any(|row| row.len() != state_count) {
        return Err(HmmError::NumericalFailure);
    }
    let mut occupancy = vec![0.0; state_count];
    for posterior in posteriors {
        for (target, probability) in occupancy.iter_mut().zip(posterior) {
            *target += *probability;
        }
    }
    for value in &mut occupancy {
        *value /= posteriors.len() as f64;
    }
    forward_backward::normalize_probability_vector(&mut occupancy)?;
    Ok(occupancy)
}

fn total_variation(left: &[f64], right: &[f64]) -> Result<f64, HmmError> {
    if left.len() != right.len() || left.is_empty() {
        return Err(HmmError::NumericalFailure);
    }
    let result = 0.5
        * left
            .iter()
            .zip(right)
            .map(|(left, right)| (*left - *right).abs())
            .sum::<f64>();
    if result.is_finite() && (0.0..=1.0 + 1e-12).contains(&result) {
        Ok(result.min(1.0))
    } else {
        Err(HmmError::NumericalFailure)
    }
}

fn checked_fit_work(
    observation_count: usize,
    dimension: usize,
    config: HmmConfig,
) -> Result<(), HmmError> {
    let work = observation_count
        .checked_mul(config.state_count())
        .and_then(|value| value.checked_mul(config.state_count().saturating_add(dimension)))
        .and_then(|value| value.checked_mul(config.max_iterations()))
        .ok_or(HmmError::WorkCapacity)?;
    if work > MAXIMUM_FIT_WORK_UNITS {
        return Err(HmmError::WorkCapacity);
    }
    Ok(())
}

fn checked_selection_work(
    folds: &[WalkForwardFold],
    final_training: &EmissionSequence,
    config: SelectionConfig,
) -> Result<(), HmmError> {
    let mut work = 0_usize;
    let dimension = final_training.feature_families().len();
    for state_count in config.minimum_states..=config.maximum_states {
        let inference_work = state_count
            .checked_mul(state_count.saturating_add(dimension))
            .ok_or(HmmError::WorkCapacity)?;
        let fit_work = inference_work
            .checked_mul(config.max_iterations)
            .ok_or(HmmError::WorkCapacity)?;
        for fold in folds {
            work = fold
                .training
                .len()
                .checked_mul(fit_work)
                .and_then(|value| work.checked_add(value))
                .ok_or(HmmError::WorkCapacity)?;
            work = fold
                .validation
                .len()
                .checked_mul(inference_work)
                .and_then(|value| work.checked_add(value))
                .ok_or(HmmError::WorkCapacity)?;
        }
        work = final_training
            .len()
            .checked_mul(fit_work)
            .and_then(|value| work.checked_add(value))
            .ok_or(HmmError::WorkCapacity)?;
    }
    if work > MAXIMUM_SELECTION_WORK_UNITS {
        return Err(HmmError::WorkCapacity);
    }
    Ok(())
}

const fn candidate_ineligibility(error: HmmError) -> Option<CandidateIneligibility> {
    match error {
        HmmError::InsufficientData => Some(CandidateIneligibility::InsufficientData),
        HmmError::DegenerateFit => Some(CandidateIneligibility::DegenerateFit),
        HmmError::LikelihoodDecreased => Some(CandidateIneligibility::LikelihoodDecreased),
        HmmError::NumericalFailure => Some(CandidateIneligibility::NumericalFailure),
        HmmError::NonConvergedFit => Some(CandidateIneligibility::NonConverged),
        _ => None,
    }
}

fn derived_seed(base: u64, state_count: usize, fold_index: usize) -> u64 {
    splitmix64(base ^ (state_count as u64).rotate_left(17) ^ (fold_index as u64).rotate_left(41))
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn hash_selection(
    folds: &[WalkForwardFold],
    final_training: &EmissionSequence,
    config: SelectionConfig,
    report: &SelectionReport,
    model_id: [u8; 32],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(SELECTION_HASH_DOMAIN);
    hasher.update(&(config.minimum_states as u64).to_le_bytes());
    hasher.update(&(config.maximum_states as u64).to_le_bytes());
    let base_config = HmmConfig {
        state_count: config.minimum_states,
        degrees_of_freedom: config.degrees_of_freedom,
        diagonal_scale_floor: config.diagonal_scale_floor,
        probability_pseudocount: config.probability_pseudocount,
        max_iterations: config.max_iterations,
        convergence_tolerance: config.convergence_tolerance,
        seed: config.seed,
    };
    hash_config(&mut hasher, base_config);
    hasher.update(&config.stability_weight.to_bits().to_le_bytes());
    hasher.update(&(folds.len() as u64).to_le_bytes());
    for fold in folds {
        hasher.update(&fold.training.evidence_id());
        hasher.update(&fold.validation.evidence_id());
    }
    hasher.update(&final_training.evidence_id());
    hasher.update(&(report.selected_state_count as u64).to_le_bytes());
    for candidate in &report.candidates {
        hasher.update(&(candidate.state_count as u64).to_le_bytes());
        hasher.update(&[u8::from(candidate.eligible)]);
        match candidate.ineligibility {
            Some(reason) => hasher.update(&[1, candidate_ineligibility_byte(reason)]),
            None => hasher.update(&[0]),
        };
        match candidate.failed_fold_index {
            Some(index) => {
                hasher.update(&[1]);
                hasher.update(&(index as u64).to_le_bytes());
            }
            None => {
                hasher.update(&[0]);
            }
        }
        for value in [
            candidate.mean_validation_negative_log_likelihood,
            candidate.mean_stationary_occupancy_distance,
            candidate.composite_score,
        ] {
            match value {
                Some(value) => {
                    hasher.update(&[1]);
                    hasher.update(&value.to_bits().to_le_bytes());
                }
                None => {
                    hasher.update(&[0]);
                }
            }
        }
        for fold_score in &candidate.fold_scores {
            hasher.update(&fold_score.training_evidence_id);
            hasher.update(&fold_score.validation_evidence_id);
            hasher.update(&fold_score.fitted_model_id);
            hasher.update(&[u8::from(fold_score.diagnostics.converged)]);
            hasher.update(&(fold_score.diagnostics.iterations as u64).to_le_bytes());
            hasher.update(
                &fold_score
                    .diagnostics
                    .log_likelihood
                    .to_bits()
                    .to_le_bytes(),
            );
            hasher.update(
                &fold_score
                    .diagnostics
                    .max_parameter_change
                    .to_bits()
                    .to_le_bytes(),
            );
            hasher.update(&fold_score.diagnostics.seed.to_le_bytes());
            hasher.update(
                &fold_score
                    .validation_negative_log_likelihood
                    .to_bits()
                    .to_le_bytes(),
            );
            hasher.update(
                &fold_score
                    .stationary_occupancy_distance
                    .to_bits()
                    .to_le_bytes(),
            );
            hasher.update(&(fold_score.observation_count as u64).to_le_bytes());
        }
    }
    hasher.update(&model_id);
    *hasher.finalize().as_bytes()
}

const fn candidate_ineligibility_byte(reason: CandidateIneligibility) -> u8 {
    match reason {
        CandidateIneligibility::InsufficientData => 1,
        CandidateIneligibility::DegenerateFit => 2,
        CandidateIneligibility::LikelihoodDecreased => 3,
        CandidateIneligibility::NumericalFailure => 4,
        CandidateIneligibility::NonConverged => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::{log_student_t_density, student_t_latent_weight};
    use crate::StudentTEmission;

    #[test]
    fn multivariate_student_t_density_and_latent_weight_match_literal_reference() {
        let emission =
            StudentTEmission::try_new(vec![0.0, 1.0], vec![1.0, 4.0], 8.0).expect("valid emission");
        let values = [2.0, 5.0];
        // Mahalanobis distance is 8. The fixed-nu latent weight is
        // (8 + 2) / (8 + 8) = 0.625.
        assert!(
            (student_t_latent_weight(&emission, &values).expect("finite weight") - 0.625).abs()
                < 1e-12
        );
        assert!(
            (log_student_t_density(&emission, &values).expect("finite density")
                - -5.996_760_149_769_017)
                .abs()
                < 1e-12
        );
    }
}
