//! Log-space run-length recurrence, truncation, reset, and evidence identity.

use crate::{BocpdError, NormalInverseGamma};

const MAXIMUM_RUN_LENGTH: usize = 65_536;
const MAXIMUM_FEATURE_FAMILY_BYTES: usize = 128;
const MODEL_HASH_DOMAIN: &[u8] = b"cmti:bocpd-model:v1\0";
const INITIAL_EVIDENCE_HASH_DOMAIN: &[u8] = b"cmti:bocpd-evidence-initial:v1\0";
const UPDATE_EVIDENCE_HASH_DOMAIN: &[u8] = b"cmti:bocpd-evidence-update:v1\0";
const RESET_EVIDENCE_HASH_DOMAIN: &[u8] = b"cmti:bocpd-evidence-reset:v1\0";
const LOG_MINIMUM_POSITIVE: f64 = -708.396_418_532_264_1;
const OBSERVATION_FAMILY_VERSION: &[u8] = b"gaussian-nig-student-t:v1\0";
const TRUNCATION_STRATEGY_VERSION: &[u8] = b"hard-discard-tail:v1\0";

/// Point-in-time quality attached to the accepted observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QualityState {
    Trusted,
    Degraded,
    Unavailable,
}

/// Immutable point-in-time standardized feature observation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BocpdObservation {
    observation_id: u64,
    event_time_ns: i64,
    as_known_at_ns: i64,
    value: f64,
    quality: QualityState,
    lineage_hash: [u8; 32],
}

impl BocpdObservation {
    pub fn try_new(
        observation_id: u64,
        event_time_ns: i64,
        as_known_at_ns: i64,
        value: f64,
        quality: QualityState,
        lineage_hash: [u8; 32],
    ) -> Result<Self, BocpdError> {
        if !value.is_finite() {
            return Err(BocpdError::NonFiniteObservation);
        }
        if quality == QualityState::Unavailable {
            return Err(BocpdError::UnavailableQuality);
        }
        if observation_id == 0
            || event_time_ns <= 0
            || as_known_at_ns < event_time_ns
            || lineage_hash == [0; 32]
        {
            return Err(BocpdError::InvalidObservation);
        }
        Ok(Self {
            observation_id,
            event_time_ns,
            as_known_at_ns,
            value: canonical_zero(value),
            quality,
            lineage_hash,
        })
    }

    pub const fn observation_id(self) -> u64 {
        self.observation_id
    }

    pub const fn event_time_ns(self) -> i64 {
        self.event_time_ns
    }

    pub const fn as_known_at_ns(self) -> i64 {
        self.as_known_at_ns
    }

    pub const fn value(self) -> f64 {
        self.value
    }

    pub const fn quality(self) -> QualityState {
        self.quality
    }

    pub const fn lineage_hash(self) -> [u8; 32] {
        self.lineage_hash
    }
}

/// Versioned state-reset policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResetPolicy {
    Manual,
}

/// Immutable BOCPD configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct BocpdConfig {
    prior: NormalInverseGamma,
    hazard_probability: f64,
    max_run_length: usize,
    changepoint_window: usize,
    feature_family: String,
    reset_policy: ResetPolicy,
}

impl BocpdConfig {
    pub fn try_new(
        prior: NormalInverseGamma,
        hazard_probability: f64,
        max_run_length: usize,
        changepoint_window: usize,
        feature_family: impl Into<String>,
        reset_policy: ResetPolicy,
    ) -> Result<Self, BocpdError> {
        let feature_family = feature_family.into();
        if !hazard_probability.is_finite()
            || hazard_probability <= 0.0
            || hazard_probability >= 1.0
            || !(1..=MAXIMUM_RUN_LENGTH).contains(&max_run_length)
            || !(1..=max_run_length).contains(&changepoint_window)
            || !valid_feature_family(&feature_family)
        {
            return Err(BocpdError::InvalidConfiguration);
        }
        Ok(Self {
            prior,
            hazard_probability,
            max_run_length,
            changepoint_window,
            feature_family,
            reset_policy,
        })
    }

    pub const fn prior(&self) -> NormalInverseGamma {
        self.prior
    }

    pub const fn hazard_probability(&self) -> f64 {
        self.hazard_probability
    }

    pub const fn max_run_length(&self) -> usize {
        self.max_run_length
    }

    pub const fn changepoint_window(&self) -> usize {
        self.changepoint_window
    }

    pub fn feature_family(&self) -> &str {
        &self.feature_family
    }

    pub const fn reset_policy(&self) -> ResetPolicy {
        self.reset_policy
    }
}

/// Immutable posterior and provenance emitted after an update or reset.
#[derive(Clone, Debug, PartialEq)]
pub struct BocpdOutput {
    pub changepoint_probability: f64,
    pub reset_probability: f64,
    pub expected_run_length: f64,
    pub run_length_entropy: f64,
    pub posterior: Vec<f64>,
    pub discarded_tail_probability: f64,
    pub observation_count: u64,
    pub reset_count: u64,
    pub last_observation_id: Option<u64>,
    pub last_event_time_ns: Option<i64>,
    pub last_as_known_at_ns: Option<i64>,
    pub feature_family: String,
    pub quality: QualityState,
    pub model_id: [u8; 32],
    pub evidence_id: [u8; 32],
}

/// Bounded causal BOCPD state.
#[derive(Clone, Debug, PartialEq)]
pub struct Bocpd {
    config: BocpdConfig,
    log_run_length: Vec<f64>,
    sufficient_statistics: Vec<NormalInverseGamma>,
    observation_count: u64,
    reset_count: u64,
    last_observation: Option<BocpdObservation>,
    quality: QualityState,
    model_id: [u8; 32],
    evidence_id: [u8; 32],
    discarded_tail_probability: f64,
}

impl Bocpd {
    pub fn try_new(config: BocpdConfig) -> Result<Self, BocpdError> {
        let capacity = config
            .max_run_length
            .checked_add(1)
            .ok_or(BocpdError::CapacityExceeded)?;
        let model_id = hash_model(&config);
        let evidence_id = hash_initial_evidence(model_id);
        let mut log_run_length = Vec::with_capacity(capacity);
        log_run_length.push(0.0);
        let mut sufficient_statistics = Vec::with_capacity(capacity);
        sufficient_statistics.push(config.prior);
        Ok(Self {
            config,
            log_run_length,
            sufficient_statistics,
            observation_count: 0,
            reset_count: 0,
            last_observation: None,
            quality: QualityState::Unavailable,
            model_id,
            evidence_id,
            discarded_tail_probability: 0.0,
        })
    }

    pub const fn model_id(&self) -> [u8; 32] {
        self.model_id
    }

    pub const fn observation_count(&self) -> u64 {
        self.observation_count
    }

    pub const fn reset_count(&self) -> u64 {
        self.reset_count
    }

    pub fn update(&mut self, observation: BocpdObservation) -> Result<BocpdOutput, BocpdError> {
        if self.last_observation.is_some_and(|previous| {
            observation.observation_id <= previous.observation_id
                || observation.event_time_ns <= previous.event_time_ns
                || observation.as_known_at_ns < previous.as_known_at_ns
        }) {
            return Err(BocpdError::NonMonotonicObservation);
        }
        let value = observation.value;
        let quality = observation.quality;
        let next_observation_count = self
            .observation_count
            .checked_add(1)
            .ok_or(BocpdError::CapacityExceeded)?;
        if self.log_run_length.len() != self.sufficient_statistics.len()
            || self.log_run_length.is_empty()
        {
            return Err(BocpdError::NumericalFailure);
        }

        let log_hazard = checked(self.config.hazard_probability.ln())?;
        let log_survival = checked((-self.config.hazard_probability).ln_1p())?;
        let predictive = self
            .sufficient_statistics
            .iter()
            .map(|state| state.log_predictive_density(value))
            .collect::<Result<Vec<_>, _>>()?;
        let reset_terms = self
            .log_run_length
            .iter()
            .zip(&predictive)
            .map(|(posterior, predictive)| checked(posterior + predictive + log_hazard))
            .collect::<Result<Vec<_>, _>>()?;
        let reset = log_sum_exp(&reset_terms)?;

        let retained_growth = self
            .log_run_length
            .iter()
            .zip(&predictive)
            .take(self.config.max_run_length)
            .map(|(posterior, predictive)| checked(posterior + predictive + log_survival))
            .collect::<Result<Vec<_>, _>>()?;
        let discarded_growth = if self.log_run_length.len() > self.config.max_run_length {
            let index = self.config.max_run_length;
            Some(checked(
                self.log_run_length[index] + predictive[index] + log_survival,
            )?)
        } else {
            None
        };

        let mut retained_joint = Vec::with_capacity(retained_growth.len() + 1);
        retained_joint.push(reset);
        retained_joint.extend(retained_growth);
        let retained_evidence = log_sum_exp(&retained_joint)?;
        let full_evidence = if let Some(discarded) = discarded_growth {
            log_add_exp(retained_evidence, discarded)?
        } else {
            retained_evidence
        };
        let discarded_tail_probability = if let Some(discarded) = discarded_growth {
            checked_probability(discarded - full_evidence)?
        } else {
            0.0
        };
        let next_log_run_length = retained_joint
            .into_iter()
            .map(|joint| {
                let normalized = checked(joint - retained_evidence)?;
                if normalized < LOG_MINIMUM_POSITIVE {
                    return Err(BocpdError::ProbabilityUnderflow);
                }
                Ok(canonical_zero(normalized))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let mut next_statistics = Vec::with_capacity(next_log_run_length.len());
        next_statistics.push(self.config.prior);
        for state in self
            .sufficient_statistics
            .iter()
            .take(self.config.max_run_length)
        {
            next_statistics.push(state.updated(value)?);
        }
        if next_statistics.len() != next_log_run_length.len() {
            return Err(BocpdError::NumericalFailure);
        }
        validate_normalized(&next_log_run_length)?;
        let next_evidence_id = hash_update_evidence(
            self.evidence_id,
            next_observation_count,
            self.reset_count,
            observation,
            discarded_tail_probability,
            &next_log_run_length,
        );

        self.log_run_length = next_log_run_length;
        self.sufficient_statistics = next_statistics;
        self.observation_count = next_observation_count;
        self.last_observation = Some(observation);
        self.quality = quality;
        self.evidence_id = next_evidence_id;
        self.discarded_tail_probability = canonical_zero(discarded_tail_probability);
        Ok(self.current_output())
    }

    pub fn reset(&mut self) -> Result<BocpdOutput, BocpdError> {
        let next_reset_count = self
            .reset_count
            .checked_add(1)
            .ok_or(BocpdError::CapacityExceeded)?;
        let next_evidence_id =
            hash_reset_evidence(self.evidence_id, next_reset_count, self.model_id);

        self.log_run_length.clear();
        self.log_run_length.push(0.0);
        self.sufficient_statistics.clear();
        self.sufficient_statistics.push(self.config.prior);
        self.observation_count = 0;
        self.reset_count = next_reset_count;
        self.last_observation = None;
        self.quality = QualityState::Unavailable;
        self.evidence_id = next_evidence_id;
        self.discarded_tail_probability = 0.0;
        Ok(self.current_output())
    }

    pub fn current_output(&self) -> BocpdOutput {
        let posterior = self
            .log_run_length
            .iter()
            .map(|value| canonical_zero(value.exp()))
            .collect::<Vec<_>>();
        let changepoint_probability = canonical_zero(
            posterior
                .iter()
                .take(self.config.changepoint_window + 1)
                .sum(),
        );
        let reset_probability = posterior.first().copied().unwrap_or(0.0);
        let expected_run_length = posterior
            .iter()
            .enumerate()
            .map(|(run_length, probability)| run_length as f64 * probability)
            .sum();
        let run_length_entropy = -posterior
            .iter()
            .filter(|probability| **probability > 0.0)
            .map(|probability| probability * probability.ln())
            .sum::<f64>();
        BocpdOutput {
            changepoint_probability,
            reset_probability: canonical_zero(reset_probability),
            expected_run_length: canonical_zero(expected_run_length),
            run_length_entropy: canonical_zero(run_length_entropy),
            posterior,
            discarded_tail_probability: self.discarded_tail_probability,
            observation_count: self.observation_count,
            reset_count: self.reset_count,
            last_observation_id: self.last_observation.map(|value| value.observation_id),
            last_event_time_ns: self.last_observation.map(|value| value.event_time_ns),
            last_as_known_at_ns: self.last_observation.map(|value| value.as_known_at_ns),
            feature_family: self.config.feature_family.clone(),
            quality: self.quality,
            model_id: self.model_id,
            evidence_id: self.evidence_id,
        }
    }
}

fn log_sum_exp(values: &[f64]) -> Result<f64, BocpdError> {
    let maximum = values
        .iter()
        .copied()
        .max_by(f64::total_cmp)
        .ok_or(BocpdError::NumericalFailure)?;
    if !maximum.is_finite() || values.iter().any(|value| !value.is_finite()) {
        return Err(BocpdError::NumericalFailure);
    }
    let scaled_sum = values
        .iter()
        .try_fold(0.0, |sum, value| checked(sum + (value - maximum).exp()))?;
    if scaled_sum <= 0.0 {
        return Err(BocpdError::ProbabilityUnderflow);
    }
    checked(maximum + checked(scaled_sum.ln())?)
}

fn log_add_exp(left: f64, right: f64) -> Result<f64, BocpdError> {
    log_sum_exp(&[left, right])
}

fn checked_probability(log_probability: f64) -> Result<f64, BocpdError> {
    if log_probability > 0.0 || !log_probability.is_finite() {
        return Err(BocpdError::NumericalFailure);
    }
    if log_probability < LOG_MINIMUM_POSITIVE {
        return Err(BocpdError::ProbabilityUnderflow);
    }
    let probability = checked(log_probability.exp())?;
    if !(0.0..=1.0).contains(&probability) {
        return Err(BocpdError::NumericalFailure);
    }
    Ok(canonical_zero(probability))
}

fn validate_normalized(log_posterior: &[f64]) -> Result<(), BocpdError> {
    let normalization = log_sum_exp(log_posterior)?;
    if normalization.abs() > 1e-12 {
        return Err(BocpdError::NumericalFailure);
    }
    Ok(())
}

fn valid_feature_family(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAXIMUM_FEATURE_FAMILY_BYTES
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':' | b'/')
        })
}

fn hash_model(config: &BocpdConfig) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(MODEL_HASH_DOMAIN);
    for value in [
        config.prior.mu(),
        config.prior.kappa(),
        config.prior.alpha(),
        config.prior.beta(),
        config.hazard_probability,
    ] {
        hasher.update(&value.to_bits().to_le_bytes());
    }
    hasher.update(&(config.max_run_length as u64).to_le_bytes());
    hasher.update(&(config.changepoint_window as u64).to_le_bytes());
    hasher.update(&[reset_policy_tag(config.reset_policy)]);
    hasher.update(OBSERVATION_FAMILY_VERSION);
    hasher.update(TRUNCATION_STRATEGY_VERSION);
    hasher.update(&(config.feature_family.len() as u64).to_le_bytes());
    hasher.update(config.feature_family.as_bytes());
    *hasher.finalize().as_bytes()
}

fn hash_initial_evidence(model_id: [u8; 32]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(INITIAL_EVIDENCE_HASH_DOMAIN);
    hasher.update(&model_id);
    *hasher.finalize().as_bytes()
}

fn hash_update_evidence(
    previous: [u8; 32],
    observation_count: u64,
    reset_count: u64,
    observation: BocpdObservation,
    discarded_tail_probability: f64,
    log_posterior: &[f64],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(UPDATE_EVIDENCE_HASH_DOMAIN);
    hasher.update(&previous);
    hasher.update(&observation_count.to_le_bytes());
    hasher.update(&reset_count.to_le_bytes());
    hasher.update(&observation.observation_id.to_le_bytes());
    hasher.update(&observation.event_time_ns.to_le_bytes());
    hasher.update(&observation.as_known_at_ns.to_le_bytes());
    hasher.update(&observation.value.to_bits().to_le_bytes());
    hasher.update(&[quality_tag(observation.quality)]);
    hasher.update(&observation.lineage_hash);
    hasher.update(&discarded_tail_probability.to_bits().to_le_bytes());
    hasher.update(&(log_posterior.len() as u64).to_le_bytes());
    for value in log_posterior {
        hasher.update(&value.to_bits().to_le_bytes());
    }
    *hasher.finalize().as_bytes()
}

fn hash_reset_evidence(previous: [u8; 32], reset_count: u64, model_id: [u8; 32]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(RESET_EVIDENCE_HASH_DOMAIN);
    hasher.update(&previous);
    hasher.update(&reset_count.to_le_bytes());
    hasher.update(&model_id);
    *hasher.finalize().as_bytes()
}

const fn reset_policy_tag(policy: ResetPolicy) -> u8 {
    match policy {
        ResetPolicy::Manual => 0,
    }
}

const fn quality_tag(quality: QualityState) -> u8 {
    match quality {
        QualityState::Trusted => 0,
        QualityState::Degraded => 1,
        QualityState::Unavailable => 2,
    }
}

fn checked(value: f64) -> Result<f64, BocpdError> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(BocpdError::NumericalFailure)
    }
}

const fn canonical_zero(value: f64) -> f64 {
    if value == 0.0 { 0.0 } else { value }
}
