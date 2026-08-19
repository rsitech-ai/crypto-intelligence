use std::num::{NonZeroU32, NonZeroU64};

use connector_binance::{
    BinanceInput, BinanceTradeNormalizationReceipt, NormalizationContext,
    normalize_trade_with_receipt, parse_durable_native_message,
};
use connector_core::{DurableRawCaptureChannel, RawCapture, wal_stream_source_identity};
use domain::{
    AssetId, AssetNamespace, ContractKind, ContractValueUnit, InstrumentDefinition,
    InstrumentDefinitionInput, InstrumentId, ProductType, SourceId, SourceKind, VenueId,
};
use event_envelope::{
    BookDelta, BookLevel, BookSnapshot, EventEnvelope, QualityFlags, Side, SnapshotKind,
    UncheckedEventMetadata, UncheckedEventPayload,
};
use feature_engine::features::{
    AggressorAuthority, AggressorSide, BookEvidencePolicy, BookMetricObservation, BookQuoteSide,
    BookStateEvidence, DisplayedDepthRecoveryEpisode, FeatureComputationError,
    FlowTradeObservation, InstrumentDefinitionEvidence, LifecycleAction, LifecycleCapability,
    LifecycleObservation, LifecycleObservationInput, LifecycleSemantics, LifecycleWindow,
    SweepEqualPricePolicy, SweepSide, Task5EmissionError, Task5FeatureEmissionInput,
    TopOfBookObservation, TopOfBookWindow, TradeFlowWindow, TradeShock, TradeShockThreshold,
    TradeShockWeighting, active_level_counts, aggressive_trade_imbalances, aggressive_trade_sums,
    aggressive_trade_totals, book_depth_and_spread_change, book_distribution, book_quote_age_ms,
    book_shape_quadratic, certified_lifecycle_metrics, certified_lifecycle_rates,
    certified_lifecycle_ratios, depth_within_band, emit_book_snapshot_core_features,
    emit_book_spread_features, emit_trade_flow_core_features, expected_sweep_cost,
    fully_observed_depth_within_band, interarrival_coefficient_of_variation,
    large_trade_cluster_activity, level_gap_density, liquidity_wall_distance_bps,
    maximum_executable_quote_notional, microprice, normalized_imbalance,
    require_order_lifecycle_semantics, signed_volume_at_price, spread, task_five_definitions,
    task_five_recipes, top_of_book_ofi, top_of_book_side_staleness, top_of_book_staleness_for_side,
    trade_cluster_statistics, trade_intensity_per_second, trade_print_sweep_direction,
    trade_shock_response, weighted_midpoint,
};
use feature_engine::{
    PartitionConfig, PartitionId, TimeWindow, WatermarkKey, WatermarkTracker, WatermarkUpdate,
};
use feature_registry::{
    CodeRevision, DurationNanos, FeatureDatum, FeatureEntity, FeatureRegistry, FeatureValue,
    FeatureValueType, MissingnessReason, ObservationRevision,
};
use fixed_decimal::{FixedDecimal, Price, Quantity};
use instrument_registry::{InstrumentRegistry, RevisionMetadata};
use orderbook::{
    BookConfig, BookEventApplyOutcome, BookSession, ChecksumPolicy, OrderBookEngine,
    SequencePolicy, SnapshotStrategy,
};
use raw_wal::{
    frame::RecordMetadata,
    manager::{RotationPolicy, SegmentedWalWriter},
    prologue::{SegmentMetadata, StreamDescriptor},
};

#[test]
fn task_five_catalogue_is_complete_unique_typed_and_documented() {
    let recipes = task_five_recipes();
    let definitions = task_five_definitions().expect("catalogue should be valid");
    assert_eq!(recipes.len(), 77);
    assert_eq!(definitions.len(), recipes.len());

    let dictionary = include_str!("../../../docs/data-dictionary/features.md");
    let mut registry = FeatureRegistry::new();
    for (recipe, definition) in recipes.iter().zip(definitions) {
        assert_eq!(definition.id().as_str(), recipe.id());
        assert_eq!(definition.value_type(), recipe.value_type());
        assert_eq!(definition.formula_hash(), recipe.formula_hash());
        let anchor = definition
            .documentation()
            .as_str()
            .strip_prefix("docs/data-dictionary/features.md#")
            .expect("documentation path should be canonical");
        assert!(
            dictionary.contains(&format!(
                "| <a id=\"{anchor}\"></a>`{}` | `{}` |",
                recipe.id(),
                recipe.formula_identity()
            )),
            "missing per-feature formula contract row for {}",
            recipe.id()
        );
        assert!(
            dictionary.contains(&format!("| `{}` | Market Microstructure |", recipe.id())),
            "missing ownership/test traceability row for {}",
            recipe.id()
        );
        registry
            .register(definition)
            .expect("catalogue definition should be unique");
    }
    assert_eq!(registry.len(), recipes.len());

    let signed_volume = recipes
        .iter()
        .find(|recipe| recipe.id() == "signed_volume_at_price")
        .expect("signed-volume recipe should be present");
    assert_eq!(
        signed_volume.value_type(),
        FeatureValueType::FixedDecimalMap
    );
    for band_bps in [1, 2, 5, 10, 25, 50, 100] {
        for prefix in ["bid_depth", "ask_depth", "book_imbalance"] {
            let id = format!("{prefix}_{band_bps}bps");
            assert!(
                recipes.iter().any(|recipe| recipe.id() == id),
                "missing required depth-band identity {id}"
            );
        }
    }
}

#[test]
fn trusted_book_core_metrics_preserve_exact_depth_and_reference_values() {
    let book = trusted_book(
        &[("100", "10"), ("99", "20"), ("98", "40"), ("95", "100")],
        &[("102", "30"), ("103", "20"), ("104", "40"), ("107", "100")],
        10,
    );

    let spread = spread(&book).expect("normal trusted book has a spread");
    assert_eq!(spread.absolute().to_string(), "2");
    assert!((spread.relative() - (2.0 / 101.0)).abs() < 1e-15);

    let depth =
        depth_within_band(&book, price("101"), 100).expect("one-percent depth is available");
    assert_eq!(depth.bid_quantity().to_string(), "10");
    assert_eq!(depth.ask_quantity().to_string(), "30");
    assert_eq!(depth.bid_notional().to_string(), "1000");
    assert_eq!(depth.ask_notional().to_string(), "3060");
    assert!((normalized_imbalance(&book, price("101"), 100).unwrap() + 0.5).abs() < 1e-15);
    assert!((microprice(&book).unwrap() - 100.5).abs() < 1e-15);

    let sweep = expected_sweep_cost(&book, SweepSide::Buy, fixed("4090"), price("101"))
        .expect("two ask levels exactly fill the requested quote notional");
    assert_eq!(sweep.executed_quote_notional().to_string(), "4090");
    assert!((sweep.estimated_base_quantity() - 40.0).abs() < 1e-15);
    assert!((sweep.estimated_average_price() - 102.25).abs() < 1e-15);
    assert!((sweep.relative_cost() - (1.25 / 101.0)).abs() < 1e-15);
}

#[test]
fn quote_notional_features_fail_closed_without_a_spot_instrument_contract() {
    let instrument = InstrumentId::new_for_product(
        VenueId::new("test").unwrap(),
        "BTCUSD",
        ProductType::Perpetual,
        1,
    )
    .unwrap();
    let book = book_observed_at_for_instrument(
        instrument.clone(),
        &[("100", "10")],
        &[("102", "10")],
        1,
        1,
        1,
    );
    assert_eq!(
        depth_within_band(&book, price("101"), 100),
        Err(FeatureComputationError::SourceNotSupported)
    );
    assert_eq!(
        expected_sweep_cost(&book, SweepSide::Buy, fixed("100"), price("101")),
        Err(FeatureComputationError::SourceNotSupported)
    );

    let trade = FlowTradeObservation::new(
        instrument,
        domain::UnixNanos::new(100),
        domain::UnixNanos::new(110),
        [9; 32],
        price("100"),
        quantity("1"),
        AggressorSide::Buy,
        AggressorAuthority::supplied("test-taker-side", 1).unwrap(),
    )
    .unwrap();
    let window = TradeFlowWindow::new(
        vec![trade],
        2,
        domain::UnixNanos::new(0),
        domain::UnixNanos::new(1_000),
        domain::UnixNanos::new(1_010),
    )
    .unwrap();
    assert_eq!(
        aggressive_trade_totals(&window),
        Err(FeatureComputationError::SourceNotSupported)
    );
    assert_eq!(
        TradeShock::from_prefix(&window, TradeShockThreshold::QuoteNotional(fixed("100"))),
        Err(FeatureComputationError::SourceNotSupported)
    );
    assert_eq!(
        trade_cluster_statistics(&window, fixed("100"), 100),
        Err(FeatureComputationError::SourceNotSupported)
    );
}

#[test]
fn depth_band_enforces_both_absolute_bounds_and_includes_exact_boundaries() {
    let outside = trusted_book(&[("88", "10")], &[("92", "30")], 10);
    let outside_depth =
        depth_within_band(&outside, price("90"), 100).expect("trusted empty band is observable");
    assert_eq!(outside_depth.bid_quantity(), quantity("0"));
    assert_eq!(outside_depth.ask_quantity(), quantity("0"));

    let boundary = trusted_book(&[("99", "10")], &[("101", "30")], 10);
    let boundary_depth =
        depth_within_band(&boundary, price("100"), 100).expect("band boundaries are inclusive");
    assert_eq!(boundary_depth.bid_quantity(), quantity("10"));
    assert_eq!(boundary_depth.ask_quantity(), quantity("30"));

    let truncated = trusted_book(&[("99", "10")], &[("101", "30")], 10);
    let truncated_depth = fully_observed_depth_within_band(&truncated, price("100"), 200).unwrap();
    assert_eq!(
        truncated_depth.bid_quantity(),
        Err(FeatureComputationError::InsufficientHistory)
    );
    assert_eq!(
        truncated_depth.ask_quantity(),
        Err(FeatureComputationError::InsufficientHistory)
    );

    let ask_truncated = trusted_book(&[("99", "10"), ("98", "5")], &[("101", "30")], 10);
    let ask_truncated_depth =
        fully_observed_depth_within_band(&ask_truncated, price("100"), 200).unwrap();
    assert_eq!(ask_truncated_depth.bid_quantity(), Ok(quantity("15")));
    assert_eq!(
        ask_truncated_depth.ask_quantity(),
        Err(FeatureComputationError::InsufficientHistory)
    );

    let bid_truncated = trusted_book(&[("99", "10")], &[("101", "30"), ("102", "5")], 10);
    let bid_truncated_depth =
        fully_observed_depth_within_band(&bid_truncated, price("100"), 200).unwrap();
    assert_eq!(
        bid_truncated_depth.bid_quantity(),
        Err(FeatureComputationError::InsufficientHistory)
    );
    assert_eq!(bid_truncated_depth.ask_quantity(), Ok(quantity("35")));
}

#[test]
fn sweep_cost_supports_an_arbitrary_partial_terminal_level_as_an_estimate() {
    let book = trusted_book(&[("100", "20")], &[("102", "30"), ("103", "20")], 10);

    let sweep = expected_sweep_cost(&book, SweepSide::Buy, fixed("4080"), price("101"))
        .expect("an analytical sweep may consume a fractional terminal level");
    let expected_base_quantity = 30.0 + 1020.0 / 103.0;
    let expected_average_price = 4080.0 / expected_base_quantity;

    assert_eq!(sweep.executed_quote_notional().to_string(), "4080");
    assert!(
        (sweep.estimated_base_quantity() - expected_base_quantity).abs() < 1e-12,
        "base estimate must preserve the terminal-level ratio"
    );
    assert!((sweep.estimated_average_price() - expected_average_price).abs() < 1e-12);
    assert!((sweep.relative_cost() - (expected_average_price / 101.0 - 1.0)).abs() < 1e-12);
}

#[test]
fn book_features_reject_a_snapshot_beyond_the_l2_freshness_budget() {
    let book = book_observed_at(&[("100", "10")], &[("102", "30")], 10, 3_001_000_010, 1);

    assert_eq!(spread(&book), Err(FeatureComputationError::Stale));
    assert_eq!(microprice(&book), Err(FeatureComputationError::Stale));
}

#[test]
fn quote_age_and_side_staleness_use_monotonic_quality_and_last_side_change() {
    let aged = book_observed_at(&[("100", "10")], &[("102", "10")], 10, 500_000_010, 1);
    assert_eq!(book_quote_age_ms(&aged).unwrap(), 500);

    let first_book = trusted_book_with_sequence(&[("100", "10")], &[("102", "10")], 10, 1);
    let bid_change_book = trusted_book_with_sequence(&[("100", "12")], &[("102", "10")], 20, 2);
    let unchanged_book = trusted_book_with_sequence(&[("100", "12")], &[("102", "10")], 30, 3);
    let top = |book: &orderbook::BookSnapshotView, event_time_ns: i64, lineage: u8| {
        TopOfBookObservation::from_snapshot(
            book,
            book_policy(),
            domain::UnixNanos::new(event_time_ns),
            domain::UnixNanos::new(event_time_ns + 10),
            [lineage; 32],
        )
        .unwrap()
    };
    let window = TopOfBookWindow::new(
        vec![
            top(&first_book, 0, 1),
            top(&bid_change_book, 1_000_000_000, 2),
            top(&unchanged_book, 2_000_000_000, 3),
        ],
        8,
        domain::UnixNanos::new(3_000_000_000),
    )
    .unwrap();
    assert_eq!(
        top_of_book_side_staleness(
            &window,
            domain::UnixNanos::new(3_000_000_000),
            1_000_000_000,
        ),
        Err(FeatureComputationError::InsufficientHistory),
        "an initial snapshot is not proof of the ask-side change time"
    );
    let bid_staleness = top_of_book_staleness_for_side(
        &window,
        BookQuoteSide::Bid,
        domain::UnixNanos::new(3_000_000_000),
        1_000_000_000,
    )
    .expect("the observed bid change remains independently usable");
    assert_eq!(bid_staleness.quote_age_ns(), 2_000_000_000);
    assert_eq!(bid_staleness.stale_duration_ns(), 1_000_000_000);
    assert_eq!(
        top_of_book_staleness_for_side(
            &window,
            BookQuoteSide::Ask,
            domain::UnixNanos::new(3_000_000_000),
            1_000_000_000,
        ),
        Err(FeatureComputationError::InsufficientHistory)
    );

    let ask_change_book = trusted_book_with_sequence(&[("100", "12")], &[("103", "10")], 40, 4);
    let complete_window = TopOfBookWindow::new(
        vec![
            top(&first_book, 0, 1),
            top(&bid_change_book, 1_000_000_000, 2),
            top(&unchanged_book, 2_000_000_000, 3),
            top(&ask_change_book, 2_500_000_000, 4),
        ],
        8,
        domain::UnixNanos::new(3_000_000_000),
    )
    .unwrap();
    let staleness = top_of_book_side_staleness(
        &complete_window,
        domain::UnixNanos::new(3_000_000_000),
        1_000_000_000,
    )
    .expect("both side change times are now observed");
    assert_eq!(staleness.bid_quote_age_ns(), 2_000_000_000);
    assert_eq!(staleness.ask_quote_age_ns(), 500_000_000);
    assert_eq!(staleness.bid_stale_duration_ns(), 1_000_000_000);
    assert_eq!(staleness.ask_stale_duration_ns(), 0);
}

#[test]
fn non_normal_market_state_remains_untrusted_even_when_also_stale() {
    let locked = book_observed_at(&[("100", "10")], &[("100", "30")], 10, 3_001_000_010, 1);
    let crossed = book_observed_at(&[("101", "10")], &[("100", "30")], 10, 3_001_000_010, 1);
    let one_sided = book_observed_at(&[("100", "10")], &[], 10, 3_001_000_010, 1);

    assert_eq!(
        spread(&locked),
        Err(FeatureComputationError::UntrustedInput)
    );
    assert_eq!(
        spread(&crossed),
        Err(FeatureComputationError::UntrustedInput)
    );
    assert_eq!(
        spread(&one_sided),
        Err(FeatureComputationError::UntrustedInput)
    );
}

#[test]
fn executable_notional_includes_only_levels_inside_the_slippage_boundary() {
    let book = trusted_book(
        &[("100", "10"), ("99", "20"), ("98", "40")],
        &[("102", "30"), ("103", "10"), ("104", "40")],
        10,
    );

    assert_eq!(
        maximum_executable_quote_notional(&book, SweepSide::Buy, 200, price("101"))
            .expect("buy-side notional"),
        fixed("4090")
    );
    assert_eq!(
        maximum_executable_quote_notional(&book, SweepSide::Sell, 200, price("101"))
            .expect("sell-side notional"),
        fixed("2980")
    );
}

#[test]
fn weighted_midpoint_level_counts_entropy_and_concentration_match_reference_book() {
    let book = trusted_book(
        &[("100", "10"), ("99", "20")],
        &[("102", "30"), ("103", "40")],
        10,
    );

    assert!((weighted_midpoint(&book).unwrap() - 101.5).abs() < 1e-15);
    let counts = active_level_counts(&book).expect("active levels");
    assert_eq!(counts.bid(), 2);
    assert_eq!(counts.ask(), 2);
    assert_eq!(counts.total(), 4);

    let distribution = book_distribution(&book).expect("quantity distribution");
    let expected_entropy = -(0.1_f64 * 0.1_f64.ln()
        + 0.2_f64 * 0.2_f64.ln()
        + 0.3_f64 * 0.3_f64.ln()
        + 0.4_f64 * 0.4_f64.ln())
        / 4.0_f64.ln();
    assert!((distribution.normalized_entropy() - expected_entropy).abs() < 1e-15);
    assert!((distribution.concentration() - 0.3).abs() < 1e-15);
}

#[test]
fn gap_density_and_material_wall_distance_use_explicit_tick_and_thresholds() {
    let book = trusted_book(
        &[("100", "10"), ("98", "40")],
        &[("102", "10"), ("105", "30")],
        10,
    );

    let gaps = level_gap_density(&book, price("1")).expect("tick-aligned gaps");
    assert!((gaps.bid() - 0.5).abs() < 1e-15);
    assert!((gaps.ask() - (2.0 / 3.0)).abs() < 1e-15);

    let walls = liquidity_wall_distance_bps(&book, price("101"), fixed("3000"))
        .expect("material wall distances");
    assert!((walls.bid().unwrap() - (3.0 / 101.0 * 10_000.0)).abs() < 1e-12);
    assert!((walls.ask().unwrap() - (4.0 / 101.0 * 10_000.0)).abs() < 1e-12);
}

#[test]
fn quadratic_book_shape_fits_cumulative_depth_in_exact_tick_space() {
    let book = trusted_book(
        &[("100", "10"), ("99", "20"), ("98", "40")],
        &[("102", "10"), ("103", "20"), ("104", "40")],
        10,
    );

    let shape = book_shape_quadratic(&book, price("1"), 3).expect("three-level exact fit");
    assert!((shape.bid().slope_at_inside() - 10.0).abs() < 1e-12);
    assert!((shape.bid().convexity() - 20.0).abs() < 1e-12);
    assert!((shape.ask().slope_at_inside() - 10.0).abs() < 1e-12);
    assert!((shape.ask().convexity() - 20.0).abs() < 1e-12);
}

#[test]
fn top_of_book_ofi_uses_exact_cont_price_indicator_semantics() {
    let before_book = trusted_book_with_sequence(&[("100", "10")], &[("102", "10")], 10, 1);
    let after_book = trusted_book_with_sequence(&[("100", "12")], &[("102", "7")], 20, 2);
    let before = TopOfBookObservation::from_snapshot(
        &before_book,
        book_policy(),
        domain::UnixNanos::new(100),
        domain::UnixNanos::new(110),
        [1; 32],
    )
    .unwrap();
    let after = TopOfBookObservation::from_snapshot(
        &after_book,
        book_policy(),
        domain::UnixNanos::new(200),
        domain::UnixNanos::new(210),
        [2; 32],
    )
    .unwrap();
    let window = TopOfBookWindow::new(vec![before, after], 8, domain::UnixNanos::new(220)).unwrap();

    assert_eq!(top_of_book_ofi(&window).unwrap(), fixed("5"));
}

#[test]
fn book_chronology_uses_sequence_for_event_time_ties_and_rejects_knowledge_regression() {
    let before_book = trusted_book_with_sequence(&[("100", "10")], &[("102", "10")], 10, 1);
    let after_book = trusted_book_with_sequence(&[("100", "12")], &[("102", "7")], 20, 2);
    let before_metric = BookMetricObservation::from_snapshot(
        &before_book,
        book_policy(),
        price("101"),
        200,
        domain::UnixNanos::new(100),
        domain::UnixNanos::new(110),
        [1; 32],
    )
    .unwrap();
    let after_metric = BookMetricObservation::from_snapshot(
        &after_book,
        book_policy(),
        price("101"),
        200,
        domain::UnixNanos::new(100),
        domain::UnixNanos::new(120),
        [2; 32],
    )
    .unwrap();
    assert!(
        book_depth_and_spread_change(&before_metric, &after_metric).is_ok(),
        "strict source sequence orders valid equal-event-time updates"
    );

    let before_top = TopOfBookObservation::from_snapshot(
        &before_book,
        book_policy(),
        domain::UnixNanos::new(100),
        domain::UnixNanos::new(300),
        [3; 32],
    )
    .unwrap();
    let after_top = TopOfBookObservation::from_snapshot(
        &after_book,
        book_policy(),
        domain::UnixNanos::new(200),
        domain::UnixNanos::new(250),
        [4; 32],
    )
    .unwrap();
    assert_eq!(
        TopOfBookWindow::new(vec![before_top, after_top], 8, domain::UnixNanos::new(400)),
        Err(FeatureComputationError::NonMonotonicTime)
    );
}

#[test]
fn temporal_book_features_reject_tick_quality_or_ordering_policy_changes() {
    let before_book = trusted_book_with_sequence(&[("100", "10")], &[("102", "10")], 10, 1);
    let after_book = trusted_book_with_sequence(&[("100", "12")], &[("102", "7")], 20, 2);
    let changed_policy = BookEvidencePolicy::new(
        price("1"),
        "normal-l2-3s",
        1,
        "event-time-proxy",
        1,
        "consolidated-fair-price",
        1,
    )
    .unwrap();
    let before_metric = BookMetricObservation::from_snapshot(
        &before_book,
        book_policy(),
        price("101"),
        200,
        domain::UnixNanos::new(100),
        domain::UnixNanos::new(110),
        [1; 32],
    )
    .unwrap();
    let after_metric = BookMetricObservation::from_snapshot(
        &after_book,
        changed_policy.clone(),
        price("101"),
        200,
        domain::UnixNanos::new(200),
        domain::UnixNanos::new(210),
        [2; 32],
    )
    .unwrap();
    assert_eq!(
        book_depth_and_spread_change(&before_metric, &after_metric),
        Err(FeatureComputationError::SourceHealthChanged)
    );

    let before_top = TopOfBookObservation::from_snapshot(
        &before_book,
        book_policy(),
        domain::UnixNanos::new(100),
        domain::UnixNanos::new(110),
        [3; 32],
    )
    .unwrap();
    let after_top = TopOfBookObservation::from_snapshot(
        &after_book,
        changed_policy,
        domain::UnixNanos::new(200),
        domain::UnixNanos::new(210),
        [4; 32],
    )
    .unwrap();
    assert_eq!(
        TopOfBookWindow::new(vec![before_top, after_top], 8, domain::UnixNanos::new(220)),
        Err(FeatureComputationError::SourceHealthChanged)
    );
}

#[test]
fn trade_flow_preserves_aggressor_authority_and_same_timestamp_event_order() {
    let event_time = domain::UnixNanos::new(1_000);
    let trades = vec![
        FlowTradeObservation::new(
            instrument(),
            event_time,
            domain::UnixNanos::new(1_010),
            [1; 32],
            price("100"),
            quantity("2"),
            AggressorSide::Buy,
            AggressorAuthority::supplied("binance-buyer-maker-inversion", 1).unwrap(),
        )
        .unwrap(),
        FlowTradeObservation::new(
            instrument(),
            event_time,
            domain::UnixNanos::new(1_011),
            [2; 32],
            price("101"),
            quantity("1"),
            AggressorSide::Sell,
            AggressorAuthority::inferred("quote-tick-rule", 1).unwrap(),
        )
        .unwrap(),
    ];
    let window = TradeFlowWindow::new(
        trades,
        8,
        event_time,
        domain::UnixNanos::new(1_001),
        domain::UnixNanos::new(1_020),
    )
    .expect("stable lineage orders equal-timestamp trades");
    assert_eq!(window.authorities().len(), 2);
    assert!(
        window
            .authorities()
            .iter()
            .any(
                |authority| authority.rule_id() == "binance-buyer-maker-inversion"
                    && authority.rule_version() == 1
                    && !authority.is_inferred()
            )
    );
    assert!(
        window
            .authorities()
            .iter()
            .any(|authority| authority.rule_id() == "quote-tick-rule"
                && authority.rule_version() == 1
                && authority.is_inferred())
    );

    let totals = aggressive_trade_totals(&window).expect("flow totals");
    assert_eq!(totals.buy_count(), 1);
    assert_eq!(totals.sell_count(), 1);
    assert_eq!(totals.buy_quantity().to_string(), "2");
    assert_eq!(totals.sell_quantity().to_string(), "1");
    assert_eq!(totals.buy_notional().to_string(), "200");
    assert_eq!(totals.sell_notional().to_string(), "101");
    assert!((totals.signed_notional_imbalance() - (99.0 / 301.0)).abs() < 1e-15);
    assert_eq!(totals.inferred_count(), 1);
}

#[tokio::test]
async fn flow_trade_observation_derives_identity_time_and_value_from_validated_event() {
    let _authenticated_constructor: fn(
        &BinanceTradeNormalizationReceipt,
    )
        -> Result<FlowTradeObservation, FeatureComputationError> =
        FlowTradeObservation::try_from_binance_receipt;
    let (event, observation) =
        authenticated_binance_flow_trade(1_672_515_782_136_000_000, 7, "100", "2", Side::Buy).await;

    assert_eq!(observation.instrument(), &binance_instrument());
    assert_eq!(
        observation.event_time(),
        domain::UnixNanos::new(1_672_515_782_136_000_000)
    );
    assert_eq!(
        observation.as_known_at(),
        domain::UnixNanos::new(1_672_515_782_136_000_002)
    );
    assert_eq!(observation.lineage(), *event.id().as_bytes());
    assert_eq!(observation.price(), price("100"));
    assert_eq!(observation.quantity(), quantity("2"));
    assert_eq!(observation.side(), AggressorSide::Buy);
    assert!(observation.is_authenticated());
    assert_eq!(
        observation.authority().rule_id(),
        "binance-buyer-maker-inversion"
    );
    assert!(!observation.authority().is_inferred());
}

#[test]
fn observed_empty_trade_window_preserves_zero_sums_and_missing_imbalances() {
    let window = TradeFlowWindow::observed_empty(
        instrument(),
        8,
        domain::UnixNanos::new(0),
        domain::UnixNanos::new(1_000_000_000),
        domain::UnixNanos::new(1_000_000_010),
    )
    .unwrap();
    let sums = aggressive_trade_sums(&window).expect("finalized empty flow has exact zero sums");
    assert_eq!(sums.buy_count(), 0);
    assert_eq!(sums.sell_count(), 0);
    assert_eq!(sums.buy_quantity(), quantity("0"));
    assert_eq!(sums.sell_quantity(), quantity("0"));
    assert_eq!(sums.buy_notional(), fixed("0"));
    assert_eq!(sums.sell_notional(), fixed("0"));
    assert_eq!(
        aggressive_trade_imbalances(&window),
        Err(FeatureComputationError::ZeroDenominator)
    );
}

#[test]
fn trade_intensity_and_interarrival_dispersion_use_declared_event_time() {
    let trades = vec![
        flow_trade(1_000_000_000, 1, AggressorSide::Buy),
        flow_trade(2_000_000_000, 2, AggressorSide::Sell),
        flow_trade(4_000_000_000, 3, AggressorSide::Buy),
    ];
    let window = TradeFlowWindow::new(
        trades,
        8,
        domain::UnixNanos::new(1_000_000_000),
        domain::UnixNanos::new(4_000_000_001),
        domain::UnixNanos::new(4_000_000_010),
    )
    .expect("chronological flow window");

    let expected_intensity = 3.0 * 1_000_000_000.0 / 3_000_000_001.0;
    assert!((trade_intensity_per_second(&window).unwrap() - expected_intensity).abs() < 1e-15);
    assert!((interarrival_coefficient_of_variation(&window).unwrap() - (1.0 / 3.0)).abs() < 1e-15);
}

#[test]
fn trade_flow_rejects_a_duplicate_lineage_even_when_other_events_separate_it() {
    let trades = vec![
        flow_trade(1_000, 1, AggressorSide::Buy),
        flow_trade(2_000, 2, AggressorSide::Sell),
        flow_trade(3_000, 1, AggressorSide::Buy),
    ];

    assert_eq!(
        TradeFlowWindow::new(
            trades,
            8,
            domain::UnixNanos::new(1_000),
            domain::UnixNanos::new(4_000),
            domain::UnixNanos::new(4_000),
        ),
        Err(FeatureComputationError::DuplicateLineage)
    );
}

#[test]
fn signed_volume_at_price_aggregates_exact_quantity_with_aggressor_sign() {
    let trades = vec![
        flow_trade_at(1_000, 1, "100", "2", AggressorSide::Buy),
        flow_trade_at(2_000, 2, "100", "1", AggressorSide::Sell),
        flow_trade_at(3_000, 3, "101", "3", AggressorSide::Sell),
    ];
    let window = TradeFlowWindow::new(
        trades,
        8,
        domain::UnixNanos::new(1_000),
        domain::UnixNanos::new(4_000),
        domain::UnixNanos::new(4_000),
    )
    .unwrap();

    let signed = signed_volume_at_price(&window).expect("exact signed volume map");
    assert_eq!(signed.get(&price("100")), Some(&fixed("1")));
    assert_eq!(signed.get(&price("101")), Some(&fixed("-3")));
    assert_eq!(signed.len(), 2);
}

#[test]
fn count_quantity_and_notional_trade_imbalances_remain_distinct() {
    let trades = vec![
        flow_trade_at(1_000, 1, "100", "2", AggressorSide::Buy),
        flow_trade_at(2_000, 2, "101", "1", AggressorSide::Buy),
        flow_trade_at(3_000, 3, "99", "3", AggressorSide::Sell),
    ];
    let window = TradeFlowWindow::new(
        trades,
        8,
        domain::UnixNanos::new(1_000),
        domain::UnixNanos::new(4_000),
        domain::UnixNanos::new(4_000),
    )
    .unwrap();
    let totals = aggressive_trade_totals(&window).unwrap();

    assert!((totals.signed_count_imbalance() - (1.0 / 3.0)).abs() < 1e-15);
    assert_eq!(totals.signed_quantity_imbalance(), 0.0);
    assert!((totals.signed_notional_imbalance() - (4.0 / 598.0)).abs() < 1e-15);
}

#[test]
fn buy_trade_shock_response_matches_impact_and_adverse_selection_fixture() {
    let evaluation_time = domain::UnixNanos::new(2_600_000_010);
    let trades = TradeFlowWindow::new(
        vec![
            flow_trade_at(1_500_000_000, 1, "100.5", "0.4", AggressorSide::Buy),
            flow_trade_at(1_600_000_000, 4, "100.5", "0.6", AggressorSide::Buy),
        ],
        8,
        domain::UnixNanos::new(1_000_000_000),
        domain::UnixNanos::new(2_000_000_000),
        evaluation_time,
    )
    .unwrap();
    let shock =
        TradeShock::from_prefix(&trades, TradeShockThreshold::QuoteNotional(fixed("100.5")))
            .expect("one exact quote-notional buy shock");
    assert_eq!(
        shock.threshold(),
        TradeShockThreshold::QuoteNotional(fixed("100.5"))
    );
    assert_eq!(shock.realized_base_quantity(), fixed("1"));
    assert_eq!(shock.realized_quote_notional(), fixed("100.5"));

    let pre_book = trusted_book_with_sequence(&[("99", "10")], &[("101", "10")], 10, 1);
    let response_book = trusted_book_with_sequence(&[("100", "10")], &[("102", "10")], 20, 2);
    let pre = TopOfBookObservation::from_snapshot(
        &pre_book,
        book_policy(),
        domain::UnixNanos::new(1_400_000_000),
        domain::UnixNanos::new(1_550_000_000),
        [2; 32],
    )
    .unwrap();
    let response = TopOfBookObservation::from_snapshot(
        &response_book,
        book_policy(),
        domain::UnixNanos::new(2_500_000_000),
        domain::UnixNanos::new(2_500_000_010),
        [3; 32],
    )
    .unwrap();
    let quotes = TopOfBookWindow::new(vec![pre, response], 8, evaluation_time).unwrap();

    let markout = trade_shock_response(
        &shock,
        &quotes,
        200_000_000,
        900_000_000,
        100,
        TradeShockWeighting::BaseQuantity,
    )
    .expect("trusted quote response at the declared horizon");
    assert!((markout.signed_impact() - 0.01).abs() < 1e-15);
    assert!((markout.effective_spread() - 0.01).abs() < 1e-15);
    assert!((markout.realized_spread() + 0.01).abs() < 1e-15);
    assert!((markout.adverse_selection_proxy() - 0.02).abs() < 1e-15);
}

#[test]
fn same_side_run_and_large_trade_precursors_use_explicit_inclusive_thresholds() {
    let trades = vec![
        flow_trade_at(1_000, 1, "100", "2", AggressorSide::Buy),
        flow_trade_at(1_001, 2, "100", "3", AggressorSide::Buy),
        flow_trade_at(1_002, 3, "100", "1", AggressorSide::Sell),
        flow_trade_at(2_000, 4, "100", "4", AggressorSide::Sell),
    ];
    let window = TradeFlowWindow::new(
        trades,
        8,
        domain::UnixNanos::new(1_000),
        domain::UnixNanos::new(3_000),
        domain::UnixNanos::new(3_000),
    )
    .unwrap();

    let statistics =
        trade_cluster_statistics(&window, fixed("200"), 2).expect("bounded cluster statistics");
    assert_eq!(statistics.longest_same_side_run(), 2);
    assert_eq!(statistics.largest_large_trade_cluster(), 2);
    assert_eq!(statistics.large_trade_count(), 3);
}

#[test]
fn trade_print_sweep_proxy_requires_monotonic_distinct_price_episodes() {
    let trades = vec![
        flow_trade_at(0, 1, "100", "1", AggressorSide::Buy),
        flow_trade_at(100_000_000, 2, "101", "1", AggressorSide::Buy),
        flow_trade_at(300_000_000, 3, "101", "1", AggressorSide::Sell),
        flow_trade_at(400_000_000, 4, "100", "1", AggressorSide::Sell),
    ];
    let window = TradeFlowWindow::new(
        trades,
        8,
        domain::UnixNanos::new(0),
        domain::UnixNanos::new(1_000_000_000),
        domain::UnixNanos::new(1_000_000_000),
    )
    .unwrap();

    let direction = trade_print_sweep_direction(
        &window,
        200_000_000,
        2,
        2,
        fixed("200"),
        SweepEqualPricePolicy::Allow,
    )
    .expect("one buy and one sell trade-print sweep proxy qualify");
    assert_eq!(direction.buy_episode_count(), 1);
    assert_eq!(direction.sell_episode_count(), 1);
    assert_eq!(direction.direction(), 0.0);

    let three_price_window = TradeFlowWindow::new(
        vec![
            flow_trade_at(0, 5, "100", "1", AggressorSide::Buy),
            flow_trade_at(100_000_000, 6, "101", "1", AggressorSide::Buy),
            flow_trade_at(200_000_000, 7, "102", "1", AggressorSide::Buy),
        ],
        8,
        domain::UnixNanos::new(0),
        domain::UnixNanos::new(1_000_000_000),
        domain::UnixNanos::new(1_000_000_000),
    )
    .unwrap();
    let independent_minima = trade_print_sweep_direction(
        &three_price_window,
        200_000_000,
        2,
        3,
        fixed("300"),
        SweepEqualPricePolicy::Allow,
    )
    .expect("three distinct prints satisfy independent minima of two prints and three prices");
    assert_eq!(independent_minima.buy_episode_count(), 1);
}

#[test]
fn large_trade_cluster_activity_counts_only_qualifying_exact_notional_clusters() {
    let trades = vec![
        flow_trade_at(0, 1, "120", "1", AggressorSide::Buy),
        flow_trade_at(50_000_000, 2, "150", "1", AggressorSide::Sell),
        flow_trade_at(200_000_000, 3, "130", "1", AggressorSide::Buy),
        flow_trade_at(250_000_000, 4, "80", "1", AggressorSide::Buy),
        flow_trade_at(400_000_000, 5, "200", "1", AggressorSide::Sell),
        flow_trade_at(450_000_000, 6, "100", "1", AggressorSide::Sell),
    ];
    let window = TradeFlowWindow::new(
        trades,
        8,
        domain::UnixNanos::new(0),
        domain::UnixNanos::new(1_000_000_000),
        domain::UnixNanos::new(1_000_000_000),
    )
    .unwrap();

    let activity = large_trade_cluster_activity(&window, fixed("100"), 100_000_000, 2)
        .expect("two inclusive exact-notional clusters qualify");
    assert_eq!(activity.cluster_count(), 2);
    assert_eq!(activity.clustered_large_count(), 4);
    assert_eq!(activity.clustered_large_notional(), fixed("570"));
    assert!((activity.cluster_activity_rate_per_second() - 2.0).abs() < 1e-15);
}

#[test]
fn cancellation_features_require_certified_and_permitted_lifecycle_semantics() {
    assert_eq!(
        require_order_lifecycle_semantics(LifecycleSemantics::AggregateL2),
        Err(FeatureComputationError::SourceNotSupported)
    );
    assert_eq!(
        require_order_lifecycle_semantics(LifecycleSemantics::LicenseRestrictedL3),
        Err(FeatureComputationError::PrivacyOrLicenseRestriction)
    );
    assert_eq!(
        require_order_lifecycle_semantics(LifecycleSemantics::CertifiedL3),
        Ok(())
    );
}

#[test]
fn certified_l3_lifecycle_rates_and_trade_ratios_match_declared_window() {
    let capability = LifecycleCapability::new(
        "kraken-l3-order-events",
        1,
        "rsi-certified-lifecycle-reconciliation",
        1,
        "kraken-market-data-license",
        1,
    )
    .unwrap();
    let session = BookSession {
        connection_epoch: 7,
        subscription_epoch: 9,
        instrument_generation: 1,
    };
    let actions = [
        LifecycleAction::Place,
        LifecycleAction::Place,
        LifecycleAction::Place,
        LifecycleAction::Cancel,
        LifecycleAction::Cancel,
        LifecycleAction::Cancel,
        LifecycleAction::Cancel,
        LifecycleAction::Modify,
    ]
    .into_iter()
    .enumerate()
    .map(|(index, action)| {
        let event_time = 100_000_000_i64 * i64::try_from(index + 1).unwrap();
        LifecycleObservation::try_new(LifecycleObservationInput {
            instrument: instrument(),
            session,
            source_sequence: u64::try_from(index + 1).unwrap(),
            order_id: format!("order-{index}"),
            capability: capability.clone(),
            event_time: domain::UnixNanos::new(event_time),
            as_known_at: domain::UnixNanos::new(event_time + 10),
            lineage: [u8::try_from(index + 10).unwrap(); 32],
            action,
        })
        .unwrap()
    })
    .collect();
    let evaluation_time = domain::UnixNanos::new(2_000_000_010);
    let lifecycle = LifecycleWindow::new(
        actions,
        16,
        domain::UnixNanos::new(0),
        domain::UnixNanos::new(2_000_000_000),
        evaluation_time,
    )
    .expect("certified L3 actions form a bounded finalized window");
    let trades = TradeFlowWindow::new(
        vec![
            flow_trade(200_000_000, 1, AggressorSide::Buy),
            flow_trade(600_000_000, 2, AggressorSide::Sell),
            flow_trade(1_000_000_000, 3, AggressorSide::Buy),
            flow_trade(1_800_000_000, 4, AggressorSide::Sell),
        ],
        8,
        domain::UnixNanos::new(0),
        domain::UnixNanos::new(2_000_000_000),
        evaluation_time,
    )
    .unwrap();

    let metrics =
        certified_lifecycle_metrics(&lifecycle, &trades).expect("shared instrument and window");
    let rates = certified_lifecycle_rates(&lifecycle).unwrap();
    let ratios = certified_lifecycle_ratios(&lifecycle, &trades).unwrap();
    assert_eq!(
        rates.placement_rate_per_second(),
        metrics.placement_rate_per_second()
    );
    assert_eq!(
        ratios.cancellation_to_trade_ratio(),
        metrics.cancellation_to_trade_ratio()
    );
    assert!((metrics.placement_rate_per_second() - 1.5).abs() < 1e-15);
    assert!((metrics.cancellation_rate_per_second() - 2.0).abs() < 1e-15);
    assert!((metrics.modification_rate_per_second() - 0.5).abs() < 1e-15);
    assert!((metrics.cancellation_to_trade_ratio() - 1.0).abs() < 1e-15);
    assert!((metrics.quote_to_trade_ratio() - 2.0).abs() < 1e-15);

    let empty_lifecycle = LifecycleWindow::observed_empty(
        instrument(),
        session,
        capability.clone(),
        16,
        domain::UnixNanos::new(0),
        domain::UnixNanos::new(2_000_000_000),
        evaluation_time,
    )
    .expect("certified finalized coverage can prove zero lifecycle actions");
    let empty_metrics = certified_lifecycle_metrics(&empty_lifecycle, &trades).unwrap();
    let empty_rates = certified_lifecycle_rates(&empty_lifecycle).unwrap();
    assert_eq!(empty_metrics.placement_rate_per_second(), 0.0);
    assert_eq!(empty_metrics.cancellation_rate_per_second(), 0.0);
    assert_eq!(empty_metrics.modification_rate_per_second(), 0.0);
    assert_eq!(empty_metrics.cancellation_to_trade_ratio(), 0.0);
    assert_eq!(empty_metrics.quote_to_trade_ratio(), 0.0);
    assert_eq!(empty_rates.placement_rate_per_second(), 0.0);
    assert_eq!(empty_rates.cancellation_rate_per_second(), 0.0);
    assert_eq!(empty_rates.modification_rate_per_second(), 0.0);

    let empty_trades = TradeFlowWindow::observed_empty(
        instrument(),
        8,
        domain::UnixNanos::new(0),
        domain::UnixNanos::new(2_000_000_000),
        evaluation_time,
    )
    .expect("finalized trade coverage can prove zero aggressive trades");
    assert_eq!(
        certified_lifecycle_metrics(&lifecycle, &empty_trades),
        Err(FeatureComputationError::ZeroDenominator)
    );
    assert_eq!(
        certified_lifecycle_ratios(&lifecycle, &empty_trades),
        Err(FeatureComputationError::ZeroDenominator)
    );
    assert_eq!(
        certified_lifecycle_rates(&lifecycle)
            .unwrap()
            .placement_rate_per_second(),
        1.5
    );
}

#[test]
fn lifecycle_window_rejects_sequence_gaps_within_a_certified_session() {
    let capability = LifecycleCapability::new(
        "kraken-l3-order-events",
        1,
        "rsi-certified-lifecycle-reconciliation",
        1,
        "kraken-market-data-license",
        1,
    )
    .unwrap();
    let session = BookSession {
        connection_epoch: 7,
        subscription_epoch: 9,
        instrument_generation: 1,
    };
    let observations = [1_u64, 3]
        .into_iter()
        .map(|source_sequence| {
            let event_time = i64::try_from(source_sequence).unwrap() * 100;
            LifecycleObservation::try_new(LifecycleObservationInput {
                instrument: instrument(),
                session,
                source_sequence,
                order_id: format!("order-{source_sequence}"),
                capability: capability.clone(),
                event_time: domain::UnixNanos::new(event_time),
                as_known_at: domain::UnixNanos::new(event_time + 10),
                lineage: [u8::try_from(source_sequence).unwrap(); 32],
                action: LifecycleAction::Place,
            })
            .unwrap()
        })
        .collect();

    assert_eq!(
        LifecycleWindow::new(
            observations,
            4,
            domain::UnixNanos::new(0),
            domain::UnixNanos::new(1_000),
            domain::UnixNanos::new(1_010),
        ),
        Err(FeatureComputationError::SequenceGap)
    );
}

#[test]
fn depth_and_spread_change_bind_session_sequence_time_and_lineage() {
    let before_book = trusted_book(&[("100", "10")], &[("102", "30")], 10);
    let after_book = trusted_book_with_sequence(&[("99", "20")], &[("103", "20")], 20, 2);
    let before = BookMetricObservation::from_snapshot(
        &before_book,
        book_policy(),
        price("101"),
        200,
        domain::UnixNanos::new(100),
        domain::UnixNanos::new(110),
        [1; 32],
    )
    .unwrap();
    let after = BookMetricObservation::from_snapshot(
        &after_book,
        book_policy(),
        price("101"),
        200,
        domain::UnixNanos::new(200),
        domain::UnixNanos::new(210),
        [2; 32],
    )
    .unwrap();

    let change = book_depth_and_spread_change(&before, &after).expect("same-session change");
    assert_eq!(change.bid_quantity().to_string(), "10");
    assert_eq!(change.ask_quantity().to_string(), "-10");
    assert_eq!(change.absolute_spread().to_string(), "2");
}

#[test]
fn book_observations_derive_time_sequence_session_and_lineage_from_validated_event() {
    let book = trusted_book_with_sequence(&[("100", "10")], &[("102", "30")], 10, 7);
    let event = book_snapshot_event(&book, 1_000, 7);
    let evidence =
        BookStateEvidence::try_from_event(&book, &event).expect("event matches trusted book state");
    let metric = BookMetricObservation::from_authenticated_snapshot(
        &book,
        book_policy(),
        price("101"),
        100,
        &evidence,
    )
    .unwrap();
    let top =
        TopOfBookObservation::from_authenticated_snapshot(&book, book_policy(), &evidence).unwrap();

    assert!(metric.is_authenticated());
    assert!(top.is_authenticated());
    assert_eq!(top.event_time(), domain::UnixNanos::new(1_000));
    assert_eq!(top.as_known_at(), domain::UnixNanos::new(1_002));
    assert_eq!(top.lineage(), *event.id().as_bytes());
}

#[test]
fn book_state_evidence_rejects_same_sequence_divergent_payload_and_quality() {
    let source_book = trusted_book_with_sequence(&[("100", "10")], &[("102", "30")], 1, 7);
    let divergent_book = trusted_book_with_sequence(&[("100", "99")], &[("102", "30")], 1, 7);
    let event = book_snapshot_event(&source_book, 1_000, 7);
    assert_eq!(
        BookStateEvidence::try_from_event(&divergent_book, &event),
        Err(FeatureComputationError::UntrustedInput)
    );

    let evidence = BookStateEvidence::try_from_event(&source_book, &event).unwrap();
    assert_eq!(evidence.quality_score_ppm(), 950_000);
    let different_apply_clock =
        trusted_book_with_sequence(&[("100", "10")], &[("102", "30")], 2, 7);
    assert!(
        !evidence.matches(&different_apply_clock),
        "authenticated evidence must bind the exact receive monotonic time"
    );
    assert_eq!(
        BookStateEvidence::try_from_event(&different_apply_clock, &event),
        Err(FeatureComputationError::UntrustedInput)
    );
    let aged_same_state = book_observed_at(&[("100", "10")], &[("102", "30")], 1, 10_000_001, 7);
    assert!(!evidence.matches(&aged_same_state));
}

#[test]
fn book_evidence_binds_the_validated_instrument_contract() {
    let book = trusted_book_with_sequence(&[("100", "10")], &[("102", "30")], 10, 7);
    let book_event = book_snapshot_event(&book, 1_000, 7);
    let definition = instrument_definition(price("1"), quantity("1"));
    let definition_event = instrument_definition_event(definition, 6);
    let definition_evidence =
        InstrumentDefinitionEvidence::try_from_event(&definition_event).unwrap();
    let evidence =
        BookStateEvidence::try_from_event_with_definition(&book, &book_event, &definition_evidence)
            .unwrap();
    assert_eq!(
        evidence.instrument_definition_lineage(),
        Some(*definition_event.id().as_bytes())
    );

    let wrong_tick_event =
        instrument_definition_event(instrument_definition(price("0.5"), quantity("1")), 8);
    let wrong_tick_evidence =
        InstrumentDefinitionEvidence::try_from_event(&wrong_tick_event).unwrap();
    assert_eq!(
        BookStateEvidence::try_from_event_with_definition(&book, &book_event, &wrong_tick_evidence),
        Err(FeatureComputationError::UntrustedInput)
    );
}

#[test]
fn post_apply_delta_receipt_authenticates_the_exact_trusted_state() {
    let mut engine = OrderBookEngine::new(BookConfig {
        instrument: instrument(),
        price_tick: price("1"),
        quantity_step: quantity("1"),
        max_levels_per_side: 32,
        max_buffered_deltas: 8,
        max_buffered_level_updates: 64,
        sequence_policy: SequencePolicy::RangeContainsNext,
        checksum_policy: ChecksumPolicy::Disabled,
        max_l3_orders: None,
        max_l3_levels_per_side: None,
    })
    .unwrap();
    engine
        .start_session(
            BookSession {
                connection_epoch: 1,
                subscription_epoch: 1,
                instrument_generation: 1,
            },
            SnapshotStrategy::StreamSnapshot,
        )
        .unwrap();
    let snapshot = normalized_book_event(
        UncheckedEventPayload::BookSnapshot(BookSnapshot {
            bids: levels(&[("100", "10"), ("95", "20")]),
            asks: levels(&[("102", "30"), ("107", "20")]),
            last_sequence: 10,
        }),
        10,
        None,
        SnapshotKind::Snapshot,
        10,
        1,
    );
    assert!(matches!(
        engine.apply_event_with_receipt(&snapshot).unwrap(),
        BookEventApplyOutcome::Applied(_)
    ));

    let delta = normalized_book_event(
        UncheckedEventPayload::BookDelta(BookDelta {
            bids: levels(&[("100", "12")]),
            asks: levels(&[("102", "0")]),
            first_sequence: 11,
            last_sequence: 11,
        }),
        11,
        Some(10),
        SnapshotKind::Delta,
        20,
        2,
    );
    let receipt = match engine.apply_event_with_receipt(&delta).unwrap() {
        BookEventApplyOutcome::Applied(receipt) => receipt,
        other => panic!("delta must apply, got {other:?}"),
    };
    let definition_event =
        instrument_definition_event(instrument_definition(price("1"), quantity("1")), 4);
    let definition_evidence =
        InstrumentDefinitionEvidence::try_from_event(&definition_event).unwrap();
    let evidence = BookStateEvidence::try_from_apply_receipt_with_definition(
        &receipt,
        &delta,
        &definition_evidence,
    )
    .unwrap();
    assert!(evidence.matches(receipt.snapshot()));
    assert_eq!(evidence.lineage(), *delta.id().as_bytes());
    assert_eq!(
        receipt
            .snapshot()
            .best_bid()
            .expect("post-apply bid")
            .quantity
            .to_string(),
        "12"
    );
    assert_eq!(
        receipt
            .snapshot()
            .best_ask()
            .expect("deleted ask is absent")
            .price
            .to_string(),
        "107"
    );
    let entity = FeatureEntity::Instrument(instrument());
    let source = SourceId::new(SourceKind::Exchange, "test", 1).unwrap();
    let key = WatermarkKey::new(source, PartitionId::new("book").unwrap());
    let mut tracker = WatermarkTracker::try_new_for_entity(
        vec![PartitionConfig::required(key.clone())],
        DurationNanos::new(5_000_000_000),
        vec![quality::SourceHealthState::Healthy],
        entity,
    )
    .unwrap();
    tracker
        .advance(
            &key,
            WatermarkUpdate::new(
                domain::UnixNanos::new(7_000_000_000),
                domain::UnixNanos::new(7_000_000_001),
                quality::SourceHealthState::Healthy,
            ),
        )
        .unwrap();
    let decision = tracker.decision(
        TimeWindow::try_new(
            domain::UnixNanos::new(1_000_000_000),
            domain::UnixNanos::new(2_000_000_000),
        )
        .unwrap(),
    );
    let mut registry = FeatureRegistry::new();
    for definition in task_five_definitions().unwrap() {
        registry.register(definition).unwrap();
    }
    let observations = emit_book_spread_features(
        &registry,
        Task5FeatureEmissionInput {
            computed_at: domain::UnixNanos::new(7_000_000_010),
            revision: ObservationRevision::new(1).unwrap(),
            code_commit: CodeRevision::new("0123456789abcdef0123456789abcdef01234567").unwrap(),
        },
        receipt.snapshot(),
        &evidence,
        &tracker,
        decision,
    )
    .unwrap();
    assert!(matches!(
        observations[0].datum(),
        FeatureDatum::Present(FeatureValue::FixedDecimal(value)) if value.to_string() == "7"
    ));
    assert_eq!(
        BookStateEvidence::try_from_event(receipt.snapshot(), &delta),
        Err(FeatureComputationError::SourceNotSupported)
    );

    let different_delta = normalized_book_event(
        UncheckedEventPayload::BookDelta(BookDelta {
            bids: levels(&[("100", "13")]),
            asks: Vec::new(),
            first_sequence: 11,
            last_sequence: 11,
        }),
        11,
        Some(10),
        SnapshotKind::Delta,
        20,
        3,
    );
    assert_eq!(
        BookStateEvidence::try_from_apply_receipt(&receipt, &different_delta),
        Err(FeatureComputationError::UntrustedInput)
    );
}

#[test]
fn authenticated_book_spread_emits_typed_registry_and_finality_bound_observations() {
    let book = trusted_book_with_sequence(&[("100", "10")], &[("102", "30")], 10, 7);
    let event = book_snapshot_event(&book, 1_500_000_000, 7);
    let book_only_evidence = BookStateEvidence::try_from_event(&book, &event).unwrap();
    let entity = FeatureEntity::Instrument(instrument());
    let source = SourceId::new(SourceKind::Exchange, "test", 1).unwrap();
    let mut tracker = WatermarkTracker::try_new_for_entity(
        vec![PartitionConfig::required(WatermarkKey::new(
            source,
            PartitionId::new("book").unwrap(),
        ))],
        DurationNanos::new(5_000_000_000),
        vec![quality::SourceHealthState::Healthy],
        entity,
    )
    .unwrap();
    tracker
        .advance(
            &WatermarkKey::new(
                SourceId::new(SourceKind::Exchange, "test", 1).unwrap(),
                PartitionId::new("book").unwrap(),
            ),
            WatermarkUpdate::new(
                domain::UnixNanos::new(7_000_000_000),
                domain::UnixNanos::new(7_000_000_001),
                quality::SourceHealthState::Healthy,
            ),
        )
        .unwrap();
    let decision = tracker.decision(
        TimeWindow::try_new(
            domain::UnixNanos::new(1_000_000_000),
            domain::UnixNanos::new(2_000_000_000),
        )
        .unwrap(),
    );
    let mut registry = FeatureRegistry::new();
    for definition in task_five_definitions().unwrap() {
        registry.register(definition).unwrap();
    }
    assert!(matches!(
        emit_book_spread_features(
            &registry,
            Task5FeatureEmissionInput {
                computed_at: domain::UnixNanos::new(7_000_000_009),
                revision: ObservationRevision::new(1).unwrap(),
                code_commit: CodeRevision::new("0123456789abcdef0123456789abcdef01234567").unwrap(),
            },
            &book,
            &book_only_evidence,
            &tracker,
            decision,
        ),
        Err(Task5EmissionError::Computation(
            FeatureComputationError::UntrustedInput
        ))
    ));
    let definition_event =
        instrument_definition_event(instrument_definition(price("1"), quantity("1")), 6);
    let definition_evidence =
        InstrumentDefinitionEvidence::try_from_event(&definition_event).unwrap();
    let evidence =
        BookStateEvidence::try_from_event_with_definition(&book, &event, &definition_evidence)
            .unwrap();
    let emission = Task5FeatureEmissionInput {
        computed_at: domain::UnixNanos::new(7_000_000_010),
        revision: ObservationRevision::new(1).unwrap(),
        code_commit: CodeRevision::new("0123456789abcdef0123456789abcdef01234567").unwrap(),
    };
    let observations = emit_book_spread_features(
        &registry,
        emission.clone(),
        &book,
        &evidence,
        &tracker,
        decision,
    )
    .expect("authenticated final book evidence should emit both spread outputs");

    assert_eq!(observations.len(), 2);
    assert_eq!(observations[0].feature_id().as_str(), "absolute_spread");
    assert!(matches!(
        observations[0].datum(),
        FeatureDatum::Present(FeatureValue::FixedDecimal(value)) if value.to_string() == "2"
    ));
    assert_eq!(observations[1].feature_id().as_str(), "relative_spread");
    assert!(matches!(
        observations[1].datum(),
        FeatureDatum::Present(FeatureValue::Float64(_))
    ));
    assert_ne!(
        observations[0].lineage_hash(),
        observations[1].lineage_hash()
    );
    let core_observations =
        emit_book_snapshot_core_features(&registry, emission, &book, &evidence, &tracker, decision)
            .unwrap();
    let core_spread_observations = core_observations
        .into_iter()
        .filter(|observation| {
            matches!(
                observation.feature_id().as_str(),
                "absolute_spread" | "relative_spread"
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        observations, core_spread_observations,
        "overlapping public emitters must produce byte-identical observations"
    );

    let alternate_definition_event = instrument_definition_event(
        instrument_definition_with_quote(price("1"), quantity("1"), "USDT"),
        5,
    );
    let alternate_definition_evidence =
        InstrumentDefinitionEvidence::try_from_event(&alternate_definition_event).unwrap();
    let alternate_evidence = BookStateEvidence::try_from_event_with_definition(
        &book,
        &event,
        &alternate_definition_evidence,
    )
    .unwrap();
    let alternate_observations = emit_book_spread_features(
        &registry,
        Task5FeatureEmissionInput {
            computed_at: domain::UnixNanos::new(7_000_000_010),
            revision: ObservationRevision::new(1).unwrap(),
            code_commit: CodeRevision::new("0123456789abcdef0123456789abcdef01234567").unwrap(),
        },
        &book,
        &alternate_evidence,
        &tracker,
        decision,
    )
    .unwrap();
    assert_ne!(
        observations[0].lineage_hash(),
        alternate_observations[0].lineage_hash(),
        "instrument-definition provenance participates in feature lineage"
    );
}

#[test]
fn book_spread_entry_points_reject_multi_source_coverage_identically() {
    let book = trusted_book_with_sequence(&[("100", "10")], &[("102", "30")], 10, 7);
    let event = book_snapshot_event(&book, 1_500_000_000, 7);
    let definition_event =
        instrument_definition_event(instrument_definition(price("1"), quantity("1")), 6);
    let definition_evidence =
        InstrumentDefinitionEvidence::try_from_event(&definition_event).unwrap();
    let evidence =
        BookStateEvidence::try_from_event_with_definition(&book, &event, &definition_evidence)
            .unwrap();
    let source = SourceId::new(SourceKind::Exchange, "test", 1).unwrap();
    let other_source = SourceId::new(SourceKind::Exchange, "test-two", 1).unwrap();
    let keys = [
        WatermarkKey::new(source, PartitionId::new("book").unwrap()),
        WatermarkKey::new(other_source, PartitionId::new("book").unwrap()),
    ];
    let mut tracker = WatermarkTracker::try_new_for_entity(
        keys.iter()
            .cloned()
            .map(PartitionConfig::required)
            .collect(),
        DurationNanos::new(5_000_000_000),
        vec![quality::SourceHealthState::Healthy],
        FeatureEntity::Instrument(instrument()),
    )
    .unwrap();
    for key in &keys {
        tracker
            .advance(
                key,
                WatermarkUpdate::new(
                    domain::UnixNanos::new(7_000_000_000),
                    domain::UnixNanos::new(7_000_000_001),
                    quality::SourceHealthState::Healthy,
                ),
            )
            .unwrap();
    }
    let decision = tracker.decision(
        TimeWindow::try_new(
            domain::UnixNanos::new(1_000_000_000),
            domain::UnixNanos::new(2_000_000_000),
        )
        .unwrap(),
    );
    let mut registry = FeatureRegistry::new();
    for definition in task_five_definitions().unwrap() {
        registry.register(definition).unwrap();
    }
    let emission = Task5FeatureEmissionInput {
        computed_at: domain::UnixNanos::new(7_000_000_010),
        revision: ObservationRevision::new(1).unwrap(),
        code_commit: CodeRevision::new("0123456789abcdef0123456789abcdef01234567").unwrap(),
    };

    assert!(matches!(
        emit_book_spread_features(
            &registry,
            emission.clone(),
            &book,
            &evidence,
            &tracker,
            decision,
        ),
        Err(Task5EmissionError::Computation(
            FeatureComputationError::EligibleUniverseChanged
        ))
    ));
    assert!(matches!(
        emit_book_snapshot_core_features(&registry, emission, &book, &evidence, &tracker, decision,),
        Err(Task5EmissionError::Computation(
            FeatureComputationError::EligibleUniverseChanged
        ))
    ));
}

#[test]
fn authenticated_book_snapshot_emits_only_the_parameter_free_core_family() {
    let book = trusted_book_with_sequence(
        &[("100", "10"), ("99", "20"), ("98", "40"), ("95", "100")],
        &[("102", "30"), ("103", "20"), ("104", "40"), ("107", "100")],
        7,
        7,
    );
    let event = book_snapshot_event(&book, 1_500_000_000, 7);
    let definition_event =
        instrument_definition_event(instrument_definition(price("1"), quantity("1")), 6);
    let definition_evidence =
        InstrumentDefinitionEvidence::try_from_event(&definition_event).unwrap();
    let evidence =
        BookStateEvidence::try_from_event_with_definition(&book, &event, &definition_evidence)
            .unwrap();
    let entity = FeatureEntity::Instrument(instrument());
    let source = SourceId::new(SourceKind::Exchange, "test", 1).unwrap();
    let key = WatermarkKey::new(source, PartitionId::new("book").unwrap());
    let mut tracker = WatermarkTracker::try_new_for_entity(
        vec![PartitionConfig::required(key.clone())],
        DurationNanos::new(5_000_000_000),
        vec![quality::SourceHealthState::Healthy],
        entity,
    )
    .unwrap();
    tracker
        .advance(
            &key,
            WatermarkUpdate::new(
                domain::UnixNanos::new(7_000_000_000),
                domain::UnixNanos::new(7_000_000_001),
                quality::SourceHealthState::Healthy,
            ),
        )
        .unwrap();
    let decision = tracker.decision(
        TimeWindow::try_new(
            domain::UnixNanos::new(1_000_000_000),
            domain::UnixNanos::new(2_000_000_000),
        )
        .unwrap(),
    );
    let mut registry = FeatureRegistry::new();
    for definition in task_five_definitions().unwrap() {
        registry.register(definition).unwrap();
    }
    let observations = emit_book_snapshot_core_features(
        &registry,
        Task5FeatureEmissionInput {
            computed_at: domain::UnixNanos::new(7_000_000_010),
            revision: ObservationRevision::new(1).unwrap(),
            code_commit: CodeRevision::new("0123456789abcdef0123456789abcdef01234567").unwrap(),
        },
        &book,
        &evidence,
        &tracker,
        decision,
    )
    .unwrap();

    assert_eq!(observations.len(), 37);
    let datum = |id: &str| {
        observations
            .iter()
            .find(|observation| observation.feature_id().as_str() == id)
            .unwrap()
            .datum()
    };
    assert!(matches!(
        datum("bid_depth_100bps"),
        FeatureDatum::Present(FeatureValue::FixedDecimal(value)) if value.to_string() == "10"
    ));
    assert!(matches!(
        datum("bid_active_price_levels"),
        FeatureDatum::Present(FeatureValue::Integer(4))
    ));
    assert!(matches!(
        datum("bid_price_level_gap_density"),
        FeatureDatum::Present(FeatureValue::Float64(value))
            if (value.value() - 0.4).abs() < 1e-15
    ));
    assert!(matches!(
        datum("ask_price_level_gap_density"),
        FeatureDatum::Present(FeatureValue::Float64(value))
            if (value.value() - 0.4).abs() < 1e-15
    ));
    assert!(matches!(
        datum("bid_book_slope"),
        FeatureDatum::Present(FeatureValue::Float64(value))
            if (value.value() - 10.0).abs() < 1e-12
    ));
    assert!(matches!(
        datum("ask_book_slope"),
        FeatureDatum::Present(FeatureValue::Float64(value))
            if (value.value() - 10.0).abs() < 1e-12
    ));
    assert!(matches!(
        datum("bid_book_convexity"),
        FeatureDatum::Present(FeatureValue::Float64(value))
            if (value.value() - 20.0).abs() < 1e-12
    ));
    assert!(matches!(
        datum("ask_book_convexity"),
        FeatureDatum::Present(FeatureValue::Float64(value))
            if (value.value() - 20.0).abs() < 1e-12
    ));
    assert!(matches!(
        datum("expected_buy_sweep_cost_usd_10000"),
        FeatureDatum::Missing(MissingnessReason::SourceNotSupported)
    ));
    let truncated_book = trusted_book_with_sequence(
        &[("100", "10"), ("95", "20"), ("90", "20")],
        &[("102", "30")],
        8,
        8,
    );
    let truncated_event = book_snapshot_event(&truncated_book, 1_500_000_000, 8);
    let truncated_evidence = BookStateEvidence::try_from_event_with_definition(
        &truncated_book,
        &truncated_event,
        &definition_evidence,
    )
    .unwrap();
    let truncated_observations = emit_book_snapshot_core_features(
        &registry,
        Task5FeatureEmissionInput {
            computed_at: domain::UnixNanos::new(7_000_000_020),
            revision: ObservationRevision::new(1).unwrap(),
            code_commit: CodeRevision::new("0123456789abcdef0123456789abcdef01234567").unwrap(),
        },
        &truncated_book,
        &truncated_evidence,
        &tracker,
        decision,
    )
    .unwrap();
    let truncated_datum = |id: &str| {
        truncated_observations
            .iter()
            .find(|observation| observation.feature_id().as_str() == id)
            .unwrap()
            .datum()
    };
    assert!(matches!(
        truncated_datum("bid_depth_100bps"),
        FeatureDatum::Present(FeatureValue::FixedDecimal(value)) if value.to_string() == "10"
    ));
    assert_eq!(
        truncated_datum("ask_depth_100bps"),
        &FeatureDatum::Missing(MissingnessReason::InsufficientHistory)
    );
    assert_eq!(
        truncated_datum("book_imbalance_100bps"),
        &FeatureDatum::Missing(MissingnessReason::InsufficientHistory)
    );
    assert!(matches!(
        truncated_datum("bid_price_level_gap_density"),
        FeatureDatum::Present(FeatureValue::Float64(value))
            if (value.value() - 0.8).abs() < 1e-15
    ));
    assert_eq!(
        truncated_datum("ask_price_level_gap_density"),
        &FeatureDatum::Missing(MissingnessReason::InsufficientHistory)
    );
    assert!(matches!(
        truncated_datum("bid_book_slope"),
        FeatureDatum::Present(FeatureValue::Float64(value))
            if (value.value() - 4.0).abs() < 1e-12
    ));
    assert!(matches!(
        truncated_datum("bid_book_convexity"),
        FeatureDatum::Present(FeatureValue::Float64(value))
            if value.value().abs() < 1e-12
    ));
    assert_eq!(
        truncated_datum("ask_book_slope"),
        &FeatureDatum::Missing(MissingnessReason::InsufficientHistory)
    );
    assert_eq!(
        truncated_datum("ask_book_convexity"),
        &FeatureDatum::Missing(MissingnessReason::InsufficientHistory)
    );

    let bid_truncated_book = trusted_book_with_sequence(
        &[("100", "10")],
        &[("102", "30"), ("107", "20"), ("112", "20")],
        9,
        9,
    );
    let bid_truncated_event = book_snapshot_event(&bid_truncated_book, 1_500_000_000, 9);
    let bid_truncated_evidence = BookStateEvidence::try_from_event_with_definition(
        &bid_truncated_book,
        &bid_truncated_event,
        &definition_evidence,
    )
    .unwrap();
    let bid_truncated_observations = emit_book_snapshot_core_features(
        &registry,
        Task5FeatureEmissionInput {
            computed_at: domain::UnixNanos::new(7_000_000_030),
            revision: ObservationRevision::new(1).unwrap(),
            code_commit: CodeRevision::new("0123456789abcdef0123456789abcdef01234567").unwrap(),
        },
        &bid_truncated_book,
        &bid_truncated_evidence,
        &tracker,
        decision,
    )
    .unwrap();
    let bid_truncated_datum = |id: &str| {
        bid_truncated_observations
            .iter()
            .find(|observation| observation.feature_id().as_str() == id)
            .unwrap()
            .datum()
    };
    assert_eq!(
        bid_truncated_datum("bid_depth_100bps"),
        &FeatureDatum::Missing(MissingnessReason::InsufficientHistory)
    );
    assert!(matches!(
        bid_truncated_datum("ask_depth_100bps"),
        FeatureDatum::Present(FeatureValue::FixedDecimal(value)) if value.to_string() == "30"
    ));
    assert_eq!(
        bid_truncated_datum("book_imbalance_100bps"),
        &FeatureDatum::Missing(MissingnessReason::InsufficientHistory)
    );
    assert_eq!(
        bid_truncated_datum("bid_price_level_gap_density"),
        &FeatureDatum::Missing(MissingnessReason::InsufficientHistory)
    );
    assert!(matches!(
        bid_truncated_datum("ask_price_level_gap_density"),
        FeatureDatum::Present(FeatureValue::Float64(value))
            if (value.value() - 0.8).abs() < 1e-15
    ));
    assert_eq!(
        bid_truncated_datum("bid_book_slope"),
        &FeatureDatum::Missing(MissingnessReason::InsufficientHistory)
    );
    assert_eq!(
        bid_truncated_datum("bid_book_convexity"),
        &FeatureDatum::Missing(MissingnessReason::InsufficientHistory)
    );
    assert!(matches!(
        bid_truncated_datum("ask_book_slope"),
        FeatureDatum::Present(FeatureValue::Float64(value))
            if (value.value() - 4.0).abs() < 1e-12
    ));
    assert!(matches!(
        bid_truncated_datum("ask_book_convexity"),
        FeatureDatum::Present(FeatureValue::Float64(value))
            if value.value().abs() < 1e-12
    ));
}

#[tokio::test]
async fn authenticated_trade_flow_emits_closed_integer_decimal_float_and_map_observations() {
    const WINDOW_START_NS: i64 = 1_672_515_780_000_000_000;
    const WINDOW_END_NS: i64 = WINDOW_START_NS + 60_000_000_000;
    let mut trades = Vec::new();
    for (event_time_ns, ordinal, price_value, quantity_value, side) in [
        (WINDOW_START_NS + 10_000_000_000, 1, "100", "2", Side::Buy),
        (WINDOW_START_NS + 20_000_000_000, 2, "101", "1", Side::Sell),
        (WINDOW_START_NS + 40_000_000_000, 3, "100", "3", Side::Buy),
    ] {
        trades.push(
            authenticated_binance_flow_trade(
                event_time_ns,
                ordinal,
                price_value,
                quantity_value,
                side,
            )
            .await
            .1,
        );
    }
    let window = TradeFlowWindow::new(
        trades,
        3,
        domain::UnixNanos::new(WINDOW_START_NS),
        domain::UnixNanos::new(WINDOW_END_NS),
        domain::UnixNanos::new(WINDOW_END_NS + 10),
    )
    .unwrap();
    let entity = FeatureEntity::Instrument(binance_instrument());
    let source = SourceId::new(SourceKind::Exchange, "binance", 1).unwrap();
    let key = WatermarkKey::new(source, PartitionId::new("trades").unwrap());
    let mut tracker = WatermarkTracker::try_new_for_entity(
        vec![PartitionConfig::required(key.clone())],
        DurationNanos::new(5_000_000_000),
        vec![quality::SourceHealthState::Healthy],
        entity,
    )
    .unwrap();
    tracker
        .advance(
            &key,
            WatermarkUpdate::new(
                domain::UnixNanos::new(WINDOW_END_NS + 5_000_000_000),
                domain::UnixNanos::new(WINDOW_END_NS + 5_000_000_001),
                quality::SourceHealthState::Healthy,
            ),
        )
        .unwrap();
    let decision = tracker.decision(
        TimeWindow::try_new(
            domain::UnixNanos::new(WINDOW_START_NS),
            domain::UnixNanos::new(WINDOW_END_NS),
        )
        .unwrap(),
    );
    let mut registry = FeatureRegistry::new();
    for definition in task_five_definitions().unwrap() {
        registry.register(definition).unwrap();
    }

    let observations = emit_trade_flow_core_features(
        &registry,
        Task5FeatureEmissionInput {
            computed_at: domain::UnixNanos::new(WINDOW_END_NS + 5_000_000_010),
            revision: ObservationRevision::new(1).unwrap(),
            code_commit: CodeRevision::new("0123456789abcdef0123456789abcdef01234567").unwrap(),
        },
        &window,
        &tracker,
        decision,
    )
    .expect("authenticated final trade flow should emit the closed core recipe set");

    assert_eq!(observations.len(), 12);
    let observation = |id: &str| {
        observations
            .iter()
            .find(|observation| observation.feature_id().as_str() == id)
            .unwrap()
    };
    assert!(matches!(
        observation("aggressive_buy_count").datum(),
        FeatureDatum::Present(FeatureValue::Integer(2))
    ));
    assert!(matches!(
        observation("aggressive_buy_quantity").datum(),
        FeatureDatum::Present(FeatureValue::FixedDecimal(value)) if value.to_string() == "5"
    ));
    assert!(matches!(
        observation("trade_intensity_per_second").datum(),
        FeatureDatum::Present(FeatureValue::Float64(value))
            if (value.value() - 0.05).abs() < 1e-15
    ));
    assert!(matches!(
        observation("signed_volume_at_price").datum(),
        FeatureDatum::Present(FeatureValue::FixedDecimalMap(value))
            if value.values().len() == 2
                && value.values().get(&fixed("100")).unwrap().to_string() == "5"
                && value.values().get(&fixed("101")).unwrap().to_string() == "-1"
    ));
    assert_ne!(
        observation("aggressive_buy_count").lineage_hash(),
        observation("signed_volume_at_price").lineage_hash()
    );
}

#[test]
fn displayed_depth_recovery_uses_exact_passive_side_loss_and_rational_thresholds() {
    let pre_book = trusted_book_with_sequence(&[("100", "10")], &[("102", "100")], 10, 1);
    let post_book = trusted_book_with_sequence(&[("100", "10")], &[("102", "60")], 20, 2);
    let half_book = trusted_book_with_sequence(&[("100", "10")], &[("102", "80")], 30, 3);
    let full_book = trusted_book_with_sequence(&[("100", "10")], &[("102", "100")], 40, 4);
    let metric = |book: &orderbook::BookSnapshotView, event_time_ns: i64, lineage_byte: u8| {
        BookMetricObservation::from_snapshot(
            book,
            book_policy(),
            price("101"),
            200,
            domain::UnixNanos::new(event_time_ns),
            domain::UnixNanos::new(event_time_ns + 10),
            [lineage_byte; 32],
        )
        .unwrap()
    };
    let pre = metric(&pre_book, 0, 1);
    let post = metric(&post_book, 1_000_000_000, 2);
    let half = metric(&half_book, 3_000_000_000, 3);
    let full = metric(&full_book, 6_000_000_000, 4);
    let trade = flow_trade_at(500_000_000, 9, "101", "40", AggressorSide::Buy);
    let episode = DisplayedDepthRecoveryEpisode::new(
        pre,
        post,
        trade,
        domain::UnixNanos::new(6_000_000_010),
        1_000_000_000,
    )
    .expect("buy shock depletes displayed ask depth");

    assert!((episode.recovery_fraction(&half).unwrap() - 0.5).abs() < 1e-15);
    assert_eq!(
        episode
            .time_to_fraction_ns(&[half.clone(), full.clone()], 8, 1, 2, 6_000_000_000)
            .unwrap(),
        2_000_000_000
    );
    assert_eq!(
        episode
            .time_to_fraction_ns(&[half.clone(), full.clone()], 8, 1, 1, 6_000_000_000)
            .unwrap(),
        5_000_000_000
    );
    assert_eq!(
        episode.time_to_fraction_ns(&[half, full], 1, 1, 1, 6_000_000_000),
        Err(FeatureComputationError::CapacityExceeded)
    );
}

fn trusted_book(
    bids: &[(&str, &str)],
    asks: &[(&str, &str)],
    updated_monotonic_ns: u64,
) -> orderbook::BookSnapshotView {
    trusted_book_with_sequence(bids, asks, updated_monotonic_ns, 1)
}

fn trusted_book_with_sequence(
    bids: &[(&str, &str)],
    asks: &[(&str, &str)],
    updated_monotonic_ns: u64,
    last_sequence: u64,
) -> orderbook::BookSnapshotView {
    book_observed_at(
        bids,
        asks,
        updated_monotonic_ns,
        updated_monotonic_ns,
        last_sequence,
    )
}

fn book_observed_at(
    bids: &[(&str, &str)],
    asks: &[(&str, &str)],
    updated_monotonic_ns: u64,
    observed_monotonic_ns: u64,
    last_sequence: u64,
) -> orderbook::BookSnapshotView {
    book_observed_at_for_instrument(
        instrument(),
        bids,
        asks,
        updated_monotonic_ns,
        observed_monotonic_ns,
        last_sequence,
    )
}

fn book_observed_at_for_instrument(
    instrument: InstrumentId,
    bids: &[(&str, &str)],
    asks: &[(&str, &str)],
    updated_monotonic_ns: u64,
    observed_monotonic_ns: u64,
    last_sequence: u64,
) -> orderbook::BookSnapshotView {
    let mut engine = OrderBookEngine::new(BookConfig {
        instrument,
        price_tick: price("1"),
        quantity_step: quantity("1"),
        max_levels_per_side: 32,
        max_buffered_deltas: 8,
        max_buffered_level_updates: 64,
        sequence_policy: SequencePolicy::RangeContainsNext,
        checksum_policy: ChecksumPolicy::Disabled,
        max_l3_orders: None,
        max_l3_levels_per_side: None,
    })
    .expect("valid test book configuration");
    let session = BookSession {
        connection_epoch: 1,
        subscription_epoch: 1,
        instrument_generation: 1,
    };
    engine
        .start_session(session, SnapshotStrategy::StreamSnapshot)
        .expect("test session starts");
    engine
        .apply_snapshot(
            BookSnapshot {
                bids: levels(bids),
                asks: levels(asks),
                last_sequence,
            },
            session,
            updated_monotonic_ns,
        )
        .expect("test snapshot is valid");
    engine
        .snapshot_at(observed_monotonic_ns)
        .expect("test book is synchronized")
}

fn flow_trade(event_time_ns: i64, lineage_byte: u8, side: AggressorSide) -> FlowTradeObservation {
    flow_trade_at(event_time_ns, lineage_byte, "100", "1", side)
}

fn flow_trade_at(
    event_time_ns: i64,
    lineage_byte: u8,
    price_value: &str,
    quantity_value: &str,
    side: AggressorSide,
) -> FlowTradeObservation {
    FlowTradeObservation::new(
        instrument(),
        domain::UnixNanos::new(event_time_ns),
        domain::UnixNanos::new(event_time_ns + 10),
        [lineage_byte; 32],
        price(price_value),
        quantity(quantity_value),
        side,
        AggressorAuthority::supplied("test-taker-side", 1).unwrap(),
    )
    .expect("valid flow trade")
}

async fn authenticated_binance_flow_trade(
    event_time_ns: i64,
    ordinal: u8,
    price_value: &str,
    quantity_value: &str,
    side: Side,
) -> (EventEnvelope, FlowTradeObservation) {
    let source = SourceId::new(SourceKind::Exchange, "binance", 1).unwrap();
    assert_eq!(event_time_ns % 1_000_000, 0);
    let event_time_ms = event_time_ns / 1_000_000;
    let buyer_is_market_maker = match side {
        Side::Buy => false,
        Side::Sell => true,
        Side::Unknown => panic!("authoritative Binance fixture side must be known"),
    };
    let first_trade_id = u64::from(ordinal) * 100;
    let raw_payload = format!(
        "{{\"e\":\"aggTrade\",\"E\":{event_time_ms},\"s\":\"BTCUSDT\",\"a\":{},\"p\":\"{price_value}\",\"q\":\"{quantity_value}\",\"f\":{first_trade_id},\"l\":{},\"T\":{event_time_ms},\"m\":{buyer_is_market_maker},\"M\":true}}",
        u64::from(ordinal),
        first_trade_id + 1,
    )
    .into_bytes();
    let receive_wall_timestamp = domain::UnixNanos::new(event_time_ns + 1);
    let receive_monotonic_ns = u64::from(ordinal);

    let directory = tempfile::tempdir().expect("temporary WAL");
    let segment_metadata = SegmentMetadata::new(
        [ordinal.max(1); 16],
        1,
        "schema",
        "installation",
        "build",
        vec![
            StreamDescriptor::new(7, wal_stream_source_identity(&source), "fixture")
                .expect("stream descriptor"),
        ],
    )
    .expect("segment metadata");
    let mut writer = SegmentedWalWriter::create(
        directory.path(),
        segment_metadata,
        RotationPolicy::default(),
        1,
    )
    .expect("WAL writer");
    let authority = writer.append_authority();
    let (proof, compression_job) = writer
        .append(
            RecordMetadata {
                flags: 0,
                stream_id: 7,
                connection_epoch: 1,
                record_sequence: u64::from(ordinal),
                receive_wall_time_ns: receive_wall_timestamp.value(),
                receive_monotonic_time_ns: receive_monotonic_ns,
            },
            &raw_payload,
            receive_monotonic_ns,
            receive_wall_timestamp.value(),
        )
        .expect("durable raw append")
        .into_parts();
    assert!(compression_job.is_none());

    let raw_channel = DurableRawCaptureChannel::new(
        authority,
        source.clone(),
        NonZeroU32::new(7).unwrap(),
        1,
        1_024,
    )
    .expect("raw channel");
    let (raw_client, mut raw_worker) = raw_channel.split();
    let capture = RawCapture::try_new(
        source,
        NonZeroU32::new(7).unwrap(),
        NonZeroU64::new(1).unwrap(),
        NonZeroU64::new(u64::from(ordinal)).unwrap(),
        receive_wall_timestamp,
        receive_monotonic_ns,
        raw_payload.clone().into_boxed_slice(),
    )
    .expect("raw capture");
    let raw_submission = tokio::spawn(async move { raw_client.submit(capture).await });
    raw_worker
        .recv()
        .await
        .expect("raw worker request")
        .acknowledge(proof)
        .expect("WAL acknowledgement");
    let raw_reference = raw_submission
        .await
        .expect("raw submit task")
        .expect("durable raw reference");
    let parsed =
        parse_durable_native_message(BinanceInput::SpotWebSocket, &raw_payload, &raw_reference)
            .expect("sealed durable Binance parse");
    let catalog = binance_catalog();
    let context = NormalizationContext::try_new(
        &catalog,
        &raw_reference,
        domain::UnixNanos::new(event_time_ns + 2),
        domain::UnixNanos::new(event_time_ns - 1),
        NonZeroU64::new(1).unwrap(),
        "feature-engine-test",
    )
    .expect("Binance normalization context");
    let receipt = normalize_trade_with_receipt(parsed, &context)
        .expect("connector-owned Binance trade receipt");
    let event = receipt.event().clone();
    let observation = FlowTradeObservation::try_from_binance_receipt(&receipt)
        .expect("sealed Binance receipt should authenticate");
    (event, observation)
}

fn book_snapshot_event(
    book: &orderbook::BookSnapshotView,
    event_time_ns: i64,
    ordinal: u8,
) -> EventEnvelope {
    let session = book.session();
    EventEnvelope::new(
        UncheckedEventMetadata {
            schema_version: 3,
            source: SourceId::new(SourceKind::Exchange, "test", 1).unwrap(),
            venue: Some(VenueId::new("test").unwrap()),
            instrument_id: Some(book.instrument().clone()),
            exchange_timestamp: Some(domain::UnixNanos::new(event_time_ns)),
            exchange_transaction_timestamp: Some(domain::UnixNanos::new(event_time_ns)),
            receive_wall_timestamp: domain::UnixNanos::new(event_time_ns + 1),
            receive_monotonic_ns: book.updated_monotonic_ns(),
            normalization_timestamp: domain::UnixNanos::new(event_time_ns + 2),
            connection_started_at: domain::UnixNanos::new(1),
            sequence_number: Some(book.last_source_sequence()),
            previous_sequence_number: None,
            connection_epoch: session.connection_epoch,
            subscription_epoch: session.subscription_epoch,
            snapshot_kind: SnapshotKind::Snapshot,
            source_checksum: None,
            raw_payload_hash: [ordinal.max(1); 32],
            parser_version: "parser-v1".to_owned(),
            normalizer_version: "normalizer-v1".to_owned(),
            ingestion_instance: "test-ingestion".to_owned(),
            quality_score_ppm: 950_000,
            quality_flags: QualityFlags::NONE,
        },
        UncheckedEventPayload::BookSnapshot(BookSnapshot {
            bids: book.bids().to_vec(),
            asks: book.asks().to_vec(),
            last_sequence: book.last_source_sequence(),
        }),
    )
    .unwrap()
}

fn normalized_book_event(
    payload: UncheckedEventPayload,
    sequence_number: u64,
    previous_sequence_number: Option<u64>,
    snapshot_kind: SnapshotKind,
    receive_monotonic_ns: u64,
    ordinal: u8,
) -> EventEnvelope {
    let event_time = i64::try_from(sequence_number).unwrap() * 100_000_000;
    EventEnvelope::new(
        UncheckedEventMetadata {
            schema_version: 3,
            source: SourceId::new(SourceKind::Exchange, "test", 1).unwrap(),
            venue: Some(VenueId::new("test").unwrap()),
            instrument_id: Some(instrument()),
            exchange_timestamp: Some(domain::UnixNanos::new(event_time)),
            exchange_transaction_timestamp: Some(domain::UnixNanos::new(event_time)),
            receive_wall_timestamp: domain::UnixNanos::new(event_time + 1),
            receive_monotonic_ns,
            normalization_timestamp: domain::UnixNanos::new(event_time + 2),
            connection_started_at: domain::UnixNanos::new(1),
            sequence_number: Some(sequence_number),
            previous_sequence_number,
            connection_epoch: 1,
            subscription_epoch: 1,
            snapshot_kind,
            source_checksum: None,
            raw_payload_hash: [ordinal; 32],
            parser_version: "parser-v1".to_owned(),
            normalizer_version: "normalizer-v1".to_owned(),
            ingestion_instance: "test-ingestion".to_owned(),
            quality_score_ppm: 950_000,
            quality_flags: QualityFlags::NONE,
        },
        payload,
    )
    .unwrap()
}

fn instrument_definition_event(definition: InstrumentDefinition, ordinal: u8) -> EventEnvelope {
    EventEnvelope::new(
        UncheckedEventMetadata {
            schema_version: 3,
            source: SourceId::new(SourceKind::Exchange, "test", 1).unwrap(),
            venue: Some(VenueId::new("test").unwrap()),
            instrument_id: Some(definition.id().clone()),
            exchange_timestamp: Some(domain::UnixNanos::new(500)),
            exchange_transaction_timestamp: None,
            receive_wall_timestamp: domain::UnixNanos::new(501),
            receive_monotonic_ns: u64::from(ordinal),
            normalization_timestamp: domain::UnixNanos::new(502),
            connection_started_at: domain::UnixNanos::new(1),
            sequence_number: None,
            previous_sequence_number: None,
            connection_epoch: 1,
            subscription_epoch: 1,
            snapshot_kind: SnapshotKind::NotApplicable,
            source_checksum: None,
            raw_payload_hash: [ordinal; 32],
            parser_version: "parser-v1".to_owned(),
            normalizer_version: "normalizer-v1".to_owned(),
            ingestion_instance: "test-ingestion".to_owned(),
            quality_score_ppm: 960_000,
            quality_flags: QualityFlags::NONE,
        },
        UncheckedEventPayload::InstrumentDefinition(definition),
    )
    .unwrap()
}

fn instrument_definition(price_tick: Price, quantity_step: Quantity) -> InstrumentDefinition {
    instrument_definition_with_quote(price_tick, quantity_step, "USD")
}

fn instrument_definition_with_quote(
    price_tick: Price,
    quantity_step: Quantity,
    quote_symbol: &str,
) -> InstrumentDefinition {
    let base = AssetId::new(AssetNamespace::Native, "bitcoin", "", "BTC", 1).unwrap();
    let quote = if quote_symbol == "USD" {
        AssetId::new(AssetNamespace::Fiat, "", "", quote_symbol, 1).unwrap()
    } else {
        AssetId::new(AssetNamespace::Native, "tether", "", quote_symbol, 1).unwrap()
    };
    InstrumentDefinition::new(InstrumentDefinitionInput {
        id: instrument(),
        product_type: ProductType::Spot,
        base_asset: base,
        quote_asset: quote.clone(),
        settlement_asset: quote,
        contract_multiplier: fixed("1"),
        contract_value_unit: ContractValueUnit::Base,
        contract_kind: ContractKind::None,
        expiry_time: None,
        strike: None,
        option_side: None,
        price_tick,
        quantity_step,
        listing_time: domain::UnixNanos::new(1),
        delisting_time: None,
    })
    .unwrap()
}

fn instrument() -> InstrumentId {
    InstrumentId::new_for_product(
        VenueId::new("test").expect("venue"),
        "BTCUSDT",
        ProductType::Spot,
        1,
    )
    .expect("instrument")
}

fn binance_instrument() -> InstrumentId {
    InstrumentId::new_for_product(
        VenueId::new("binance").expect("venue"),
        "BTCUSDT",
        ProductType::Spot,
        1,
    )
    .expect("instrument")
}

fn binance_catalog() -> std::sync::Arc<instrument_registry::CatalogSnapshot> {
    let quote = AssetId::new(AssetNamespace::Native, "tether", "", "USDT", 1).unwrap();
    let definition = InstrumentDefinition::new(InstrumentDefinitionInput {
        id: binance_instrument(),
        product_type: ProductType::Spot,
        base_asset: AssetId::new(AssetNamespace::Native, "bitcoin", "", "BTC", 1).unwrap(),
        quote_asset: quote.clone(),
        settlement_asset: quote,
        contract_multiplier: fixed("1"),
        contract_value_unit: ContractValueUnit::Base,
        contract_kind: ContractKind::None,
        expiry_time: None,
        strike: None,
        option_side: None,
        price_tick: price("0.01"),
        quantity_step: quantity("0.001"),
        listing_time: domain::UnixNanos::new(1),
        delisting_time: None,
    })
    .unwrap();
    let mut registry = InstrumentRegistry::new();
    registry
        .append_definition(
            definition,
            RevisionMetadata::try_new(domain::UnixNanos::new(1), "feature-engine:binance").unwrap(),
        )
        .unwrap();
    registry.snapshot().unwrap()
}

fn book_policy() -> BookEvidencePolicy {
    BookEvidencePolicy::new(
        price("1"),
        "normal-l2-3s",
        1,
        "event-time-proxy",
        1,
        "same-venue-midpoint",
        1,
    )
    .expect("valid test book evidence policy")
}

fn levels(values: &[(&str, &str)]) -> Vec<BookLevel> {
    values
        .iter()
        .map(|(price_value, quantity_value)| BookLevel {
            price: price(price_value),
            quantity: quantity(quantity_value),
            order_count: None,
        })
        .collect()
}

fn fixed(value: &str) -> FixedDecimal {
    FixedDecimal::parse_canonical(value).expect("canonical fixed decimal")
}

fn price(value: &str) -> Price {
    Price::new(fixed(value)).expect("positive price")
}

fn quantity(value: &str) -> Quantity {
    Quantity::new(fixed(value)).expect("nonnegative quantity")
}
