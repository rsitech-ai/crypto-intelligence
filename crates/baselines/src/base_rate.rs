//! Point-in-time event-frequency baselines with exact Beta posterior intervals.
//!
//! Time-of-week buckets are derived from Unix UTC timestamps with Monday 00:00
//! as bucket zero. Combined queries fall back through regime, time-of-week, and
//! unconditional evidence in that order. Rolling queries fall back directly to
//! unconditional evidence. Every fallback is surfaced in the returned kind and
//! level rather than being presented as the requested finer-grained estimate.

use std::collections::{BTreeMap, BTreeSet};

use labels::{ExclusionReason, LabelOutcome};
use serde::Serialize;
use statrs::distribution::{Beta, ContinuousCDF};
use thiserror::Error;

const NANOS_PER_SECOND: u64 = 1_000_000_000;
const NANOS_PER_SECOND_I64: i64 = 1_000_000_000;
const SECONDS_PER_HOUR: i64 = 3_600;
const HOURS_PER_WEEK: i64 = 168;
const UNIX_EPOCH_WEEKDAY_OFFSET_HOURS: i64 = 3 * 24;
const MAXIMUM_OBSERVATIONS: usize = 1_000_000;
const MAXIMUM_HORIZON_SECONDS: u64 = 315_360_000;
const MAXIMUM_REGIMES: usize = 256;
const MAXIMUM_IDENTIFIER_BYTES: usize = 128;
const MODEL_HASH_DOMAIN: &[u8] = b"cmti:base-rate-model:v1\0";
const EVIDENCE_HASH_DOMAIN: &[u8] = b"cmti:base-rate-evidence:v1\0";
const FORECAST_HASH_DOMAIN: &[u8] = b"cmti:base-rate-forecast:v1\0";

/// Validated input for one outcome available to model fitting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservationInput {
    pub observation_id: u64,
    pub asset: String,
    pub horizon_seconds: u64,
    pub origin_time_ns: i64,
    pub outcome: LabelOutcome,
    pub outcome_known_at_ns: i64,
    pub regime: Option<String>,
    pub label_definition_hash: [u8; 32],
    pub source_range_hash: [u8; 32],
}

/// One immutable labeled outcome with point-in-time evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Observation {
    observation_id: u64,
    asset: String,
    horizon_seconds: u64,
    origin_time_ns: i64,
    outcome: LabelOutcome,
    outcome_known_at_ns: i64,
    regime: Option<String>,
    label_definition_hash: [u8; 32],
    source_range_hash: [u8; 32],
}

impl Observation {
    pub fn try_new(input: ObservationInput) -> Result<Self, BaselineError> {
        validate_observation(&input)?;
        Ok(Self {
            observation_id: input.observation_id,
            asset: input.asset,
            horizon_seconds: input.horizon_seconds,
            origin_time_ns: input.origin_time_ns,
            outcome: input.outcome,
            outcome_known_at_ns: input.outcome_known_at_ns,
            regime: input.regime,
            label_definition_hash: input.label_definition_hash,
            source_range_hash: input.source_range_hash,
        })
    }

    pub const fn observation_id(&self) -> u64 {
        self.observation_id
    }

    pub fn asset(&self) -> &str {
        &self.asset
    }

    pub const fn horizon_seconds(&self) -> u64 {
        self.horizon_seconds
    }

    pub const fn origin_time_ns(&self) -> i64 {
        self.origin_time_ns
    }

    pub const fn outcome(&self) -> LabelOutcome {
        self.outcome
    }

    pub const fn outcome_known_at_ns(&self) -> i64 {
        self.outcome_known_at_ns
    }

    pub fn regime(&self) -> Option<&str> {
        self.regime.as_deref()
    }

    pub const fn label_definition_hash(&self) -> [u8; 32] {
        self.label_definition_hash
    }

    pub const fn source_range_hash(&self) -> [u8; 32] {
        self.source_range_hash
    }

    pub const fn is_positive(&self) -> bool {
        matches!(self.outcome, LabelOutcome::Occurred { .. })
    }
}

/// Validated fitting and fallback policy for a base-rate model.
#[derive(Clone, Debug, PartialEq)]
pub struct BaseRateConfig {
    alpha: f64,
    beta: f64,
    credible_mass: f64,
    minimum_stratum_count: u64,
    rolling_window_seconds: u64,
    regime_ids: BTreeSet<String>,
}

impl BaseRateConfig {
    pub fn try_new<I, S>(
        alpha: f64,
        beta: f64,
        credible_mass: f64,
        minimum_stratum_count: u64,
        rolling_window_seconds: u64,
        regime_ids: I,
    ) -> Result<Self, BaselineError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        if !alpha.is_finite()
            || alpha <= 0.0
            || !beta.is_finite()
            || beta <= 0.0
            || !credible_mass.is_finite()
            || credible_mass <= 0.0
            || credible_mass >= 1.0
            || minimum_stratum_count == 0
            || usize::try_from(minimum_stratum_count)
                .ok()
                .is_none_or(|value| value > MAXIMUM_OBSERVATIONS)
            || rolling_window_seconds == 0
            || rolling_window_seconds > MAXIMUM_HORIZON_SECONDS
        {
            return Err(BaselineError::InvalidConfiguration);
        }
        let mut regimes = BTreeSet::new();
        for regime in regime_ids {
            let regime = regime.into();
            if !valid_identifier(&regime)
                || regimes.len() == MAXIMUM_REGIMES
                || !regimes.insert(regime)
            {
                return Err(BaselineError::InvalidConfiguration);
            }
        }
        Ok(Self {
            alpha,
            beta,
            credible_mass,
            minimum_stratum_count,
            rolling_window_seconds,
            regime_ids: regimes,
        })
    }

    pub const fn alpha(&self) -> f64 {
        self.alpha
    }

    pub const fn beta(&self) -> f64 {
        self.beta
    }

    pub const fn credible_mass(&self) -> f64 {
        self.credible_mass
    }

    pub const fn minimum_stratum_count(&self) -> u64 {
        self.minimum_stratum_count
    }

    pub const fn rolling_window_seconds(&self) -> u64 {
        self.rolling_window_seconds
    }

    pub fn regime_ids(&self) -> &BTreeSet<String> {
        &self.regime_ids
    }
}

/// Requested series and optional predeclared conditioning evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BaseRateQuery {
    asset: String,
    horizon_seconds: u64,
    origin_time_ns: Option<i64>,
    regime: Option<String>,
}

impl BaseRateQuery {
    pub fn try_new(asset: impl Into<String>, horizon_seconds: u64) -> Result<Self, BaselineError> {
        let asset = asset.into();
        if !valid_identifier(&asset)
            || horizon_seconds == 0
            || horizon_seconds > MAXIMUM_HORIZON_SECONDS
        {
            return Err(BaselineError::InvalidQuery);
        }
        Ok(Self {
            asset,
            horizon_seconds,
            origin_time_ns: None,
            regime: None,
        })
    }

    pub fn with_origin_time_ns(mut self, origin_time_ns: i64) -> Result<Self, BaselineError> {
        if origin_time_ns <= 0 {
            return Err(BaselineError::InvalidQuery);
        }
        self.origin_time_ns = Some(origin_time_ns);
        Ok(self)
    }

    pub fn with_regime(mut self, regime: impl Into<String>) -> Result<Self, BaselineError> {
        let regime = regime.into();
        if !valid_identifier(&regime) {
            return Err(BaselineError::InvalidQuery);
        }
        self.regime = Some(regime);
        Ok(self)
    }
}

/// Which historical slice supplied an estimate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EstimateKind {
    Unconditional,
    Rolling,
    TimeOfWeek,
    Regime,
    RegimeAndTimeOfWeek,
}

/// Pure numerical Beta-Binomial posterior summary.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct BetaBinomialEstimate {
    pub probability: f64,
    pub lower: f64,
    pub upper: f64,
    pub positive_count: u64,
    pub total_count: u64,
}

/// Auditable persisted base-rate forecast and its selected evidence.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct BaseRateEstimate {
    pub probability: f64,
    pub lower: f64,
    pub upper: f64,
    pub positive_count: u64,
    pub total_count: u64,
    pub censored_count: u64,
    pub excluded_count: u64,
    pub fallback_level: u8,
    pub minimum_count_met: bool,
    pub kind: EstimateKind,
    pub model_id: [u8; 32],
    pub evidence_hash: [u8; 32],
    pub forecast_id: [u8; 32],
}

/// Frozen historical baseline fitted from one bounded point-in-time training slice.
#[derive(Clone, Debug, PartialEq)]
pub struct BaseRateModel {
    config: BaseRateConfig,
    fit_cutoff_ns: i64,
    fit_rows: usize,
    model_id: [u8; 32],
    unconditional: BTreeMap<SeriesKey, Counts>,
    rolling: BTreeMap<SeriesKey, Counts>,
    seasonal: BTreeMap<SeasonalKey, Counts>,
    regime: BTreeMap<RegimeKey, Counts>,
    combined: BTreeMap<CombinedKey, Counts>,
}

impl BaseRateModel {
    pub fn fit(
        observations: &[Observation],
        fit_cutoff_ns: i64,
        config: BaseRateConfig,
    ) -> Result<Self, BaselineError> {
        if observations.is_empty()
            || observations.len() > MAXIMUM_OBSERVATIONS
            || fit_cutoff_ns <= 0
        {
            return Err(BaselineError::InvalidFit);
        }
        let rolling_window_ns = seconds_to_ns(config.rolling_window_seconds)?;
        let rolling_start_ns = fit_cutoff_ns
            .checked_sub(rolling_window_ns)
            .ok_or(BaselineError::TimeOverflow)?;
        let mut canonical = observations.iter().collect::<Vec<_>>();
        canonical.sort_by_key(|observation| observation.observation_id);
        if canonical
            .windows(2)
            .any(|pair| pair[0].observation_id == pair[1].observation_id)
        {
            let duplicate = canonical
                .windows(2)
                .find(|pair| pair[0].observation_id == pair[1].observation_id)
                .map(|pair| pair[0].observation_id)
                .ok_or(BaselineError::InvalidFit)?;
            return Err(BaselineError::DuplicateObservation {
                observation_id: duplicate,
            });
        }

        let mut unconditional = BTreeMap::new();
        let mut rolling = BTreeMap::new();
        let mut seasonal = BTreeMap::new();
        let mut regime = BTreeMap::new();
        let mut combined = BTreeMap::new();
        for observation in &canonical {
            if observation.outcome_known_at_ns > fit_cutoff_ns {
                return Err(BaselineError::OutcomeKnownAfterFitCutoff {
                    observation_id: observation.observation_id,
                });
            }
            if observation.origin_time_ns > fit_cutoff_ns {
                return Err(BaselineError::OriginAfterFitCutoff {
                    observation_id: observation.observation_id,
                });
            }
            if observation
                .regime
                .as_ref()
                .is_some_and(|value| !config.regime_ids.contains(value))
            {
                return Err(BaselineError::UndeclaredRegime {
                    observation_id: observation.observation_id,
                });
            }
            let series = SeriesKey {
                asset: observation.asset.clone(),
                horizon_seconds: observation.horizon_seconds,
            };
            add_count(&mut unconditional, series.clone(), observation.outcome)?;
            if observation.origin_time_ns >= rolling_start_ns {
                add_count(&mut rolling, series.clone(), observation.outcome)?;
            }
            let time_of_week = hour_of_week(observation.origin_time_ns)?;
            add_count(
                &mut seasonal,
                SeasonalKey {
                    series: series.clone(),
                    hour_of_week: time_of_week,
                },
                observation.outcome,
            )?;
            if let Some(regime_id) = &observation.regime {
                add_count(
                    &mut regime,
                    RegimeKey {
                        series: series.clone(),
                        regime: regime_id.clone(),
                    },
                    observation.outcome,
                )?;
                add_count(
                    &mut combined,
                    CombinedKey {
                        series,
                        hour_of_week: time_of_week,
                        regime: regime_id.clone(),
                    },
                    observation.outcome,
                )?;
            }
        }
        let model_id = hash_model(&canonical, fit_cutoff_ns, &config)?;
        Ok(Self {
            config,
            fit_cutoff_ns,
            fit_rows: canonical.len(),
            model_id,
            unconditional,
            rolling,
            seasonal,
            regime,
            combined,
        })
    }

    pub const fn fit_cutoff_ns(&self) -> i64 {
        self.fit_cutoff_ns
    }

    pub const fn fit_rows(&self) -> usize {
        self.fit_rows
    }

    pub const fn model_id(&self) -> [u8; 32] {
        self.model_id
    }

    pub const fn config(&self) -> &BaseRateConfig {
        &self.config
    }

    pub fn probability(&self, asset: &str, horizon_seconds: u64) -> Result<f64, BaselineError> {
        Ok(self
            .estimate(&BaseRateQuery::try_new(asset, horizon_seconds)?)?
            .probability)
    }

    pub fn estimate(&self, query: &BaseRateQuery) -> Result<BaseRateEstimate, BaselineError> {
        if query
            .regime
            .as_ref()
            .is_some_and(|value| !self.config.regime_ids.contains(value))
        {
            return Err(BaselineError::InvalidQuery);
        }
        let series = SeriesKey {
            asset: query.asset.clone(),
            horizon_seconds: query.horizon_seconds,
        };
        let unconditional = self
            .unconditional
            .get(&series)
            .ok_or(BaselineError::UnknownSeries)?;
        let hour_of_week = query.origin_time_ns.map(hour_of_week).transpose()?;
        let mut fallback_level = 0_u8;

        if let (Some(regime_id), Some(hour)) = (&query.regime, hour_of_week) {
            let key = CombinedKey {
                series: series.clone(),
                hour_of_week: hour,
                regime: regime_id.clone(),
            };
            if let Some(counts) = self
                .combined
                .get(&key)
                .filter(|counts| counts.total() >= self.config.minimum_stratum_count)
            {
                return self.make_estimate(
                    query,
                    *counts,
                    EstimateKind::RegimeAndTimeOfWeek,
                    fallback_level,
                );
            }
            fallback_level = fallback_level.saturating_add(1);
        }
        if let Some(regime_id) = &query.regime {
            let key = RegimeKey {
                series: series.clone(),
                regime: regime_id.clone(),
            };
            if let Some(counts) = self
                .regime
                .get(&key)
                .filter(|counts| counts.total() >= self.config.minimum_stratum_count)
            {
                return self.make_estimate(query, *counts, EstimateKind::Regime, fallback_level);
            }
            fallback_level = fallback_level.saturating_add(1);
        }
        if let Some(hour) = hour_of_week {
            let key = SeasonalKey {
                series,
                hour_of_week: hour,
            };
            if let Some(counts) = self
                .seasonal
                .get(&key)
                .filter(|counts| counts.total() >= self.config.minimum_stratum_count)
            {
                return self.make_estimate(
                    query,
                    *counts,
                    EstimateKind::TimeOfWeek,
                    fallback_level,
                );
            }
            fallback_level = fallback_level.saturating_add(1);
        }
        self.make_estimate(
            query,
            *unconditional,
            EstimateKind::Unconditional,
            fallback_level,
        )
    }

    pub fn rolling_estimate(
        &self,
        asset: &str,
        horizon_seconds: u64,
    ) -> Result<BaseRateEstimate, BaselineError> {
        let query = BaseRateQuery::try_new(asset, horizon_seconds)?;
        let series = SeriesKey {
            asset: query.asset.clone(),
            horizon_seconds,
        };
        let unconditional = self
            .unconditional
            .get(&series)
            .ok_or(BaselineError::UnknownSeries)?;
        if let Some(counts) = self
            .rolling
            .get(&series)
            .filter(|counts| counts.total() >= self.config.minimum_stratum_count)
        {
            self.make_estimate(&query, *counts, EstimateKind::Rolling, 0)
        } else {
            self.make_estimate(&query, *unconditional, EstimateKind::Unconditional, 1)
        }
    }

    fn make_estimate(
        &self,
        query: &BaseRateQuery,
        counts: Counts,
        kind: EstimateKind,
        fallback_level: u8,
    ) -> Result<BaseRateEstimate, BaselineError> {
        let posterior = beta_binomial_estimate(
            counts.positive,
            counts.total(),
            self.config.alpha,
            self.config.beta,
            self.config.credible_mass,
        )?;
        let evidence_hash = hash_evidence(self.model_id, query, counts, kind, fallback_level)?;
        let forecast_id = hash_forecast(
            self.model_id,
            evidence_hash,
            kind,
            fallback_level,
            posterior,
        )?;
        Ok(BaseRateEstimate {
            probability: posterior.probability,
            lower: posterior.lower,
            upper: posterior.upper,
            positive_count: posterior.positive_count,
            total_count: posterior.total_count,
            censored_count: counts.censored,
            excluded_count: counts.excluded,
            fallback_level,
            minimum_count_met: posterior.total_count >= self.config.minimum_stratum_count,
            kind,
            model_id: self.model_id,
            evidence_hash,
            forecast_id,
        })
    }
}

/// Computes the posterior mean and equal-tailed interval for a Beta-Binomial model.
pub fn beta_binomial_estimate(
    positive: u64,
    total: u64,
    alpha: f64,
    beta: f64,
    credible_mass: f64,
) -> Result<BetaBinomialEstimate, BaselineError> {
    if positive > total
        || total > MAXIMUM_OBSERVATIONS as u64
        || !alpha.is_finite()
        || alpha <= 0.0
        || !beta.is_finite()
        || beta <= 0.0
        || !credible_mass.is_finite()
        || credible_mass <= 0.0
        || credible_mass >= 1.0
    {
        return Err(BaselineError::InvalidInput);
    }
    let posterior_alpha = positive as f64 + alpha;
    let posterior_beta = (total - positive) as f64 + beta;
    let denominator = posterior_alpha + posterior_beta;
    if !posterior_alpha.is_finite()
        || !posterior_beta.is_finite()
        || !denominator.is_finite()
        || denominator <= 0.0
    {
        return Err(BaselineError::NumericalFailure);
    }
    let distribution =
        Beta::new(posterior_alpha, posterior_beta).map_err(|_| BaselineError::NumericalFailure)?;
    let tail = (1.0 - credible_mass) / 2.0;
    let probability = posterior_alpha / denominator;
    let lower = distribution.inverse_cdf(tail);
    let upper = distribution.inverse_cdf(1.0 - tail);
    if !probability.is_finite()
        || !lower.is_finite()
        || !upper.is_finite()
        || !(0.0..=1.0).contains(&lower)
        || !(0.0..=1.0).contains(&upper)
        || lower > probability
        || probability > upper
    {
        return Err(BaselineError::NumericalFailure);
    }
    Ok(BetaBinomialEstimate {
        probability,
        lower,
        upper,
        positive_count: positive,
        total_count: total,
    })
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct SeriesKey {
    asset: String,
    horizon_seconds: u64,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct SeasonalKey {
    series: SeriesKey,
    hour_of_week: u16,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct RegimeKey {
    series: SeriesKey,
    regime: String,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CombinedKey {
    series: SeriesKey,
    hour_of_week: u16,
    regime: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
struct Counts {
    positive: u64,
    negative: u64,
    censored: u64,
    excluded: u64,
}

impl Counts {
    const fn total(self) -> u64 {
        self.positive + self.negative
    }

    fn add(&mut self, outcome: LabelOutcome) -> Result<(), BaselineError> {
        let target = match outcome {
            LabelOutcome::Occurred { .. } => &mut self.positive,
            LabelOutcome::NotOccurred => &mut self.negative,
            LabelOutcome::Censored { .. } => &mut self.censored,
            LabelOutcome::Excluded(_) => &mut self.excluded,
        };
        *target = target.checked_add(1).ok_or(BaselineError::CountOverflow)?;
        Ok(())
    }
}

fn add_count<K: Ord>(
    counts: &mut BTreeMap<K, Counts>,
    key: K,
    outcome: LabelOutcome,
) -> Result<(), BaselineError> {
    counts.entry(key).or_default().add(outcome)
}

fn validate_observation(input: &ObservationInput) -> Result<(), BaselineError> {
    if input.observation_id == 0
        || !valid_identifier(&input.asset)
        || input.horizon_seconds == 0
        || input.horizon_seconds > MAXIMUM_HORIZON_SECONDS
        || input.origin_time_ns <= 0
        || input.outcome_known_at_ns < input.origin_time_ns
        || input
            .regime
            .as_ref()
            .is_some_and(|value| !valid_identifier(value))
        || input.label_definition_hash == [0; 32]
        || input.source_range_hash == [0; 32]
    {
        return Err(BaselineError::InvalidObservation);
    }
    let horizon_ns = seconds_to_ns(input.horizon_seconds)?;
    let horizon_end_ns = input
        .origin_time_ns
        .checked_add(horizon_ns)
        .ok_or(BaselineError::TimeOverflow)?;
    let minimum_known_at_ns = match input.outcome {
        LabelOutcome::Occurred { offset_seconds } => {
            if offset_seconds == 0 || offset_seconds > input.horizon_seconds {
                return Err(BaselineError::InvalidObservation);
            }
            input
                .origin_time_ns
                .checked_add(seconds_to_ns(offset_seconds)?)
                .ok_or(BaselineError::TimeOverflow)?
        }
        LabelOutcome::NotOccurred => horizon_end_ns,
        LabelOutcome::Censored { observed_seconds } => {
            if observed_seconds >= input.horizon_seconds {
                return Err(BaselineError::InvalidObservation);
            }
            input
                .origin_time_ns
                .checked_add(seconds_to_ns(observed_seconds)?)
                .ok_or(BaselineError::TimeOverflow)?
        }
        LabelOutcome::Excluded(_) => input.origin_time_ns,
    };
    if input.outcome_known_at_ns < minimum_known_at_ns {
        return Err(BaselineError::InvalidObservation);
    }
    Ok(())
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAXIMUM_IDENTIFIER_BYTES
        && value
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn seconds_to_ns(seconds: u64) -> Result<i64, BaselineError> {
    seconds
        .checked_mul(NANOS_PER_SECOND)
        .and_then(|value| i64::try_from(value).ok())
        .ok_or(BaselineError::TimeOverflow)
}

fn hour_of_week(origin_time_ns: i64) -> Result<u16, BaselineError> {
    if origin_time_ns <= 0 {
        return Err(BaselineError::InvalidQuery);
    }
    let seconds = origin_time_ns / NANOS_PER_SECOND_I64;
    let hours = seconds / SECONDS_PER_HOUR;
    u16::try_from((hours + UNIX_EPOCH_WEEKDAY_OFFSET_HOURS) % HOURS_PER_WEEK)
        .map_err(|_| BaselineError::TimeOverflow)
}

#[derive(Serialize)]
struct ModelHashWire<'a> {
    schema_version: u32,
    fit_cutoff_ns: i64,
    alpha_bits: u64,
    beta_bits: u64,
    credible_mass_bits: u64,
    minimum_stratum_count: u64,
    rolling_window_seconds: u64,
    regime_ids: Vec<&'a str>,
    observations: Vec<ObservationHashWire<'a>>,
}

#[derive(Serialize)]
struct ObservationHashWire<'a> {
    observation_id: u64,
    asset: &'a str,
    horizon_seconds: u64,
    origin_time_ns: i64,
    outcome_kind: &'static str,
    outcome_value: u64,
    exclusion_reason: Option<&'static str>,
    outcome_known_at_ns: i64,
    regime: Option<&'a str>,
    label_definition_hash: [u8; 32],
    source_range_hash: [u8; 32],
}

fn hash_model(
    observations: &[&Observation],
    fit_cutoff_ns: i64,
    config: &BaseRateConfig,
) -> Result<[u8; 32], BaselineError> {
    let wire = ModelHashWire {
        schema_version: 1,
        fit_cutoff_ns,
        alpha_bits: config.alpha.to_bits(),
        beta_bits: config.beta.to_bits(),
        credible_mass_bits: config.credible_mass.to_bits(),
        minimum_stratum_count: config.minimum_stratum_count,
        rolling_window_seconds: config.rolling_window_seconds,
        regime_ids: config.regime_ids.iter().map(String::as_str).collect(),
        observations: observations
            .iter()
            .map(|observation| observation_hash_wire(observation))
            .collect(),
    };
    hash_wire(MODEL_HASH_DOMAIN, &wire)
}

fn observation_hash_wire(observation: &Observation) -> ObservationHashWire<'_> {
    let (outcome_kind, outcome_value, exclusion_reason) = match observation.outcome {
        LabelOutcome::Occurred { offset_seconds } => ("occurred", offset_seconds, None),
        LabelOutcome::NotOccurred => ("not_occurred", 0, None),
        LabelOutcome::Censored { observed_seconds } => ("censored", observed_seconds, None),
        LabelOutcome::Excluded(reason) => ("excluded", 0, Some(exclusion_reason(reason))),
    };
    ObservationHashWire {
        observation_id: observation.observation_id,
        asset: &observation.asset,
        horizon_seconds: observation.horizon_seconds,
        origin_time_ns: observation.origin_time_ns,
        outcome_kind,
        outcome_value,
        exclusion_reason,
        outcome_known_at_ns: observation.outcome_known_at_ns,
        regime: observation.regime.as_deref(),
        label_definition_hash: observation.label_definition_hash,
        source_range_hash: observation.source_range_hash,
    }
}

#[derive(Serialize)]
struct EvidenceHashWire<'a> {
    schema_version: u32,
    model_id: [u8; 32],
    asset: &'a str,
    horizon_seconds: u64,
    origin_time_ns: Option<i64>,
    regime: Option<&'a str>,
    counts: Counts,
    kind: EstimateKind,
    fallback_level: u8,
}

fn hash_evidence(
    model_id: [u8; 32],
    query: &BaseRateQuery,
    counts: Counts,
    kind: EstimateKind,
    fallback_level: u8,
) -> Result<[u8; 32], BaselineError> {
    hash_wire(
        EVIDENCE_HASH_DOMAIN,
        &EvidenceHashWire {
            schema_version: 1,
            model_id,
            asset: &query.asset,
            horizon_seconds: query.horizon_seconds,
            origin_time_ns: query.origin_time_ns,
            regime: query.regime.as_deref(),
            counts,
            kind,
            fallback_level,
        },
    )
}

#[derive(Serialize)]
struct ForecastHashWire {
    schema_version: u32,
    model_id: [u8; 32],
    evidence_hash: [u8; 32],
    kind: EstimateKind,
    fallback_level: u8,
    probability_bits: u64,
    lower_bits: u64,
    upper_bits: u64,
    positive_count: u64,
    total_count: u64,
}

fn hash_forecast(
    model_id: [u8; 32],
    evidence_hash: [u8; 32],
    kind: EstimateKind,
    fallback_level: u8,
    estimate: BetaBinomialEstimate,
) -> Result<[u8; 32], BaselineError> {
    hash_wire(
        FORECAST_HASH_DOMAIN,
        &ForecastHashWire {
            schema_version: 1,
            model_id,
            evidence_hash,
            kind,
            fallback_level,
            probability_bits: estimate.probability.to_bits(),
            lower_bits: estimate.lower.to_bits(),
            upper_bits: estimate.upper.to_bits(),
            positive_count: estimate.positive_count,
            total_count: estimate.total_count,
        },
    )
}

fn hash_wire(domain: &[u8], value: &impl Serialize) -> Result<[u8; 32], BaselineError> {
    let bytes = serde_json::to_vec(value).map_err(|_| BaselineError::Serialization)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain);
    hasher.update(&bytes);
    Ok(*hasher.finalize().as_bytes())
}

const fn exclusion_reason(reason: ExclusionReason) -> &'static str {
    match reason {
        ExclusionReason::UnhealthyOrigin => "unhealthy_origin",
        ExclusionReason::LowQualityOrigin => "low_quality_origin",
        ExclusionReason::MissingEvidence => "missing_evidence",
        ExclusionReason::InvalidVolatilityScale => "invalid_volatility_scale",
        ExclusionReason::InsufficientCorroboration => "insufficient_corroboration",
        ExclusionReason::InsufficientConfidence => "insufficient_confidence",
        ExclusionReason::HaltedOrDelisted => "halted_or_delisted",
        ExclusionReason::InstrumentDefinitionChanged => "instrument_definition_changed",
        ExclusionReason::UnresolvedCorrection => "unresolved_correction",
        ExclusionReason::TimestampIntegrityCompromised => "timestamp_integrity_compromised",
        ExclusionReason::SimultaneousCompetingEvents => "simultaneous_competing_events",
    }
}

/// Fail-closed base-rate fitting and estimation error.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum BaselineError {
    #[error("base-rate input is invalid")]
    InvalidInput,
    #[error("base-rate configuration is invalid")]
    InvalidConfiguration,
    #[error("base-rate observation is invalid")]
    InvalidObservation,
    #[error("base-rate fit input is empty, oversized, or has an invalid cutoff")]
    InvalidFit,
    #[error("duplicate observation identity {observation_id}")]
    DuplicateObservation { observation_id: u64 },
    #[error("observation {observation_id} origin is after the fit cutoff")]
    OriginAfterFitCutoff { observation_id: u64 },
    #[error("observation {observation_id} outcome was known after the fit cutoff")]
    OutcomeKnownAfterFitCutoff { observation_id: u64 },
    #[error("observation {observation_id} uses an undeclared regime")]
    UndeclaredRegime { observation_id: u64 },
    #[error("base-rate query is invalid or uses an undeclared stratum")]
    InvalidQuery,
    #[error("requested asset and horizon were not fitted")]
    UnknownSeries,
    #[error("base-rate count overflowed")]
    CountOverflow,
    #[error("base-rate time arithmetic overflowed")]
    TimeOverflow,
    #[error("base-rate posterior calculation failed")]
    NumericalFailure,
    #[error("base-rate identity serialization failed")]
    Serialization,
}
