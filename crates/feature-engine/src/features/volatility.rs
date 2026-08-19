//! Point-in-time adapters from exact prices into bounded volatility measures.

use blake3::Hasher;
use feature_registry::{
    DurationNanos, EntityScope, EventTimePolicy, FeatureDefinition, FeatureDefinitionInput,
    FeatureDocumentation, FeatureEntity, FeatureId, FeatureObservation, FeatureStatus,
    FeatureValue, FeatureValueType, FinalityState, FormulaHash, InputRequirement,
    MissingnessPolicy, NormalizationKind, NormalizationPolicy, QualityRequirement, QualityScore,
    RegistryError, WindowDefinition, WindowId, WindowKind,
};
use fixed_decimal::Price;
use quality::SourceHealthState;
use semver::Version;
use volatility::{MeasureError, OhlcBar, SeasonalBaseline};

use super::Task4FeatureKind;
use super::{
    FeatureComputationError, PriceWindow, SecondaryEvidence, consecutive_log_returns,
    finite_analytical,
};
use crate::HalfLifeEwmaState;

pub const REALIZED_VOLATILITY_V2_SAMPLING_NANOS: u64 = 60_000_000_000;
const FIVE_MINUTES_NANOS: u64 = 300_000_000_000;
const FIFTEEN_MINUTES_NANOS: u64 = 900_000_000_000;
const ONE_HOUR_NANOS: u64 = 3_600_000_000_000;
const UNIX_EPOCH_MONDAY_OFFSET_HOURS: i64 = 72;
const FIVE_SECONDS_NANOS: u64 = 5_000_000_000;
const ONE_DAY_NANOS: u64 = 86_400_000_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VolatilityFormulaPolicy {
    sampling_interval_nanos: u64,
    annualization_periods: Option<u32>,
}

pub fn realized_volatility_v2_policy() -> VolatilityFormulaPolicy {
    VolatilityFormulaPolicy::try_new(REALIZED_VOLATILITY_V2_SAMPLING_NANOS, None)
        .expect("frozen realized-volatility v2 policy is valid")
}

pub fn realized_volatility_v2_definition() -> Result<FeatureDefinition, RegistryError> {
    let version = Version::new(2, 0, 0);
    FeatureDefinition::try_new(FeatureDefinitionInput {
        id: FeatureId::new("realized_volatility")?,
        version: version.clone(),
        status: FeatureStatus::Required,
        value_type: FeatureValueType::Float64,
        entities: EntityScope::Asset,
        required_inputs: vec![InputRequirement::new("consolidated.fair_price")?],
        event_time_policy: EventTimePolicy::SourceEventTime,
        windows: [
            ("rolling_5m", FIVE_MINUTES_NANOS),
            ("rolling_15m", FIFTEEN_MINUTES_NANOS),
            ("rolling_1h", ONE_HOUR_NANOS),
        ]
        .into_iter()
        .map(|(id, extent)| {
            WindowDefinition::try_new_time(
                WindowId::new(id)?,
                WindowKind::Sliding,
                DurationNanos::new(extent),
                Some(DurationNanos::new(REALIZED_VOLATILITY_V2_SAMPLING_NANOS)),
            )
        })
        .collect::<Result<Vec<_>, _>>()?,
        output_resolution: DurationNanos::new(REALIZED_VOLATILITY_V2_SAMPLING_NANOS),
        allowed_lateness: DurationNanos::new(FIVE_SECONDS_NANOS),
        time_to_live: DurationNanos::new(ONE_DAY_NANOS),
        normalization: NormalizationPolicy::try_new(NormalizationKind::None, version)?,
        missingness: MissingnessPolicy::Explicit,
        quality_gate: QualityRequirement::try_new(
            QualityScore::from_millionths(900_000)?,
            QualityScore::from_millionths(1_000_000)?,
            vec![SourceHealthState::Healthy, SourceHealthState::Recovering],
        )?,
        formula_hash: realized_volatility_v2_policy().formula_hash(),
        documentation: FeatureDocumentation::new(
            "docs/data-dictionary/features.md#realized-volatility",
        )?,
    })
}

impl VolatilityFormulaPolicy {
    pub fn try_new(
        sampling_interval_nanos: u64,
        annualization_periods: Option<u32>,
    ) -> Result<Self, FeatureComputationError> {
        if sampling_interval_nanos == 0 || annualization_periods == Some(0) {
            return Err(FeatureComputationError::InvalidParameter);
        }
        Ok(Self {
            sampling_interval_nanos,
            annualization_periods,
        })
    }

    pub const fn sampling_interval_nanos(self) -> u64 {
        self.sampling_interval_nanos
    }

    pub const fn annualization_periods(self) -> Option<u32> {
        self.annualization_periods
    }

    pub fn formula_hash(self) -> FormulaHash {
        let mut hasher = Hasher::new();
        hasher.update(b"crypto-intelligence/realized-volatility/v2");
        hasher.update(b"return=consecutive-log");
        hasher.update(b"rv=sum-squared-returns");
        hasher.update(b"vol=sqrt-rv-times-annualization-periods");
        hasher.update(b"bv=pi-over-2-adjacent-absolute-products");
        hasher.update(b"jump=max-rv-minus-bv-zero");
        hasher.update(b"output=float64-finite-canonical-zero");
        hasher.update(&self.sampling_interval_nanos.to_be_bytes());
        match self.annualization_periods {
            Some(periods) => {
                hasher.update(&[1]);
                hasher.update(&periods.to_be_bytes());
            }
            None => {
                hasher.update(&[0]);
            }
        }
        hasher.update(&(super::MAX_PRICE_OBSERVATIONS as u64).to_be_bytes());
        hasher.update(&(volatility::MAX_MEASURE_OBSERVATIONS as u64).to_be_bytes());
        FormulaHash::new(*hasher.finalize().as_bytes())
            .expect("domain-separated formula digest is nonzero")
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RealizedMeasures {
    realized_variance: f64,
    realized_volatility: f64,
    downside_semivariance: f64,
    upside_semivariance: f64,
    bipower_variation: f64,
    jump_variation: f64,
}

impl RealizedMeasures {
    pub fn from_price_window(
        window: &PriceWindow,
        closing: &super::PriceObservation,
        policy: VolatilityFormulaPolicy,
    ) -> Result<Self, FeatureComputationError> {
        validate_sampling(window, closing, policy)?;
        let mut returns = consecutive_log_returns(window)?;
        let last = window
            .observations()
            .last()
            .ok_or(FeatureComputationError::InsufficientHistory)?;
        returns.push(super::log_return(closing.price(), last.price())?);
        let realized_variance =
            volatility::realized_variance(&returns).map_err(map_measure_error)?;
        let annualization = f64::from(policy.annualization_periods.unwrap_or(1));
        let realized_volatility = finite_analytical((realized_variance * annualization).sqrt())?;
        let downside_semivariance =
            volatility::downside_semivariance(&returns).map_err(map_measure_error)?;
        let upside_semivariance =
            volatility::upside_semivariance(&returns).map_err(map_measure_error)?;
        let bipower_variation =
            volatility::bipower_variation(&returns).map_err(map_measure_error)?;
        let jump_variation = volatility::jump_variation(&returns).map_err(map_measure_error)?;
        Ok(Self {
            realized_variance,
            realized_volatility,
            downside_semivariance,
            upside_semivariance,
            bipower_variation,
            jump_variation,
        })
    }

    pub const fn realized_variance(self) -> f64 {
        self.realized_variance
    }

    pub const fn realized_volatility(self) -> f64 {
        self.realized_volatility
    }

    pub const fn downside_semivariance(self) -> f64 {
        self.downside_semivariance
    }

    pub const fn upside_semivariance(self) -> f64 {
        self.upside_semivariance
    }

    pub const fn bipower_variation(self) -> f64 {
        self.bipower_variation
    }

    pub const fn jump_variation(self) -> f64 {
        self.jump_variation
    }
}

pub fn compute_realized_volatility_v2(
    window: &PriceWindow,
    closing: &super::PriceObservation,
    tracker: &crate::WatermarkTracker,
) -> Result<super::ComputedAnalyticalFeature, FeatureComputationError> {
    let result =
        RealizedMeasures::from_price_window(window, closing, realized_volatility_v2_policy())
            .map(|measures| measures.realized_volatility());
    super::ComputedAnalyticalFeature::from_price_window_analytical(
        Task4FeatureKind::RealizedVolatility,
        window,
        Some(closing),
        tracker,
        super::AnalyticalFeature::from_result(result)?,
    )
}

/// Computes one registry-bound realized-measure recipe from the same complete
/// consolidated-price grid and finalization evidence.
pub fn compute_realized_measure_feature(
    kind: Task4FeatureKind,
    window: &PriceWindow,
    closing: &super::PriceObservation,
    tracker: &crate::WatermarkTracker,
) -> Result<super::ComputedAnalyticalFeature, FeatureComputationError> {
    if kind == Task4FeatureKind::RealizedVolatility {
        return compute_realized_volatility_v2(window, closing, tracker);
    }
    let result =
        RealizedMeasures::from_price_window(window, closing, realized_volatility_v2_policy())
            .and_then(|measures| match kind {
                Task4FeatureKind::RealizedVariance => Ok(measures.realized_variance()),
                Task4FeatureKind::UpsideSemivariance => Ok(measures.upside_semivariance()),
                Task4FeatureKind::DownsideSemivariance => Ok(measures.downside_semivariance()),
                Task4FeatureKind::BipowerVariation => Ok(measures.bipower_variation()),
                Task4FeatureKind::JumpVariation => Ok(measures.jump_variation()),
                _ => Err(FeatureComputationError::InvalidParameter),
            });
    super::ComputedAnalyticalFeature::from_price_window_analytical(
        kind,
        window,
        Some(closing),
        tracker,
        super::AnalyticalFeature::from_result(result)?,
    )
}

/// Computes covariance or correlation from two aligned, finalized
/// consolidated-price grids.
///
/// Both assets must use the frozen realized-volatility sampling policy. The
/// emitted entity and lineage are canonically ordered by asset identity, so
/// swapping the caller's left and right inputs produces the same observation.
pub fn compute_realized_pair_feature(
    kind: Task4FeatureKind,
    left: (
        &PriceWindow,
        &super::PriceObservation,
        &crate::WatermarkTracker,
    ),
    right: (
        &PriceWindow,
        &super::PriceObservation,
        &crate::WatermarkTracker,
    ),
) -> Result<super::ComputedAnalyticalFeature, FeatureComputationError> {
    let result = (|| {
        validate_sampling(left.0, left.1, realized_volatility_v2_policy())?;
        validate_sampling(right.0, right.1, realized_volatility_v2_policy())?;
        if left.0.time_window() != right.0.time_window() {
            return Err(FeatureComputationError::LengthMismatch);
        }
        let left_returns = complete_log_returns(left.0, left.1)?;
        let right_returns = complete_log_returns(right.0, right.1)?;
        match kind {
            Task4FeatureKind::RealizedCovariance => {
                volatility::realized_covariance(&left_returns, &right_returns)
            }
            Task4FeatureKind::RealizedCorrelation => {
                volatility::realized_correlation(&left_returns, &right_returns)
            }
            _ => return Err(FeatureComputationError::InvalidParameter),
        }
        .map_err(map_measure_error)
    })();
    super::ComputedAnalyticalFeature::from_paired_price_windows_recipe(
        kind,
        left,
        right,
        super::AnalyticalFeature::from_result(result)?,
    )
}

/// Computes the frozen five-minute half-life EWMA from the same finalized
/// one-minute return grid used by the realized measures.
pub fn compute_exponentially_weighted_volatility(
    window: &PriceWindow,
    closing: &super::PriceObservation,
    tracker: &crate::WatermarkTracker,
) -> Result<super::ComputedAnalyticalFeature, FeatureComputationError> {
    let result = (|| {
        validate_sampling(window, closing, realized_volatility_v2_policy())?;
        let returns = complete_log_returns(window, closing)?;
        let definition = Task4FeatureKind::ExponentiallyWeightedVolatility
            .definition()
            .map_err(|_| FeatureComputationError::InvalidParameter)?;
        let window_definition = definition
            .windows()
            .first()
            .ok_or(FeatureComputationError::InvalidParameter)?;
        let mut state =
            HalfLifeEwmaState::try_from_definition(window_definition, returns.len(), tracker)
                .map_err(map_window_error)?;
        let return_times = window
            .observations()
            .iter()
            .skip(1)
            .map(super::PriceObservation::event_time)
            .chain(std::iter::once(closing.event_time()));
        for (event_time, value) in return_times.zip(returns) {
            state
                .update_at(event_time, finite_analytical(value * value)?)
                .map_err(map_window_error)?;
        }
        ewma_volatility_from_state(&state)
    })();
    super::ComputedAnalyticalFeature::from_price_window_analytical(
        Task4FeatureKind::ExponentiallyWeightedVolatility,
        window,
        Some(closing),
        tracker,
        super::AnalyticalFeature::from_result(result)?,
    )
}

/// A finalized point-in-time series of emitted realized-volatility features.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FinalizedVolatilitySeries {
    values: Vec<feature_registry::FiniteF64>,
    evidence: super::DerivedFeatureEvidence,
}

impl FinalizedVolatilitySeries {
    pub fn try_new(
        time_window: crate::TimeWindow,
        observations: Vec<FeatureObservation>,
    ) -> Result<Self, FeatureComputationError> {
        if observations.len() < 2 {
            return Err(FeatureComputationError::InsufficientHistory);
        }
        if observations.len() > volatility::MAX_MEASURE_OBSERVATIONS {
            return Err(FeatureComputationError::CapacityExceeded);
        }
        let extent = time_window
            .end()
            .value()
            .checked_sub(time_window.start().value())
            .ok_or(FeatureComputationError::InvalidInput)?;
        let cadence = i64::try_from(REALIZED_VOLATILITY_V2_SAMPLING_NANOS)
            .map_err(|_| FeatureComputationError::InvalidParameter)?;
        if extent % cadence != 0
            || usize::try_from(extent / cadence).ok() != Some(observations.len())
        {
            return Err(FeatureComputationError::SequenceGap);
        }
        if observations.windows(2).any(|pair| {
            pair[0].event_time_end() >= pair[1].event_time_end()
                || pair[0].watermark() > pair[1].watermark()
        }) {
            return Err(FeatureComputationError::NonMonotonicTime);
        }
        for (index, observation) in observations.iter().enumerate() {
            let step =
                i64::try_from(index + 1).map_err(|_| FeatureComputationError::CapacityExceeded)?;
            let expected_end = time_window
                .start()
                .value()
                .checked_add(
                    step.checked_mul(cadence)
                        .ok_or(FeatureComputationError::InvalidInput)?,
                )
                .ok_or(FeatureComputationError::InvalidInput)?;
            if observation.event_time_end().value() != expected_end {
                return Err(FeatureComputationError::SequenceGap);
            }
        }
        let (asset, _) = realized_volatility_observation(&observations[0])?;
        let first_coverage = observations[0].source_coverage().clone();
        let mut values = Vec::with_capacity(observations.len());
        let mut quality_score = observations[0].quality_score();
        let mut as_of = observations[0].as_known_at();
        let mut finality_as_known_at = observations[0].finality_as_known_at();
        let mut hasher = Hasher::new();
        hasher.update(b"crypto-intelligence/finalized-volatility-series/v1");
        hasher.update(&time_window.start().value().to_be_bytes());
        hasher.update(&time_window.end().value().to_be_bytes());
        for observation in &observations {
            let (observation_asset, value) = realized_volatility_observation(observation)?;
            if observation_asset != asset || observation.source_coverage() != &first_coverage {
                return Err(FeatureComputationError::EligibleUniverseChanged);
            }
            values.push(value);
            quality_score = quality_score.min(observation.quality_score());
            as_of = as_of.max(observation.as_known_at());
            finality_as_known_at = finality_as_known_at.max(observation.finality_as_known_at());
            hasher.update(&observation.lineage_hash().bytes());
            hasher.update(&observation.event_time_start().value().to_be_bytes());
            hasher.update(&observation.event_time_end().value().to_be_bytes());
        }
        let watermark = observations
            .last()
            .and_then(FeatureObservation::watermark)
            .ok_or(FeatureComputationError::WindowNotFinal)?;
        Ok(Self {
            values,
            evidence: super::DerivedFeatureEvidence::new(
                time_window,
                FeatureEntity::Asset(asset),
                as_of,
                *hasher.finalize().as_bytes(),
                observations.len(),
                watermark,
                finality_as_known_at,
                first_coverage,
                quality_score,
            ),
        })
    }
}

pub fn compute_volatility_of_volatility(
    series: &FinalizedVolatilitySeries,
) -> Result<super::ComputedAnalyticalFeature, FeatureComputationError> {
    let values = series
        .values
        .iter()
        .map(|value| value.value())
        .collect::<Vec<_>>();
    let value = volatility::volatility_of_volatility(&values).map_err(map_measure_error)?;
    super::ComputedAnalyticalFeature::from_derived_feature_recipe(
        Task4FeatureKind::VolatilityOfVolatility,
        series.evidence.clone(),
        value,
    )
}

pub fn compute_volatility_term_ratio(
    short: &FeatureObservation,
    long: &FeatureObservation,
) -> Result<super::ComputedAnalyticalFeature, FeatureComputationError> {
    let (short_asset, short_value) = realized_volatility_observation(short)?;
    let (long_asset, long_value) = realized_volatility_observation(long)?;
    if short_asset != long_asset {
        return Err(FeatureComputationError::MixedEntity);
    }
    if short.event_time_end() != long.event_time_end() {
        return Err(FeatureComputationError::NonMonotonicTime);
    }
    let short_extent = short
        .event_time_end()
        .value()
        .checked_sub(short.event_time_start().value())
        .ok_or(FeatureComputationError::InvalidInput)?;
    let long_extent = long
        .event_time_end()
        .value()
        .checked_sub(long.event_time_start().value())
        .ok_or(FeatureComputationError::InvalidInput)?;
    if !matches!(
        (
            u64::try_from(short_extent).ok(),
            u64::try_from(long_extent).ok()
        ),
        (Some(FIVE_MINUTES_NANOS), Some(FIFTEEN_MINUTES_NANOS))
            | (Some(FIFTEEN_MINUTES_NANOS), Some(ONE_HOUR_NANOS))
    ) {
        return Err(FeatureComputationError::InvalidParameter);
    }
    let time_window = crate::TimeWindow::try_new(long.event_time_start(), long.event_time_end())
        .map_err(|_| FeatureComputationError::InvalidInput)?;
    let source_coverage =
        super::merge_source_coverage(short.source_coverage(), long.source_coverage())?;
    let mut hasher = Hasher::new();
    hasher.update(b"crypto-intelligence/volatility-term-ratio-input/v1");
    hasher.update(&short.lineage_hash().bytes());
    hasher.update(&long.lineage_hash().bytes());
    let value = volatility::volatility_term_ratio(short_value.value(), long_value.value())
        .map_err(map_measure_error)?;
    super::ComputedAnalyticalFeature::from_derived_feature_recipe(
        Task4FeatureKind::VolatilityTermRatio,
        super::DerivedFeatureEvidence::new(
            time_window,
            FeatureEntity::Asset(short_asset),
            short.as_known_at().max(long.as_known_at()),
            *hasher.finalize().as_bytes(),
            2,
            short
                .watermark()
                .zip(long.watermark())
                .map(|(short, long)| short.min(long))
                .ok_or(FeatureComputationError::WindowNotFinal)?,
            short
                .finality_as_known_at()
                .max(long.finality_as_known_at()),
            source_coverage,
            short.quality_score().min(long.quality_score()),
        ),
        value,
    )
}

/// Point-in-time model-package evidence for a volatility forecast.
#[derive(Clone, Debug, PartialEq)]
pub struct VolatilityForecastEvidence {
    asset: domain::AssetId,
    target_window: crate::TimeWindow,
    forecast: f64,
    generated_at: domain::UnixNanos,
    as_known_at: domain::UnixNanos,
    model_package_id: String,
    model_version: Version,
    artifact_hash: [u8; 32],
    lineage_hash: [u8; 32],
    quality_score: QualityScore,
}

impl VolatilityForecastEvidence {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        asset: domain::AssetId,
        target_window: crate::TimeWindow,
        forecast: f64,
        generated_at: domain::UnixNanos,
        as_known_at: domain::UnixNanos,
        model_package_id: impl Into<String>,
        model_version: Version,
        artifact_hash: [u8; 32],
        lineage_hash: [u8; 32],
        quality_score: QualityScore,
    ) -> Result<Self, FeatureComputationError> {
        let model_package_id = model_package_id.into();
        if generated_at.value() <= 0
            || generated_at > as_known_at
            || as_known_at > target_window.start()
        {
            return Err(FeatureComputationError::FutureKnowledge);
        }
        if !forecast.is_finite()
            || forecast < 0.0
            || model_package_id.is_empty()
            || model_package_id.len() > 128
            || !model_package_id.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')
            })
            || artifact_hash == [0; 32]
            || lineage_hash == [0; 32]
        {
            return Err(FeatureComputationError::InvalidInput);
        }
        Ok(Self {
            asset,
            target_window,
            forecast,
            generated_at,
            as_known_at,
            model_package_id,
            model_version,
            artifact_hash,
            lineage_hash,
            quality_score,
        })
    }
}

pub fn compute_volatility_forecast_residual(
    realized: &FeatureObservation,
    forecast: &VolatilityForecastEvidence,
) -> Result<super::ComputedAnalyticalFeature, FeatureComputationError> {
    let (asset, realized_value) = realized_volatility_observation(realized)?;
    if asset != forecast.asset {
        return Err(FeatureComputationError::MixedEntity);
    }
    if realized.event_time_start() != forecast.target_window.start()
        || realized.event_time_end() != forecast.target_window.end()
    {
        return Err(FeatureComputationError::InvalidParameter);
    }
    let mut hasher = Hasher::new();
    hasher.update(b"crypto-intelligence/volatility-forecast-residual-input/v1");
    hasher.update(&realized.lineage_hash().bytes());
    super::hash_bytes(&mut hasher, forecast.model_package_id.as_bytes());
    super::hash_bytes(&mut hasher, forecast.model_version.to_string().as_bytes());
    hasher.update(&forecast.generated_at.value().to_be_bytes());
    hasher.update(&forecast.as_known_at.value().to_be_bytes());
    hasher.update(&forecast.forecast.to_bits().to_be_bytes());
    hasher.update(&forecast.artifact_hash);
    hasher.update(&forecast.lineage_hash);
    let value = volatility::forecast_residual(realized_value.value(), forecast.forecast)
        .map_err(map_measure_error)?;
    super::ComputedAnalyticalFeature::from_derived_feature_recipe(
        Task4FeatureKind::VolatilityForecastResidual,
        super::DerivedFeatureEvidence::new(
            forecast.target_window,
            FeatureEntity::Asset(asset),
            realized.as_known_at().max(forecast.as_known_at),
            *hasher.finalize().as_bytes(),
            2,
            realized
                .watermark()
                .ok_or(FeatureComputationError::WindowNotFinal)?,
            realized.finality_as_known_at(),
            realized.source_coverage().clone(),
            realized.quality_score().min(forecast.quality_score),
        ),
        value,
    )
}

/// Creates an explicit missing observation from current watermark evidence.
///
/// Only errors classified as expected data absence are accepted. The tracker
/// must be the exact tracker that produced the decision and must not have
/// advanced since that decision was made.
pub fn compute_missing_realized_volatility_v2(
    asset: domain::AssetId,
    tracker: &crate::WatermarkTracker,
    decision: crate::FinalizationDecision,
    error: FeatureComputationError,
) -> Result<super::ComputedAnalyticalFeature, FeatureComputationError> {
    super::compute_missing_task_four_feature(
        Task4FeatureKind::RealizedVolatility,
        FeatureEntity::Asset(asset),
        tracker,
        decision,
        error,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PriceOhlcBar {
    open: Price,
    high: Price,
    low: Price,
    close: Price,
}

/// Finalized OHLC evidence derived from the exact consolidated-price grid.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FinalizedOhlcWindow {
    prices: PriceWindow,
    closing: super::PriceObservation,
    bar: PriceOhlcBar,
}

impl FinalizedOhlcWindow {
    pub fn try_from_prices(
        prices: &PriceWindow,
        closing: &super::PriceObservation,
        tracker: &crate::WatermarkTracker,
    ) -> Result<Self, FeatureComputationError> {
        validate_sampling(prices, closing, realized_volatility_v2_policy())?;
        let decision = prices
            .finalization()
            .ok_or(FeatureComputationError::UntrustedInput)?;
        if !tracker.validates_decision(decision, prices.entity())
            || decision.state() != crate::Finalization::Final
        {
            return Err(FeatureComputationError::UntrustedInput);
        }
        prices.validates_consolidated_closing_observation(closing)?;
        let open = prices
            .observations()
            .first()
            .ok_or(FeatureComputationError::InsufficientHistory)?
            .price();
        let mut high = open.max(closing.price());
        let mut low = open.min(closing.price());
        for observation in prices.observations() {
            high = high.max(observation.price());
            low = low.min(observation.price());
        }
        Ok(Self {
            prices: prices.clone(),
            closing: closing.clone(),
            bar: PriceOhlcBar::try_new(open, high, low, closing.price())?,
        })
    }

    pub const fn bar(&self) -> PriceOhlcBar {
        self.bar
    }
}

pub fn compute_range_volatility_feature(
    kind: Task4FeatureKind,
    input: &FinalizedOhlcWindow,
    tracker: &crate::WatermarkTracker,
) -> Result<super::ComputedAnalyticalFeature, FeatureComputationError> {
    let result = match kind {
        Task4FeatureKind::ParkinsonVolatility => parkinson_variance(&[input.bar]),
        Task4FeatureKind::GarmanKlassVolatility => garman_klass_variance(&[input.bar]),
        _ => return Err(FeatureComputationError::InvalidParameter),
    }
    .and_then(|variance| finite_analytical(variance.sqrt()));
    super::ComputedAnalyticalFeature::from_price_window_analytical(
        kind,
        &input.prices,
        Some(&input.closing),
        tracker,
        super::AnalyticalFeature::from_result(result)?,
    )
}

impl PriceOhlcBar {
    pub fn try_new(
        open: Price,
        high: Price,
        low: Price,
        close: Price,
    ) -> Result<Self, FeatureComputationError> {
        if high < open.max(close) || low > open.min(close) || low > high {
            return Err(FeatureComputationError::InvalidInput);
        }
        Ok(Self {
            open,
            high,
            low,
            close,
        })
    }

    fn analytical(self) -> Result<OhlcBar, FeatureComputationError> {
        OhlcBar::try_new(
            analytical_price(self.open),
            analytical_price(self.high),
            analytical_price(self.low),
            analytical_price(self.close),
        )
        .map_err(map_measure_error)
    }
}

pub fn parkinson_variance(bars: &[PriceOhlcBar]) -> Result<f64, FeatureComputationError> {
    let analytical = analytical_bars(bars)?;
    volatility::parkinson_variance(&analytical).map_err(map_measure_error)
}

pub fn garman_klass_variance(bars: &[PriceOhlcBar]) -> Result<f64, FeatureComputationError> {
    let analytical = analytical_bars(bars)?;
    volatility::garman_klass_variance(&analytical).map_err(map_measure_error)
}

pub fn seasonality_adjusted_volatility(
    volatility: f64,
    baseline: SeasonalBaseline,
    window: &PriceWindow,
) -> Result<f64, FeatureComputationError> {
    volatility::seasonality_adjusted_volatility(
        volatility,
        baseline,
        window.time_window().start().value(),
    )
    .map_err(map_measure_error)
}

/// Point-in-time fitted seasonal artifact required by the production recipe.
#[derive(Clone, Debug, PartialEq)]
pub struct SeasonalBaselineEvidence {
    asset: domain::AssetId,
    utc_hour_of_week: u16,
    fit_window: crate::TimeWindow,
    baseline: SeasonalBaseline,
    sample_count: usize,
    quality_score: QualityScore,
    lineage_digest: [u8; 32],
}

impl SeasonalBaselineEvidence {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        asset: domain::AssetId,
        utc_hour_of_week: u16,
        fit_window: crate::TimeWindow,
        baseline: SeasonalBaseline,
        sample_count: usize,
        quality_score: QualityScore,
        lineage_digest: [u8; 32],
    ) -> Result<Self, FeatureComputationError> {
        if utc_hour_of_week >= 168
            || sample_count == 0
            || sample_count > super::MAX_PRICE_OBSERVATIONS
            || lineage_digest == [0; 32]
            || fit_window.end().value() != baseline.fitted_through_nanos()
        {
            return Err(FeatureComputationError::InvalidInput);
        }
        Ok(Self {
            asset,
            utc_hour_of_week,
            fit_window,
            baseline,
            sample_count,
            quality_score,
            lineage_digest,
        })
    }

    fn secondary_evidence(&self) -> SecondaryEvidence {
        let mut hasher = Hasher::new();
        hasher.update(b"crypto-intelligence/seasonal-baseline-evidence/v1");
        super::hash_entity(
            &mut hasher,
            &feature_registry::FeatureEntity::Asset(self.asset.clone()),
        );
        hasher.update(&self.utc_hour_of_week.to_be_bytes());
        hasher.update(&self.fit_window.start().value().to_be_bytes());
        hasher.update(&self.fit_window.end().value().to_be_bytes());
        hasher.update(&self.baseline.factor().to_bits().to_be_bytes());
        hasher.update(&self.baseline.fitted_through_nanos().to_be_bytes());
        hasher.update(&self.baseline.as_known_at_nanos().to_be_bytes());
        hasher.update(&self.baseline.version_hash());
        hasher.update(&(self.sample_count as u64).to_be_bytes());
        hasher.update(&self.quality_score.millionths().to_be_bytes());
        hasher.update(&self.lineage_digest);
        SecondaryEvidence::new(
            domain::UnixNanos::new(self.baseline.as_known_at_nanos()),
            *hasher.finalize().as_bytes(),
            self.sample_count,
            self.quality_score,
        )
    }
}

pub fn compute_seasonality_adjusted_volatility(
    window: &PriceWindow,
    closing: &super::PriceObservation,
    tracker: &crate::WatermarkTracker,
    baseline: &SeasonalBaselineEvidence,
) -> Result<super::ComputedAnalyticalFeature, FeatureComputationError> {
    if window.entity() != &feature_registry::FeatureEntity::Asset(baseline.asset.clone())
        || baseline.baseline.fitted_through_nanos() > window.time_window().start().value()
        || baseline.baseline.as_known_at_nanos() > window.time_window().start().value()
    {
        return Err(FeatureComputationError::FutureKnowledge);
    }
    let utc_hour_of_week = (window.time_window().start().value()
        / i64::try_from(ONE_HOUR_NANOS).map_err(|_| FeatureComputationError::InvalidParameter)?
        + UNIX_EPOCH_MONDAY_OFFSET_HOURS)
        .rem_euclid(168);
    if u16::try_from(utc_hour_of_week).ok() != Some(baseline.utc_hour_of_week) {
        return Err(FeatureComputationError::InvalidParameter);
    }
    let measures =
        RealizedMeasures::from_price_window(window, closing, realized_volatility_v2_policy())?;
    let value =
        seasonality_adjusted_volatility(measures.realized_volatility(), baseline.baseline, window)?;
    super::ComputedAnalyticalFeature::from_price_window_recipe_with_secondary(
        Task4FeatureKind::SeasonalityAdjustedVolatility,
        window,
        Some(closing),
        tracker,
        value,
        Some(baseline.secondary_evidence()),
    )
}

/// Converts a registry-bound, time-aware EWMA variance state into volatility.
///
/// The state must be updated with squared returns. Its registered window
/// definition supplies the half-life and its watermark policy controls replay.
pub fn ewma_volatility_from_state(
    state: &HalfLifeEwmaState,
) -> Result<f64, FeatureComputationError> {
    let variance = state
        .value()
        .ok_or(FeatureComputationError::InsufficientHistory)?;
    if variance < 0.0 {
        return Err(FeatureComputationError::InvalidInput);
    }
    finite_analytical(variance.sqrt())
}

pub(crate) fn validate_sampling(
    window: &PriceWindow,
    closing: &super::PriceObservation,
    policy: VolatilityFormulaPolicy,
) -> Result<(), FeatureComputationError> {
    let expected = i64::try_from(policy.sampling_interval_nanos)
        .map_err(|_| FeatureComputationError::InvalidParameter)?;
    let extent = window
        .time_window()
        .end()
        .value()
        .checked_sub(window.time_window().start().value())
        .ok_or(FeatureComputationError::InvalidInput)?;
    if extent % expected != 0 {
        return Err(FeatureComputationError::InvalidParameter);
    }
    let expected_samples = usize::try_from(extent / expected)
        .map_err(|_| FeatureComputationError::CapacityExceeded)?;
    if window.observations().len() != expected_samples
        || closing.entity() != window.entity()
        || closing.event_time() != window.time_window().end()
    {
        return Err(FeatureComputationError::SequenceGap);
    }
    for (index, observation) in window.observations().iter().enumerate() {
        let offset = i64::try_from(index).map_err(|_| FeatureComputationError::CapacityExceeded)?;
        let expected_time = window
            .time_window()
            .start()
            .value()
            .checked_add(
                offset
                    .checked_mul(expected)
                    .ok_or(FeatureComputationError::InvalidInput)?,
            )
            .ok_or(FeatureComputationError::InvalidInput)?;
        if observation.event_time().value() != expected_time {
            return Err(FeatureComputationError::SequenceGap);
        }
    }
    Ok(())
}

fn complete_log_returns(
    window: &PriceWindow,
    closing: &super::PriceObservation,
) -> Result<Vec<f64>, FeatureComputationError> {
    let mut returns = consecutive_log_returns(window)?;
    let last = window
        .observations()
        .last()
        .ok_or(FeatureComputationError::InsufficientHistory)?;
    returns.push(super::log_return(closing.price(), last.price())?);
    Ok(returns)
}

fn analytical_bars(bars: &[PriceOhlcBar]) -> Result<Vec<OhlcBar>, FeatureComputationError> {
    if bars.len() > volatility::MAX_MEASURE_OBSERVATIONS {
        return Err(FeatureComputationError::CapacityExceeded);
    }
    bars.iter().map(|bar| bar.analytical()).collect()
}

fn analytical_price(price: Price) -> f64 {
    price.value().to_f64_lossy_for_analysis()
}

fn realized_volatility_observation(
    observation: &FeatureObservation,
) -> Result<(domain::AssetId, feature_registry::FiniteF64), FeatureComputationError> {
    let mut registry = feature_registry::FeatureRegistry::new();
    registry
        .register(
            realized_volatility_v2_definition()
                .map_err(|_| FeatureComputationError::UntrustedInput)?,
        )
        .map_err(|_| FeatureComputationError::UntrustedInput)?;
    registry
        .validate_observation(observation)
        .map_err(|_| FeatureComputationError::UntrustedInput)?;
    if observation.feature_id().as_str() != Task4FeatureKind::RealizedVolatility.id()
        || observation.feature_version() != &Task4FeatureKind::RealizedVolatility.version()
        || observation.formula_hash() != Task4FeatureKind::RealizedVolatility.formula_hash()
        || observation.finality_state() != FinalityState::Final
        || observation.watermark().is_none()
        || observation.source_coverage().coverage_millionths() < QualityScore::MAX_MILLIONTHS
    {
        return Err(FeatureComputationError::UntrustedInput);
    }
    let FeatureEntity::Asset(asset) = observation.entity() else {
        return Err(FeatureComputationError::MixedEntity);
    };
    let feature_registry::FeatureDatum::Present(FeatureValue::Float64(value)) = observation.datum()
    else {
        return Err(FeatureComputationError::InsufficientHistory);
    };
    if value.value() < 0.0 {
        return Err(FeatureComputationError::InvalidInput);
    }
    Ok((asset.clone(), *value))
}

fn map_measure_error(error: MeasureError) -> FeatureComputationError {
    match error {
        MeasureError::InvalidInput | MeasureError::InvalidOhlc => {
            FeatureComputationError::InvalidInput
        }
        MeasureError::InsufficientHistory => FeatureComputationError::InsufficientHistory,
        MeasureError::CapacityExceeded => FeatureComputationError::CapacityExceeded,
        MeasureError::LengthMismatch => FeatureComputationError::LengthMismatch,
        MeasureError::InvalidParameter => FeatureComputationError::InvalidParameter,
        MeasureError::ZeroVariance => FeatureComputationError::ZeroDenominator,
        MeasureError::AnalyticalUnavailable => FeatureComputationError::AnalyticalUnavailable,
    }
}

fn map_window_error(error: crate::WindowError) -> FeatureComputationError {
    match error {
        crate::WindowError::MembershipCapacity
        | crate::WindowError::InvalidEwmaCapacity
        | crate::WindowError::EwmaCapacity
        | crate::WindowError::WindowCapacity
        | crate::WindowError::RevisionCapacity => FeatureComputationError::CapacityExceeded,
        crate::WindowError::EwmaWatermarkUnavailable => FeatureComputationError::SourceDisconnected,
        crate::WindowError::EwmaWatermarkRegression
        | crate::WindowError::EwmaBeforeWatermark
        | crate::WindowError::StaleFinalizationDecision => FeatureComputationError::Stale,
        crate::WindowError::ForeignWatermarkPolicy => FeatureComputationError::UntrustedInput,
        crate::WindowError::InvalidTimeWindow
        | crate::WindowError::InvalidWindowSpec
        | crate::WindowError::InvalidEventTime
        | crate::WindowError::DefinitionKindMismatch
        | crate::WindowError::InvalidEwmaInput
        | crate::WindowError::InvalidWindowState
        | crate::WindowError::InvalidThreshold
        | crate::WindowError::InvalidAmount
        | crate::WindowError::InvalidLifecycle
        | crate::WindowError::UnknownWindow
        | crate::WindowError::InvalidTransition
        | crate::WindowError::ArithmeticOverflow
        | crate::WindowError::Decimal(_) => FeatureComputationError::InvalidInput,
    }
}
