use consolidated_market::{
    CandidatePriceKind, ConsolidatedOutcome, EstimatorConfigInput, FairPriceEstimator,
    InstrumentProvenance, MetadataStatus, PolicyId, Ppm, VenueQuote, VenueQuoteInput,
    VenueTradingState,
};
use domain::{
    AssetId, AssetNamespace, ContractKind, ContractValueUnit, InstrumentDefinition,
    InstrumentDefinitionInput, ProductType, SourceId, SourceKind, UnixNanos, VenueId,
};
use event_envelope::{
    EventEnvelope, QualityFlags, Side, SnapshotKind, Trade, UncheckedEventMetadata,
    UncheckedEventPayload,
};
use feature_engine::{
    Finalization, HalfLifeEwmaState, PartitionConfig, PartitionId, TimeWindow, WatermarkKey,
    WatermarkTracker, WatermarkUpdate,
    features::{
        AnalyticalFeature, FeatureComputationError, FinalizedTradeWindow,
        FloatFeatureEmissionInput, PriceObservation, PriceObservationInput, PriceWindow,
        Task4FeatureKind, TradeObservation, compute_consolidated_fair_price_distance,
        compute_finalized_interval_gap_feature, compute_price_path_feature,
        compute_vwap_distance_feature, consecutive_log_returns, cumulative_return,
        emit_float_feature, finalized_interval_gap, high_low_range, log_return, maximum_drawdown,
        maximum_run_up, multi_horizon_log_returns, relative_distance, return_autocorrelation,
        return_reversal, rolling_excess_kurtosis, rolling_skewness, task_four_definitions,
        time_weighted_trend_acceleration, time_weighted_trend_slope, trend_acceleration,
        trend_slope, variance_ratio,
        volatility::{
            FinalizedOhlcWindow, FinalizedVolatilitySeries, PriceOhlcBar, RealizedMeasures,
            SeasonalBaselineEvidence, VolatilityForecastEvidence, VolatilityFormulaPolicy,
            compute_exponentially_weighted_volatility, compute_missing_realized_volatility_v2,
            compute_range_volatility_feature, compute_realized_measure_feature,
            compute_realized_pair_feature, compute_realized_volatility_v2,
            compute_seasonality_adjusted_volatility, compute_volatility_forecast_residual,
            compute_volatility_of_volatility, compute_volatility_term_ratio,
            ewma_volatility_from_state, garman_klass_variance, parkinson_variance,
            realized_volatility_v2_definition, seasonality_adjusted_volatility,
        },
        vwap_distance,
    },
};
use feature_registry::{
    CodeRevision, DurationNanos, FeatureDatum, FeatureEntity, FeatureObservation, FeatureRegistry,
    FeatureValue, FinalityState, MissingnessReason, ObservationRevision, WindowDefinition,
    WindowId,
};
use fixed_decimal::{FixedDecimal, Notional, Price, Quantity};
use quality::SourceHealthState;
use volatility::SeasonalBaseline;

const AS_OF: UnixNanos = UnixNanos::new(2_000);
const EPSILON: f64 = 1e-12;

#[test]
fn task_four_catalogue_is_complete_unique_and_documented() {
    let definitions = task_four_definitions().expect("catalogue should be valid");
    assert_eq!(definitions.len(), Task4FeatureKind::ALL.len());
    let dictionary = include_str!("../../../docs/data-dictionary/features.md");
    let mut registry = FeatureRegistry::new();
    for (kind, definition) in Task4FeatureKind::ALL.into_iter().zip(definitions) {
        assert_eq!(definition.id().as_str(), kind.id());
        assert_eq!(definition.formula_hash(), kind.formula_hash());
        let anchor = definition
            .documentation()
            .as_str()
            .strip_prefix("docs/data-dictionary/features.md#")
            .expect("documentation path should be canonical");
        assert!(
            dictionary.contains(&format!("<a id=\"{anchor}\"></a>")),
            "missing dictionary anchor for {}",
            kind.id()
        );
        assert!(
            dictionary.contains(&format!("| `{}` | Feature Platform |", kind.id())),
            "missing ownership/test traceability row for {}",
            kind.id()
        );
        registry
            .register(definition)
            .expect("catalogue definition should be unique");
    }
    assert_eq!(registry.len(), Task4FeatureKind::ALL.len());
}

fn price(value: &str) -> Price {
    Price::new(FixedDecimal::parse_canonical(value).expect("decimal should be valid"))
        .expect("price should be positive")
}

fn quantity(value: &str) -> Quantity {
    Quantity::new(FixedDecimal::parse_canonical(value).expect("decimal should be valid"))
        .expect("quantity should be nonnegative")
}

fn notional(value: &str) -> Notional {
    Notional::new(FixedDecimal::parse_canonical(value).expect("decimal should be valid"))
        .expect("notional should be nonnegative")
}

fn entity(symbol: &str) -> FeatureEntity {
    FeatureEntity::Instrument(
        domain::InstrumentId::new(
            VenueId::new("binance").expect("venue should be valid"),
            symbol,
            1,
        )
        .expect("instrument should be valid"),
    )
}

fn observation_for(
    entity: FeatureEntity,
    value: &str,
    event_time: i64,
    as_known_at: i64,
    finalized: bool,
) -> PriceObservation {
    let digest_byte =
        u8::try_from(event_time.rem_euclid(254) + 1).expect("fixture remainder is a nonzero byte");
    PriceObservation::try_new(PriceObservationInput {
        entity,
        price: price(value),
        event_time: UnixNanos::new(event_time),
        as_known_at: UnixNanos::new(as_known_at),
        finalized,
        lineage_hash: [digest_byte; 32],
    })
    .expect("observation should be valid")
}

fn observation(
    value: &str,
    event_time: i64,
    as_known_at: i64,
    finalized: bool,
) -> PriceObservation {
    observation_for(entity("BTCUSDT"), value, event_time, as_known_at, finalized)
}

fn window_with_bounds(start: i64, end: i64, values: &[(&str, i64)]) -> PriceWindow {
    PriceWindow::try_new(
        TimeWindow::try_new(UnixNanos::new(start), UnixNanos::new(end))
            .expect("time window should be valid"),
        entity("BTCUSDT"),
        AS_OF,
        values
            .iter()
            .map(|(value, time)| observation(value, *time, *time + 1, true))
            .collect(),
    )
    .expect("window should be point-in-time valid")
}

fn window(values: &[(&str, i64)]) -> PriceWindow {
    window_with_bounds(600, 1_000, values)
}

fn closing(value: &str, event_time: i64) -> PriceObservation {
    observation(value, event_time, event_time + 1, true)
}

#[test]
fn point_in_time_window_enforces_entity_half_open_time_lineage_and_capacity() {
    let time_window = TimeWindow::try_new(UnixNanos::new(600), UnixNanos::new(1_000))
        .expect("window should be valid");
    assert_eq!(
        PriceWindow::try_new(
            time_window,
            entity("BTCUSDT"),
            AS_OF,
            vec![observation("100", 1_000, 1_001, true)],
        ),
        Err(FeatureComputationError::OutsideWindow)
    );
    assert_eq!(
        PriceWindow::try_new(
            time_window,
            entity("BTCUSDT"),
            AS_OF,
            vec![observation_for(entity("ETHUSDT"), "100", 900, 901, true)],
        ),
        Err(FeatureComputationError::MixedEntity)
    );
    assert_eq!(
        PriceWindow::try_new(
            time_window,
            entity("BTCUSDT"),
            AS_OF,
            vec![
                observation("100", 900, 901, true),
                observation("101", 899, 902, true),
            ],
        ),
        Err(FeatureComputationError::NonMonotonicTime)
    );
    let duplicate = observation("100", 900, 901, true);
    let mut second = observation("101", 910, 911, true);
    second = PriceObservation::try_new(PriceObservationInput {
        entity: entity("BTCUSDT"),
        price: second.price(),
        event_time: second.event_time(),
        as_known_at: second.as_known_at(),
        finalized: second.is_finalized(),
        lineage_hash: *duplicate.lineage_hash(),
    })
    .expect("fixture observation is valid");
    assert_eq!(
        PriceWindow::try_new(
            time_window,
            entity("BTCUSDT"),
            AS_OF,
            vec![duplicate, second],
        ),
        Err(FeatureComputationError::DuplicateLineage)
    );
    let oversized = (0..=feature_engine::features::MAX_PRICE_OBSERVATIONS)
        .map(|index| {
            let time = i64::try_from(index + 1).expect("index fits");
            PriceObservation::try_new(PriceObservationInput {
                entity: entity("BTCUSDT"),
                price: price("100"),
                event_time: UnixNanos::new(time),
                as_known_at: UnixNanos::new(time + 1),
                finalized: true,
                lineage_hash: blake3::hash(&time.to_be_bytes()).into(),
            })
            .expect("fixture observation is valid")
        })
        .collect();
    assert_eq!(
        PriceWindow::try_new(time_window, entity("BTCUSDT"), AS_OF, oversized),
        Err(FeatureComputationError::CapacityExceeded)
    );
}

#[test]
fn returns_ranges_vwap_and_multi_horizon_results_match_hand_calculation() {
    assert!(
        (log_return(price("110"), price("100")).expect("prices are valid")
            - 0.095_310_179_804_324_93)
            .abs()
            < EPSILON
    );
    assert!(
        (relative_distance(price("105"), price("100")).expect("prices are valid") - 0.05).abs()
            < EPSILON
    );
    let prices = window(&[("100", 600), ("110", 700), ("121", 800), ("133.1", 900)]);
    assert!((cumulative_return(&prices).expect("history exists") - 0.331).abs() < EPSILON);
    assert!((high_low_range(&prices).expect("history exists") - 0.331).abs() < EPSILON);
    assert!(
        vwap_distance(
            price("110"),
            &[
                (price("999"), quantity("0")),
                (price("100"), quantity("1")),
                (price("120"), quantity("1")),
            ],
        )
        .expect("VWAP is valid")
        .abs()
            < EPSILON
    );
    assert_eq!(
        vwap_distance(price("110"), &[(price("100"), quantity("0"))]),
        Err(FeatureComputationError::ZeroDenominator)
    );
    let returns = multi_horizon_log_returns(&prices, &[1, 2, 3]).expect("horizons are valid");
    assert_eq!(returns.len(), 3);
    assert!(
        (returns[0].value() - 0.095_310_179_804_324_93).abs() < EPSILON
            && (returns[2].value() - 0.285_930_539_412_974_8).abs() < EPSILON
    );
}

#[test]
fn trend_drawdown_runup_and_distribution_features_match_reference_paths() {
    let prices = window(&[("100", 600), ("120", 700), ("90", 800), ("108", 900)]);
    assert!((maximum_drawdown(&prices).expect("history is sufficient") - 0.25).abs() < EPSILON);
    assert!((maximum_run_up(&prices).expect("history is sufficient") - 0.2).abs() < EPSILON);
    let irregular = window_with_bounds(600, 1_000, &[("100", 600), ("110", 700), ("150", 900)]);
    assert!(time_weighted_trend_slope(&irregular).expect("history is sufficient") > 0.0);
    assert!(time_weighted_trend_acceleration(&irregular).expect("history is sufficient") > 0.0);

    let linear = [1.0, 2.0, 3.0, 4.0];
    assert!((trend_slope(&linear).expect("history is sufficient") - 1.0).abs() < EPSILON);
    assert!(
        trend_acceleration(&linear)
            .expect("history is sufficient")
            .abs()
            < EPSILON
    );
    assert!(
        (return_autocorrelation(&linear, 1).expect("history is sufficient") - 1.0).abs() < EPSILON
    );
    assert!(
        rolling_skewness(&[-1.0, 0.0, 1.0])
            .expect("history is sufficient")
            .abs()
            < EPSILON
    );
    assert!(
        (rolling_excess_kurtosis(&[-1.0, -1.0, 1.0, 1.0]).expect("history is sufficient") + 2.0)
            .abs()
            < EPSILON
    );
    assert!(variance_ratio(&[1.0, -1.0, 1.0, -1.0], 2).expect("history is sufficient") < EPSILON);
    assert!(
        (return_reversal(&[0.1, -0.04, -0.2, 0.05], 0.1).expect("events exist") - 0.045).abs()
            < EPSILON
    );
}

#[test]
fn exact_prices_flow_to_realized_and_range_measures() {
    let prices = window(&[("100", 600), ("110", 700), ("99", 800), ("108.9", 900)]);
    let policy = VolatilityFormulaPolicy::try_new(100, None).expect("policy is valid");
    let returns = consecutive_log_returns(&prices).expect("returns are valid");
    let measures = RealizedMeasures::from_price_window(&prices, &closing("108.9", 1_000), policy)
        .expect("sampling and prices are valid");
    assert!(
        (measures.realized_variance()
            - volatility::realized_variance(&returns).expect("returns are finite"))
        .abs()
            < EPSILON
    );
    assert!(measures.realized_volatility().is_finite());
    assert!(
        (measures.downside_semivariance() + measures.upside_semivariance()
            - measures.realized_variance())
        .abs()
            < EPSILON
    );
    assert!(measures.bipower_variation() >= 0.0 && measures.jump_variation() >= 0.0);

    let bar = PriceOhlcBar::try_new(price("100"), price("110"), price("90"), price("105"))
        .expect("exact OHLC is valid");
    assert!(parkinson_variance(&[bar]).expect("bar is valid") > 0.0);
    assert!(garman_klass_variance(&[bar]).expect("bar is valid") > 0.0);
    assert!(PriceOhlcBar::try_new(price("100"), price("99"), price("90"), price("105")).is_err());
}

#[test]
fn sampling_formula_identity_and_seasonality_are_point_in_time() {
    let base = VolatilityFormulaPolicy::try_new(100, None).expect("policy is valid");
    let other_cadence = VolatilityFormulaPolicy::try_new(50, None).expect("policy is valid");
    let annualized = VolatilityFormulaPolicy::try_new(100, Some(365)).expect("policy is valid");
    assert_ne!(base.formula_hash(), other_cadence.formula_hash());
    assert_ne!(base.formula_hash(), annualized.formula_hash());

    let prices = window(&[("100", 600), ("101", 700), ("102", 800)]);
    let baseline = SeasonalBaseline::try_new(2.0, 500, 550, [8; 32]).expect("baseline is valid");
    assert_eq!(
        seasonality_adjusted_volatility(0.4, baseline, &prices),
        Ok(0.2)
    );
    let leaked =
        SeasonalBaseline::try_new(2.0, 650, 651, [8; 32]).expect("baseline is internally valid");
    assert_eq!(
        seasonality_adjusted_volatility(0.4, leaked, &prices),
        Err(FeatureComputationError::InvalidParameter)
    );
    assert_eq!(
        RealizedMeasures::from_price_window(&prices, &closing("103", 1_000), other_cadence,),
        Err(FeatureComputationError::SequenceGap)
    );

    let incomplete = window_with_bounds(600, 900, &[("100", 600), ("101", 700)]);
    assert_eq!(
        RealizedMeasures::from_price_window(&incomplete, &closing("102", 900), base),
        Err(FeatureComputationError::SequenceGap)
    );
    let phase_shifted = window_with_bounds(600, 900, &[("100", 610), ("101", 710), ("102", 810)]);
    assert_eq!(
        RealizedMeasures::from_price_window(&phase_shifted, &closing("103", 900), base),
        Err(FeatureComputationError::SequenceGap)
    );
}

#[test]
fn ewma_volatility_uses_registry_bound_half_life_state() {
    let definition = WindowDefinition::try_new_exponentially_weighted(
        WindowId::new("ewma_100ns").expect("window ID is valid"),
        DurationNanos::new(100),
    )
    .expect("EWMA definition is valid");
    let key = WatermarkKey::new(
        source("binance"),
        PartitionId::new("btc").expect("partition ID is valid"),
    );
    let tracker = WatermarkTracker::try_new(
        vec![PartitionConfig::required(key)],
        DurationNanos::new(0),
        vec![SourceHealthState::Healthy],
    )
    .expect("tracker policy is valid");
    let mut state = HalfLifeEwmaState::try_from_definition(&definition, 8, &tracker)
        .expect("state is registry bound");
    state
        .update_at(UnixNanos::new(100), 0.01)
        .expect("first squared return is valid");
    state
        .update_at(UnixNanos::new(200), 0.04)
        .expect("second squared return is valid");
    assert!(
        (ewma_volatility_from_state(&state).expect("state has history") - 0.158_113_883_008_418_97)
            .abs()
            < EPSILON
    );
}

#[test]
fn previous_interval_gap_requires_finalized_point_in_time_inputs() {
    let previous = observation("100", 900, 901, true);
    let current = observation("105", 910, 911, true);
    assert!(
        (finalized_interval_gap(&previous, &current).expect("both are final") - 0.05).abs()
            < EPSILON
    );
    let provisional = observation("105", 910, 911, false);
    assert_eq!(
        finalized_interval_gap(&previous, &provisional),
        Err(FeatureComputationError::WindowNotFinal)
    );
}

fn source(name: &str) -> SourceId {
    SourceId::new(SourceKind::Exchange, name, 1).expect("source should be valid")
}

fn asset(namespace: AssetNamespace, chain: &str, symbol: &str) -> AssetId {
    AssetId::new(namespace, chain, "", symbol, 1).expect("asset should be valid")
}

fn btc() -> AssetId {
    asset(AssetNamespace::Native, "bitcoin", "BTC")
}

fn eth() -> AssetId {
    asset(AssetNamespace::Native, "ethereum", "ETH")
}

fn usd() -> AssetId {
    asset(AssetNamespace::Fiat, "", "USD")
}

fn consolidated_instrument_for(venue: &str, base_asset: AssetId) -> InstrumentDefinition {
    let symbol = format!("{}USD", base_asset.canonical_symbol());
    InstrumentDefinition::new(InstrumentDefinitionInput {
        id: domain::InstrumentId::new_for_product(
            VenueId::new(venue).expect("venue should be valid"),
            symbol,
            ProductType::Spot,
            1,
        )
        .expect("instrument should be valid"),
        product_type: ProductType::Spot,
        base_asset,
        quote_asset: usd(),
        settlement_asset: usd(),
        contract_multiplier: FixedDecimal::parse_canonical("1").expect("decimal is valid"),
        contract_value_unit: ContractValueUnit::Base,
        contract_kind: ContractKind::None,
        expiry_time: None,
        strike: None,
        option_side: None,
        price_tick: price("0.01"),
        quantity_step: quantity("0.001"),
        listing_time: UnixNanos::new(1),
        delisting_time: None,
    })
    .expect("instrument should be valid")
}

fn fair_price_estimator() -> FairPriceEstimator {
    fair_price_estimator_for(btc())
}

fn fair_price_estimator_for(base_asset: AssetId) -> FairPriceEstimator {
    FairPriceEstimator::try_new_with_eligible_sources(
        EstimatorConfigInput {
            policy_id: PolicyId::new("fair-price-v1").expect("policy ID should be valid"),
            base_asset,
            reference_asset: usd(),
            product_type: ProductType::Spot,
            minimum_venues: 2,
            maximum_venues: 8,
            quote_ttl: DurationNanos::new(5_000_000_000),
            conversion_ttl: DurationNanos::new(5_000_000_000),
            depth_cap_reference: notional("1000"),
            minimum_quality: Ppm::new(500_000).expect("quality should be valid"),
            minimum_freshness: Ppm::new(500_000).expect("freshness should be valid"),
            maximum_venue_weight: Ppm::new(500_000).expect("weight should be valid"),
            outlier_threshold: Ppm::new(200_000).expect("threshold should be valid"),
            minimum_conversion_sources: 2,
            maximum_conversion_interval_width: Ppm::new(100_000)
                .expect("interval width should be valid"),
            require_catalog_provenance: false,
        },
        vec![source("alpha"), source("beta")],
    )
    .expect("estimator should be valid")
}

fn consolidated_quote(venue: &str, midpoint: &str, event_time: i64) -> VenueQuote {
    consolidated_quote_for_asset(venue, midpoint, event_time, btc())
}

fn consolidated_quote_for_asset(
    venue: &str,
    midpoint: &str,
    event_time: i64,
    base_asset: AssetId,
) -> VenueQuote {
    let midpoint = FixedDecimal::parse_canonical(midpoint).expect("midpoint should be valid");
    let half = FixedDecimal::parse_canonical("0.5").expect("spread should be valid");
    VenueQuote::try_new(VenueQuoteInput {
        source: source(venue),
        instrument: consolidated_instrument_for(venue, base_asset),
        candidate_kind: CandidatePriceKind::Midpoint,
        bid: Price::new(midpoint.checked_sub(half).expect("bid should calculate"))
            .expect("bid should be valid"),
        ask: Price::new(midpoint.checked_add(half).expect("ask should calculate"))
            .expect("ask should be valid"),
        executable_quantity: quantity("10"),
        quality: Ppm::new(900_000).expect("quality should be valid"),
        freshness: Ppm::new(900_000).expect("freshness should be valid"),
        source_health: SourceHealthState::Healthy,
        metadata_status: MetadataStatus::Current,
        venue_status: VenueTradingState::Normal,
        clock_healthy: true,
        parser_healthy: true,
        book_healthy: true,
        event_time: UnixNanos::new(event_time),
        as_known_at: UnixNanos::new(event_time + 1),
        conversion: None,
        instrument_provenance: InstrumentProvenance::direct_definition(),
    })
    .expect("quote should be valid")
}

fn consolidated_observation(
    estimator: &FairPriceEstimator,
    midpoint: &str,
    event_time: i64,
    reverse_quotes: bool,
) -> PriceObservation {
    consolidated_observation_for_asset(estimator, midpoint, event_time, reverse_quotes, btc())
}

fn consolidated_observation_for_asset(
    estimator: &FairPriceEstimator,
    midpoint: &str,
    event_time: i64,
    reverse_quotes: bool,
    base_asset: AssetId,
) -> PriceObservation {
    let mut quotes = vec![
        consolidated_quote_for_asset("alpha", midpoint, event_time, base_asset.clone()),
        consolidated_quote_for_asset("beta", midpoint, event_time, base_asset),
    ];
    if reverse_quotes {
        quotes.reverse();
    }
    let fair_price = match estimator
        .estimate(UnixNanos::new(event_time + 1), &quotes)
        .expect("consolidation should succeed")
    {
        ConsolidatedOutcome::Available(fair_price) => fair_price,
        ConsolidatedOutcome::Abstained { .. } => panic!("two healthy venues should be available"),
    };
    PriceObservation::from_consolidated(&fair_price)
        .expect("consolidated observation should be valid")
}

fn production_window(reverse_quotes: bool) -> (PriceWindow, PriceObservation, WatermarkTracker) {
    production_window_for_asset(
        btc(),
        [
            ("100", 600_000_000_000),
            ("101", 660_000_000_000),
            ("100", 720_000_000_000),
            ("102", 780_000_000_000),
            ("101", 840_000_000_000),
        ],
        "103",
        reverse_quotes,
    )
}

fn production_window_for_asset(
    base_asset: AssetId,
    values: [(&str, i64); 5],
    closing_value: &str,
    reverse_quotes: bool,
) -> (PriceWindow, PriceObservation, WatermarkTracker) {
    const START: i64 = 600_000_000_000;
    const END: i64 = 900_000_000_000;
    finalized_fair_window_for_asset(
        base_asset,
        START,
        END,
        &values,
        closing_value,
        reverse_quotes,
    )
}

fn finalized_fair_window_for_asset(
    base_asset: AssetId,
    start: i64,
    end: i64,
    values: &[(&str, i64)],
    closing_value: &str,
    reverse_quotes: bool,
) -> (PriceWindow, PriceObservation, WatermarkTracker) {
    let time_window = TimeWindow::try_new(UnixNanos::new(start), UnixNanos::new(end))
        .expect("production window is valid");
    let estimator = fair_price_estimator_for(base_asset.clone());
    let observations = values
        .iter()
        .map(|(value, time)| {
            consolidated_observation_for_asset(
                &estimator,
                value,
                *time,
                reverse_quotes,
                base_asset.clone(),
            )
        })
        .collect();
    let closing = consolidated_observation_for_asset(
        &estimator,
        closing_value,
        end,
        reverse_quotes,
        base_asset.clone(),
    );

    let partition = format!("{}-usd", base_asset.canonical_symbol().to_ascii_lowercase());
    let alpha = WatermarkKey::new(
        source("alpha"),
        PartitionId::new(partition.clone()).expect("partition should be valid"),
    );
    let beta = WatermarkKey::new(
        source("beta"),
        PartitionId::new(partition).expect("partition should be valid"),
    );
    let mut tracker = WatermarkTracker::try_new_for_entity(
        vec![
            PartitionConfig::required(alpha.clone()),
            PartitionConfig::required(beta.clone()),
        ],
        DurationNanos::new(5_000_000_000),
        vec![SourceHealthState::Healthy, SourceHealthState::Recovering],
        FeatureEntity::Asset(base_asset.clone()),
    )
    .expect("watermark tracker should be valid");
    let watermark_order = if reverse_quotes {
        [&beta, &alpha]
    } else {
        [&alpha, &beta]
    };
    for key in watermark_order {
        tracker
            .advance(
                key,
                WatermarkUpdate::new(
                    UnixNanos::new(end + 5_000_000_000),
                    UnixNanos::new(end + 5_000_000_001),
                    SourceHealthState::Healthy,
                ),
            )
            .expect("watermark should advance");
    }
    let decision = tracker.decision(time_window);
    assert_eq!(decision.state(), Finalization::Final);
    let window = PriceWindow::try_from_finalization(
        FeatureEntity::Asset(base_asset),
        &tracker,
        decision,
        observations,
    )
    .expect("production price window should be valid");
    (window, closing, tracker)
}

fn normalized_trade(
    venue: &str,
    base_asset: AssetId,
    value: &str,
    size: &str,
    event_time: i64,
    ordinal: u8,
) -> TradeObservation {
    let instrument = consolidated_instrument_for(venue, base_asset);
    let envelope = EventEnvelope::new(
        UncheckedEventMetadata {
            schema_version: 3,
            source: source(venue),
            venue: Some(VenueId::new(venue).expect("venue should be valid")),
            instrument_id: Some(instrument.id().clone()),
            exchange_timestamp: Some(UnixNanos::new(event_time)),
            exchange_transaction_timestamp: Some(UnixNanos::new(event_time)),
            receive_wall_timestamp: UnixNanos::new(event_time + 1),
            receive_monotonic_ns: u64::from(ordinal),
            normalization_timestamp: UnixNanos::new(event_time + 2),
            connection_started_at: UnixNanos::new(1),
            sequence_number: None,
            previous_sequence_number: None,
            connection_epoch: 1,
            subscription_epoch: 1,
            snapshot_kind: SnapshotKind::NotApplicable,
            source_checksum: None,
            raw_payload_hash: [ordinal.max(1); 32],
            parser_version: "parser-v1".to_owned(),
            normalizer_version: "normalizer-v1".to_owned(),
            ingestion_instance: "test-ingestion".to_owned(),
            quality_score_ppm: 950_000,
            quality_flags: QualityFlags::NONE,
        },
        UncheckedEventPayload::Trade(Trade {
            trade_id: format!("trade-{venue}-{ordinal}"),
            price: price(value),
            quantity: quantity(size),
            side: Side::Buy,
        }),
    )
    .expect("normalized trade event should be valid");
    TradeObservation::try_from_event(&envelope, &instrument)
        .expect("trade observation should derive from the event")
}

#[test]
fn finalized_window_requires_the_same_source_universe_as_consolidated_inputs() {
    let time_window = TimeWindow::try_new(
        UnixNanos::new(600_000_000_000),
        UnixNanos::new(900_000_000_000),
    )
    .expect("window should be valid");
    let wrong_key = WatermarkKey::new(
        source("gamma"),
        PartitionId::new("btc-usd").expect("partition should be valid"),
    );
    let mut tracker = WatermarkTracker::try_new_for_entity(
        vec![PartitionConfig::required(wrong_key.clone())],
        DurationNanos::new(5_000_000_000),
        vec![SourceHealthState::Healthy],
        FeatureEntity::Asset(btc()),
    )
    .expect("tracker should be valid");
    tracker
        .advance(
            &wrong_key,
            WatermarkUpdate::new(
                UnixNanos::new(905_000_000_000),
                UnixNanos::new(905_000_000_001),
                SourceHealthState::Healthy,
            ),
        )
        .expect("watermark should advance");
    let estimator = fair_price_estimator();
    let observation = consolidated_observation(&estimator, "100", 600_000_000_000, false);
    assert_eq!(
        PriceWindow::try_from_finalization(
            FeatureEntity::Asset(btc()),
            &tracker,
            tracker.decision(time_window),
            vec![observation],
        ),
        Err(FeatureComputationError::UntrustedInput)
    );
}

#[test]
fn consolidated_observation_derives_latest_source_event_time() {
    let estimator = fair_price_estimator();
    let fair_price = match estimator
        .estimate(
            UnixNanos::new(603),
            &[
                consolidated_quote("alpha", "100", 600),
                consolidated_quote("beta", "100", 602),
            ],
        )
        .expect("consolidation should succeed")
    {
        ConsolidatedOutcome::Available(fair_price) => fair_price,
        ConsolidatedOutcome::Abstained { .. } => panic!("healthy venues should be available"),
    };
    let observation =
        PriceObservation::from_consolidated(&fair_price).expect("observation should be valid");
    assert_eq!(observation.event_time(), UnixNanos::new(602));
}

fn emission_input() -> FloatFeatureEmissionInput {
    emission_input_at(1_100_000_000_000)
}

fn emission_input_at(computed_at: i64) -> FloatFeatureEmissionInput {
    FloatFeatureEmissionInput {
        computed_at: UnixNanos::new(computed_at),
        revision: ObservationRevision::new(1).expect("revision should be valid"),
        code_commit: CodeRevision::new("0123456789abcdef0123456789abcdef01234567")
            .expect("revision should be valid"),
    }
}

fn emitted_realized_volatility(
    start: i64,
    values: &[&str],
    closing_value: &str,
) -> FeatureObservation {
    let end =
        start + i64::try_from(values.len()).expect("fixture length should fit") * 60_000_000_000;
    let timed_values = values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            (
                *value,
                start + i64::try_from(index).expect("fixture index should fit") * 60_000_000_000,
            )
        })
        .collect::<Vec<_>>();
    let (window, closing, tracker) =
        finalized_fair_window_for_asset(btc(), start, end, &timed_values, closing_value, false);
    let mut registry = FeatureRegistry::new();
    registry
        .register(realized_volatility_v2_definition().expect("definition should be valid"))
        .expect("definition should register");
    emit_float_feature(
        &registry,
        emission_input_at(end + 10_000_000_000),
        compute_realized_volatility_v2(&window, &closing, &tracker)
            .expect("realized volatility should compute"),
    )
    .expect("realized volatility should emit")
}

#[test]
fn emission_is_bound_to_computed_window_formula_and_expected_missingness() {
    let mut registry = FeatureRegistry::new();
    registry
        .register(realized_volatility_v2_definition().expect("production definition is valid"))
        .expect("definition should register");
    let (prices, closing, tracker) = production_window(false);
    let computed = compute_realized_volatility_v2(&prices, &closing, &tracker)
        .expect("trusted finalized measure should compute");
    let present = emit_float_feature(&registry, emission_input(), computed.clone())
        .expect("registered value should emit");
    assert!(matches!(
        present.datum(),
        FeatureDatum::Present(FeatureValue::Float64(value))
            if value.value().is_finite()
    ));
    assert_eq!(present.entity(), prices.entity());
    assert_eq!(present.event_time_start(), prices.time_window().start());
    assert_eq!(present.event_time_end(), prices.time_window().end());
    assert_eq!(present.watermark(), Some(UnixNanos::new(905_000_000_000)));
    assert_eq!(present.finality_state(), FinalityState::Final);
    assert_eq!(present.source_coverage().coverage_millionths(), 1_000_000);
    assert_eq!(present.quality_score().millionths(), 900_000);
    let mut other_commit = emission_input();
    other_commit.code_commit = CodeRevision::new("fedcba9876543210fedcba9876543210fedcba98")
        .expect("revision should be valid");
    let other_commit = emit_float_feature(&registry, other_commit, computed.clone())
        .expect("same feature at another code revision should emit");
    assert_ne!(present.lineage_hash(), other_commit.lineage_hash());

    assert_eq!(
        AnalyticalFeature::from_result(Err(FeatureComputationError::CapacityExceeded)),
        Err(FeatureComputationError::CapacityExceeded)
    );
    assert_eq!(
        AnalyticalFeature::from_result(Err(FeatureComputationError::FutureKnowledge)),
        Err(FeatureComputationError::FutureKnowledge)
    );
}

#[test]
fn realized_measure_recipe_family_emits_registered_observations() {
    let (window, closing, tracker) = production_window(false);
    let mut registry = FeatureRegistry::new();
    for definition in task_four_definitions().expect("catalogue should be valid") {
        registry
            .register(definition)
            .expect("definition should register");
    }
    for kind in [
        Task4FeatureKind::RealizedVariance,
        Task4FeatureKind::RealizedVolatility,
        Task4FeatureKind::UpsideSemivariance,
        Task4FeatureKind::DownsideSemivariance,
        Task4FeatureKind::BipowerVariation,
        Task4FeatureKind::JumpVariation,
    ] {
        let computed = compute_realized_measure_feature(kind, &window, &closing, &tracker)
            .expect("recipe should compute");
        let observation =
            emit_float_feature(&registry, emission_input(), computed).expect("recipe should emit");
        assert_eq!(observation.feature_id().as_str(), kind.id());
        assert!(matches!(
            observation.datum(),
            FeatureDatum::Present(FeatureValue::Float64(value)) if value.value().is_finite()
        ));
    }
}

#[test]
fn exponentially_weighted_recipe_uses_the_registry_half_life_window() {
    let (window, closing, tracker) = production_window(false);
    let mut registry = FeatureRegistry::new();
    registry
        .register(
            Task4FeatureKind::ExponentiallyWeightedVolatility
                .definition()
                .expect("definition should be valid"),
        )
        .expect("definition should register");
    let observation = emit_float_feature(
        &registry,
        emission_input(),
        compute_exponentially_weighted_volatility(&window, &closing, &tracker)
            .expect("EWMA recipe should compute"),
    )
    .expect("EWMA recipe should emit");
    assert_eq!(
        observation.window_id(),
        &WindowId::new("ewma_5m").expect("window ID should be valid")
    );
    assert!(matches!(
        observation.datum(),
        FeatureDatum::Present(FeatureValue::Float64(value)) if value.value() >= 0.0
    ));
}

#[test]
fn rolling_and_anchored_vwap_use_finalized_normalized_trade_evidence() {
    const ROLLING_START: i64 = 600_000_000_000;
    let (rolling_prices, rolling_close, rolling_tracker) = production_window(false);
    let rolling_trades = FinalizedTradeWindow::try_from_finalization(
        btc(),
        &rolling_tracker,
        rolling_tracker.decision(rolling_prices.time_window()),
        vec![
            normalized_trade(
                "alpha",
                btc(),
                "100",
                "2",
                ROLLING_START + 30_000_000_000,
                1,
            ),
            normalized_trade("beta", btc(), "104", "1", ROLLING_START + 90_000_000_000, 2),
        ],
    )
    .expect("rolling trades should finalize");

    const DAY: i64 = 86_400_000_000_000;
    let (anchored_prices, anchored_close, anchored_tracker) = finalized_fair_window_for_asset(
        btc(),
        DAY,
        2 * DAY,
        &[("100", DAY), ("104", DAY + DAY / 2)],
        "106",
        false,
    );
    let anchored_trades = FinalizedTradeWindow::try_from_finalization(
        btc(),
        &anchored_tracker,
        anchored_tracker.decision(anchored_prices.time_window()),
        vec![
            normalized_trade("alpha", btc(), "100", "1", DAY + 1_000, 3),
            normalized_trade("beta", btc(), "104", "3", DAY + 2_000, 4),
        ],
    )
    .expect("anchored trades should finalize");

    for (kind, trades, prices, closing, tracker, input, expected_window) in [
        (
            Task4FeatureKind::RollingVwapDistance,
            &rolling_trades,
            &rolling_prices,
            &rolling_close,
            &rolling_tracker,
            emission_input(),
            "rolling_5m",
        ),
        (
            Task4FeatureKind::AnchoredVwapDistance,
            &anchored_trades,
            &anchored_prices,
            &anchored_close,
            &anchored_tracker,
            emission_input_at(3 * DAY),
            "utc_day",
        ),
    ] {
        let mut registry = FeatureRegistry::new();
        registry
            .register(kind.definition().expect("definition should be valid"))
            .expect("definition should register");
        let observation = emit_float_feature(
            &registry,
            input,
            compute_vwap_distance_feature(kind, trades, prices, closing, tracker)
                .expect("VWAP recipe should compute"),
        )
        .expect("VWAP recipe should emit");
        assert_eq!(
            observation.window_id(),
            &WindowId::new(expected_window).expect("window ID should be valid")
        );
        assert_eq!(
            observation.source_coverage().coverage_millionths(),
            1_000_000
        );
        assert_eq!(observation.quality_score().millionths(), 900_000);
    }

    let mut fair_distance_registry = FeatureRegistry::new();
    fair_distance_registry
        .register(
            Task4FeatureKind::ConsolidatedFairPriceDistance
                .definition()
                .expect("definition should be valid"),
        )
        .expect("definition should register");
    let fair_distance = emit_float_feature(
        &fair_distance_registry,
        emission_input(),
        compute_consolidated_fair_price_distance(
            &rolling_trades,
            &rolling_prices,
            &rolling_close,
            &rolling_tracker,
        )
        .expect("fair-price distance should compute"),
    )
    .expect("fair-price distance should emit");
    assert!(matches!(
        fair_distance.datum(),
        FeatureDatum::Present(FeatureValue::Float64(value)) if value.value().is_finite()
    ));

    let incomplete_trades = FinalizedTradeWindow::try_from_finalization(
        btc(),
        &rolling_tracker,
        rolling_tracker.decision(rolling_prices.time_window()),
        vec![normalized_trade(
            "alpha",
            btc(),
            "100",
            "2",
            ROLLING_START + 30_000_000_000,
            5,
        )],
    )
    .expect("partial trade coverage should remain representable");
    assert!(matches!(
        emit_float_feature(
            &fair_distance_registry,
            emission_input(),
            compute_consolidated_fair_price_distance(
                &incomplete_trades,
                &rolling_prices,
                &rolling_close,
                &rolling_tracker,
            )
            .expect("calculation can preserve partial coverage"),
        ),
        Err(feature_engine::features::FeatureEmissionError::Registry(
            feature_registry::RegistryError::CoverageBelowRequirement
        ))
    ));
}

#[test]
fn finalized_interval_gap_binds_both_adjacent_window_decisions() {
    const FIRST_START: i64 = 600_000_000_000;
    const FIRST_END: i64 = 900_000_000_000;
    const SECOND_END: i64 = 1_200_000_000_000;
    let (previous, previous_close, previous_tracker) = production_window(false);
    let (current, _current_close, current_tracker) = finalized_fair_window_for_asset(
        btc(),
        FIRST_END,
        SECOND_END,
        &[
            ("105", FIRST_END),
            ("106", FIRST_END + 60_000_000_000),
            ("104", FIRST_END + 120_000_000_000),
            ("107", FIRST_END + 180_000_000_000),
            ("108", FIRST_END + 240_000_000_000),
        ],
        "109",
        false,
    );
    assert_eq!(previous.time_window().start(), UnixNanos::new(FIRST_START));
    let mut registry = FeatureRegistry::new();
    registry
        .register(
            Task4FeatureKind::FinalizedIntervalGap
                .definition()
                .expect("definition should be valid"),
        )
        .expect("definition should register");
    let observation = emit_float_feature(
        &registry,
        emission_input_at(1_300_000_000_000),
        compute_finalized_interval_gap_feature(
            (&previous, &previous_close, &previous_tracker),
            (&current, &current_tracker),
        )
        .expect("adjacent finalized intervals should compute"),
    )
    .expect("gap should emit");
    assert!(matches!(
        observation.datum(),
        FeatureDatum::Present(FeatureValue::Float64(value))
            if (value.value() - (105.0 / 103.0 - 1.0)).abs() < EPSILON
    ));
}

#[test]
fn price_path_recipe_family_emits_from_the_complete_finalized_grid() {
    const START: i64 = 600_000_000_000;
    let (window, closing, tracker) = production_window_for_asset(
        btc(),
        [
            ("100", START),
            ("106", START + 60_000_000_000),
            ("100", START + 120_000_000_000),
            ("107", START + 180_000_000_000),
            ("101", START + 240_000_000_000),
        ],
        "108",
        false,
    );
    let mut registry = FeatureRegistry::new();
    for kind in [
        Task4FeatureKind::LogReturn,
        Task4FeatureKind::CumulativeReturn,
        Task4FeatureKind::HighLowRange,
        Task4FeatureKind::OpenCloseRange,
        Task4FeatureKind::TrendSlope,
        Task4FeatureKind::TrendAcceleration,
        Task4FeatureKind::MaximumDrawdown,
        Task4FeatureKind::MaximumRunUp,
        Task4FeatureKind::ReturnAutocorrelation,
        Task4FeatureKind::VarianceRatio,
        Task4FeatureKind::RollingSkewness,
        Task4FeatureKind::RollingExcessKurtosis,
        Task4FeatureKind::ReturnReversal,
    ] {
        registry
            .register(kind.definition().expect("definition should be valid"))
            .expect("definition should register");
        let observation = emit_float_feature(
            &registry,
            emission_input(),
            compute_price_path_feature(kind, &window, &closing, &tracker)
                .expect("price-path recipe should compute"),
        )
        .expect("price-path recipe should emit");
        assert_eq!(observation.feature_id().as_str(), kind.id());
        assert!(matches!(
            observation.datum(),
            FeatureDatum::Present(FeatureValue::Float64(value)) if value.value().is_finite()
        ));
    }
}

#[test]
fn paired_realized_recipes_are_canonical_and_input_order_independent() {
    const START: i64 = 600_000_000_000;
    let (btc_window, btc_close, btc_tracker) = production_window(false);
    let (eth_window, eth_close, eth_tracker) = production_window_for_asset(
        eth(),
        [
            ("200", START),
            ("204", START + 60_000_000_000),
            ("202", START + 120_000_000_000),
            ("207", START + 180_000_000_000),
            ("205", START + 240_000_000_000),
        ],
        "210",
        true,
    );
    let mut registry = FeatureRegistry::new();
    for kind in [
        Task4FeatureKind::RealizedCovariance,
        Task4FeatureKind::RealizedCorrelation,
    ] {
        registry
            .register(kind.definition().expect("definition should be valid"))
            .expect("definition should register");
        let forward = emit_float_feature(
            &registry,
            emission_input(),
            compute_realized_pair_feature(
                kind,
                (&btc_window, &btc_close, &btc_tracker),
                (&eth_window, &eth_close, &eth_tracker),
            )
            .expect("aligned pair should compute"),
        )
        .expect("pair should emit");
        let reversed = emit_float_feature(
            &registry,
            emission_input(),
            compute_realized_pair_feature(
                kind,
                (&eth_window, &eth_close, &eth_tracker),
                (&btc_window, &btc_close, &btc_tracker),
            )
            .expect("reversed aligned pair should compute"),
        )
        .expect("reversed pair should emit");
        assert_eq!(forward, reversed);
        assert_eq!(forward.entity(), &FeatureEntity::AssetPair(btc(), eth()));
        assert!(matches!(
            forward.datum(),
            FeatureDatum::Present(FeatureValue::Float64(value)) if value.value().is_finite()
        ));
    }
}

#[test]
fn derived_volatility_recipes_preserve_feature_and_model_lineage() {
    let early =
        emitted_realized_volatility(660_000_000_000, &["100", "101", "100", "102", "101"], "103");
    let early_middle =
        emitted_realized_volatility(720_000_000_000, &["100", "102", "100", "103", "101"], "104");
    let middle =
        emitted_realized_volatility(780_000_000_000, &["100", "102", "101", "104", "103"], "105");
    let late_middle =
        emitted_realized_volatility(840_000_000_000, &["100", "103", "101", "104", "102"], "105");
    let short =
        emitted_realized_volatility(900_000_000_000, &["100", "103", "101", "105", "102"], "106");
    let series_window = TimeWindow::try_new(
        UnixNanos::new(900_000_000_000),
        UnixNanos::new(1_200_000_000_000),
    )
    .expect("series window should be valid");
    let series = FinalizedVolatilitySeries::try_new(
        series_window,
        vec![early, early_middle, middle, late_middle, short.clone()],
    )
    .expect("volatility series should be valid");

    let long_values = [
        "100", "101", "100", "102", "101", "103", "102", "104", "103", "105", "104", "106", "105",
        "107", "106",
    ];
    let long = emitted_realized_volatility(300_000_000_000, &long_values, "108");
    let forecast = VolatilityForecastEvidence::try_new(
        btc(),
        series_window,
        0.05,
        UnixNanos::new(800_000_000_000),
        UnixNanos::new(850_000_000_000),
        "har-rv",
        semver::Version::new(1, 2, 0),
        [7; 32],
        [8; 32],
        feature_registry::QualityScore::from_millionths(950_000).expect("quality should be valid"),
    )
    .expect("forecast evidence should be point-in-time valid");
    assert!(matches!(
        VolatilityForecastEvidence::try_new(
            btc(),
            series_window,
            0.05,
            UnixNanos::new(800_000_000_000),
            UnixNanos::new(900_000_000_001),
            "har-rv",
            semver::Version::new(1, 2, 0),
            [7; 32],
            [8; 32],
            feature_registry::QualityScore::from_millionths(950_000)
                .expect("quality should be valid"),
        ),
        Err(FeatureComputationError::FutureKnowledge)
    ));
    let mut drifted_input = short.clone().into_input();
    drifted_input.formula_hash =
        feature_registry::FormulaHash::new([9; 32]).expect("hash should be valid");
    let drifted =
        FeatureObservation::try_new(drifted_input).expect("drifted shape should remain valid");
    assert_eq!(
        compute_volatility_term_ratio(&drifted, &long),
        Err(FeatureComputationError::UntrustedInput)
    );

    let mut registry = FeatureRegistry::new();
    for kind in [
        Task4FeatureKind::VolatilityOfVolatility,
        Task4FeatureKind::VolatilityTermRatio,
        Task4FeatureKind::VolatilityForecastResidual,
    ] {
        registry
            .register(kind.definition().expect("definition should be valid"))
            .expect("definition should register");
    }
    let vol_of_vol = emit_float_feature(
        &registry,
        emission_input_at(1_300_000_000_000),
        compute_volatility_of_volatility(&series).expect("vol-of-vol should compute"),
    )
    .expect("vol-of-vol should emit");
    let term_ratio = emit_float_feature(
        &registry,
        emission_input_at(1_300_000_000_000),
        compute_volatility_term_ratio(&short, &long).expect("term ratio should compute"),
    )
    .expect("term ratio should emit");
    let residual = emit_float_feature(
        &registry,
        emission_input_at(1_300_000_000_000),
        compute_volatility_forecast_residual(&short, &forecast)
            .expect("forecast residual should compute"),
    )
    .expect("forecast residual should emit");
    assert_eq!(
        vol_of_vol.window_id(),
        &WindowId::new("rolling_5m").expect("window should be valid")
    );
    assert_eq!(
        term_ratio.window_id(),
        &WindowId::new("rolling_15m").expect("window should be valid")
    );
    assert_ne!(vol_of_vol.lineage_hash(), residual.lineage_hash());
    for observation in [vol_of_vol, term_ratio, residual] {
        assert!(matches!(
            observation.datum(),
            FeatureDatum::Present(FeatureValue::Float64(value)) if value.value().is_finite()
        ));
    }
}

#[test]
fn range_volatility_emission_is_bound_to_derived_ohlc_lineage() {
    let (window, closing, tracker) = production_window(false);
    let ohlc = FinalizedOhlcWindow::try_from_prices(&window, &closing, &tracker)
        .expect("OHLC evidence should derive from trusted prices");
    let mut registry = FeatureRegistry::new();
    for definition in task_four_definitions().expect("catalogue should be valid") {
        registry
            .register(definition)
            .expect("definition should register");
    }
    for kind in [
        Task4FeatureKind::ParkinsonVolatility,
        Task4FeatureKind::GarmanKlassVolatility,
    ] {
        let computed = compute_range_volatility_feature(kind, &ohlc, &tracker)
            .expect("range recipe should compute");
        let observation = emit_float_feature(&registry, emission_input(), computed)
            .expect("range recipe should emit");
        assert_eq!(observation.feature_id().as_str(), kind.id());
        assert!(matches!(
            observation.datum(),
            FeatureDatum::Present(FeatureValue::Float64(value)) if value.value() >= 0.0
        ));
    }
}

#[test]
fn seasonal_baseline_availability_and_version_enter_output_lineage() {
    let (window, closing, tracker) = production_window(false);
    let fit_window = TimeWindow::try_new(
        UnixNanos::new(100_000_000_000),
        UnixNanos::new(500_000_000_000),
    )
    .expect("fit window should be valid");
    let baseline = |version_hash| {
        SeasonalBaselineEvidence::try_new(
            btc(),
            72,
            fit_window,
            SeasonalBaseline::try_new(2.0, 500_000_000_000, 550_000_000_000, version_hash)
                .expect("baseline should be valid"),
            120,
            feature_registry::QualityScore::from_millionths(925_000)
                .expect("quality should be valid"),
            [9; 32],
        )
        .expect("baseline evidence should be valid")
    };
    let mut registry = FeatureRegistry::new();
    registry
        .register(
            Task4FeatureKind::SeasonalityAdjustedVolatility
                .definition()
                .expect("definition should be valid"),
        )
        .expect("definition should register");
    let first = emit_float_feature(
        &registry,
        emission_input(),
        compute_seasonality_adjusted_volatility(&window, &closing, &tracker, &baseline([7; 32]))
            .expect("seasonal recipe should compute"),
    )
    .expect("seasonal output should emit");
    let second = emit_float_feature(
        &registry,
        emission_input(),
        compute_seasonality_adjusted_volatility(&window, &closing, &tracker, &baseline([8; 32]))
            .expect("seasonal recipe should compute"),
    )
    .expect("seasonal output should emit");
    assert_ne!(first.lineage_hash(), second.lineage_hash());
}

#[test]
fn present_computation_rejects_a_decision_after_tracker_state_changes() {
    let (window, closing, mut tracker) = production_window(false);
    let alpha = WatermarkKey::new(
        source("alpha"),
        PartitionId::new("btc-usd").expect("partition should be valid"),
    );
    tracker
        .advance(
            &alpha,
            WatermarkUpdate::new(
                UnixNanos::new(906_000_000_000),
                UnixNanos::new(906_000_000_001),
                SourceHealthState::Unhealthy,
            ),
        )
        .expect("health update should be recorded");
    assert_eq!(
        compute_realized_volatility_v2(&window, &closing, &tracker),
        Err(FeatureComputationError::UntrustedInput)
    );
}

#[test]
fn empty_finalized_window_emits_honest_missingness_and_rejects_stale_evidence() {
    let time_window = TimeWindow::try_new(
        UnixNanos::new(600_000_000_000),
        UnixNanos::new(900_000_000_000),
    )
    .expect("window should be valid");
    let keys = ["alpha", "beta"].map(|name| {
        WatermarkKey::new(
            source(name),
            PartitionId::new("btc-usd").expect("partition should be valid"),
        )
    });
    let mut tracker = WatermarkTracker::try_new_for_entity(
        keys.iter()
            .cloned()
            .map(PartitionConfig::required)
            .collect(),
        DurationNanos::new(5_000_000_000),
        vec![SourceHealthState::Healthy],
        FeatureEntity::Asset(btc()),
    )
    .expect("tracker should be valid");
    for key in &keys {
        tracker
            .advance(
                key,
                WatermarkUpdate::new(
                    UnixNanos::new(905_000_000_000),
                    UnixNanos::new(905_000_000_001),
                    SourceHealthState::Healthy,
                ),
            )
            .expect("watermark should advance");
    }
    let decision = tracker.decision(time_window);
    let missing = compute_missing_realized_volatility_v2(
        btc(),
        &tracker,
        decision,
        FeatureComputationError::InsufficientHistory,
    )
    .expect("expected absence should produce an evidence-bound result");
    let mut registry = FeatureRegistry::new();
    registry
        .register(realized_volatility_v2_definition().expect("definition should be valid"))
        .expect("definition should register");
    let observation = emit_float_feature(&registry, emission_input(), missing)
        .expect("typed missingness should emit");
    assert_eq!(
        observation.datum().missingness_reason(),
        Some(MissingnessReason::InsufficientHistory)
    );
    assert_eq!(observation.finality_state(), FinalityState::Invalid);
    assert_eq!(
        observation.source_coverage().coverage_millionths(),
        1_000_000
    );
    assert_eq!(observation.quality_score().millionths(), 0);

    tracker
        .advance(
            &keys[0],
            WatermarkUpdate::new(
                UnixNanos::new(906_000_000_000),
                UnixNanos::new(906_000_000_001),
                SourceHealthState::Healthy,
            ),
        )
        .expect("tracker should advance");
    assert_eq!(
        compute_missing_realized_volatility_v2(
            btc(),
            &tracker,
            decision,
            FeatureComputationError::InsufficientHistory,
        ),
        Err(FeatureComputationError::UntrustedInput)
    );
    assert_eq!(
        compute_missing_realized_volatility_v2(
            btc(),
            &tracker,
            tracker.decision(time_window),
            FeatureComputationError::InvalidInput,
        ),
        Err(FeatureComputationError::InvalidInput)
    );
    assert_eq!(
        compute_missing_realized_volatility_v2(
            asset(AssetNamespace::Native, "ethereum", "ETH"),
            &tracker,
            tracker.decision(time_window),
            FeatureComputationError::InsufficientHistory,
        ),
        Err(FeatureComputationError::UntrustedInput)
    );
}

#[test]
fn never_seen_required_sources_emit_disconnected_missingness() {
    let time_window = TimeWindow::try_new(
        UnixNanos::new(600_000_000_000),
        UnixNanos::new(900_000_000_000),
    )
    .expect("window should be valid");
    let tracker = WatermarkTracker::try_new_for_entity(
        vec![PartitionConfig::required(WatermarkKey::new(
            source("alpha"),
            PartitionId::new("btc-usd").expect("partition should be valid"),
        ))],
        DurationNanos::new(5_000_000_000),
        vec![SourceHealthState::Healthy],
        FeatureEntity::Asset(btc()),
    )
    .expect("tracker should be valid");
    let computed = compute_missing_realized_volatility_v2(
        btc(),
        &tracker,
        tracker.decision(time_window),
        FeatureComputationError::SourceDisconnected,
    )
    .expect("never-seen source should produce typed missingness");
    let mut registry = FeatureRegistry::new();
    registry
        .register(realized_volatility_v2_definition().expect("definition should be valid"))
        .expect("definition should register");
    let observation = emit_float_feature(&registry, emission_input(), computed)
        .expect("disconnected missingness should emit");
    assert_eq!(
        observation.datum().missingness_reason(),
        Some(MissingnessReason::SourceDisconnected)
    );
    assert_eq!(observation.source_coverage().coverage_millionths(), 0);
    assert_eq!(observation.watermark(), None);
}

#[test]
fn consolidated_input_order_produces_bit_identical_feature_observations() {
    let mut registry = FeatureRegistry::new();
    registry
        .register(realized_volatility_v2_definition().expect("production definition is valid"))
        .expect("definition should register");
    let (ordered_window, ordered_close, ordered_tracker) = production_window(false);
    let (reversed_window, reversed_close, reversed_tracker) = production_window(true);
    let ordered = compute_realized_volatility_v2(&ordered_window, &ordered_close, &ordered_tracker)
        .expect("ordered output should compute");
    let reversed =
        compute_realized_volatility_v2(&reversed_window, &reversed_close, &reversed_tracker)
            .expect("reversed output should compute");
    assert_eq!(
        emit_float_feature(&registry, emission_input(), ordered)
            .expect("ordered output should emit"),
        emit_float_feature(&registry, emission_input(), reversed)
            .expect("reversed output should emit")
    );
}

#[test]
fn return_reversal_emits_explicit_missingness_when_no_move_qualifies() {
    let (window, closing, tracker) = production_window(false);
    let mut registry = FeatureRegistry::new();
    registry
        .register(
            Task4FeatureKind::ReturnReversal
                .definition()
                .expect("definition should be valid"),
        )
        .expect("definition should register");
    let observation = emit_float_feature(
        &registry,
        emission_input(),
        compute_price_path_feature(
            Task4FeatureKind::ReturnReversal,
            &window,
            &closing,
            &tracker,
        )
        .expect("expected absence should remain an observation"),
    )
    .expect("missing observation should emit");
    assert_eq!(
        observation.datum().missingness_reason(),
        Some(MissingnessReason::InsufficientHistory)
    );
    assert_eq!(observation.finality_state(), FinalityState::Invalid);
}

#[test]
fn volatility_missingness_preserves_the_partial_input_lineage() {
    let first_values = [
        ("100", 600_000_000_000),
        ("101", 660_000_000_000),
        ("102", 780_000_000_000),
        ("101", 840_000_000_000),
    ];
    let second_values = [
        ("100", 600_000_000_000),
        ("101", 660_000_000_000),
        ("105", 780_000_000_000),
        ("101", 840_000_000_000),
    ];
    let first = finalized_fair_window_for_asset(
        btc(),
        600_000_000_000,
        900_000_000_000,
        &first_values,
        "103",
        false,
    );
    let second = finalized_fair_window_for_asset(
        btc(),
        600_000_000_000,
        900_000_000_000,
        &second_values,
        "103",
        false,
    );

    for kind in [
        Task4FeatureKind::RealizedVariance,
        Task4FeatureKind::ExponentiallyWeightedVolatility,
    ] {
        let mut registry = FeatureRegistry::new();
        registry
            .register(kind.definition().expect("definition should be valid"))
            .expect("definition should register");
        let compute = |input: &(PriceWindow, PriceObservation, WatermarkTracker)| match kind {
            Task4FeatureKind::RealizedVariance => {
                compute_realized_measure_feature(kind, &input.0, &input.1, &input.2)
            }
            Task4FeatureKind::ExponentiallyWeightedVolatility => {
                compute_exponentially_weighted_volatility(&input.0, &input.1, &input.2)
            }
            _ => unreachable!("test closes over two volatility kinds"),
        };
        let first_missing = emit_float_feature(
            &registry,
            emission_input(),
            compute(&first).expect("sequence gap should remain an observation"),
        )
        .expect("first missing observation should emit");
        let second_missing = emit_float_feature(
            &registry,
            emission_input(),
            compute(&second).expect("sequence gap should remain an observation"),
        )
        .expect("second missing observation should emit");
        assert_eq!(
            first_missing.datum().missingness_reason(),
            Some(MissingnessReason::SequenceGap)
        );
        assert_eq!(
            second_missing.datum().missingness_reason(),
            Some(MissingnessReason::SequenceGap)
        );
        assert_ne!(
            first_missing.lineage_hash(),
            second_missing.lineage_hash(),
            "{kind:?} must commit partial input lineage"
        );
    }
}

#[test]
fn finality_availability_changes_emitted_lineage_identity() {
    let (first_window, closing, first_tracker) = production_window(false);
    let keys = ["alpha", "beta"].map(|name| {
        WatermarkKey::new(
            source(name),
            PartitionId::new("btc-usd").expect("partition should be valid"),
        )
    });
    let mut second_tracker = WatermarkTracker::try_new_for_entity(
        keys.iter()
            .cloned()
            .map(PartitionConfig::required)
            .collect(),
        DurationNanos::new(5_000_000_000),
        vec![SourceHealthState::Healthy, SourceHealthState::Recovering],
        FeatureEntity::Asset(btc()),
    )
    .expect("tracker should be valid");
    for key in &keys {
        second_tracker
            .advance(
                key,
                WatermarkUpdate::new(
                    UnixNanos::new(905_000_000_000),
                    UnixNanos::new(905_000_000_002),
                    SourceHealthState::Healthy,
                ),
            )
            .expect("watermark should advance");
    }
    let second_window = PriceWindow::try_from_finalization(
        FeatureEntity::Asset(btc()),
        &second_tracker,
        second_tracker.decision(first_window.time_window()),
        first_window.observations().to_vec(),
    )
    .expect("same inputs under later finality evidence should be valid");
    let mut registry = FeatureRegistry::new();
    registry
        .register(realized_volatility_v2_definition().expect("definition should be valid"))
        .expect("definition should register");
    let first = emit_float_feature(
        &registry,
        emission_input(),
        compute_realized_volatility_v2(&first_window, &closing, &first_tracker)
            .expect("first feature should compute"),
    )
    .expect("first feature should emit");
    let second = emit_float_feature(
        &registry,
        emission_input(),
        compute_realized_volatility_v2(&second_window, &closing, &second_tracker)
            .expect("second feature should compute"),
    )
    .expect("second feature should emit");
    assert_ne!(first.finality_as_known_at(), second.finality_as_known_at());
    assert_ne!(first.lineage_hash(), second.lineage_hash());
}

#[test]
fn normalized_trade_rejects_cross_venue_source_spoofing() {
    let instrument = consolidated_instrument_for("alpha", btc());
    let envelope = EventEnvelope::new(
        UncheckedEventMetadata {
            schema_version: 3,
            source: source("beta"),
            venue: Some(VenueId::new("alpha").expect("venue should be valid")),
            instrument_id: Some(instrument.id().clone()),
            exchange_timestamp: Some(UnixNanos::new(600_000_000_000)),
            exchange_transaction_timestamp: Some(UnixNanos::new(600_000_000_000)),
            receive_wall_timestamp: UnixNanos::new(600_000_000_001),
            receive_monotonic_ns: 1,
            normalization_timestamp: UnixNanos::new(600_000_000_002),
            connection_started_at: UnixNanos::new(1),
            sequence_number: None,
            previous_sequence_number: None,
            connection_epoch: 1,
            subscription_epoch: 1,
            snapshot_kind: SnapshotKind::NotApplicable,
            source_checksum: None,
            raw_payload_hash: [1; 32],
            parser_version: "parser-v1".to_owned(),
            normalizer_version: "normalizer-v1".to_owned(),
            ingestion_instance: "test-ingestion".to_owned(),
            quality_score_ppm: 950_000,
            quality_flags: QualityFlags::NONE,
        },
        UncheckedEventPayload::Trade(Trade {
            trade_id: "spoofed-trade".to_owned(),
            price: price("100"),
            quantity: quantity("1"),
            side: Side::Buy,
        }),
    )
    .expect("envelope should be structurally valid");
    assert_eq!(
        TradeObservation::try_from_event(&envelope, &instrument),
        Err(FeatureComputationError::UntrustedInput)
    );
}

#[test]
fn derived_volatility_recipes_reject_identity_ambiguity() {
    let sparse = vec![
        emitted_realized_volatility(660_000_000_000, &["100"; 5], "101"),
        emitted_realized_volatility(780_000_000_000, &["100"; 5], "101"),
        emitted_realized_volatility(900_000_000_000, &["100"; 5], "101"),
    ];
    let series_window = TimeWindow::try_new(
        UnixNanos::new(900_000_000_000),
        UnixNanos::new(1_200_000_000_000),
    )
    .expect("series window should be valid");
    assert_eq!(
        FinalizedVolatilitySeries::try_new(series_window, sparse),
        Err(FeatureComputationError::SequenceGap)
    );

    let short = emitted_realized_volatility(3_900_000_000_000, &["100"; 5], "101");
    let long = emitted_realized_volatility(600_000_000_000, &["100"; 60], "101");
    assert_eq!(
        compute_volatility_term_ratio(&short, &long),
        Err(FeatureComputationError::InvalidParameter)
    );
}

#[test]
fn ewma_and_seasonality_reject_wrong_declared_identity() {
    let values = ["100"; 15];
    let timed_values = values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            (
                *value,
                600_000_000_000 + i64::try_from(index).expect("index should fit") * 60_000_000_000,
            )
        })
        .collect::<Vec<_>>();
    let (window, closing, tracker) = finalized_fair_window_for_asset(
        btc(),
        600_000_000_000,
        1_500_000_000_000,
        &timed_values,
        "101",
        false,
    );
    assert_eq!(
        compute_exponentially_weighted_volatility(&window, &closing, &tracker),
        Err(FeatureComputationError::InvalidParameter)
    );

    let (window, closing, tracker) = production_window(false);
    let fit_window = TimeWindow::try_new(
        UnixNanos::new(100_000_000_000),
        UnixNanos::new(500_000_000_000),
    )
    .expect("fit window should be valid");
    let wrong_bucket = SeasonalBaselineEvidence::try_new(
        btc(),
        71,
        fit_window,
        SeasonalBaseline::try_new(2.0, 500_000_000_000, 550_000_000_000, [7; 32])
            .expect("baseline should be valid"),
        120,
        feature_registry::QualityScore::from_millionths(925_000).expect("quality should be valid"),
        [9; 32],
    )
    .expect("baseline evidence should be structurally valid");
    assert_eq!(
        compute_seasonality_adjusted_volatility(&window, &closing, &tracker, &wrong_bucket),
        Err(FeatureComputationError::InvalidParameter)
    );
    assert_eq!(
        SeasonalBaselineEvidence::try_new(
            btc(),
            72,
            fit_window,
            SeasonalBaseline::try_new(2.0, 499_999_999_999, 550_000_000_000, [7; 32])
                .expect("baseline should be valid"),
            120,
            feature_registry::QualityScore::from_millionths(925_000)
                .expect("quality should be valid"),
            [9; 32],
        ),
        Err(FeatureComputationError::InvalidInput)
    );
}
