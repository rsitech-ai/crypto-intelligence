//! Inner-walk-forward selection and volatility-normalized state.

use std::collections::BTreeSet;

use dataset::{InnerFold, Sample, TimeRange};
use thiserror::Error;

use crate::{EwmaVolatility, HarRvModel, HarRvObservation};

const MAXIMUM_OBSERVATIONS: usize = 65_536;
const MAXIMUM_FOLDS: usize = 1_024;
const MAXIMUM_CANDIDATES_PER_FAMILY: usize = 64;
const MAXIMUM_SELECTION_WORK_ITEMS: usize = 10_000_000;
const SELECTION_HASH_DOMAIN: &[u8] = b"cmti:volatility-selection:v1\0";

/// Forecast model family.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelFamily {
    Ewma,
    HarRv,
}

/// One candidate parameterization.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CandidateModel {
    Ewma { lambda: f64 },
    HarRv { ridge: f64 },
}

impl CandidateModel {
    pub const fn family(self) -> ModelFamily {
        match self {
            Self::Ewma { .. } => ModelFamily::Ewma,
            Self::HarRv { .. } => ModelFamily::HarRv,
        }
    }
}

/// Validation score retained for every candidate, including non-winners.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CandidateScore {
    pub candidate: CandidateModel,
    pub mean_squared_error: f64,
    pub validation_rows: u64,
    pub fold_count: u32,
}

/// Deterministic inner-fold selection report.
#[derive(Clone, Debug, PartialEq)]
pub struct SelectionReport {
    pub selected: CandidateModel,
    pub candidates: Vec<CandidateScore>,
    pub selection_id: [u8; 32],
}

/// Bounded candidate grid and minimum training evidence.
#[derive(Clone, Debug, PartialEq)]
pub struct SelectionConfig {
    ewma_lambdas: Vec<f64>,
    har_ridges: Vec<f64>,
    minimum_training_rows: usize,
}

impl SelectionConfig {
    pub fn try_new(
        ewma_lambdas: impl IntoIterator<Item = f64>,
        har_ridges: impl IntoIterator<Item = f64>,
        minimum_training_rows: usize,
    ) -> Result<Self, ForecastError> {
        let mut ewma_lambdas = ewma_lambdas.into_iter().collect::<Vec<_>>();
        let mut har_ridges = har_ridges.into_iter().collect::<Vec<_>>();
        if ewma_lambdas.is_empty()
            || ewma_lambdas.len() > MAXIMUM_CANDIDATES_PER_FAMILY
            || har_ridges.is_empty()
            || har_ridges.len() > MAXIMUM_CANDIDATES_PER_FAMILY
            || !(5..=MAXIMUM_OBSERVATIONS).contains(&minimum_training_rows)
            || ewma_lambdas
                .iter()
                .any(|value| !value.is_finite() || *value <= 0.0 || *value >= 1.0)
            || har_ridges
                .iter()
                .any(|value| !value.is_finite() || *value <= 0.0)
        {
            return Err(ForecastError::InvalidConfiguration);
        }
        ewma_lambdas.sort_by(f64::total_cmp);
        har_ridges.sort_by(f64::total_cmp);
        if has_duplicate_bits(&ewma_lambdas) || has_duplicate_bits(&har_ridges) {
            return Err(ForecastError::InvalidConfiguration);
        }
        Ok(Self {
            ewma_lambdas,
            har_ridges,
            minimum_training_rows,
        })
    }

    pub fn ewma_lambdas(&self) -> &[f64] {
        &self.ewma_lambdas
    }

    pub fn har_ridges(&self) -> &[f64] {
        &self.har_ridges
    }

    pub const fn minimum_training_rows(&self) -> usize {
        self.minimum_training_rows
    }
}

/// Point-in-time feature and target row used by inner-fold selection.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SelectionObservation {
    sample: Sample,
    predictors_as_known_at_ns: i64,
    return_value: f64,
    daily: f64,
    weekly: f64,
    monthly: f64,
    target: f64,
    lineage_hash: [u8; 32],
}

impl SelectionObservation {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        sample: Sample,
        predictors_as_known_at_ns: i64,
        return_value: f64,
        daily: f64,
        weekly: f64,
        monthly: f64,
        target: f64,
        lineage_hash: [u8; 32],
    ) -> Result<Self, ForecastError> {
        if !return_value.is_finite()
            || predictors_as_known_at_ns <= 0
            || predictors_as_known_at_ns > sample.origin_time_ns()
            || sample.outcome_end_ns() <= sample.origin_time_ns()
            || sample.as_known_at_ns() <= sample.origin_time_ns()
            || [daily, weekly, monthly, target]
                .iter()
                .any(|value| !value.is_finite() || *value < 0.0)
            || lineage_hash == [0; 32]
        {
            return Err(ForecastError::InvalidObservation);
        }
        Ok(Self {
            sample,
            predictors_as_known_at_ns,
            return_value: canonical_zero(return_value),
            daily: canonical_zero(daily),
            weekly: canonical_zero(weekly),
            monthly: canonical_zero(monthly),
            target: canonical_zero(target),
            lineage_hash,
        })
    }

    pub const fn sample(self) -> Sample {
        self.sample
    }

    pub const fn target(self) -> f64 {
        self.target
    }
}

/// Selects a volatility model using only declared inner training/validation folds.
pub fn select_volatility_model(
    observations: &[SelectionObservation],
    folds: &[InnerFold],
    config: &SelectionConfig,
) -> Result<SelectionReport, ForecastError> {
    if observations.is_empty()
        || observations.len() > MAXIMUM_OBSERVATIONS
        || folds.is_empty()
        || folds.len() > MAXIMUM_FOLDS
    {
        return Err(ForecastError::InvalidSelection);
    }
    let mut canonical = observations.iter().collect::<Vec<_>>();
    canonical.sort_by_key(|row| row.sample.id());
    for pair in canonical.windows(2) {
        if pair[0].sample.id() == pair[1].sample.id() {
            return Err(ForecastError::DuplicateObservation {
                observation_id: pair[0].sample.id(),
            });
        }
        if pair[0].sample.origin_time_ns() >= pair[1].sample.origin_time_ns() {
            return Err(ForecastError::NonMonotonicObservations);
        }
    }

    let prepared = folds
        .iter()
        .map(|fold| {
            let training = select(&canonical, fold.training());
            let validation = select(&canonical, fold.validation());
            if training.len() < config.minimum_training_rows || validation.is_empty() {
                return Err(ForecastError::InsufficientFoldEvidence);
            }
            Ok(PreparedFold {
                training,
                validation,
                training_end_ns: fold.training().end_ns(),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let candidate_count = config
        .ewma_lambdas
        .len()
        .checked_add(config.har_ridges.len())
        .ok_or(ForecastError::CapacityExceeded)?;
    let eligible_rows = prepared.iter().try_fold(0_usize, |total, fold| {
        total
            .checked_add(fold.training.len())
            .and_then(|value| value.checked_add(fold.validation.len()))
            .ok_or(ForecastError::CapacityExceeded)
    })?;
    if candidate_count
        .checked_mul(eligible_rows)
        .is_none_or(|work| work > MAXIMUM_SELECTION_WORK_ITEMS)
    {
        return Err(ForecastError::CapacityExceeded);
    }

    let mut candidates = Vec::with_capacity(config.ewma_lambdas.len() + config.har_ridges.len());
    for lambda in &config.ewma_lambdas {
        candidates.push(score_ewma(&prepared, *lambda)?);
    }
    for ridge in &config.har_ridges {
        candidates.push(score_har(&prepared, *ridge)?);
    }
    let selected = candidates
        .iter()
        .min_by(|left, right| compare_scores(left, right))
        .map(|score| score.candidate)
        .ok_or(ForecastError::InvalidSelection)?;
    let selection_id = hash_selection(&canonical, &prepared, folds, config, &candidates, selected);
    Ok(SelectionReport {
        selected,
        candidates,
        selection_id,
    })
}

/// Specification state `y = (return - mean) / max(sigma_hat, sigma_min)`.
pub fn volatility_normalized_state(
    return_value: f64,
    mean: f64,
    forecast_volatility: f64,
    minimum_volatility: f64,
) -> Result<f64, ForecastError> {
    if !return_value.is_finite() || !mean.is_finite() {
        return Err(ForecastError::NonFinite);
    }
    if !forecast_volatility.is_finite()
        || forecast_volatility < 0.0
        || !minimum_volatility.is_finite()
        || minimum_volatility <= 0.0
    {
        return Err(ForecastError::InvalidVolatility);
    }
    let value = (return_value - mean) / forecast_volatility.max(minimum_volatility);
    if value.is_finite() {
        Ok(canonical_zero(value))
    } else {
        Err(ForecastError::NumericalFailure)
    }
}

fn score_ewma(folds: &[PreparedFold<'_>], lambda: f64) -> Result<CandidateScore, ForecastError> {
    let mut squared_error_sum = 0.0;
    let mut validation_rows = 0_u64;
    for fold in folds {
        let mut model = EwmaVolatility::try_new(lambda)?;
        for row in &fold.training {
            model.update(row.return_value)?;
        }
        for row in &fold.validation {
            model.update(row.return_value)?;
            let error = model.forecast()?.volatility - row.target;
            squared_error_sum = checked_add(squared_error_sum, error * error)?;
            validation_rows = validation_rows
                .checked_add(1)
                .ok_or(ForecastError::CapacityExceeded)?;
        }
    }
    candidate_score(
        CandidateModel::Ewma { lambda },
        squared_error_sum,
        validation_rows,
        folds.len(),
    )
}

fn score_har(folds: &[PreparedFold<'_>], ridge: f64) -> Result<CandidateScore, ForecastError> {
    let mut squared_error_sum = 0.0;
    let mut validation_rows = 0_u64;
    for fold in folds {
        let training = fold
            .training
            .iter()
            .copied()
            .map(as_har_observation)
            .collect::<Result<Vec<_>, _>>()?;
        let model = HarRvModel::fit(&training, fold.training_end_ns, ridge)?;
        for row in &fold.validation {
            let error = model
                .forecast(row.daily, row.weekly, row.monthly)?
                .volatility
                - row.target;
            squared_error_sum = checked_add(squared_error_sum, error * error)?;
            validation_rows = validation_rows
                .checked_add(1)
                .ok_or(ForecastError::CapacityExceeded)?;
        }
    }
    candidate_score(
        CandidateModel::HarRv { ridge },
        squared_error_sum,
        validation_rows,
        folds.len(),
    )
}

fn as_har_observation(row: &SelectionObservation) -> Result<HarRvObservation, ForecastError> {
    HarRvObservation::try_new(
        row.sample.id(),
        row.sample.origin_time_ns(),
        row.predictors_as_known_at_ns,
        row.sample.as_known_at_ns(),
        row.daily,
        row.weekly,
        row.monthly,
        row.target,
        row.lineage_hash,
    )
}

fn select<'a>(
    observations: &[&'a SelectionObservation],
    range: TimeRange,
) -> Vec<&'a SelectionObservation> {
    let start = observations.partition_point(|row| row.sample.origin_time_ns() < range.start_ns());
    let end = observations.partition_point(|row| row.sample.origin_time_ns() < range.end_ns());
    observations[start..end]
        .iter()
        .copied()
        .filter(|row| eligible_in_range(row, range))
        .collect()
}

fn eligible_in_range(row: &SelectionObservation, range: TimeRange) -> bool {
    row.sample.origin_time_ns() >= range.start_ns()
        && row.sample.origin_time_ns() < range.end_ns()
        && row.sample.outcome_end_ns() <= range.end_ns()
        && row.sample.as_known_at_ns() <= range.end_ns()
}

fn candidate_score(
    candidate: CandidateModel,
    squared_error_sum: f64,
    validation_rows: u64,
    fold_count: usize,
) -> Result<CandidateScore, ForecastError> {
    if validation_rows == 0 {
        return Err(ForecastError::InsufficientFoldEvidence);
    }
    let mean_squared_error = checked_add(squared_error_sum / validation_rows as f64, 0.0)?;
    Ok(CandidateScore {
        candidate,
        mean_squared_error: canonical_zero(mean_squared_error),
        validation_rows,
        fold_count: u32::try_from(fold_count).map_err(|_| ForecastError::CapacityExceeded)?,
    })
}

fn compare_scores(left: &CandidateScore, right: &CandidateScore) -> std::cmp::Ordering {
    left.mean_squared_error
        .total_cmp(&right.mean_squared_error)
        .then_with(|| candidate_rank(left.candidate).cmp(&candidate_rank(right.candidate)))
        .then_with(|| {
            candidate_parameter(left.candidate).total_cmp(&candidate_parameter(right.candidate))
        })
}

const fn candidate_rank(candidate: CandidateModel) -> u8 {
    match candidate {
        CandidateModel::Ewma { .. } => 0,
        CandidateModel::HarRv { .. } => 1,
    }
}

const fn candidate_parameter(candidate: CandidateModel) -> f64 {
    match candidate {
        CandidateModel::Ewma { lambda } => lambda,
        CandidateModel::HarRv { ridge } => ridge,
    }
}

fn hash_selection(
    observations: &[&SelectionObservation],
    prepared: &[PreparedFold<'_>],
    folds: &[InnerFold],
    config: &SelectionConfig,
    candidates: &[CandidateScore],
    selected: CandidateModel,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(SELECTION_HASH_DOMAIN);
    hasher.update(&(folds.len() as u64).to_le_bytes());
    for fold in folds {
        hasher.update(&fold.fold_hash());
    }
    let evidence_ids = prepared
        .iter()
        .flat_map(|fold| fold.training.iter().chain(&fold.validation))
        .map(|row| row.sample.id())
        .collect::<BTreeSet<_>>();
    for row in observations
        .iter()
        .filter(|row| evidence_ids.contains(&row.sample.id()))
    {
        hasher.update(&row.sample.id().to_le_bytes());
        hasher.update(&row.sample.origin_time_ns().to_le_bytes());
        hasher.update(&row.predictors_as_known_at_ns.to_le_bytes());
        hasher.update(&row.sample.outcome_end_ns().to_le_bytes());
        hasher.update(&row.sample.as_known_at_ns().to_le_bytes());
        for value in [
            row.return_value,
            row.daily,
            row.weekly,
            row.monthly,
            row.target,
        ] {
            hasher.update(&value.to_bits().to_le_bytes());
        }
        hasher.update(&row.lineage_hash);
    }
    hasher.update(&(config.minimum_training_rows as u64).to_le_bytes());
    for value in config.ewma_lambdas.iter().chain(config.har_ridges.iter()) {
        hasher.update(&value.to_bits().to_le_bytes());
    }
    for score in candidates {
        hasher.update(&[candidate_rank(score.candidate)]);
        hasher.update(&candidate_parameter(score.candidate).to_bits().to_le_bytes());
        hasher.update(&score.mean_squared_error.to_bits().to_le_bytes());
        hasher.update(&score.validation_rows.to_le_bytes());
        hasher.update(&score.fold_count.to_le_bytes());
    }
    hasher.update(&[candidate_rank(selected)]);
    hasher.update(&candidate_parameter(selected).to_bits().to_le_bytes());
    *hasher.finalize().as_bytes()
}

struct PreparedFold<'a> {
    training: Vec<&'a SelectionObservation>,
    validation: Vec<&'a SelectionObservation>,
    training_end_ns: i64,
}

fn has_duplicate_bits(values: &[f64]) -> bool {
    values
        .windows(2)
        .any(|pair| pair[0].to_bits() == pair[1].to_bits())
}

fn checked_add(left: f64, right: f64) -> Result<f64, ForecastError> {
    let value = left + right;
    if value.is_finite() {
        Ok(value)
    } else {
        Err(ForecastError::NumericalFailure)
    }
}

const fn canonical_zero(value: f64) -> f64 {
    if value == 0.0 { 0.0 } else { value }
}

/// Fail-closed forecast validation and numerical errors.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ForecastError {
    #[error("EWMA lambda must be finite and strictly between zero and one")]
    InvalidLambda,
    #[error("forecast input is nonfinite")]
    NonFinite,
    #[error("volatility input or floor is outside its supported domain")]
    InvalidVolatility,
    #[error("forecast observation is invalid")]
    InvalidObservation,
    #[error("forecast configuration is invalid")]
    InvalidConfiguration,
    #[error("forecast history is insufficient")]
    InsufficientHistory,
    #[error("walk-forward fold does not contain sufficient eligible evidence")]
    InsufficientFoldEvidence,
    #[error("walk-forward selection input is invalid")]
    InvalidSelection,
    #[error("forecast input exceeds its bounded capacity")]
    CapacityExceeded,
    #[error("duplicate observation id {observation_id}")]
    DuplicateObservation { observation_id: u64 },
    #[error("observations are not strictly chronological by canonical id")]
    NonMonotonicObservations,
    #[error("observation {observation_id} origin is after the fit cutoff")]
    OriginAfterFitCutoff { observation_id: u64 },
    #[error("observation {observation_id} target was known after the fit cutoff")]
    OutcomeKnownAfterFitCutoff { observation_id: u64 },
    #[error("forecast calculation cannot be represented finitely")]
    NumericalFailure,
}
