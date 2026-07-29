//! Pure feature calculations and registry-validated observation emission.

use blake3::Hasher;
use feature_registry::{
    FeatureDatum, FeatureEntity, FeatureId, FeatureObservation, FeatureObservationInput,
    FeatureRegistry, FeatureValue, FeatureValueType, FinalityState, FiniteF64, FormulaHash,
    LineageHash, MissingnessReason, ObservationRevision, QualityScore, RegistryError,
    SourceCoverage, SourceCoverageEntry, WindowId,
};
use semver::Version;
use thiserror::Error;

use domain::{UnixNanos, VenueId};
use feature_registry::CodeRevision;

pub mod catalog;
pub mod cross_venue;
pub mod derivatives;
pub mod microstructure_catalog;
pub mod microstructure_emission;
pub mod orderbook;
pub mod orderflow;
pub mod price;
pub mod quality;
pub mod volatility;

pub use catalog::{Task4FeatureKind, task_four_definitions};
pub use microstructure_catalog::{Task5FeatureRecipe, task_five_definitions, task_five_recipes};
pub use microstructure_emission::{
    Task5EmissionError, Task5FeatureEmissionInput, emit_book_snapshot_core_features,
    emit_book_spread_features, emit_trade_flow_core_features,
};
pub use orderbook::{
    ActiveLevelCounts, BookDistribution, BookEvidencePolicy, BookMetricObservation, BookSideShape,
    BookStateEvidence, DepthBand, DepthSpreadChange, DisplayedDepthRecoveryEpisode,
    FullyObservedDepthBand, InstrumentDefinitionEvidence, L2_UNAVAILABLE_AFTER_MS, LevelGapDensity,
    LiquidityWallDistances, MAX_BOOK_SHAPE_LEVELS, MAX_DEPTH_BAND_BPS, MAX_RECOVERY_OBSERVATIONS,
    QuadraticBookShape, SpreadMetrics, SweepCost, SweepSide, active_level_counts,
    ask_book_shape_quadratic, ask_level_gap_density, bid_book_shape_quadratic,
    bid_level_gap_density, book_depth_and_spread_change, book_distribution, book_quote_age_ms,
    book_shape_quadratic, depth_within_band, expected_sweep_cost, fully_observed_depth_within_band,
    level_gap_density, liquidity_wall_distance_bps, maximum_executable_quote_notional, microprice,
    normalized_imbalance, spread, weighted_midpoint,
};
pub use orderflow::{
    AggressiveTradeImbalances, AggressiveTradeSums, AggressiveTradeTotals, AggressorAuthority,
    AggressorSide, BookQuoteSide, FlowTradeObservation, LargeTradeClusterActivity, LifecycleAction,
    LifecycleCapability, LifecycleMetrics, LifecycleObservation, LifecycleObservationInput,
    LifecycleRates, LifecycleRatios, LifecycleSemantics, LifecycleWindow, MAX_FLOW_TRADES,
    QuoteSideStaleness, SweepEqualPricePolicy, TopOfBookObservation, TopOfBookSideStaleness,
    TopOfBookWindow, TradeClusterStatistics, TradeFlowWindow, TradePrintSweepDirection, TradeShock,
    TradeShockResponse, TradeShockThreshold, TradeShockWeighting, aggressive_trade_imbalances,
    aggressive_trade_sums, aggressive_trade_totals, certified_lifecycle_metrics,
    certified_lifecycle_rates, certified_lifecycle_ratios, interarrival_coefficient_of_variation,
    large_trade_cluster_activity, require_order_lifecycle_semantics, signed_volume_at_price,
    top_of_book_ofi, top_of_book_side_staleness, top_of_book_staleness_for_side,
    trade_cluster_statistics, trade_intensity_per_second, trade_print_sweep_direction,
    trade_shock_response,
};

pub use price::{
    FinalizedTradeWindow, HorizonReturn, MAX_PRICE_OBSERVATIONS, PriceObservation,
    PriceObservationInput, PriceWindow, TradeObservation, compute_consolidated_fair_price_distance,
    compute_finalized_interval_gap_feature, compute_price_path_feature,
    compute_vwap_distance_feature, consecutive_log_returns, cumulative_return,
    finalized_interval_gap, high_low_range, log_return, maximum_drawdown, maximum_run_up,
    multi_horizon_log_returns, open_close_range, relative_distance, return_autocorrelation,
    return_reversal, rolling_excess_kurtosis, rolling_skewness, time_weighted_trend_acceleration,
    time_weighted_trend_slope, trend_acceleration, trend_slope, variance_ratio, vwap_distance,
};

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum FeatureComputationError {
    #[error("feature input is invalid or nonfinite")]
    InvalidInput,
    #[error("feature input does not contain sufficient history")]
    InsufficientHistory,
    #[error("feature input exceeds its bounded capacity")]
    CapacityExceeded,
    #[error("feature observations are not in strictly increasing point-in-time order")]
    NonMonotonicTime,
    #[error("feature input was not knowable at the requested point in time")]
    FutureKnowledge,
    #[error("feature window is not final")]
    WindowNotFinal,
    #[error("feature parameter is invalid")]
    InvalidParameter,
    #[error("feature input lengths do not match")]
    LengthMismatch,
    #[error("feature calculation has a zero denominator")]
    ZeroDenominator,
    #[error("feature input has a sequence gap")]
    SequenceGap,
    #[error("feature input is stale")]
    Stale,
    #[error("required source is disconnected")]
    SourceDisconnected,
    #[error("the source does not support the required feature semantics")]
    SourceNotSupported,
    #[error("the required feature semantics are unavailable under the source license")]
    PrivacyOrLicenseRestriction,
    #[error("feature input is below the declared liquidity threshold")]
    BelowLiquidityThreshold,
    #[error("the declared analytical model does not apply to this input")]
    ModelNotApplicable,
    #[error("feature observation is outside the declared half-open window")]
    OutsideWindow,
    #[error("feature input lineage contains a duplicate")]
    DuplicateLineage,
    #[error("feature window contains observations from another entity")]
    MixedEntity,
    #[error("feature computation requires consolidated-market evidence")]
    UntrustedInput,
    #[error("the point-in-time eligible source universe changed inside one feature window")]
    EligibleUniverseChanged,
    #[error("source health changed incompatibly inside one feature window")]
    SourceHealthChanged,
    #[error("analytical data cannot be represented as a finite feature value")]
    AnalyticalUnavailable,
}

impl FeatureComputationError {
    pub const fn missingness_reason(self) -> Option<MissingnessReason> {
        match self {
            Self::InsufficientHistory => Some(MissingnessReason::InsufficientHistory),
            Self::WindowNotFinal => Some(MissingnessReason::WindowNotFinal),
            Self::SequenceGap => Some(MissingnessReason::SequenceGap),
            Self::Stale => Some(MissingnessReason::Stale),
            Self::SourceDisconnected => Some(MissingnessReason::SourceDisconnected),
            Self::SourceNotSupported => Some(MissingnessReason::SourceNotSupported),
            Self::PrivacyOrLicenseRestriction => {
                Some(MissingnessReason::PrivacyOrLicenseRestriction)
            }
            Self::BelowLiquidityThreshold => Some(MissingnessReason::BelowLiquidityThreshold),
            Self::ModelNotApplicable => Some(MissingnessReason::ModelNotApplicable),
            Self::InvalidInput
            | Self::CapacityExceeded
            | Self::NonMonotonicTime
            | Self::FutureKnowledge
            | Self::InvalidParameter
            | Self::LengthMismatch
            | Self::ZeroDenominator
            | Self::OutsideWindow
            | Self::DuplicateLineage
            | Self::MixedEntity
            | Self::UntrustedInput
            | Self::EligibleUniverseChanged
            | Self::SourceHealthChanged => None,
            Self::AnalyticalUnavailable => Some(MissingnessReason::Unknown),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AnalyticalFeature {
    Present(FiniteF64),
    Missing(MissingnessReason),
}

impl AnalyticalFeature {
    pub fn present(value: f64) -> Result<Self, FeatureComputationError> {
        FiniteF64::new(value)
            .map(Self::Present)
            .map_err(|_| FeatureComputationError::InvalidInput)
    }

    pub fn from_result(
        result: Result<f64, FeatureComputationError>,
    ) -> Result<Self, FeatureComputationError> {
        match result {
            Ok(value) => Self::present(value),
            Err(error) => error.missingness_reason().map(Self::Missing).ok_or(error),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComputedAnalyticalFeature {
    feature_id: FeatureId,
    feature_version: Version,
    window_id: WindowId,
    time_window: crate::TimeWindow,
    entity: FeatureEntity,
    as_of: UnixNanos,
    input_lineage_digest: [u8; 32],
    input_count: usize,
    formula_hash: FormulaHash,
    analytical: AnalyticalFeature,
    finality_state: FinalityState,
    watermark: Option<UnixNanos>,
    finality_as_known_at: UnixNanos,
    source_coverage: SourceCoverage,
    quality_score: QualityScore,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SecondaryEvidence {
    as_known_at: UnixNanos,
    lineage_digest: [u8; 32],
    input_count: usize,
    quality_score: QualityScore,
    source_coverage: Option<SourceCoverage>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DerivedFeatureEvidence {
    time_window: crate::TimeWindow,
    entity: FeatureEntity,
    as_of: UnixNanos,
    lineage_digest: [u8; 32],
    input_count: usize,
    watermark: UnixNanos,
    finality_as_known_at: UnixNanos,
    source_coverage: SourceCoverage,
    quality_score: QualityScore,
}

impl DerivedFeatureEvidence {
    #[allow(clippy::too_many_arguments)]
    pub(crate) const fn new(
        time_window: crate::TimeWindow,
        entity: FeatureEntity,
        as_of: UnixNanos,
        lineage_digest: [u8; 32],
        input_count: usize,
        watermark: UnixNanos,
        finality_as_known_at: UnixNanos,
        source_coverage: SourceCoverage,
        quality_score: QualityScore,
    ) -> Self {
        Self {
            time_window,
            entity,
            as_of,
            lineage_digest,
            input_count,
            watermark,
            finality_as_known_at,
            source_coverage,
            quality_score,
        }
    }
}

impl SecondaryEvidence {
    pub(crate) const fn new(
        as_known_at: UnixNanos,
        lineage_digest: [u8; 32],
        input_count: usize,
        quality_score: QualityScore,
    ) -> Self {
        Self {
            as_known_at,
            lineage_digest,
            input_count,
            quality_score,
            source_coverage: None,
        }
    }

    pub(crate) fn with_source_coverage(mut self, source_coverage: SourceCoverage) -> Self {
        self.source_coverage = Some(source_coverage);
        self
    }
}

impl ComputedAnalyticalFeature {
    pub(crate) fn from_derived_feature_recipe(
        kind: Task4FeatureKind,
        evidence: DerivedFeatureEvidence,
        value: f64,
    ) -> Result<Self, FeatureComputationError> {
        if evidence.input_count == 0
            || evidence.lineage_digest == [0; 32]
            || evidence.as_of < evidence.time_window.end()
            || evidence.watermark < evidence.time_window.end()
            || evidence.finality_as_known_at > evidence.as_of
        {
            return Err(FeatureComputationError::UntrustedInput);
        }
        Ok(Self {
            feature_id: FeatureId::new(kind.id()).expect("closed catalogue ID is valid"),
            feature_version: kind.version(),
            window_id: kind.output_window_id(evidence.time_window)?,
            time_window: evidence.time_window,
            entity: evidence.entity,
            as_of: evidence.as_of,
            input_lineage_digest: evidence.lineage_digest,
            input_count: evidence.input_count,
            formula_hash: kind.formula_hash(),
            analytical: AnalyticalFeature::present(value)?,
            finality_state: FinalityState::Final,
            watermark: Some(evidence.watermark),
            finality_as_known_at: evidence.finality_as_known_at,
            source_coverage: evidence.source_coverage,
            quality_score: evidence.quality_score,
        })
    }

    pub(crate) fn from_price_window_recipe_with_secondary(
        kind: Task4FeatureKind,
        window: &PriceWindow,
        closing: Option<&PriceObservation>,
        tracker: &crate::WatermarkTracker,
        value: f64,
        secondary: Option<SecondaryEvidence>,
    ) -> Result<Self, FeatureComputationError> {
        Self::from_price_window_analytical_with_secondary(
            kind,
            window,
            closing,
            tracker,
            AnalyticalFeature::present(value)?,
            secondary,
        )
    }

    pub(crate) fn from_price_window_analytical(
        kind: Task4FeatureKind,
        window: &PriceWindow,
        closing: Option<&PriceObservation>,
        tracker: &crate::WatermarkTracker,
        analytical: AnalyticalFeature,
    ) -> Result<Self, FeatureComputationError> {
        Self::from_price_window_analytical_with_secondary(
            kind, window, closing, tracker, analytical, None,
        )
    }

    fn from_price_window_analytical_with_secondary(
        kind: Task4FeatureKind,
        window: &PriceWindow,
        closing: Option<&PriceObservation>,
        tracker: &crate::WatermarkTracker,
        analytical: AnalyticalFeature,
        secondary: Option<SecondaryEvidence>,
    ) -> Result<Self, FeatureComputationError> {
        let decision = window
            .finalization()
            .ok_or(FeatureComputationError::UntrustedInput)?;
        if !tracker.validates_decision(decision, window.entity()) {
            return Err(FeatureComputationError::UntrustedInput);
        }
        if matches!(analytical, AnalyticalFeature::Present(_))
            && decision.state() != crate::Finalization::Final
        {
            return Err(FeatureComputationError::WindowNotFinal);
        }
        let (
            mut as_of,
            mut input_lineage_digest,
            mut input_count,
            mut quality_score,
            mut source_coverage,
        ) = match closing {
            Some(closing) => (
                closing.as_known_at().max(window.as_of()),
                window.closing_lineage_digest(closing)?,
                window.observations().len() + 1,
                window.quality_with_closing(closing)?,
                window
                    .source_coverage()
                    .cloned()
                    .ok_or(FeatureComputationError::UntrustedInput)?,
            ),
            None => (
                decision
                    .watermark()
                    .unwrap_or(window.time_window().end())
                    .max(window.as_of())
                    .max(window.time_window().end()),
                window.finalized_lineage_digest()?,
                window.observations().len(),
                window
                    .quality_score()
                    .ok_or(FeatureComputationError::UntrustedInput)?,
                window
                    .source_coverage()
                    .cloned()
                    .ok_or(FeatureComputationError::UntrustedInput)?,
            ),
        };
        as_of = as_of.max(decision.as_known_at());
        if let Some(secondary) = secondary {
            let mut hasher = Hasher::new();
            hasher.update(b"crypto-intelligence/primary-secondary-lineage/v1");
            hasher.update(&input_lineage_digest);
            hasher.update(&secondary.lineage_digest);
            input_lineage_digest = *hasher.finalize().as_bytes();
            as_of = as_of.max(secondary.as_known_at);
            input_count = input_count
                .checked_add(secondary.input_count)
                .ok_or(FeatureComputationError::CapacityExceeded)?;
            quality_score = quality_score.min(secondary.quality_score);
            if let Some(secondary_coverage) = secondary.source_coverage {
                source_coverage = merge_source_coverage(&source_coverage, &secondary_coverage)?;
            }
        }
        Ok(Self {
            feature_id: FeatureId::new(kind.id()).expect("closed catalogue ID is valid"),
            feature_version: kind.version(),
            window_id: kind.output_window_id(window.time_window())?,
            time_window: window.time_window(),
            entity: window.entity().clone(),
            as_of,
            input_lineage_digest,
            input_count,
            formula_hash: kind.formula_hash(),
            analytical,
            finality_state: finality_state(decision.state()),
            watermark: decision.watermark(),
            finality_as_known_at: decision.as_known_at(),
            source_coverage,
            quality_score,
        })
    }

    pub(crate) fn from_paired_price_windows_recipe(
        kind: Task4FeatureKind,
        left: (&PriceWindow, &PriceObservation, &crate::WatermarkTracker),
        right: (&PriceWindow, &PriceObservation, &crate::WatermarkTracker),
        analytical: AnalyticalFeature,
    ) -> Result<Self, FeatureComputationError> {
        if !matches!(
            kind,
            Task4FeatureKind::RealizedCovariance | Task4FeatureKind::RealizedCorrelation
        ) || left.0.time_window() != right.0.time_window()
        {
            return Err(FeatureComputationError::InvalidParameter);
        }
        let (left_asset, right_asset) = match (left.0.entity(), right.0.entity()) {
            (FeatureEntity::Asset(left), FeatureEntity::Asset(right)) if left != right => {
                (left, right)
            }
            _ => return Err(FeatureComputationError::MixedEntity),
        };
        let (first, second, first_asset, second_asset) =
            if compare_asset(left_asset, right_asset).is_lt() {
                (left, right, left_asset, right_asset)
            } else {
                (right, left, right_asset, left_asset)
            };
        for (window, closing, tracker) in [first, second] {
            let decision = window
                .finalization()
                .ok_or(FeatureComputationError::UntrustedInput)?;
            if !tracker.validates_decision(decision, window.entity())
                || decision.state() != crate::Finalization::Final
            {
                return Err(FeatureComputationError::UntrustedInput);
            }
            window.validates_consolidated_closing_observation(closing)?;
        }
        let first_decision = first
            .0
            .finalization()
            .ok_or(FeatureComputationError::UntrustedInput)?;
        let second_decision = second
            .0
            .finalization()
            .ok_or(FeatureComputationError::UntrustedInput)?;
        let source_coverage = merge_source_coverage(
            first
                .0
                .source_coverage()
                .ok_or(FeatureComputationError::UntrustedInput)?,
            second
                .0
                .source_coverage()
                .ok_or(FeatureComputationError::UntrustedInput)?,
        )?;
        let mut hasher = Hasher::new();
        hasher.update(b"crypto-intelligence/paired-price-window-lineage/v1");
        hasher.update(&first.0.closing_lineage_digest(first.1)?);
        hasher.update(&second.0.closing_lineage_digest(second.1)?);
        let input_count = first
            .0
            .observations()
            .len()
            .checked_add(second.0.observations().len())
            .and_then(|count| count.checked_add(2))
            .ok_or(FeatureComputationError::CapacityExceeded)?;
        Ok(Self {
            feature_id: FeatureId::new(kind.id()).expect("closed catalogue ID is valid"),
            feature_version: kind.version(),
            window_id: kind.output_window_id(first.0.time_window())?,
            time_window: first.0.time_window(),
            entity: FeatureEntity::AssetPair(first_asset.clone(), second_asset.clone()),
            as_of: first
                .1
                .as_known_at()
                .max(second.1.as_known_at())
                .max(first.0.as_of())
                .max(second.0.as_of())
                .max(first_decision.as_known_at())
                .max(second_decision.as_known_at()),
            input_lineage_digest: *hasher.finalize().as_bytes(),
            input_count,
            formula_hash: kind.formula_hash(),
            analytical,
            finality_state: FinalityState::Final,
            watermark: Some(
                first_decision
                    .watermark()
                    .zip(second_decision.watermark())
                    .map(|(left, right)| left.min(right))
                    .ok_or(FeatureComputationError::WindowNotFinal)?,
            ),
            finality_as_known_at: first_decision
                .as_known_at()
                .max(second_decision.as_known_at()),
            source_coverage,
            quality_score: first
                .0
                .quality_with_closing(first.1)?
                .min(second.0.quality_with_closing(second.1)?),
        })
    }

    pub(crate) fn missing_task_four(
        kind: Task4FeatureKind,
        entity: FeatureEntity,
        tracker: &crate::WatermarkTracker,
        decision: crate::FinalizationDecision,
        error: FeatureComputationError,
    ) -> Result<Self, FeatureComputationError> {
        let reason = error.missingness_reason().ok_or(error)?;
        if !tracker.validates_decision(decision, &entity) {
            return Err(FeatureComputationError::UntrustedInput);
        }
        let entity_matches = matches!(
            (&entity, kind),
            (
                FeatureEntity::AssetPair(_, _),
                Task4FeatureKind::RealizedCovariance | Task4FeatureKind::RealizedCorrelation
            ) | (
                FeatureEntity::Asset(_),
                Task4FeatureKind::LogReturn
                    | Task4FeatureKind::CumulativeReturn
                    | Task4FeatureKind::HighLowRange
                    | Task4FeatureKind::OpenCloseRange
                    | Task4FeatureKind::RollingVwapDistance
                    | Task4FeatureKind::AnchoredVwapDistance
                    | Task4FeatureKind::ConsolidatedFairPriceDistance
                    | Task4FeatureKind::TrendSlope
                    | Task4FeatureKind::TrendAcceleration
                    | Task4FeatureKind::MaximumDrawdown
                    | Task4FeatureKind::MaximumRunUp
                    | Task4FeatureKind::UpsideSemivariance
                    | Task4FeatureKind::DownsideSemivariance
                    | Task4FeatureKind::ReturnAutocorrelation
                    | Task4FeatureKind::VarianceRatio
                    | Task4FeatureKind::RollingSkewness
                    | Task4FeatureKind::RollingExcessKurtosis
                    | Task4FeatureKind::ReturnReversal
                    | Task4FeatureKind::FinalizedIntervalGap
                    | Task4FeatureKind::RealizedVariance
                    | Task4FeatureKind::RealizedVolatility
                    | Task4FeatureKind::ExponentiallyWeightedVolatility
                    | Task4FeatureKind::ParkinsonVolatility
                    | Task4FeatureKind::GarmanKlassVolatility
                    | Task4FeatureKind::BipowerVariation
                    | Task4FeatureKind::JumpVariation
                    | Task4FeatureKind::VolatilityOfVolatility
                    | Task4FeatureKind::VolatilityTermRatio
                    | Task4FeatureKind::SeasonalityAdjustedVolatility
                    | Task4FeatureKind::VolatilityForecastResidual
            )
        );
        if !entity_matches {
            return Err(FeatureComputationError::MixedEntity);
        }
        let source_coverage = tracker
            .coverage_for_decision(decision)
            .ok_or(FeatureComputationError::UntrustedInput)?;
        let watermark = decision.watermark();
        let finality_state = match decision.state() {
            crate::Finalization::Provisional => FinalityState::Provisional,
            crate::Finalization::Final => FinalityState::Final,
            crate::Finalization::Invalid => FinalityState::Invalid,
        };
        Ok(Self {
            feature_id: FeatureId::new(kind.id()).expect("closed catalogue ID is valid"),
            feature_version: kind.version(),
            window_id: kind.output_window_id(decision.window())?,
            time_window: decision.window(),
            entity,
            as_of: decision.as_known_at(),
            input_lineage_digest: decision.evidence_digest(),
            input_count: 0,
            formula_hash: kind.formula_hash(),
            analytical: AnalyticalFeature::Missing(reason),
            finality_state,
            watermark,
            finality_as_known_at: decision.as_known_at(),
            source_coverage,
            quality_score: QualityScore::from_millionths(0)
                .expect("zero is a valid bounded quality score"),
        })
    }
}

/// Emits evidence-bound explicit missingness for any closed Task 4 recipe.
///
/// Only errors classified as expected data absence are accepted. The entity,
/// exact window, source universe, health, decision availability, and formula
/// identity are all derived from the matching tracker decision.
pub fn compute_missing_task_four_feature(
    kind: Task4FeatureKind,
    entity: FeatureEntity,
    tracker: &crate::WatermarkTracker,
    decision: crate::FinalizationDecision,
    error: FeatureComputationError,
) -> Result<ComputedAnalyticalFeature, FeatureComputationError> {
    ComputedAnalyticalFeature::missing_task_four(kind, entity, tracker, decision, error)
}

fn merge_source_coverage(
    left: &SourceCoverage,
    right: &SourceCoverage,
) -> Result<SourceCoverage, FeatureComputationError> {
    let mut expected = left.expected_sources().to_vec();
    for source in right.expected_sources() {
        if !expected.contains(source) {
            expected.push(source.clone());
        }
    }
    let mut observed = Vec::new();
    for source in &expected {
        let required_left = left.expected_sources().contains(source);
        let required_right = right.expected_sources().contains(source);
        let observed_left = left.entries().iter().find(|entry| entry.source() == source);
        let observed_right = right
            .entries()
            .iter()
            .find(|entry| entry.source() == source);
        if (required_left && observed_left.is_none())
            || (required_right && observed_right.is_none())
        {
            continue;
        }
        let health = match (observed_left, observed_right) {
            (Some(left), Some(right)) if left.health() != right.health() => {
                return Err(FeatureComputationError::SourceHealthChanged);
            }
            (Some(left), Some(_)) | (Some(left), None) => left.health(),
            (None, Some(right)) => right.health(),
            (None, None) => continue,
        };
        observed.push(SourceCoverageEntry::new(source.clone(), health));
    }
    SourceCoverage::try_new_partial(expected, observed).map_err(|error| match error {
        RegistryError::InvalidSourceCoverage => FeatureComputationError::CapacityExceeded,
        RegistryError::DuplicateSource | RegistryError::UnexpectedSource => {
            FeatureComputationError::EligibleUniverseChanged
        }
        _ => FeatureComputationError::InvalidInput,
    })
}

fn compare_asset(left: &domain::AssetId, right: &domain::AssetId) -> std::cmp::Ordering {
    (left.namespace() as u8)
        .cmp(&(right.namespace() as u8))
        .then_with(|| left.chain_id().cmp(right.chain_id()))
        .then_with(|| left.contract_or_mint().cmp(right.contract_or_mint()))
        .then_with(|| left.canonical_symbol().cmp(right.canonical_symbol()))
        .then_with(|| left.generation().cmp(&right.generation()))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FloatFeatureEmissionInput {
    pub computed_at: UnixNanos,
    pub revision: ObservationRevision,
    pub code_commit: CodeRevision,
}

#[derive(Debug, Error)]
pub enum FeatureEmissionError {
    #[error("feature definition is not registered")]
    UnknownDefinition,
    #[error("feature definition is not a float64 feature")]
    WrongValueType,
    #[error("computed formula does not match the registered formula")]
    FormulaMismatch,
    #[error("computed window does not match the registered window definition")]
    WindowMismatch,
    #[error(transparent)]
    Registry(#[from] RegistryError),
}

pub fn emit_float_feature(
    registry: &FeatureRegistry,
    input: FloatFeatureEmissionInput,
    computed: ComputedAnalyticalFeature,
) -> Result<FeatureObservation, FeatureEmissionError> {
    let definition = registry
        .get(&computed.feature_id, &computed.feature_version)
        .ok_or(FeatureEmissionError::UnknownDefinition)?;
    if definition.value_type() != FeatureValueType::Float64 {
        return Err(FeatureEmissionError::WrongValueType);
    }
    if definition.formula_hash() != computed.formula_hash {
        return Err(FeatureEmissionError::FormulaMismatch);
    }
    if !registered_window_matches(definition, &computed.window_id, computed.time_window) {
        return Err(FeatureEmissionError::WindowMismatch);
    }

    let (datum, finality_state) = match &computed.analytical {
        AnalyticalFeature::Present(value) => (
            FeatureDatum::Present(FeatureValue::Float64(*value)),
            computed.finality_state,
        ),
        AnalyticalFeature::Missing(reason) => {
            (FeatureDatum::Missing(*reason), FinalityState::Invalid)
        }
    };
    let lineage_hash = feature_lineage_hash(definition, &input, &computed, &datum, finality_state)?;
    let observation = FeatureObservation::try_new(FeatureObservationInput {
        feature_id: computed.feature_id,
        feature_version: computed.feature_version,
        entity: computed.entity,
        window_id: computed.window_id,
        resolution: definition.output_resolution(),
        datum,
        value_type: FeatureValueType::Float64,
        event_time_start: computed.time_window.start(),
        event_time_end: computed.time_window.end(),
        as_known_at: computed.as_of,
        computed_at: input.computed_at,
        watermark: computed.watermark,
        finality_as_known_at: computed.finality_as_known_at,
        finality_state,
        revision: input.revision,
        source_coverage: computed.source_coverage,
        quality_score: computed.quality_score,
        normalization_version: definition.normalization().version().clone(),
        formula_hash: definition.formula_hash(),
        code_commit: input.code_commit,
        lineage_hash,
    })?;
    registry.validate_observation(&observation)?;
    Ok(observation)
}

fn registered_window_matches(
    definition: &feature_registry::FeatureDefinition,
    window_id: &WindowId,
    time_window: crate::TimeWindow,
) -> bool {
    let Some(window) = definition.window(window_id) else {
        return false;
    };
    if window.kind() == feature_registry::WindowKind::ExponentiallyWeighted {
        return true;
    }
    let Ok(spec) = crate::TimeWindowSpec::try_from_definition(window) else {
        return false;
    };
    let Some(last_event) = time_window.end().value().checked_sub(1).map(UnixNanos::new) else {
        return false;
    };
    spec.windows_for(last_event)
        .is_ok_and(|windows| windows.contains(&time_window))
}

pub(crate) fn finite_analytical(value: f64) -> Result<f64, FeatureComputationError> {
    if value.is_finite() {
        Ok(if value == 0.0 { 0.0 } else { value })
    } else {
        Err(FeatureComputationError::AnalyticalUnavailable)
    }
}

fn feature_lineage_hash(
    definition: &feature_registry::FeatureDefinition,
    input: &FloatFeatureEmissionInput,
    computed: &ComputedAnalyticalFeature,
    datum: &FeatureDatum,
    finality_state: FinalityState,
) -> Result<LineageHash, RegistryError> {
    let mut hasher = Hasher::new();
    hasher.update(b"crypto-intelligence/analytical-feature-lineage/v1");
    hash_bytes(&mut hasher, definition.id().as_str().as_bytes());
    hash_bytes(&mut hasher, definition.version().to_string().as_bytes());
    hasher.update(&definition.formula_hash().bytes());
    hasher.update(&definition.output_resolution().value().to_be_bytes());
    hasher.update(&[normalization_kind_tag(definition.normalization().kind())]);
    hash_bytes(
        &mut hasher,
        definition.normalization().version().to_string().as_bytes(),
    );
    hash_bytes(&mut hasher, computed.window_id.as_str().as_bytes());
    hash_entity(&mut hasher, &computed.entity);
    hasher.update(&computed.time_window.start().value().to_be_bytes());
    hasher.update(&computed.time_window.end().value().to_be_bytes());
    hasher.update(&computed.as_of.value().to_be_bytes());
    hasher.update(&computed.finality_as_known_at.value().to_be_bytes());
    hasher.update(&input.computed_at.value().to_be_bytes());
    match computed.watermark {
        Some(watermark) => {
            hasher.update(&[1]);
            hasher.update(&watermark.value().to_be_bytes());
        }
        None => {
            hasher.update(&[0]);
        }
    }
    hasher.update(&input.revision.value().to_be_bytes());
    hasher.update(&computed.quality_score.millionths().to_be_bytes());
    hash_bytes(&mut hasher, input.code_commit.as_str().as_bytes());
    hasher.update(&[finality_tag(finality_state)]);
    for source in computed.source_coverage.expected_sources() {
        hasher.update(&[1]);
        hash_source(&mut hasher, source);
    }
    for entry in computed.source_coverage.entries() {
        hasher.update(&[2]);
        hash_source(&mut hasher, entry.source());
        hash_bytes(&mut hasher, entry.health().as_str().as_bytes());
    }
    match datum {
        FeatureDatum::Present(FeatureValue::Float64(value)) => {
            hasher.update(&[1]);
            hasher.update(&value.value().to_bits().to_be_bytes());
        }
        FeatureDatum::Missing(reason) => {
            hasher.update(&[2, missingness_tag(*reason)]);
        }
        FeatureDatum::Present(_) => unreachable!("float emitter constructs only float values"),
    }
    hasher.update(&(computed.input_count as u64).to_be_bytes());
    hasher.update(&computed.input_lineage_digest);
    LineageHash::new(*hasher.finalize().as_bytes())
}

pub(crate) fn hash_entity(hasher: &mut Hasher, entity: &FeatureEntity) {
    match entity {
        FeatureEntity::Instrument(instrument) => {
            hasher.update(&[1]);
            hash_bytes(hasher, instrument.venue().as_str().as_bytes());
            hash_bytes(hasher, instrument.venue_symbol().as_bytes());
            hasher.update(&[instrument.product_type() as u8]);
            hasher.update(&instrument.generation().to_be_bytes());
        }
        FeatureEntity::Asset(asset) => {
            hasher.update(&[2, asset.namespace() as u8]);
            hash_bytes(hasher, asset.chain_id().as_bytes());
            hash_bytes(hasher, asset.contract_or_mint().as_bytes());
            hash_bytes(hasher, asset.canonical_symbol().as_bytes());
            hasher.update(&asset.generation().to_be_bytes());
        }
        FeatureEntity::AssetPair(left, right) => {
            hasher.update(&[6]);
            hash_asset(hasher, left);
            hash_asset(hasher, right);
        }
        FeatureEntity::Venue(venue) => {
            hasher.update(&[3]);
            hash_venue(hasher, venue);
        }
        FeatureEntity::Source(source) => {
            hasher.update(&[4]);
            hash_source(hasher, source);
        }
        FeatureEntity::Global => {
            hasher.update(&[5]);
        }
    }
}

fn hash_venue(hasher: &mut Hasher, venue: &VenueId) {
    hash_bytes(hasher, venue.as_str().as_bytes());
}

fn hash_source(hasher: &mut Hasher, source: &domain::SourceId) {
    hasher.update(&[source.kind() as u8]);
    hash_bytes(hasher, source.name().as_bytes());
    hasher.update(&source.generation().to_be_bytes());
}

fn hash_asset(hasher: &mut Hasher, asset: &domain::AssetId) {
    hasher.update(&[asset.namespace() as u8]);
    hash_bytes(hasher, asset.chain_id().as_bytes());
    hash_bytes(hasher, asset.contract_or_mint().as_bytes());
    hash_bytes(hasher, asset.canonical_symbol().as_bytes());
    hasher.update(&asset.generation().to_be_bytes());
}

const fn finality_tag(finality: FinalityState) -> u8 {
    match finality {
        FinalityState::Provisional => 1,
        FinalityState::Final => 2,
        FinalityState::Corrected => 3,
        FinalityState::Invalid => 4,
    }
}

const fn finality_state(finality: crate::Finalization) -> FinalityState {
    match finality {
        crate::Finalization::Provisional => FinalityState::Provisional,
        crate::Finalization::Final => FinalityState::Final,
        crate::Finalization::Invalid => FinalityState::Invalid,
    }
}

const fn normalization_kind_tag(kind: feature_registry::NormalizationKind) -> u8 {
    match kind {
        feature_registry::NormalizationKind::None => 1,
        feature_registry::NormalizationKind::RobustRolling => 2,
        feature_registry::NormalizationKind::ExpandingQuantile => 3,
        feature_registry::NormalizationKind::TrainingFoldZScore => 4,
        feature_registry::NormalizationKind::Log => 5,
        feature_registry::NormalizationKind::SignedLog => 6,
        feature_registry::NormalizationKind::VolatilityScaled => 7,
        feature_registry::NormalizationKind::AssetRelative => 8,
        feature_registry::NormalizationKind::CrossSectionalRank => 9,
        feature_registry::NormalizationKind::TimeOfWeekSeasonal => 10,
        feature_registry::NormalizationKind::LiquidityBucket => 11,
    }
}

const fn missingness_tag(reason: MissingnessReason) -> u8 {
    match reason {
        MissingnessReason::NotListed => 1,
        MissingnessReason::SourceNotSupported => 2,
        MissingnessReason::SourceDisconnected => 3,
        MissingnessReason::SequenceGap => 4,
        MissingnessReason::Stale => 5,
        MissingnessReason::InsufficientHistory => 6,
        MissingnessReason::WindowNotFinal => 7,
        MissingnessReason::BelowLiquidityThreshold => 8,
        MissingnessReason::VendorRevisionPending => 9,
        MissingnessReason::ModelNotApplicable => 10,
        MissingnessReason::PrivacyOrLicenseRestriction => 11,
        MissingnessReason::Unknown => 12,
    }
}

fn hash_bytes(hasher: &mut Hasher, value: &[u8]) {
    hasher.update(&(value.len() as u64).to_be_bytes());
    hasher.update(value);
}
