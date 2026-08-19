use std::{
    collections::BTreeSet,
    fs::File,
    num::{NonZeroU32, NonZeroU64},
    os::fd::AsFd,
    str::FromStr,
};

use collector_runtime::{
    AdmissionLimits, CollectorSupervisor, CoverageTier, OperationalQualityEvent,
    OperationalQualitySampleInput, RetryPolicy, SourcePolicy,
};
use connector_binance::{
    BinanceDerivativeStream, BinanceInput, NormalizationContext,
    normalize_derivative_with_receipts, parse_durable_native_message,
};
use connector_core::{
    Completeness, DerivativeNormalizationReceipt, DerivativeStream, DurableRawCaptureChannel,
    DurableRawReference, RawCapture, StreamClass, wal_stream_source_identity,
};
use consolidated_market::{
    CandidatePriceKind, ConsolidatedOutcome, EstimatorConfigInput, FairPrice, FairPriceEstimator,
    InstrumentProvenance, MetadataStatus, PolicyId, Ppm, TrustedBookAdmission,
    TrustedBookAdmissionInput, VenueQuote, VenueQuoteInput, VenueTradingState,
    VerifiedCatalogSnapshot,
};
use domain::{
    AssetId, AssetNamespace, ContractKind, ContractValueUnit, InstrumentDefinition,
    InstrumentDefinitionInput, InstrumentId, ProductType, SourceId, SourceKind, UnixNanos, VenueId,
};
use event_envelope::{EventEnvelope, QualityFlags};
use feature_engine::{
    FinalizationDecision, PartitionConfig, PartitionId, TimeWindow, WatermarkKey, WatermarkTracker,
    WatermarkUpdate,
    features::{
        CrossVenueSnapshot, DispersionClassification, LiquidationNotionalInterpretation,
        MAX_CROSS_VENUE_OBSERVATIONS, Task6ComputationAvailability, Task6EmissionError,
        Task6FeatureEmissionInput, Task6MissingEmissionInput, emit_cascade_eligibility_gate,
        emit_cross_venue_aggregate_features, emit_cross_venue_source_features,
        emit_derivative_change_features, emit_derivative_snapshot_features,
        emit_liquidation_completeness_feature, emit_liquidation_features,
        emit_operational_source_features, emit_storage_pressure_feature,
        emit_unavailable_task_six_feature, funding_change, indicative_price_dispersion,
        liquidation_velocity, mark_index_divergence, open_interest_change,
        operational_quality_snapshot, predicted_funding_rate, task_six_definitions,
        task_six_recipes, venue_depth_concentration,
    },
};
use feature_registry::{
    CodeRevision, DurationNanos, EntityScope, FeatureDatum, FeatureEntity, FeatureId,
    FeatureRegistry, FinalityState, MissingnessReason, ObservationRevision, RegistryError,
};
use fixed_decimal::{FixedDecimal, Notional, Price, Quantity};
use instrument_registry::{InstrumentRegistry, RevisionMetadata};
use orderbook::{
    BookConfig, BookSession, ChecksumPolicy, OrderBookEngine, SequencePolicy, SnapshotStrategy,
};
use quality::SourceHealthState;
use raw_wal::{
    frame::RecordMetadata,
    manager::{RotationPolicy, SegmentedWalWriter},
    prologue::{SegmentMetadata, StreamDescriptor},
};
use semver::Version;

const USDM_LIQUIDATION: &[u8] =
    include_bytes!("../../../fixtures/exchanges/binance/usdm-liquidation.json");
const USDM_MARK: &[u8] = include_bytes!("../../../fixtures/exchanges/binance/usdm-mark-price.json");
const USDM_OI: &[u8] =
    include_bytes!("../../../fixtures/exchanges/binance/usdm-open-interest.json");
const RECEIVE_TIME: UnixNanos = UnixNanos::new(1_672_515_782_137_000_000);
const NORMALIZATION_TIME: UnixNanos = UnixNanos::new(1_672_515_782_137_000_001);
const CONNECTION_START: UnixNanos = UnixNanos::new(1_672_515_700_000_000_000);

#[tokio::test]
async fn mark_funding_and_open_interest_require_their_exact_sealed_stream_receipts() {
    let mark_receipts = derivative_receipts(USDM_MARK, BinanceInput::UsdMMarkPriceWebSocket).await;
    let mark = mark_receipts
        .iter()
        .find(|receipt| receipt.stream() == BinanceDerivativeStream::MarkIndex)
        .expect("mark receipt");
    let funding = mark_receipts
        .iter()
        .find(|receipt| receipt.stream() == BinanceDerivativeStream::Funding)
        .expect("funding receipt");
    let projection = predicted_funding_rate(funding).expect("sealed funding projection");
    assert_eq!(projection.rate().to_string(), "0.0001");
    assert_eq!(
        projection.observed_at(),
        UnixNanos::new(1_672_515_782_136_000_000)
    );
    assert_eq!(
        projection.next_funding_time(),
        UnixNanos::new(1_672_531_200_000_000_000)
    );

    let divergence = mark_index_divergence(mark).expect("sealed mark/index observation");
    assert!((divergence.value() - (16_694.77 / 16_693.54 - 1.0)).abs() < 1e-12);
    assert!(mark_index_divergence(funding).is_err());

    let later_mark = String::from_utf8(USDM_MARK.to_vec())
        .expect("utf8 mark fixture")
        .replace("\"E\": 1672515782136", "\"E\": 1672515782137")
        .replace("\"r\": \"0.0001\"", "\"r\": \"0.0003\"");
    let later_receipts =
        derivative_receipts(later_mark.as_bytes(), BinanceInput::UsdMMarkPriceWebSocket).await;
    let later_funding = later_receipts
        .iter()
        .find(|receipt| receipt.stream() == BinanceDerivativeStream::Funding)
        .expect("later funding receipt");
    assert_eq!(
        funding_change(funding, later_funding)
            .expect("monotonic same-stream funding")
            .to_string(),
        "0.0002"
    );
    assert!(funding_change(later_funding, funding).is_err());

    let first_oi = derivative_receipts(USDM_OI, BinanceInput::UsdMOpenInterestRest).await;
    let later_oi_json = String::from_utf8(USDM_OI.to_vec())
        .expect("utf8 OI fixture")
        .replace("\"10659.509\"", "\"10000\"")
        .replace("1672515782136", "1672515782137");
    let later_oi =
        derivative_receipts(later_oi_json.as_bytes(), BinanceInput::UsdMOpenInterestRest).await;
    let instrument = linear_perpetual();
    let change = open_interest_change(&first_oi[0], &later_oi[0], &instrument)
        .expect("sealed monotonic OI observations");
    assert_eq!(change.native_change().to_string(), "-659.509");
    assert_eq!(change.current_quote_notional(), None);
    assert!((change.relative_change().value() - (10_000.0 / 10_659.509 - 1.0)).abs() < 1e-12);
    assert_eq!(
        open_interest_change(&first_oi[0], &later_oi[0], &same_id_inverse_perpetual(),),
        Err(feature_engine::features::FeatureComputationError::UntrustedInput)
    );
}

#[tokio::test]
async fn typed_derivative_emitters_require_sealed_receipts_and_exact_finality() {
    let registry = task_six_registry();
    let instrument = linear_perpetual();
    let source_ids = vec![source("binance")];
    let snapshot_window = TimeWindow::try_new(
        UnixNanos::new(1_672_515_782_000_000_000),
        UnixNanos::new(1_672_515_783_000_000_000),
    )
    .expect("snapshot window");
    let entity = FeatureEntity::Instrument(instrument.id().clone());
    let (snapshot_tracker, snapshot_decision) = finalized_tracker(
        entity.clone(),
        &source_ids,
        snapshot_window,
        "derivative-snapshot",
    );
    let emission = task_six_emission(UnixNanos::new(1_672_515_788_000_000_001));
    let mark_receipts = derivative_receipts(USDM_MARK, BinanceInput::UsdMMarkPriceWebSocket).await;
    let mut snapshot_ids = BTreeSet::new();
    for receipt in &mark_receipts {
        let observations = emit_derivative_snapshot_features(
            &registry,
            emission.clone(),
            receipt,
            &snapshot_tracker,
            snapshot_decision,
        )
        .expect("mark/funding observation");
        snapshot_ids.insert(observations[0].feature_id().as_str().to_owned());
    }
    let oi_receipts = derivative_receipts(USDM_OI, BinanceInput::UsdMOpenInterestRest).await;
    let oi_observations = emit_derivative_snapshot_features(
        &registry,
        emission.clone(),
        &oi_receipts[0],
        &snapshot_tracker,
        snapshot_decision,
    )
    .expect("open-interest observation");
    snapshot_ids.insert(oi_observations[0].feature_id().as_str().to_owned());
    assert_eq!(
        snapshot_ids,
        BTreeSet::from([
            "mark_index_divergence".to_owned(),
            "open_interest_native".to_owned(),
            "predicted_funding_rate".to_owned(),
        ])
    );

    let later_mark = String::from_utf8(USDM_MARK.to_vec())
        .expect("mark fixture")
        .replace("\"E\": 1672515782136", "\"E\": 1672515782137")
        .replace("\"r\": \"0.0001\"", "\"r\": \"0.0003\"");
    let later_mark_receipts =
        derivative_receipts(later_mark.as_bytes(), BinanceInput::UsdMMarkPriceWebSocket).await;
    let previous_funding = mark_receipts
        .iter()
        .find(|receipt| receipt.stream() == BinanceDerivativeStream::Funding)
        .expect("previous funding");
    let current_funding = later_mark_receipts
        .iter()
        .find(|receipt| receipt.stream() == BinanceDerivativeStream::Funding)
        .expect("current funding");

    let later_oi_json = String::from_utf8(USDM_OI.to_vec())
        .expect("OI fixture")
        .replace("\"10659.509\"", "\"10000\"")
        .replace("1672515782136", "1672515782137");
    let later_oi =
        derivative_receipts(later_oi_json.as_bytes(), BinanceInput::UsdMOpenInterestRest).await;
    let flow_window = TimeWindow::try_new(
        UnixNanos::new(1_672_515_723_000_000_000),
        UnixNanos::new(1_672_515_783_000_000_000),
    )
    .expect("flow window");
    let (flow_tracker, flow_decision) =
        finalized_tracker(entity, &source_ids, flow_window, "derivative-flow");
    let funding_change_observation = emit_derivative_change_features(
        &registry,
        emission.clone(),
        previous_funding,
        current_funding,
        &flow_tracker,
        flow_decision,
    )
    .expect("funding change");
    assert_eq!(
        funding_change_observation[0].feature_id().as_str(),
        "funding_rate_change"
    );
    let oi_change_observation = emit_derivative_change_features(
        &registry,
        emission,
        &oi_receipts[0],
        &later_oi[0],
        &flow_tracker,
        flow_decision,
    )
    .expect("OI change");
    assert_eq!(
        oi_change_observation[0].feature_id().as_str(),
        "open_interest_relative_change"
    );

    assert!(matches!(
        emit_derivative_snapshot_features(
            &registry,
            task_six_emission(NORMALIZATION_TIME),
            &mark_receipts[0],
            &snapshot_tracker,
            snapshot_decision,
        ),
        Err(Task6EmissionError::Computation(
            feature_engine::features::FeatureComputationError::FutureKnowledge
        ))
    ));
}

#[tokio::test]
async fn derivative_emission_cannot_upgrade_low_or_flagged_input_quality() {
    let registry = task_six_registry();
    let instrument = linear_perpetual();
    let entity = FeatureEntity::Instrument(instrument.id().clone());
    let window = TimeWindow::try_new(
        UnixNanos::new(1_672_515_782_000_000_000),
        UnixNanos::new(1_672_515_783_000_000_000),
    )
    .expect("snapshot window");
    let (tracker, decision) =
        finalized_tracker(entity, &[source("binance")], window, "derivative-quality");
    let emission = task_six_emission(UnixNanos::new(1_672_515_788_000_000_001));

    let low_quality = derivative_receipt_with_quality(
        USDM_MARK,
        BinanceInput::UsdMMarkPriceWebSocket,
        DerivativeStream::MarkIndex,
        0,
        QualityFlags::NONE,
    )
    .await;
    assert!(matches!(
        emit_derivative_snapshot_features(
            &registry,
            emission.clone(),
            &low_quality,
            &tracker,
            decision,
        ),
        Err(Task6EmissionError::Registry(
            RegistryError::QualityBelowRequirement
        ))
    ));

    let degraded = derivative_receipt_with_quality(
        USDM_MARK,
        BinanceInput::UsdMMarkPriceWebSocket,
        DerivativeStream::MarkIndex,
        event_envelope::MAX_QUALITY_SCORE_PPM,
        QualityFlags::SOURCE_DEGRADED,
    )
    .await;
    assert!(matches!(
        emit_derivative_snapshot_features(&registry, emission, &degraded, &tracker, decision,),
        Err(Task6EmissionError::Computation(
            feature_engine::features::FeatureComputationError::UntrustedInput
        ))
    ));
}

#[test]
fn midprice_dispersion_is_explicitly_non_executable() {
    let feature = indicative_price_dispersion(&[price("99"), price("100"), price("102")])
        .expect("valid venue prices");

    assert_eq!(
        feature.classification(),
        DispersionClassification::IndicativeMidpriceOnly
    );
    assert!(!feature.is_executable());
    assert!((feature.relative_range().value() - 0.03).abs() < 1e-12);
    assert!((feature.relative_mad().value() - 0.01).abs() < 1e-12);
}

#[test]
fn venue_depth_concentration_is_permutation_invariant_and_bounded() {
    let left = venue_depth_concentration(&[fixed("50"), fixed("30"), fixed("20")])
        .expect("positive depth");
    let right = venue_depth_concentration(&[fixed("20"), fixed("50"), fixed("30")])
        .expect("positive depth");

    assert_eq!(left, right);
    assert!((left.value() - 0.38).abs() < 1e-12);
    assert!(venue_depth_concentration(&[fixed("0"), fixed("0")]).is_err());
    assert_eq!(
        venue_depth_concentration(&vec![fixed("1"); MAX_CROSS_VENUE_OBSERVATIONS + 1]),
        Err(feature_engine::features::FeatureComputationError::CapacityExceeded)
    );
}

#[test]
fn cross_venue_snapshot_requires_catalog_bound_book_state_and_stays_non_executable() {
    let direct = direct_cross_venue_fair_price();
    assert!(CrossVenueSnapshot::try_from_fair_price(&direct).is_err());

    let trusted = trusted_cross_venue_fair_price();
    let snapshot =
        CrossVenueSnapshot::try_from_fair_price(&trusted).expect("catalog-bound book state");
    assert_eq!(snapshot.venues().len(), 2);
    assert!(!snapshot.is_executable());
    assert_eq!(snapshot.healthy_venue_fraction().value(), 1.0);
    assert!(snapshot.depth_concentration().value() >= 0.5);
    assert!(snapshot.depth_concentration().value() <= 1.0);
    assert_eq!(snapshot.stale_quote_count(), 0);
    assert_ne!(snapshot.lineage_digest(), [0; 32]);
    let share_sum = snapshot
        .venues()
        .iter()
        .map(|venue| venue.reference_depth_share().value())
        .sum::<f64>();
    assert!((share_sum - 1.0).abs() < 1e-12);
}

#[test]
fn typed_cross_venue_emitters_preserve_asset_and_source_identity() {
    let fair_price = trusted_cross_venue_fair_price();
    let window = TimeWindow::try_new(UnixNanos::new(1_000_000_000), UnixNanos::new(2_000_000_000))
        .expect("snapshot window");
    let sources = vec![source("alpha"), source("beta")];
    let (asset_tracker, asset_decision) = finalized_tracker(
        FeatureEntity::Asset(btc()),
        &sources,
        window,
        "cross-venue-asset",
    );
    let registry = task_six_registry();
    let emission = task_six_emission(UnixNanos::new(7_000_000_001));
    let aggregate = emit_cross_venue_aggregate_features(
        &registry,
        emission.clone(),
        &fair_price,
        &asset_tracker,
        asset_decision,
    )
    .expect("aggregate observations");
    assert_eq!(aggregate.len(), 6);
    assert!(aggregate.iter().all(|observation| {
        observation.entity() == &FeatureEntity::Asset(btc())
            && observation.finality_state() == FinalityState::Final
            && matches!(observation.datum(), FeatureDatum::Present(_))
    }));
    assert_eq!(
        aggregate
            .iter()
            .map(|observation| observation.feature_id().as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "cross_source_disagreement",
            "cross_venue_median_absolute_dispersion",
            "healthy_venue_fraction",
            "indicative_cross_venue_price_range",
            "source_coverage_fraction",
            "venue_concentration_index",
        ])
    );
    let flow_window = TimeWindow::try_new(
        UnixNanos::new(1_000_000_000),
        UnixNanos::new(61_000_000_000),
    )
    .expect("flow window");
    let (cascade_tracker, cascade_decision) = finalized_tracker(
        FeatureEntity::Asset(btc()),
        &sources,
        flow_window,
        "cross-venue-cascade",
    );
    let cascade = emit_cascade_eligibility_gate(
        &registry,
        task_six_emission(UnixNanos::new(70_000_000_000)),
        &fair_price,
        &cascade_tracker,
        cascade_decision,
    )
    .expect("cascade gate");
    assert_eq!(cascade.feature_id().as_str(), "cascade_eligibility_gate");

    let source_entity = FeatureEntity::AssetSource(btc(), source("alpha"));
    let (source_tracker, source_decision) = finalized_tracker(
        source_entity.clone(),
        &sources,
        window,
        "cross-venue-source",
    );
    let per_source = emit_cross_venue_source_features(
        &registry,
        emission,
        &fair_price,
        &source("alpha"),
        &source_tracker,
        source_decision,
    )
    .expect("source observations");
    assert_eq!(per_source.len(), 2);
    assert!(per_source.iter().all(|observation| {
        observation.entity() == &source_entity
            && observation.finality_state() == FinalityState::Final
    }));
    assert_eq!(
        per_source
            .iter()
            .map(|observation| observation.feature_id().as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["venue_depth_share", "venue_midprice_deviation"])
    );
}

#[tokio::test]
async fn sampled_liquidations_are_observed_lower_bounds_not_complete_market_flow() {
    let instrument = linear_perpetual();
    let raw = durable_reference(
        USDM_LIQUIDATION,
        BinanceInput::UsdMLiquidationWebSocket.wal_stream_name(),
    )
    .await;
    let parsed = parse_durable_native_message(
        BinanceInput::UsdMLiquidationWebSocket,
        USDM_LIQUIDATION,
        &raw,
    )
    .expect("durable liquidation parse");
    let catalog = derivative_catalog();
    let context = NormalizationContext::try_new(
        &catalog,
        &raw,
        NORMALIZATION_TIME,
        CONNECTION_START,
        NonZeroU64::new(1).expect("subscription epoch"),
        "task6-test",
    )
    .expect("normalization context");
    let receipts = normalize_derivative_with_receipts(parsed, &context)
        .expect("connector-owned derivative receipt");
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].stream(), BinanceDerivativeStream::Liquidation);

    let window = TimeWindow::try_new(
        UnixNanos::new(1_672_515_723_000_000_000),
        UnixNanos::new(1_672_515_783_000_000_000),
    )
    .expect("valid window");
    let key = WatermarkKey::new(
        source("binance"),
        PartitionId::new("liquidation").expect("partition"),
    );
    let mut tracker = WatermarkTracker::try_new_for_entity(
        vec![PartitionConfig::required(key.clone())],
        DurationNanos::new(0),
        vec![SourceHealthState::Healthy],
        FeatureEntity::Instrument(instrument.id().clone()),
    )
    .expect("watermark tracker");
    let provisional = tracker.decision(window);
    assert_eq!(
        liquidation_velocity(&receipts, &instrument, &tracker, provisional),
        Err(feature_engine::features::FeatureComputationError::UntrustedInput)
    );
    tracker
        .advance(
            &key,
            WatermarkUpdate::new(
                UnixNanos::new(1_672_515_788_000_000_000),
                UnixNanos::new(1_672_515_788_000_000_000),
                SourceHealthState::Healthy,
            ),
        )
        .expect("final watermark");
    let decision = tracker.decision(window);
    let metrics = liquidation_velocity(&receipts, &instrument, &tracker, decision)
        .expect("sampled observations remain measurable");

    assert_eq!(metrics.observed_count(), 1);
    assert_eq!(metrics.observed_quote_notional().to_string(), "233.6572");
    assert_eq!(
        metrics.notional_interpretation(),
        LiquidationNotionalInterpretation::ObservedLowerBound
    );
    assert!(metrics.coverage().is_sampled());
    assert!(!metrics.coverage().is_complete());
    assert_eq!(metrics.coverage().sampling_window_ms(), Some(1_000));
    assert!((metrics.observed_count_per_second().value() - (1.0 / 60.0)).abs() < 1e-12);

    let observations = emit_liquidation_features(
        &task_six_registry(),
        task_six_emission(UnixNanos::new(1_672_515_788_000_000_001)),
        &receipts,
        &instrument,
        &tracker,
        decision,
    )
    .expect("typed liquidation observations");
    assert_eq!(observations.len(), 3);
    assert!(observations.iter().all(|observation| {
        observation.entity() == &FeatureEntity::Instrument(instrument.id().clone())
            && observation.finality_state() == FinalityState::Final
    }));
    let alternate_payload = String::from_utf8(USDM_LIQUIDATION.to_vec())
        .expect("utf8 liquidation fixture")
        .replace("\"0.014\"", "\"0.015\"");
    let alternate_receipts = derivative_receipts(
        alternate_payload.as_bytes(),
        BinanceInput::UsdMLiquidationWebSocket,
    )
    .await;
    let mut receipt_set = vec![receipts[0].clone(), alternate_receipts[0].clone()];
    let forward = emit_liquidation_features(
        &task_six_registry(),
        task_six_emission(UnixNanos::new(1_672_515_788_000_000_001)),
        &receipt_set,
        &instrument,
        &tracker,
        decision,
    )
    .expect("canonical liquidation observations");
    receipt_set.reverse();
    let reversed = emit_liquidation_features(
        &task_six_registry(),
        task_six_emission(UnixNanos::new(1_672_515_788_000_000_001)),
        &receipt_set,
        &instrument,
        &tracker,
        decision,
    )
    .expect("permuted liquidation observations");
    assert_eq!(
        forward, reversed,
        "a receipt set must emit bit-identical observations regardless of input order"
    );
    let (completeness_tracker, completeness_decision) = finalized_tracker(
        FeatureEntity::Source(source("binance")),
        &[source("binance")],
        window,
        "liquidation-completeness",
    );
    let completeness = emit_liquidation_completeness_feature(
        &task_six_registry(),
        task_six_emission(UnixNanos::new(1_672_515_788_000_000_001)),
        &receipts,
        &completeness_tracker,
        completeness_decision,
    )
    .expect("source-bound completeness observation");
    assert_eq!(
        completeness.feature_id().as_str(),
        "liquidation_source_completeness_flag"
    );
    assert_eq!(
        completeness.datum(),
        &FeatureDatum::Present(feature_registry::FeatureValue::Integer(3))
    );
    assert!(matches!(
        emit_liquidation_completeness_feature(
            &task_six_registry(),
            task_six_emission(UnixNanos::new(1_672_515_788_000_000_001)),
            &[receipts[0].clone(), receipts[0].clone()],
            &completeness_tracker,
            completeness_decision,
        ),
        Err(Task6EmissionError::Computation(
            feature_engine::features::FeatureComputationError::DuplicateLineage
        ))
    ));

    assert_eq!(
        liquidation_velocity(
            &[receipts[0].clone(), receipts[0].clone()],
            &instrument,
            &tracker,
            decision,
        ),
        Err(feature_engine::features::FeatureComputationError::DuplicateLineage)
    );
    assert_eq!(
        liquidation_velocity(&[], &instrument, &tracker, decision),
        Err(feature_engine::features::FeatureComputationError::InsufficientHistory)
    );
}

#[test]
fn operational_uncertainty_fields_are_separate_from_fail_closed_gating_fields() {
    let source_id = source("binance");
    let mut supervisor = operational_supervisor(&source_id, StreamClass::Liquidations);
    record_operational_events(&mut supervisor, &source_id, UnixNanos::new(1_500_000_000));
    let receipt = supervisor
        .issue_operational_quality_receipt(
            &source_id,
            OperationalQualitySampleInput {
                window_start: UnixNanos::new(1_000_000_000),
                window_end: UnixNanos::new(2_000_000_000),
                last_trusted_event_time: UnixNanos::new(1_000_000_000),
                receive_wall_time: UnixNanos::new(1_025_000_000),
                as_known_at: UnixNanos::new(4_000_000_000),
                stale_after_ns: 2_000_000_000,
                feed_jitter_ns: 5_000_000,
                clock_skew_estimate_ns: -2_000_000,
            },
        )
        .expect("supervisor-sealed quality input");
    let snapshot = operational_quality_snapshot(&receipt).expect("valid quality receipt");

    assert_eq!(snapshot.uncertainty_fields().sequence_gap_count(), 3);
    assert!(snapshot.uncertainty_fields().stale());
    assert!(snapshot.uncertainty_fields().checksum_failed());
    assert_eq!(snapshot.uncertainty_fields().checksum_failure_count(), 1);
    assert_eq!(snapshot.uncertainty_fields().feed_jitter_ns(), 5_000_000);
    assert_eq!(
        snapshot.uncertainty_fields().clock_skew_estimate_ns(),
        -2_000_000
    );
    assert_eq!(
        snapshot.uncertainty_fields().source_health(),
        SourceHealthState::Recovering
    );
    assert_eq!(
        snapshot.uncertainty_fields().stream(),
        StreamClass::Liquidations
    );
    assert_eq!(
        snapshot.uncertainty_fields().completeness(),
        Completeness::SampledLargestPerSymbolWindow {
            window_ms: NonZeroU32::new(1_000).expect("nonzero"),
        }
    );
    assert!(!snapshot.gating_fields().eligible());
    assert!(!snapshot.gating_fields().complete_for_cascade());
    assert!(snapshot.gating_fields().authority_available());
    assert_ne!(
        std::any::type_name_of_val(snapshot.uncertainty_fields()),
        std::any::type_name_of_val(snapshot.gating_fields())
    );

    supervisor
        .mark_healthy(&source_id, UnixNanos::new(5_000_000_000))
        .expect("verified recovery");
    let healthy_receipt = supervisor
        .issue_operational_quality_receipt(
            &source_id,
            OperationalQualitySampleInput {
                window_start: UnixNanos::new(5_000_000_001),
                window_end: UnixNanos::new(6_000_000_001),
                last_trusted_event_time: UnixNanos::new(5_900_000_000),
                receive_wall_time: UnixNanos::new(5_901_000_000),
                as_known_at: UnixNanos::new(6_100_000_000),
                stale_after_ns: 2_000_000_000,
                feed_jitter_ns: 100_000,
                clock_skew_estimate_ns: 0,
            },
        )
        .expect("healthy supervisor receipt");
    let healthy = operational_quality_snapshot(&healthy_receipt).expect("healthy snapshot");
    assert!(healthy.gating_fields().authority_available());
    assert!(healthy.gating_fields().eligible());
    assert!(
        !healthy.gating_fields().complete_for_cascade(),
        "source recovery cannot relabel a bound sampled feed as complete"
    );
}

#[test]
fn collector_receipts_emit_operational_quality_and_gates_at_exact_scope() {
    let source_id = source("binance");
    let mut supervisor = operational_supervisor(&source_id, StreamClass::MarkAndIndex);
    supervisor
        .mark_healthy(&source_id, UnixNanos::new(500_000_000))
        .expect("verified source recovery");
    let registry = task_six_registry();
    let emission = task_six_emission(UnixNanos::new(80_000_000_000));

    let snapshot_window =
        TimeWindow::try_new(UnixNanos::new(1_000_000_000), UnixNanos::new(2_000_000_000))
            .expect("snapshot window");
    record_operational_events(&mut supervisor, &source_id, UnixNanos::new(1_500_000_000));
    let snapshot_receipt = supervisor
        .issue_operational_quality_receipt(
            &source_id,
            operational_sample(snapshot_window, UnixNanos::new(7_000_000_000)),
        )
        .expect("snapshot quality receipt");
    let (snapshot_tracker, snapshot_decision) = finalized_tracker(
        FeatureEntity::Source(source_id.clone()),
        std::slice::from_ref(&source_id),
        snapshot_window,
        "quality-snapshot",
    );
    let snapshot_features = emit_operational_source_features(
        &registry,
        emission.clone(),
        &snapshot_receipt,
        &snapshot_tracker,
        snapshot_decision,
    )
    .expect("snapshot operational features");
    assert_eq!(snapshot_features.len(), 7);
    assert!(snapshot_features.iter().all(|observation| {
        observation.entity() == &FeatureEntity::Source(source_id.clone())
            && observation.finality_state() == FinalityState::Final
    }));

    let flow_window = TimeWindow::try_new(
        UnixNanos::new(1_000_000_000),
        UnixNanos::new(61_000_000_000),
    )
    .expect("flow window");
    let flow_receipt = supervisor
        .issue_operational_quality_receipt(
            &source_id,
            operational_sample(flow_window, UnixNanos::new(70_000_000_000)),
        )
        .expect("flow quality receipt");
    let (flow_tracker, flow_decision) = finalized_tracker(
        FeatureEntity::Source(source_id.clone()),
        std::slice::from_ref(&source_id),
        flow_window,
        "quality-flow",
    );
    let flow_features = emit_operational_source_features(
        &registry,
        emission.clone(),
        &flow_receipt,
        &flow_tracker,
        flow_decision,
    )
    .expect("flow operational features");
    assert_eq!(flow_features.len(), 9);

    let data_volume = tempfile::tempdir().expect("data volume");
    let data_volume_handle = File::open(data_volume.path()).expect("open data volume");
    supervisor
        .bind_data_volume(data_volume_handle.as_fd())
        .expect("bind data volume");
    let stale_pressure = supervisor
        .issue_storage_pressure_receipt(UnixNanos::new(7_000_000_000))
        .expect("storage pressure receipt");
    let pressure = supervisor
        .issue_storage_pressure_receipt(UnixNanos::new(1_500_000_000))
        .expect("in-window storage pressure receipt");
    let expected_pressure = pressure.used_bytes() as f64 / pressure.capacity_bytes().get() as f64;
    let (global_tracker, global_decision) = finalized_tracker(
        FeatureEntity::Global,
        std::slice::from_ref(&source_id),
        snapshot_window,
        "quality-storage",
    );
    let disk = emit_storage_pressure_feature(
        &registry,
        emission,
        pressure,
        &global_tracker,
        global_decision,
    )
    .expect("disk pressure feature");
    assert!(matches!(
        emit_storage_pressure_feature(
            &registry,
            task_six_emission(UnixNanos::new(80_000_000_000)),
            stale_pressure,
            &global_tracker,
            global_decision,
        ),
        Err(Task6EmissionError::UntrustedFinality)
    ));
    assert_eq!(disk.feature_id().as_str(), "disk_pressure_fraction");
    let FeatureDatum::Present(feature_registry::FeatureValue::Float64(value)) = disk.datum() else {
        panic!("disk pressure must be a finite float")
    };
    assert_eq!(value.value(), expected_pressure);
}

#[test]
fn task_six_catalogue_is_closed_documented_and_enforces_consumption_roles() {
    const DATA_DICTIONARY: &str = include_str!("../../../docs/data-dictionary/features.md");
    let recipes = task_six_recipes();
    let definitions = task_six_definitions().expect("valid Task 6 definitions");

    assert_eq!(recipes.len(), 63);
    assert!(
        recipes
            .iter()
            .any(|recipe| recipe.id() == "predicted_funding_rate")
    );
    assert!(
        !recipes
            .iter()
            .any(|recipe| recipe.id() == "current_funding_rate")
    );
    assert_eq!(definitions.len(), recipes.len());
    assert_eq!(
        recipes
            .iter()
            .find(|recipe| recipe.id() == "mark_index_divergence")
            .expect("implemented recipe")
            .computation_availability(),
        Task6ComputationAvailability::Implemented
    );
    assert_eq!(
        recipes
            .iter()
            .find(|recipe| recipe.id() == "executable_price_dispersion")
            .expect("unavailable recipe")
            .computation_availability(),
        Task6ComputationAvailability::ExplicitlyUnavailable
    );
    assert_eq!(
        recipes
            .iter()
            .filter(|recipe| {
                recipe.computation_availability() == Task6ComputationAvailability::Implemented
            })
            .map(|recipe| recipe.id())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "cascade_eligibility_gate",
            "checksum_failure_count",
            "clock_skew_estimate_ns",
            "correction_count",
            "cross_venue_median_absolute_dispersion",
            "cross_source_disagreement",
            "disk_pressure_fraction",
            "feature_age_ns",
            "feed_jitter_ns",
            "funding_rate_change",
            "healthy_venue_fraction",
            "indicative_cross_venue_price_range",
            "liquidation_observed_count",
            "liquidation_observed_notional",
            "liquidation_observed_velocity",
            "liquidation_source_completeness_flag",
            "local_processing_lag_ns",
            "mark_index_divergence",
            "open_interest_native",
            "open_interest_relative_change",
            "predicted_funding_rate",
            "raw_to_normalized_rejection_count",
            "reconnect_count",
            "recovery_count",
            "revision_count",
            "sequence_gap_count",
            "source_completeness_gate",
            "source_coverage_fraction",
            "source_health_gate",
            "source_latency_ns",
            "source_outage_indicator",
            "stale_quote_duration_ns",
            "venue_concentration_index",
            "venue_depth_share",
            "venue_midprice_deviation",
        ])
    );
    for (id, expected_scope) in [
        ("venue_midprice_deviation", EntityScope::AssetSource),
        ("venue_depth_share", EntityScope::AssetSource),
        ("venue_volume_share", EntityScope::AssetSource),
        ("local_move_classifier_input", EntityScope::AssetSource),
        ("venue_lead_lag", EntityScope::AssetSourcePair),
    ] {
        assert_eq!(
            recipes
                .iter()
                .find(|recipe| recipe.id() == id)
                .expect("per-source recipe")
                .entity(),
            expected_scope,
            "{id} must retain the source identity needed to interpret its value"
        );
    }
    for required_quality_id in [
        "source_latency_ns",
        "feed_jitter_ns",
        "clock_skew_estimate_ns",
        "sequence_gap_count",
        "checksum_failure_count",
        "reconnect_count",
        "recovery_count",
        "stale_quote_duration_ns",
        "source_coverage_fraction",
        "feature_age_ns",
        "cross_source_disagreement",
        "correction_count",
        "revision_count",
        "raw_to_normalized_rejection_count",
        "disk_pressure_fraction",
        "local_processing_lag_ns",
    ] {
        assert!(
            recipes
                .iter()
                .any(|recipe| recipe.id() == required_quality_id),
            "missing normative quality feature {required_quality_id}"
        );
        assert_eq!(
            recipes
                .iter()
                .find(|recipe| recipe.id() == required_quality_id)
                .expect("quality recipe")
                .computation_availability(),
            Task6ComputationAvailability::Implemented,
            "collector-owned receipt must emit {required_quality_id}"
        );
    }

    let mut ids = BTreeSet::new();
    let mut registry = FeatureRegistry::new();
    for (recipe, definition) in recipes.iter().copied().zip(definitions) {
        assert!(ids.insert(recipe.id()), "duplicate recipe {}", recipe.id());
        assert_eq!(definition.id().as_str(), recipe.id());
        assert_eq!(definition.version(), &Version::new(1, 0, 0));
        assert_eq!(definition.status(), recipe.status());
        assert_eq!(definition.consumption_role(), recipe.consumption_role());
        assert_eq!(definition.value_type(), recipe.value_type());
        assert_eq!(definition.entities(), recipe.entity());
        assert_eq!(definition.formula_hash(), recipe.formula_hash());
        assert_eq!(
            definition
                .required_inputs()
                .iter()
                .map(|input| input.id())
                .collect::<BTreeSet<_>>(),
            recipe
                .required_inputs()
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
        );
        let anchor = format!("<a id=\"{}\"></a>", recipe.id().replace('_', "-"));
        assert!(
            DATA_DICTIONARY.contains(&anchor),
            "missing data-dictionary anchor {anchor}"
        );
        assert!(
            DATA_DICTIONARY.contains(recipe.formula_identity()),
            "missing formula identity for {}",
            recipe.id()
        );
        registry.register(definition).expect("unique definition");
    }
    assert!(DATA_DICTIONARY.contains("Market Structure and Feature Integrity team"));
    assert!(DATA_DICTIONARY.contains("Present-value emission is implemented only for:"));

    let executable = recipes
        .iter()
        .find(|recipe| recipe.id() == "executable_price_dispersion")
        .expect("executable dispersion recipe");
    assert_eq!(
        executable.required_inputs(),
        &[
            "execution.authenticated_depth_walk",
            "execution.contract_conversion",
            "execution.fee_schedule",
            "execution.latency_buffer",
            "execution.lot_and_tick",
            "execution.settlement_compatibility",
        ]
    );
    for (id, required_input) in [
        ("venue_lead_lag", "window.finalization"),
        (
            "venue_volume_share",
            "connector.trade_normalization_receipt",
        ),
        (
            "realized_funding_rate",
            "connector.realized_funding_receipt",
        ),
        (
            "open_interest_usd_notional",
            "consolidated.point_in_time_quote_usd_conversion",
        ),
        (
            "perpetual_spot_basis",
            "consolidated.aligned_reference_price",
        ),
        (
            "futures_curve_curvature",
            "instrument.point_in_time_maturity_set",
        ),
        (
            "liquidation_to_volume_ratio",
            "connector.trade_normalization_receipt",
        ),
        (
            "liquidation_to_open_interest_ratio",
            "connector.derivative_normalization_receipt",
        ),
        (
            "liquidation_source_completeness_flag",
            "connector.liquidation_normalization_receipt",
        ),
        (
            "source_coverage_fraction",
            "consolidated.eligible_source_universe",
        ),
        (
            "cross_source_disagreement",
            "consolidated.catalog_bound_book_state",
        ),
        (
            "disk_pressure_fraction",
            "collector.storage_pressure_receipt",
        ),
        ("cascade_eligibility_gate", "window.finalization"),
    ] {
        let recipe = recipes
            .iter()
            .find(|recipe| recipe.id() == id)
            .expect("catalogue recipe");
        assert!(
            recipe.required_inputs().contains(&required_input),
            "{id} must declare {required_input}"
        );
    }

    let version = Version::from_str("1.0.0").expect("version");
    assert!(
        registry
            .ensure_model_input(
                &FeatureId::new("venue_depth_share").expect("feature ID"),
                &version,
            )
            .is_ok()
    );
    for id in ["source_outage_indicator", "source_health_gate"] {
        assert_eq!(
            registry.ensure_model_input(&FeatureId::new(id).expect("feature ID"), &version),
            Err(RegistryError::DefinitionNotModelEligible)
        );
    }
}

#[test]
fn unavailable_task_six_recipes_emit_final_explicit_missingness_only() {
    let entity = FeatureEntity::Asset(btc());
    let key = WatermarkKey::new(
        source("binance"),
        PartitionId::new("cross-venue").expect("partition"),
    );
    let mut tracker = WatermarkTracker::try_new_for_entity(
        vec![PartitionConfig::required(key.clone())],
        DurationNanos::new(0),
        vec![SourceHealthState::Healthy],
        entity.clone(),
    )
    .expect("tracker");
    let window = TimeWindow::try_new(UnixNanos::new(1_000_000_000), UnixNanos::new(2_000_000_000))
        .expect("window");
    tracker
        .advance(
            &key,
            WatermarkUpdate::new(
                UnixNanos::new(7_000_000_000),
                UnixNanos::new(7_000_000_000),
                SourceHealthState::Healthy,
            ),
        )
        .expect("watermark");
    let decision = tracker.decision(window);
    let mut registry = FeatureRegistry::new();
    for definition in task_six_definitions().expect("definitions") {
        registry.register(definition).expect("register");
    }
    let input = Task6MissingEmissionInput {
        computed_at: UnixNanos::new(7_000_000_001),
        revision: ObservationRevision::new(1).expect("revision"),
        code_commit: CodeRevision::new("0123456789abcdef0123456789abcdef01234567").expect("commit"),
    };
    let observation = emit_unavailable_task_six_feature(
        &registry,
        input.clone(),
        "executable_price_dispersion",
        entity.clone(),
        &tracker,
        decision,
    )
    .expect("explicit missing observation");
    assert_eq!(
        observation.datum(),
        &FeatureDatum::Missing(MissingnessReason::ModelNotApplicable)
    );
    assert_eq!(
        emit_unavailable_task_six_feature(
            &registry,
            input,
            "mark_index_divergence",
            entity,
            &tracker,
            decision,
        )
        .expect_err("implemented recipes require their typed emitter")
        .to_string(),
        Task6EmissionError::ImplementedRecipe.to_string()
    );
}

fn task_six_registry() -> FeatureRegistry {
    let mut registry = FeatureRegistry::new();
    for definition in task_six_definitions().expect("definitions") {
        registry.register(definition).expect("register");
    }
    registry
}

fn task_six_emission(computed_at: UnixNanos) -> Task6FeatureEmissionInput {
    Task6FeatureEmissionInput {
        computed_at,
        revision: ObservationRevision::new(1).expect("revision"),
        code_commit: CodeRevision::new("0123456789abcdef0123456789abcdef01234567").expect("commit"),
    }
}

fn finalized_tracker(
    entity: FeatureEntity,
    sources: &[SourceId],
    window: TimeWindow,
    partition_prefix: &str,
) -> (WatermarkTracker, FinalizationDecision) {
    let keys = sources
        .iter()
        .map(|source_id| {
            WatermarkKey::new(
                source_id.clone(),
                PartitionId::new(format!("{partition_prefix}-{}", source_id.name()))
                    .expect("partition"),
            )
        })
        .collect::<Vec<_>>();
    let mut tracker = WatermarkTracker::try_new_for_entity(
        keys.iter()
            .cloned()
            .map(PartitionConfig::required)
            .collect(),
        DurationNanos::new(0),
        vec![SourceHealthState::Healthy],
        entity,
    )
    .expect("tracker");
    let final_watermark = UnixNanos::new(
        window
            .end()
            .value()
            .checked_add(5_000_000_000)
            .expect("watermark"),
    );
    for key in &keys {
        tracker
            .advance(
                key,
                WatermarkUpdate::new(final_watermark, final_watermark, SourceHealthState::Healthy),
            )
            .expect("advance");
    }
    let decision = tracker.decision(window);
    (tracker, decision)
}

fn linear_perpetual() -> InstrumentDefinition {
    let venue = VenueId::new("binance").expect("venue");
    let base = AssetId::new(AssetNamespace::Native, "bitcoin", "", "BTC", 1).expect("base");
    let quote = AssetId::new(AssetNamespace::Synthetic, "", "", "USDT", 1).expect("quote");
    InstrumentDefinition::new(InstrumentDefinitionInput {
        id: InstrumentId::new_for_product(venue, "btcusdt", ProductType::Perpetual, 1)
            .expect("instrument id"),
        product_type: ProductType::Perpetual,
        base_asset: base,
        quote_asset: quote.clone(),
        settlement_asset: quote,
        contract_multiplier: fixed("1"),
        contract_value_unit: ContractValueUnit::Base,
        contract_kind: ContractKind::Linear,
        expiry_time: None,
        strike: None,
        option_side: None,
        price_tick: price("0.1"),
        quantity_step: quantity("0.001"),
        listing_time: UnixNanos::new(1),
        delisting_time: None,
    })
    .expect("linear perpetual")
}

fn same_id_inverse_perpetual() -> InstrumentDefinition {
    let venue = VenueId::new("binance").expect("venue");
    let base = AssetId::new(AssetNamespace::Native, "bitcoin", "", "BTC", 1).expect("base");
    let quote = AssetId::new(AssetNamespace::Synthetic, "", "", "USDT", 1).expect("quote");
    InstrumentDefinition::new(InstrumentDefinitionInput {
        id: InstrumentId::new_for_product(venue, "btcusdt", ProductType::Perpetual, 1)
            .expect("instrument id"),
        product_type: ProductType::Perpetual,
        base_asset: base.clone(),
        quote_asset: quote,
        settlement_asset: base,
        contract_multiplier: fixed("100"),
        contract_value_unit: ContractValueUnit::Quote,
        contract_kind: ContractKind::Inverse,
        expiry_time: None,
        strike: None,
        option_side: None,
        price_tick: price("0.1"),
        quantity_step: quantity("1"),
        listing_time: UnixNanos::new(1),
        delisting_time: None,
    })
    .expect("inverse perpetual")
}

fn trusted_cross_venue_fair_price() -> FairPrice {
    let mut registry = InstrumentRegistry::new();
    let alpha = spot_instrument("alpha");
    let beta = spot_instrument("beta");
    for (ordinal, instrument) in [alpha.clone(), beta.clone()].into_iter().enumerate() {
        registry
            .append_definition(
                instrument,
                RevisionMetadata::try_new(
                    UnixNanos::new(i64::try_from(ordinal + 1).expect("ordinal")),
                    format!("task6:venue-{ordinal}"),
                )
                .expect("revision"),
            )
            .expect("definition");
    }
    let catalog = registry.snapshot().expect("catalog");
    let verified = VerifiedCatalogSnapshot::try_new(&catalog).expect("verified catalog");
    let estimator = cross_venue_estimator(true);
    let mut quotes = Vec::new();
    for (ordinal, instrument, bid, ask) in
        [(1_u64, alpha, "99", "101"), (2_u64, beta, "101", "103")]
    {
        let session = BookSession {
            connection_epoch: 1,
            subscription_epoch: 1,
            instrument_generation: instrument.id().generation(),
        };
        let mut engine = OrderBookEngine::new(BookConfig {
            instrument: instrument.id().clone(),
            price_tick: price("0.01"),
            quantity_step: quantity("0.001"),
            max_levels_per_side: 8,
            max_buffered_deltas: 8,
            max_buffered_level_updates: 32,
            sequence_policy: SequencePolicy::ExactNext,
            checksum_policy: ChecksumPolicy::Disabled,
            max_l3_orders: None,
            max_l3_levels_per_side: None,
        })
        .expect("book");
        engine
            .start_session(session, SnapshotStrategy::StreamSnapshot)
            .expect("session");
        engine
            .apply_snapshot(
                event_envelope::BookSnapshot {
                    bids: vec![event_envelope::BookLevel {
                        price: price(bid),
                        quantity: quantity("2"),
                        order_count: Some(1),
                    }],
                    asks: vec![event_envelope::BookLevel {
                        price: price(ask),
                        quantity: quantity("2"),
                        order_count: Some(1),
                    }],
                    last_sequence: ordinal,
                },
                session,
                ordinal * 1_000_000_000,
            )
            .expect("snapshot");
        let TrustedBookAdmission::Accepted(quote) = estimator
            .admit_trusted_book(TrustedBookAdmissionInput {
                source: source(instrument.id().venue().as_str()),
                engine: &engine,
                catalog: &verified,
                now_monotonic_ns: ordinal * 1_000_000_000 + 1_000_000,
                candidate_kind: CandidatePriceKind::Midpoint,
                depth_levels: 1,
                source_health: SourceHealthState::Healthy,
                metadata_status: MetadataStatus::Current,
                venue_status: VenueTradingState::Normal,
                clock_healthy: true,
                parser_healthy: true,
                event_time: UnixNanos::new(
                    1_000_000_900 + i64::try_from(ordinal).expect("ordinal"),
                ),
                as_known_at: UnixNanos::new(
                    1_000_000_950 + i64::try_from(ordinal).expect("ordinal"),
                ),
                conversion: None,
            })
            .expect("admission")
        else {
            panic!("trusted book should be admitted");
        };
        quotes.push(*quote);
    }
    let ConsolidatedOutcome::Available(fair_price) = estimator
        .estimate(UnixNanos::new(1_000_001_000), &quotes)
        .expect("estimate")
    else {
        panic!("fair price should be available");
    };
    fair_price
}

fn direct_cross_venue_fair_price() -> FairPrice {
    let estimator = cross_venue_estimator(false);
    let quotes = ["alpha", "beta"]
        .into_iter()
        .zip(["100", "102"])
        .map(|(venue, midpoint)| {
            let midpoint = fixed(midpoint);
            VenueQuote::try_new(VenueQuoteInput {
                source: source(venue),
                instrument: spot_instrument(venue),
                candidate_kind: CandidatePriceKind::Midpoint,
                bid: Price::new(midpoint.checked_sub(fixed("1")).expect("bid")).expect("bid"),
                ask: Price::new(midpoint.checked_add(fixed("1")).expect("ask")).expect("ask"),
                executable_quantity: quantity("2"),
                quality: Ppm::new(900_000).expect("quality"),
                freshness: Ppm::new(900_000).expect("freshness"),
                source_health: SourceHealthState::Healthy,
                metadata_status: MetadataStatus::Current,
                venue_status: VenueTradingState::Normal,
                clock_healthy: true,
                parser_healthy: true,
                book_healthy: true,
                event_time: UnixNanos::new(900),
                as_known_at: UnixNanos::new(950),
                conversion: None,
                instrument_provenance: InstrumentProvenance::direct_definition(),
            })
            .expect("direct quote")
        })
        .collect::<Vec<_>>();
    let ConsolidatedOutcome::Available(fair_price) = estimator
        .estimate(UnixNanos::new(1_000), &quotes)
        .expect("estimate")
    else {
        panic!("direct fair price should be available");
    };
    fair_price
}

fn cross_venue_estimator(require_catalog_provenance: bool) -> FairPriceEstimator {
    FairPriceEstimator::try_new_with_eligible_sources(
        EstimatorConfigInput {
            policy_id: PolicyId::new("task6-cross-venue-v1").expect("policy"),
            base_asset: btc(),
            reference_asset: usd(),
            product_type: ProductType::Spot,
            minimum_venues: 2,
            maximum_venues: 4,
            quote_ttl: DurationNanos::new(5_000_000_000),
            conversion_ttl: DurationNanos::new(5_000_000_000),
            depth_cap_reference: notional("1000"),
            minimum_quality: Ppm::new(500_000).expect("quality"),
            minimum_freshness: Ppm::new(500_000).expect("freshness"),
            maximum_venue_weight: Ppm::new(500_000).expect("weight"),
            outlier_threshold: Ppm::new(200_000).expect("outlier"),
            minimum_conversion_sources: 2,
            maximum_conversion_interval_width: Ppm::new(100_000).expect("conversion"),
            require_catalog_provenance,
        },
        vec![source("alpha"), source("beta")],
    )
    .expect("estimator")
}

fn spot_instrument(venue: &str) -> InstrumentDefinition {
    InstrumentDefinition::new(InstrumentDefinitionInput {
        id: InstrumentId::new_for_product(
            VenueId::new(venue).expect("venue"),
            "btcusd",
            ProductType::Spot,
            1,
        )
        .expect("instrument"),
        product_type: ProductType::Spot,
        base_asset: btc(),
        quote_asset: usd(),
        settlement_asset: usd(),
        contract_multiplier: fixed("1"),
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
    .expect("spot instrument")
}

fn btc() -> AssetId {
    AssetId::new(AssetNamespace::Native, "bitcoin", "", "BTC", 1).expect("btc")
}

fn usd() -> AssetId {
    AssetId::new(AssetNamespace::Fiat, "", "", "USD", 1).expect("usd")
}

fn derivative_catalog() -> std::sync::Arc<instrument_registry::CatalogSnapshot> {
    let mut registry = InstrumentRegistry::new();
    registry
        .append_definition(
            linear_perpetual(),
            RevisionMetadata::try_new(UnixNanos::new(1), "task6:perpetual")
                .expect("revision metadata"),
        )
        .expect("perpetual definition");
    registry.snapshot().expect("catalog")
}

async fn durable_reference(payload: &[u8], stream_name: &str) -> DurableRawReference {
    let directory = tempfile::tempdir().expect("temporary WAL");
    let mut segment_id = [0_u8; 16];
    segment_id.copy_from_slice(&blake3::hash(payload).as_bytes()[..16]);
    let metadata = SegmentMetadata::new(
        segment_id,
        1,
        "schema",
        "installation",
        "build",
        vec![
            StreamDescriptor::new(
                7,
                wal_stream_source_identity(&source("binance")),
                stream_name,
            )
            .expect("stream"),
        ],
    )
    .expect("segment");
    let mut writer =
        SegmentedWalWriter::create(directory.path(), metadata, RotationPolicy::default(), 1)
            .expect("writer");
    let authority = writer.append_authority();
    let (proof, compression) = writer
        .append(
            RecordMetadata {
                flags: 0,
                stream_id: 7,
                connection_epoch: 3,
                record_sequence: 1,
                receive_wall_time_ns: RECEIVE_TIME.value(),
                receive_monotonic_time_ns: 9,
            },
            payload,
            9,
            RECEIVE_TIME.value(),
        )
        .expect("append")
        .into_parts();
    assert!(compression.is_none());

    let channel = DurableRawCaptureChannel::new(
        authority,
        source("binance"),
        NonZeroU32::new(7).expect("stream"),
        1,
        payload.len() + 1,
    )
    .expect("channel");
    let (client, mut worker) = channel.split();
    let pending = client
        .try_submit(
            RawCapture::try_new(
                source("binance"),
                NonZeroU32::new(7).expect("stream"),
                NonZeroU64::new(3).expect("epoch"),
                NonZeroU64::new(1).expect("record"),
                RECEIVE_TIME,
                9,
                payload.to_vec().into_boxed_slice(),
            )
            .expect("capture"),
        )
        .expect("submit");
    worker
        .recv()
        .await
        .expect("pending capture")
        .acknowledge(proof)
        .expect("acknowledge");
    pending.wait().await.expect("durable reference")
}

async fn derivative_receipts(
    payload: &[u8],
    input: BinanceInput,
) -> Vec<connector_binance::BinanceDerivativeNormalizationReceipt> {
    let raw = durable_reference(payload, input.wal_stream_name()).await;
    let parsed =
        parse_durable_native_message(input, payload, &raw).expect("durable derivative parse");
    let catalog = derivative_catalog();
    let context = NormalizationContext::try_new(
        &catalog,
        &raw,
        NORMALIZATION_TIME,
        CONNECTION_START,
        NonZeroU64::new(1).expect("subscription epoch"),
        "task6-test",
    )
    .expect("normalization context");
    normalize_derivative_with_receipts(parsed, &context)
        .expect("connector-owned derivative receipts")
}

async fn derivative_receipt_with_quality(
    payload: &[u8],
    input: BinanceInput,
    stream: DerivativeStream,
    quality_score_ppm: u32,
    quality_flags: QualityFlags,
) -> DerivativeNormalizationReceipt {
    let raw = durable_reference(payload, input.wal_stream_name()).await;
    let parsed =
        parse_durable_native_message(input, payload, &raw).expect("durable derivative parse");
    let catalog = derivative_catalog();
    let context = NormalizationContext::try_new(
        &catalog,
        &raw,
        NORMALIZATION_TIME,
        CONNECTION_START,
        NonZeroU64::new(1).expect("subscription epoch"),
        "task6-quality-test",
    )
    .expect("normalization context");
    let receipt = normalize_derivative_with_receipts(parsed, &context)
        .expect("connector receipt")
        .into_iter()
        .find(|receipt| receipt.stream() == stream)
        .expect("requested stream");
    let mut metadata = receipt.event().metadata().as_unchecked().clone();
    metadata.quality_score_ppm = quality_score_ppm;
    metadata.quality_flags = quality_flags;
    let event = EventEnvelope::new(metadata, receipt.event().payload().as_unchecked().clone())
        .expect("quality-adjusted canonical event");
    DerivativeNormalizationReceipt::try_from_verified(
        &connector_binance::binance_capabilities().expect("capabilities"),
        &raw,
        &catalog,
        event,
    )
    .expect("receipt remains bound to verified raw and catalog")
}

fn source(name: &str) -> SourceId {
    SourceId::new(SourceKind::Exchange, name, 1).expect("source")
}

fn operational_supervisor(source_id: &SourceId, stream: StreamClass) -> CollectorSupervisor {
    let mut supervisor = CollectorSupervisor::try_new(
        AdmissionLimits::try_new(1, 0, 0, 1).expect("admission limits"),
    )
    .expect("collector supervisor");
    supervisor
        .admit(
            source_id.clone(),
            SourcePolicy::new(
                CoverageTier::A,
                NonZeroU32::new(1).expect("instrument capacity"),
            ),
            RetryPolicy::try_new(3, 100, 1_000, 0, 7).expect("retry policy"),
        )
        .expect("admitted quality source");
    supervisor
        .bind_source_completeness(
            source_id,
            &connector_binance::binance_capabilities().expect("capabilities"),
            stream,
        )
        .expect("bind connector completeness");
    supervisor
}

fn operational_sample(window: TimeWindow, as_known_at: UnixNanos) -> OperationalQualitySampleInput {
    OperationalQualitySampleInput {
        window_start: window.start(),
        window_end: window.end(),
        last_trusted_event_time: UnixNanos::new(window.end().value() - 1),
        receive_wall_time: UnixNanos::new(window.end().value() + 24_999_999),
        as_known_at,
        stale_after_ns: 10_000_000_000,
        feed_jitter_ns: 5_000_000,
        clock_skew_estimate_ns: -2_000_000,
    }
}

fn record_operational_events(
    supervisor: &mut CollectorSupervisor,
    source_id: &SourceId,
    observed_at: UnixNanos,
) {
    for (kind, count) in [
        (OperationalQualityEvent::SequenceGap, 3),
        (OperationalQualityEvent::ChecksumFailure, 1),
        (OperationalQualityEvent::Reconnect, 2),
        (OperationalQualityEvent::Recovery, 1),
        (OperationalQualityEvent::Correction, 4),
        (OperationalQualityEvent::Revision, 5),
        (OperationalQualityEvent::RawToNormalizedRejection, 6),
    ] {
        for _ in 0..count {
            supervisor
                .record_operational_event(source_id, kind, observed_at)
                .expect("collector-owned operational event");
        }
    }
}

fn fixed(value: &str) -> FixedDecimal {
    FixedDecimal::parse(value).expect("fixed decimal")
}

fn price(value: &str) -> Price {
    Price::new(fixed(value)).expect("price")
}

fn quantity(value: &str) -> Quantity {
    Quantity::new(fixed(value)).expect("quantity")
}

fn notional(value: &str) -> Notional {
    Notional::new(fixed(value)).expect("notional")
}
