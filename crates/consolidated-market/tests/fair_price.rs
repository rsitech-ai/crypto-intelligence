use consolidated_market::{
    AbstentionReason, CandidatePriceKind, ConsolidatedOutcome, EstimatorConfigInput,
    FairPriceEstimator, InstrumentProvenance, MetadataStatus, PolicyId, Ppm,
    QuoteConversionReference, StablecoinDislocationState, StablecoinReferenceConfigInput,
    StablecoinReferenceEstimator, StablecoinReferenceOutcome, StablecoinVenueQuote,
    StablecoinVenueQuoteInput, TrustedBookAdmission, TrustedBookAdmissionInput,
    VenueExclusionReason, VenueQuote, VenueQuoteInput, VenueTradingState, VerifiedCatalogSnapshot,
};
use domain::{
    AssetId, AssetNamespace, ContractKind, ContractValueUnit, InstrumentDefinition,
    InstrumentDefinitionInput, InstrumentId, ProductType, SourceId, SourceKind, UnixNanos, VenueId,
};
use event_envelope::{BookLevel, BookSnapshot};
use feature_registry::DurationNanos;
use fixed_decimal::{FixedDecimal, Notional, Price, Quantity};
use instrument_registry::{InstrumentRegistry, RevisionMetadata};
use orderbook::{
    ApplyResult, BookConfig, BookSession, ChecksumPolicy, OrderBookEngine, SequencePolicy,
    SnapshotStrategy,
};
use quality::SourceHealthState;

const AS_OF: UnixNanos = UnixNanos::new(1_000);

fn decimal(value: &str) -> FixedDecimal {
    FixedDecimal::parse_canonical(value).expect("decimal should be canonical")
}

fn price(value: &str) -> Price {
    Price::new(decimal(value)).expect("price should be positive")
}

fn quantity(value: &str) -> Quantity {
    Quantity::new(decimal(value)).expect("quantity should be nonnegative")
}

fn notional(value: &str) -> Notional {
    Notional::new(decimal(value)).expect("notional should be nonnegative")
}

fn asset(namespace: AssetNamespace, chain: &str, symbol: &str) -> AssetId {
    AssetId::new(namespace, chain, "", symbol, 1).expect("asset should be valid")
}

fn usd() -> AssetId {
    asset(AssetNamespace::Fiat, "", "USD")
}

fn usdt() -> AssetId {
    asset(AssetNamespace::Synthetic, "", "USDT")
}

fn btc() -> AssetId {
    asset(AssetNamespace::Native, "bitcoin", "BTC")
}

fn source(venue: &str) -> SourceId {
    source_generation(venue, 1)
}

fn source_generation(venue: &str, generation: u32) -> SourceId {
    SourceId::new(SourceKind::Exchange, venue, generation).expect("source should be valid")
}

fn definition(
    venue: &str,
    quote: AssetId,
    product: ProductType,
    contract_kind: ContractKind,
) -> InstrumentDefinition {
    let base = btc();
    let settlement = if contract_kind == ContractKind::Inverse {
        base.clone()
    } else {
        quote.clone()
    };
    InstrumentDefinition::new(InstrumentDefinitionInput {
        id: InstrumentId::new_for_product(
            VenueId::new(venue).expect("venue should be valid"),
            format!("BTC{}", quote.canonical_symbol()),
            product,
            1,
        )
        .expect("instrument should be valid"),
        product_type: product,
        base_asset: base,
        quote_asset: quote.clone(),
        settlement_asset: settlement,
        contract_multiplier: decimal(if contract_kind == ContractKind::Inverse {
            "100"
        } else {
            "1"
        }),
        contract_value_unit: if contract_kind == ContractKind::Inverse {
            ContractValueUnit::Quote
        } else {
            ContractValueUnit::Base
        },
        contract_kind,
        expiry_time: None,
        strike: None,
        option_side: None,
        price_tick: price("0.01"),
        quantity_step: quantity("0.001"),
        listing_time: UnixNanos::new(1),
        delisting_time: None,
    })
    .expect("definition should be valid")
}

fn estimator() -> FairPriceEstimator {
    FairPriceEstimator::try_new(EstimatorConfigInput {
        policy_id: PolicyId::new("fair-price-v1").expect("policy ID should be valid"),
        base_asset: btc(),
        reference_asset: usd(),
        product_type: ProductType::Spot,
        minimum_venues: 2,
        maximum_venues: 64,
        quote_ttl: DurationNanos::new(100),
        conversion_ttl: DurationNanos::new(100),
        depth_cap_reference: notional("1000"),
        minimum_quality: Ppm::new(500_000).expect("quality should be valid"),
        minimum_freshness: Ppm::new(500_000).expect("freshness should be valid"),
        maximum_venue_weight: Ppm::new(500_000).expect("cap should be valid"),
        outlier_threshold: Ppm::new(200_000).expect("threshold should be valid"),
        minimum_conversion_sources: 2,
        maximum_conversion_interval_width: Ppm::new(100_000)
            .expect("interval width should be valid"),
        require_catalog_provenance: false,
    })
    .expect("estimator should be valid")
}

fn quote(
    venue: &str,
    midpoint: &str,
    depth_quantity: &str,
    health: SourceHealthState,
    conversion: Option<QuoteConversionReference>,
) -> VenueQuote {
    let quote_asset = conversion
        .as_ref()
        .map_or_else(usd, |reference| reference.quote_asset().clone());
    quote_in_asset(
        venue,
        midpoint,
        depth_quantity,
        health,
        quote_asset,
        conversion,
    )
}

fn quote_in_asset(
    venue: &str,
    midpoint: &str,
    depth_quantity: &str,
    health: SourceHealthState,
    quote_asset: AssetId,
    conversion: Option<QuoteConversionReference>,
) -> VenueQuote {
    let mid = decimal(midpoint);
    let half = decimal("0.5");
    VenueQuote::try_new(VenueQuoteInput {
        source: source(venue),
        instrument: definition(venue, quote_asset, ProductType::Spot, ContractKind::None),
        candidate_kind: CandidatePriceKind::Midpoint,
        bid: Price::new(mid.checked_sub(half).expect("bid should calculate"))
            .expect("bid should be valid"),
        ask: Price::new(mid.checked_add(half).expect("ask should calculate"))
            .expect("ask should be valid"),
        executable_quantity: quantity(depth_quantity),
        quality: Ppm::new(900_000).expect("quality should be valid"),
        freshness: Ppm::new(900_000).expect("freshness should be valid"),
        source_health: health,
        metadata_status: MetadataStatus::Current,
        venue_status: VenueTradingState::Normal,
        clock_healthy: true,
        parser_healthy: true,
        book_healthy: true,
        event_time: UnixNanos::new(950),
        as_known_at: UnixNanos::new(960),
        conversion,
        instrument_provenance: InstrumentProvenance::direct_definition(),
    })
    .expect("quote should be valid")
}

fn quote_with_policy_fields(
    venue: &str,
    generation: u32,
    candidate_kind: CandidatePriceKind,
    event_time: UnixNanos,
    as_known_at: UnixNanos,
    book_healthy: bool,
) -> VenueQuote {
    VenueQuote::try_new(VenueQuoteInput {
        source: source_generation(venue, generation),
        instrument: definition(venue, usd(), ProductType::Spot, ContractKind::None),
        candidate_kind,
        bid: price("99.5"),
        ask: price("100.5"),
        executable_quantity: quantity("10"),
        quality: Ppm::new(900_000).expect("quality should be valid"),
        freshness: Ppm::new(900_000).expect("freshness should be valid"),
        source_health: SourceHealthState::Healthy,
        metadata_status: MetadataStatus::Current,
        venue_status: VenueTradingState::Normal,
        clock_healthy: true,
        parser_healthy: true,
        book_healthy,
        event_time,
        as_known_at,
        conversion: None,
        instrument_provenance: InstrumentProvenance::direct_definition(),
    })
    .expect("quote should be valid")
}

fn usdt_reference(estimate: &str, sources: usize) -> QuoteConversionReference {
    usdt_reference_with_prefix(estimate, sources, "stable")
}

fn usdt_reference_with_prefix(
    estimate: &str,
    sources: usize,
    prefix: &str,
) -> QuoteConversionReference {
    usdt_reference_at(
        estimate,
        sources,
        prefix,
        UnixNanos::new(940),
        UnixNanos::new(950),
        AS_OF,
    )
}

fn usdt_reference_at(
    estimate: &str,
    sources: usize,
    prefix: &str,
    event_time: UnixNanos,
    as_known_at: UnixNanos,
    as_of: UnixNanos,
) -> QuoteConversionReference {
    let estimator = stablecoin_estimator();
    let quotes: Vec<_> = (0..sources)
        .map(|index| {
            stablecoin_quote_with_all_fields(
                &format!("{prefix}-{index}"),
                &format!("{prefix}-{index}"),
                estimate,
                true,
                false,
                false,
                SourceHealthState::Healthy,
                event_time,
                as_known_at,
            )
        })
        .collect();
    let StablecoinReferenceOutcome::Available(reference) = estimator
        .estimate(as_of, &quotes)
        .expect("reference estimation should complete")
    else {
        panic!("covered reference should be available");
    };
    *reference
}

fn stablecoin_estimator() -> StablecoinReferenceEstimator {
    StablecoinReferenceEstimator::try_new(StablecoinReferenceConfigInput {
        policy_id: PolicyId::new("usdt-usd-v1").expect("policy should be valid"),
        quote_asset: usdt(),
        reference_asset: usd(),
        nominal_rate: price("1"),
        minimum_sources: 2,
        maximum_sources: 64,
        quote_ttl: DurationNanos::new(100),
        depth_cap_reference: notional("1000"),
        minimum_quality: Ppm::new(500_000).expect("quality should be valid"),
        minimum_freshness: Ppm::new(500_000).expect("freshness should be valid"),
        maximum_venue_weight: Ppm::new(500_000).expect("weight should be valid"),
        material_deviation: Ppm::new(10_000).expect("deviation should be valid"),
    })
    .expect("reference estimator should be valid")
}

fn stablecoin_quote(venue: &str, estimate: &str, persistent: bool) -> StablecoinVenueQuote {
    stablecoin_quote_for_source(venue, venue, estimate, persistent)
}

fn stablecoin_quote_for_source(
    source_name: &str,
    venue: &str,
    estimate: &str,
    persistent: bool,
) -> StablecoinVenueQuote {
    stablecoin_quote_with_flags(source_name, venue, estimate, persistent, false, false)
}

fn stablecoin_quote_with_flags(
    source_name: &str,
    venue: &str,
    estimate: &str,
    persistent: bool,
    liquidity_failure: bool,
    redemption_or_reserve_event: bool,
) -> StablecoinVenueQuote {
    stablecoin_quote_with_all_fields(
        source_name,
        venue,
        estimate,
        persistent,
        liquidity_failure,
        redemption_or_reserve_event,
        SourceHealthState::Healthy,
        UnixNanos::new(940),
        UnixNanos::new(950),
    )
}

#[allow(clippy::too_many_arguments)]
fn stablecoin_quote_with_all_fields(
    source_name: &str,
    venue: &str,
    estimate: &str,
    persistent: bool,
    liquidity_failure: bool,
    redemption_or_reserve_event: bool,
    source_health: SourceHealthState,
    event_time: UnixNanos,
    as_known_at: UnixNanos,
) -> StablecoinVenueQuote {
    let midpoint = decimal(estimate);
    let half_spread = decimal("0.01");
    StablecoinVenueQuote::try_new(StablecoinVenueQuoteInput {
        source: source(source_name),
        venue: VenueId::new(venue).expect("venue should be valid"),
        quote_asset: usdt(),
        reference_asset: usd(),
        bid: Price::new(
            midpoint
                .checked_sub(half_spread)
                .expect("bid should calculate"),
        )
        .expect("bid should be valid"),
        ask: Price::new(
            midpoint
                .checked_add(half_spread)
                .expect("ask should calculate"),
        )
        .expect("ask should be valid"),
        executable_depth_reference: notional("100"),
        quality: Ppm::new(900_000).expect("quality should be valid"),
        freshness: Ppm::new(900_000).expect("freshness should be valid"),
        source_health,
        event_time,
        as_known_at,
        persistent,
        liquidity_failure,
        redemption_or_reserve_event,
    })
    .expect("stablecoin quote should be valid")
}

#[test]
fn unhealthy_outlier_venue_cannot_move_fair_price() {
    let quotes = vec![
        quote("alpha", "100", "10", SourceHealthState::Healthy, None),
        quote("beta", "101", "10", SourceHealthState::Healthy, None),
        quote(
            "gamma",
            "1000",
            "1000",
            SourceHealthState::Quarantined,
            None,
        ),
    ];

    let ConsolidatedOutcome::Available(result) = estimator()
        .estimate(AS_OF, &quotes)
        .expect("estimation should complete")
    else {
        panic!("two healthy venues should produce a price");
    };
    assert_eq!(result.price(), price("100.5"));
    assert_eq!(
        result.lineage().decision(&source("gamma")),
        Some(&VenueExclusionReason::SourceUnhealthy)
    );
    assert!(!result.is_executable());
}

#[test]
fn stablecoin_adjustment_is_versioned_and_insufficient_coverage_abstains() {
    let reference = usdt_reference("0.98", 3);
    let quotes = vec![
        quote(
            "alpha",
            "100",
            "10",
            SourceHealthState::Healthy,
            Some(reference.clone()),
        ),
        quote(
            "beta",
            "102",
            "10",
            SourceHealthState::Healthy,
            Some(reference),
        ),
    ];
    let ConsolidatedOutcome::Available(result) = estimator()
        .estimate(AS_OF, &quotes)
        .expect("estimation should complete")
    else {
        panic!("covered conversion should produce a price");
    };
    assert_eq!(result.price(), price("98.98"));
    assert_eq!(result.interval().lower(), price("97.97"));
    assert_eq!(result.interval().upper(), price("99.99"));

    let insufficient = vec![
        quote_in_asset(
            "alpha",
            "100",
            "10",
            SourceHealthState::Healthy,
            usdt(),
            None,
        ),
        quote_in_asset(
            "beta",
            "102",
            "10",
            SourceHealthState::Healthy,
            usdt(),
            None,
        ),
    ];
    let ConsolidatedOutcome::Abstained { reason, lineage } = estimator()
        .estimate(AS_OF, &insufficient)
        .expect("runtime insufficiency should retain lineage")
    else {
        panic!("insufficient reference coverage must abstain");
    };
    assert_eq!(reason, AbstentionReason::InsufficientHealthyVenues);
    assert_eq!(lineage.excluded_count(), 2);
}

#[test]
fn normalized_weight_cap_and_input_order_are_deterministic() {
    let dominant = quote("alpha", "90", "100000", SourceHealthState::Healthy, None);
    let second = quote("beta", "100", "1", SourceHealthState::Healthy, None);
    let third = quote("gamma", "110", "1", SourceHealthState::Healthy, None);
    let left = estimator()
        .estimate(AS_OF, &[dominant.clone(), second.clone(), third.clone()])
        .expect("estimation should complete");
    let right = estimator()
        .estimate(AS_OF, &[third, second, dominant])
        .expect("estimation should complete");

    let (ConsolidatedOutcome::Available(left), ConsolidatedOutcome::Available(right)) =
        (left, right)
    else {
        panic!("healthy inputs should be available");
    };
    assert_eq!(left.price(), right.price());
    assert_eq!(left.lineage().digest(), right.lineage().digest());
    assert!(
        left.lineage()
            .included()
            .iter()
            .all(|entry| entry.effective_weight().value() <= 500_000)
    );
    assert_eq!(
        left.lineage()
            .included()
            .iter()
            .map(|entry| u64::from(entry.effective_weight().value()))
            .sum::<u64>(),
        1_000_000
    );
    assert!(
        left.lineage()
            .included()
            .iter()
            .all(|entry| entry.effective_weight().value() > 0)
    );
    assert_eq!(left.lineage().as_of(), AS_OF);
}

#[test]
fn inverse_contract_depth_uses_domain_quote_notional_without_inverting_price() {
    let instrument = definition(
        "alpha",
        usd(),
        ProductType::Perpetual,
        ContractKind::Inverse,
    );
    let quote = VenueQuote::try_new(VenueQuoteInput {
        source: source("alpha"),
        instrument,
        candidate_kind: CandidatePriceKind::Midpoint,
        bid: price("99"),
        ask: price("101"),
        executable_quantity: quantity("2"),
        quality: Ppm::new(900_000).expect("quality"),
        freshness: Ppm::new(900_000).expect("freshness"),
        source_health: SourceHealthState::Healthy,
        metadata_status: MetadataStatus::Current,
        venue_status: VenueTradingState::Normal,
        clock_healthy: true,
        parser_healthy: true,
        book_healthy: true,
        event_time: UnixNanos::new(950),
        as_known_at: UnixNanos::new(960),
        conversion: None,
        instrument_provenance: InstrumentProvenance::direct_definition(),
    })
    .expect("quote should be valid");

    assert_eq!(quote.midpoint(), price("100"));
    assert_eq!(quote.executable_depth_quote(), notional("200"));
}

#[test]
fn lineage_covers_every_input_and_commits_material_quote_data() {
    let first = estimator()
        .estimate(
            AS_OF,
            &[
                quote("alpha", "100", "10", SourceHealthState::Healthy, None),
                quote("beta", "101", "10", SourceHealthState::Healthy, None),
            ],
        )
        .expect("first estimate should complete");
    let changed = estimator()
        .estimate(
            AS_OF,
            &[
                quote("alpha", "100", "10", SourceHealthState::Healthy, None),
                quote("beta", "102", "10", SourceHealthState::Healthy, None),
            ],
        )
        .expect("changed estimate should complete");
    let (ConsolidatedOutcome::Available(first), ConsolidatedOutcome::Available(changed)) =
        (first, changed)
    else {
        panic!("healthy quotes should produce prices");
    };
    assert_eq!(first.lineage().input_count(), 2);
    assert_eq!(changed.lineage().input_count(), 2);
    assert_ne!(first.lineage().digest(), changed.lineage().digest());
    assert!(
        first
            .lineage()
            .included()
            .iter()
            .all(|entry| { entry.raw_weight() > 0 && entry.effective_weight().value() > 0 })
    );
}

#[test]
fn event_time_staleness_cannot_be_hidden_by_a_fresh_as_known_time() {
    let stale = quote_with_policy_fields(
        "alpha",
        1,
        CandidatePriceKind::Midpoint,
        UnixNanos::new(800),
        UnixNanos::new(990),
        true,
    );
    let healthy = quote("beta", "101", "10", SourceHealthState::Healthy, None);

    let ConsolidatedOutcome::Abstained { lineage, .. } = estimator()
        .estimate(AS_OF, &[stale, healthy])
        .expect("staleness is a runtime decision")
    else {
        panic!("one fresh venue is insufficient");
    };
    assert_eq!(lineage.input_count(), 2);
    assert_eq!(
        lineage.decision(&source("alpha")),
        Some(&VenueExclusionReason::QuoteStale)
    );
    assert_eq!(
        lineage.decision(&source("beta")),
        Some(&VenueExclusionReason::InsufficientConsolidatedCoverage)
    );
}

#[test]
fn unsupported_candidate_kind_is_explicitly_excluded() {
    let unsupported = quote_with_policy_fields(
        "alpha",
        1,
        CandidatePriceKind::Microprice,
        UnixNanos::new(950),
        UnixNanos::new(960),
        true,
    );
    let healthy = quote("beta", "101", "10", SourceHealthState::Healthy, None);

    let ConsolidatedOutcome::Abstained { lineage, .. } = estimator()
        .estimate(AS_OF, &[unsupported, healthy])
        .expect("unsupported candidate is an exclusion")
    else {
        panic!("one supported venue is insufficient");
    };
    assert_eq!(
        lineage.decision(&source("alpha")),
        Some(&VenueExclusionReason::UnsupportedCandidateKind)
    );
}

#[test]
fn degraded_book_is_materialized_as_an_exclusion_record() {
    let degraded = quote_with_policy_fields(
        "alpha",
        1,
        CandidatePriceKind::Midpoint,
        UnixNanos::new(950),
        UnixNanos::new(960),
        false,
    );
    let healthy = quote("beta", "101", "10", SourceHealthState::Healthy, None);
    let ConsolidatedOutcome::Abstained { lineage, .. } = estimator()
        .estimate(AS_OF, &[degraded, healthy])
        .expect("book degradation should retain lineage")
    else {
        panic!("one healthy book is insufficient");
    };
    assert_eq!(
        lineage.decision(&source("alpha")),
        Some(&VenueExclusionReason::BookDegraded)
    );
    assert_eq!(lineage.excluded().len(), 2);
}

#[test]
fn duplicate_venue_generations_are_rejected_before_weighting() {
    let first = quote_with_policy_fields(
        "alpha",
        1,
        CandidatePriceKind::Midpoint,
        UnixNanos::new(950),
        UnixNanos::new(960),
        true,
    );
    let second = quote_with_policy_fields(
        "alpha",
        2,
        CandidatePriceKind::Midpoint,
        UnixNanos::new(950),
        UnixNanos::new(960),
        true,
    );
    assert_eq!(
        estimator().estimate(AS_OF, &[first, second]),
        Err(consolidated_market::ConsolidatedError::DuplicateVenueIdentity)
    );
}

#[test]
fn trusted_book_adapter_requires_synchronized_catalog_bound_state() {
    let instrument = definition("alpha", usd(), ProductType::Spot, ContractKind::None);
    let mut registry = InstrumentRegistry::new();
    registry
        .append_definition(
            instrument.clone(),
            RevisionMetadata::try_new(UnixNanos::new(900), "fixture:alpha")
                .expect("metadata should be valid"),
        )
        .expect("definition should append");
    let other_instrument = definition("beta", usd(), ProductType::Spot, ContractKind::None);
    registry
        .append_definition(
            other_instrument.clone(),
            RevisionMetadata::try_new(UnixNanos::new(901), "fixture:beta")
                .expect("metadata should be valid"),
        )
        .expect("second definition should append");
    let catalog = registry.snapshot().expect("snapshot should build");
    let verified_catalog =
        VerifiedCatalogSnapshot::try_new(&catalog).expect("catalog integrity should verify");

    let session = BookSession {
        connection_epoch: 1,
        subscription_epoch: 1,
        instrument_generation: instrument.id().generation(),
    };
    let book_config = BookConfig {
        instrument: instrument.id().clone(),
        price_tick: price("0.01"),
        quantity_step: quantity("0.001"),
        max_levels_per_side: 10,
        max_buffered_deltas: 10,
        max_buffered_level_updates: 100,
        sequence_policy: SequencePolicy::ExactNext,
        checksum_policy: ChecksumPolicy::Disabled,
        max_l3_orders: None,
        max_l3_levels_per_side: None,
    };
    let mut engine =
        OrderBookEngine::new(book_config.clone()).expect("book config should be valid");
    engine
        .start_session(session, SnapshotStrategy::StreamSnapshot)
        .expect("session should start");
    assert_eq!(
        engine
            .apply_snapshot(
                BookSnapshot {
                    bids: vec![BookLevel {
                        price: price("99"),
                        quantity: quantity("2"),
                        order_count: Some(1),
                    }],
                    asks: vec![BookLevel {
                        price: price("101"),
                        quantity: quantity("1"),
                        order_count: Some(1),
                    }],
                    last_sequence: 1,
                },
                session,
                1_000_000_000,
            )
            .expect("snapshot should apply"),
        ApplyResult::Applied
    );
    let TrustedBookAdmission::Accepted(adapted) = estimator()
        .admit_trusted_book(TrustedBookAdmissionInput {
            source: source("alpha"),
            engine: &engine,
            catalog: &verified_catalog,
            now_monotonic_ns: 1_010_000_000,
            candidate_kind: CandidatePriceKind::Midpoint,
            depth_levels: 1,
            source_health: SourceHealthState::Healthy,
            metadata_status: MetadataStatus::Current,
            venue_status: VenueTradingState::Normal,
            clock_healthy: true,
            parser_healthy: true,
            event_time: UnixNanos::new(950),
            as_known_at: UnixNanos::new(960),
            conversion: None,
        })
        .expect("trusted book should admit")
    else {
        panic!("synchronized book should be accepted");
    };

    assert_eq!(adapted.midpoint(), price("100"));
    assert_eq!(adapted.executable_depth_quote(), notional("101"));
    assert!(adapted.instrument_provenance().is_catalog_bound());
    assert_eq!(
        adapted
            .book_provenance()
            .expect("book provenance should be retained")
            .source_sequence(),
        1
    );
    let relabeled = estimator().admit_trusted_book(TrustedBookAdmissionInput {
        source: source("beta"),
        engine: &engine,
        catalog: &verified_catalog,
        now_monotonic_ns: 1_010_000_000,
        candidate_kind: CandidatePriceKind::Midpoint,
        depth_levels: 1,
        source_health: SourceHealthState::Healthy,
        metadata_status: MetadataStatus::Current,
        venue_status: VenueTradingState::Normal,
        clock_healthy: true,
        parser_healthy: true,
        event_time: UnixNanos::new(950),
        as_known_at: UnixNanos::new(960),
        conversion: None,
    });
    assert!(matches!(
        relabeled,
        Err(consolidated_market::ConsolidatedError::InvalidQuote)
    ));

    let disconnected_engine =
        OrderBookEngine::new(book_config.clone()).expect("book config should be valid");
    let degraded = estimator()
        .admit_trusted_book(TrustedBookAdmissionInput {
            source: source("alpha"),
            engine: &disconnected_engine,
            catalog: &verified_catalog,
            now_monotonic_ns: 1_010_000_000,
            candidate_kind: CandidatePriceKind::Midpoint,
            depth_levels: 1,
            source_health: SourceHealthState::Unhealthy,
            metadata_status: MetadataStatus::Current,
            venue_status: VenueTradingState::Normal,
            clock_healthy: true,
            parser_healthy: true,
            event_time: UnixNanos::new(950),
            as_known_at: UnixNanos::new(960),
            conversion: None,
        })
        .expect("degraded admission should be recorded");
    let TrustedBookAdmission::Excluded(exclusion) = &degraded else {
        panic!("disconnected engine must be excluded");
    };
    assert_eq!(exclusion.state(), orderbook::BookState::Disconnected);
    assert_eq!(exclusion.error(), &orderbook::BookError::Untrusted);
    let ConsolidatedOutcome::Abstained { lineage, .. } = estimator()
        .estimate_admissions(AS_OF, &[degraded])
        .expect("degraded-only input should abstain with lineage")
    else {
        panic!("no trusted book can produce a price");
    };
    assert_eq!(lineage.book_admission_excluded().len(), 1);
    assert_eq!(lineage.input_count(), 1);

    let mut locked_engine =
        OrderBookEngine::new(book_config.clone()).expect("book config should be valid");
    locked_engine
        .start_session(session, SnapshotStrategy::StreamSnapshot)
        .expect("locked-book session should start");
    locked_engine
        .apply_snapshot(
            BookSnapshot {
                bids: vec![BookLevel {
                    price: price("100"),
                    quantity: quantity("2"),
                    order_count: Some(1),
                }],
                asks: vec![BookLevel {
                    price: price("100"),
                    quantity: quantity("1"),
                    order_count: Some(1),
                }],
                last_sequence: 1,
            },
            session,
            1_000_000_000,
        )
        .expect("locked snapshot should be structurally accepted by the engine");
    let locked = estimator()
        .admit_trusted_book(TrustedBookAdmissionInput {
            source: source("alpha"),
            engine: &locked_engine,
            catalog: &verified_catalog,
            now_monotonic_ns: 1_010_000_000,
            candidate_kind: CandidatePriceKind::Midpoint,
            depth_levels: 1,
            source_health: SourceHealthState::Healthy,
            metadata_status: MetadataStatus::Current,
            venue_status: VenueTradingState::Normal,
            clock_healthy: true,
            parser_healthy: true,
            event_time: UnixNanos::new(950),
            as_known_at: UnixNanos::new(960),
            conversion: None,
        })
        .expect("locked book should become an auditable admission exclusion");
    let TrustedBookAdmission::Excluded(locked_exclusion) = &locked else {
        panic!("locked book must not become a price candidate");
    };
    assert_eq!(locked_exclusion.state(), orderbook::BookState::Synchronized);
    assert_eq!(locked_exclusion.error(), &orderbook::BookError::Untrusted);
    let ConsolidatedOutcome::Abstained { lineage, .. } = estimator()
        .estimate_admissions(AS_OF, &[locked])
        .expect("locked-only input should abstain with lineage")
    else {
        panic!("locked book cannot produce a price");
    };
    assert_eq!(lineage.book_admission_excluded().len(), 1);
}

#[test]
fn two_extreme_unconfirmed_quotes_abstain_instead_of_inventing_a_midpoint() {
    let quotes = [
        quote("alpha", "100", "10", SourceHealthState::Healthy, None),
        quote("beta", "1000", "10", SourceHealthState::Healthy, None),
    ];
    let ConsolidatedOutcome::Abstained { lineage, .. } = estimator()
        .estimate(AS_OF, &quotes)
        .expect("disagreement should be a lineage-preserving abstention")
    else {
        panic!("extreme two-venue disagreement must not publish a price");
    };
    assert_eq!(lineage.input_count(), 2);
    assert!(
        lineage
            .excluded()
            .iter()
            .all(|entry| { entry.reason() == VenueExclusionReason::UnconfirmedOutlier })
    );
}

#[test]
fn stablecoin_reference_requires_real_distinct_multi_venue_evidence() {
    let one_quote = [stablecoin_quote("stable-a", "0.98", true)];
    let StablecoinReferenceOutcome::Abstained {
        state,
        considered_sources,
        considered,
        excluded,
        evidence_digest,
        ..
    } = stablecoin_estimator()
        .estimate(AS_OF, &one_quote)
        .expect("insufficient evidence should be an outcome")
    else {
        panic!("one source cannot establish a reference");
    };
    assert_eq!(state, StablecoinDislocationState::InsufficientEvidence);
    assert_eq!(considered_sources.len(), 1);
    assert_eq!(considered.len(), 1);
    let observation = &considered[0];
    assert_eq!(observation.source(), &source("stable-a"));
    assert_eq!(
        observation.venue(),
        &VenueId::new("stable-a").expect("venue should be valid")
    );
    assert_eq!(observation.quote_asset(), &usdt());
    assert_eq!(observation.reference_asset(), &usd());
    assert_eq!(observation.bid(), price("0.97"));
    assert_eq!(observation.ask(), price("0.99"));
    assert_eq!(observation.executable_depth_reference(), notional("100"));
    assert_eq!(
        observation.quality(),
        Ppm::new(900_000).expect("quality should be valid")
    );
    assert_eq!(
        observation.freshness(),
        Ppm::new(900_000).expect("freshness should be valid")
    );
    assert_eq!(observation.source_health(), SourceHealthState::Healthy);
    assert_eq!(observation.event_time(), UnixNanos::new(940));
    assert_eq!(observation.as_known_at(), UnixNanos::new(950));
    assert!(observation.persistent());
    assert!(!observation.liquidity_failure());
    assert!(!observation.redemption_or_reserve_event());
    assert!(excluded.is_empty());
    assert_ne!(evidence_digest, [0; 32]);

    let three_quotes = [
        stablecoin_quote("stable-a", "0.98", true),
        stablecoin_quote("stable-b", "0.98", true),
        stablecoin_quote("stable-c", "1", false),
    ];
    let StablecoinReferenceOutcome::Available(reference) = stablecoin_estimator()
        .estimate(AS_OF, &three_quotes)
        .expect("covered evidence should estimate")
    else {
        panic!("three healthy sources should establish a reference");
    };
    assert_eq!(reference.source_ids().len(), 2);
    assert_eq!(
        reference.dislocation(),
        StablecoinDislocationState::SustainedMarketWide
    );
    assert_eq!(reference.executable_depth_reference(), notional("200"));
    assert_ne!(reference.evidence_digest(), &[0; 32]);
}

#[test]
fn one_source_stablecoin_abstention_digest_commits_the_considered_observation() {
    let first = [stablecoin_quote("stable-a", "0.98", true)];
    let second = [stablecoin_quote("stable-a", "0.99", true)];
    let StablecoinReferenceOutcome::Abstained {
        evidence_digest: first_digest,
        ..
    } = stablecoin_estimator()
        .estimate(AS_OF, &first)
        .expect("one source should abstain")
    else {
        panic!("one source cannot publish a reference");
    };
    let StablecoinReferenceOutcome::Abstained {
        evidence_digest: second_digest,
        ..
    } = stablecoin_estimator()
        .estimate(AS_OF, &second)
        .expect("one source should abstain")
    else {
        panic!("one source cannot publish a reference");
    };
    assert_ne!(first_digest, second_digest);
}

#[test]
fn excluded_stablecoin_material_is_auditable_and_changes_evidence_identity() {
    let baseline = [
        stablecoin_quote_with_flags("stable-a", "stable-a", "1", false, false, false),
        stablecoin_quote_with_flags("stable-b", "stable-b", "1", false, false, false),
        stablecoin_quote_with_flags("stable-c", "stable-c", "1", false, false, false),
        stablecoin_quote_with_flags("stable-d", "stable-d", "2", false, false, false),
        stablecoin_quote_with_flags("stable-e", "stable-e", "2", false, false, false),
    ];
    let changed = [
        stablecoin_quote_with_flags("stable-a", "stable-a", "1", false, false, false),
        stablecoin_quote_with_flags("stable-b", "stable-b", "1", false, false, false),
        stablecoin_quote_with_flags("stable-c", "stable-c", "1", false, false, false),
        stablecoin_quote_with_flags("stable-d", "stable-d", "2", true, false, true),
        stablecoin_quote_with_flags("stable-e", "stable-e", "2", true, false, true),
    ];
    let StablecoinReferenceOutcome::Available(baseline_reference) = stablecoin_estimator()
        .estimate(AS_OF, &baseline)
        .expect("retained consensus should establish a reference")
    else {
        panic!("three retained venues should publish a reference");
    };
    let StablecoinReferenceOutcome::Available(changed_reference) = stablecoin_estimator()
        .estimate(AS_OF, &changed)
        .expect("excluded critical flags must not classify retained evidence")
    else {
        panic!("three retained venues should publish a reference");
    };
    assert_eq!(
        baseline_reference.dislocation(),
        StablecoinDislocationState::Normal
    );
    assert_eq!(
        changed_reference.dislocation(),
        StablecoinDislocationState::Normal
    );
    assert_ne!(
        baseline_reference.evidence_digest(),
        changed_reference.evidence_digest()
    );

    let contradictory = [
        stablecoin_quote_with_flags("stable-a", "stable-a", "0.5", true, true, true),
        stablecoin_quote_with_flags("stable-b", "stable-b", "1.5", true, true, true),
    ];
    let StablecoinReferenceOutcome::Abstained { excluded, .. } = stablecoin_estimator()
        .estimate(AS_OF, &contradictory)
        .expect("contradictory evidence should abstain")
    else {
        panic!("contradictory evidence cannot publish a reference");
    };
    assert!(excluded.iter().all(|entry| {
        entry.quote_asset() == &usdt()
            && entry.reference_asset() == &usd()
            && entry.quality() == Ppm::new(900_000).expect("quality should be valid")
            && entry.freshness() == Ppm::new(900_000).expect("freshness should be valid")
            && entry.source_health() == SourceHealthState::Healthy
            && entry.persistent()
            && entry.liquidity_failure()
            && entry.redemption_or_reserve_event()
    }));
}

#[test]
fn contradictory_two_source_stablecoin_quotes_abstain() {
    let contradictory = [
        stablecoin_quote("stable-a", "0.5", false),
        stablecoin_quote("stable-b", "1.5", false),
    ];
    let StablecoinReferenceOutcome::Abstained {
        state,
        considered_sources,
        excluded,
        ..
    } = stablecoin_estimator()
        .estimate(AS_OF, &contradictory)
        .expect("contradiction should be an auditable abstention")
    else {
        panic!("contradictory stablecoin observations must not be averaged");
    };
    assert_eq!(state, StablecoinDislocationState::InsufficientEvidence);
    assert!(considered_sources.is_empty());
    assert_eq!(excluded.len(), 2);
    assert!(excluded.iter().all(|entry| {
        entry.reason() == consolidated_market::StablecoinExclusionReason::UnconfirmedOutlier
    }));
}

#[test]
fn two_feeds_from_one_venue_do_not_satisfy_stablecoin_coverage() {
    let duplicated_venue = [
        stablecoin_quote_for_source("feed-a", "shared", "0.98", true),
        stablecoin_quote_for_source("feed-b", "shared", "0.98", true),
    ];
    assert_eq!(
        stablecoin_estimator().estimate(AS_OF, &duplicated_venue),
        Err(consolidated_market::ConsolidatedError::DuplicateVenueIdentity)
    );
}

#[test]
fn one_venue_cannot_assert_a_global_stablecoin_failure_state() {
    let observations = [
        stablecoin_quote_with_flags("stable-a", "stable-a", "0.98", true, true, false),
        stablecoin_quote_with_flags("stable-b", "stable-b", "0.98", true, false, false),
    ];
    let StablecoinReferenceOutcome::Available(reference) = stablecoin_estimator()
        .estimate(AS_OF, &observations)
        .expect("corroborated prices should produce a reference")
    else {
        panic!("consistent prices should remain usable");
    };
    assert_ne!(
        reference.dislocation(),
        StablecoinDislocationState::LiquidityFailure
    );
    assert_ne!(
        reference.dislocation(),
        StablecoinDislocationState::RedemptionOrReserveEvent
    );
}

#[test]
fn inconsistent_conversion_evidence_is_excluded_for_every_affected_venue() {
    let quotes = [
        quote(
            "alpha",
            "100",
            "10",
            SourceHealthState::Healthy,
            Some(usdt_reference("0.98", 3)),
        ),
        quote(
            "beta",
            "100",
            "10",
            SourceHealthState::Healthy,
            Some(usdt_reference("0.97", 3)),
        ),
    ];
    let ConsolidatedOutcome::Abstained { lineage, .. } = estimator()
        .estimate(AS_OF, &quotes)
        .expect("mixed evidence should abstain")
    else {
        panic!("mixed conversion versions must not be combined");
    };
    assert!(
        lineage
            .excluded()
            .iter()
            .all(|entry| { entry.reason() == VenueExclusionReason::ConversionInconsistent })
    );
}

#[test]
fn unhealthy_conflicting_conversion_cannot_suppress_healthy_consensus() {
    let consensus = usdt_reference("0.98", 3);
    let conflicting = usdt_reference("0.97", 3);
    let quotes = [
        quote(
            "alpha",
            "100",
            "10",
            SourceHealthState::Healthy,
            Some(consensus.clone()),
        ),
        quote(
            "beta",
            "101",
            "10",
            SourceHealthState::Healthy,
            Some(consensus),
        ),
        quote(
            "gamma",
            "100",
            "10",
            SourceHealthState::Quarantined,
            Some(conflicting),
        ),
    ];
    let ConsolidatedOutcome::Available(result) = estimator()
        .estimate(AS_OF, &quotes)
        .expect("unhealthy evidence should be excluded before consistency")
    else {
        panic!("healthy consensus should remain available");
    };
    assert_eq!(
        result.lineage().decision(&source("gamma")),
        Some(&VenueExclusionReason::SourceUnhealthy)
    );
}

#[test]
fn stale_conflicting_conversion_cannot_suppress_healthy_consensus() {
    let consensus = usdt_reference_with_prefix("0.98", 3, "fresh");
    let stale_conflicting = usdt_reference_at(
        "0.97",
        3,
        "stale",
        UnixNanos::new(800),
        UnixNanos::new(810),
        UnixNanos::new(850),
    );
    let quotes = [
        quote(
            "alpha",
            "100",
            "10",
            SourceHealthState::Healthy,
            Some(consensus.clone()),
        ),
        quote(
            "beta",
            "101",
            "10",
            SourceHealthState::Healthy,
            Some(consensus),
        ),
        quote(
            "gamma",
            "100",
            "10",
            SourceHealthState::Healthy,
            Some(stale_conflicting),
        ),
    ];
    let ConsolidatedOutcome::Available(fair_price) = estimator()
        .estimate(AS_OF, &quotes)
        .expect("stale conflicting conversion should be excluded independently")
    else {
        panic!("fresh conversion consensus should remain available");
    };
    assert_eq!(
        fair_price.lineage().decision(&source("gamma")),
        Some(&VenueExclusionReason::ConversionStale)
    );
    assert_eq!(fair_price.lineage().included().len(), 2);
}

#[test]
fn consolidated_lineage_commits_stablecoin_evidence_membership() {
    let first_reference = usdt_reference_with_prefix("0.98", 3, "first");
    let second_reference = usdt_reference_with_prefix("0.98", 3, "second");
    let first = estimator()
        .estimate(
            AS_OF,
            &[
                quote(
                    "alpha",
                    "100",
                    "10",
                    SourceHealthState::Healthy,
                    Some(first_reference.clone()),
                ),
                quote(
                    "beta",
                    "101",
                    "10",
                    SourceHealthState::Healthy,
                    Some(first_reference),
                ),
            ],
        )
        .expect("first estimate should complete");
    let second = estimator()
        .estimate(
            AS_OF,
            &[
                quote(
                    "alpha",
                    "100",
                    "10",
                    SourceHealthState::Healthy,
                    Some(second_reference.clone()),
                ),
                quote(
                    "beta",
                    "101",
                    "10",
                    SourceHealthState::Healthy,
                    Some(second_reference),
                ),
            ],
        )
        .expect("second estimate should complete");
    let (ConsolidatedOutcome::Available(first), ConsolidatedOutcome::Available(second)) =
        (first, second)
    else {
        panic!("both evidence sets should produce a price");
    };
    assert_ne!(first.lineage().digest(), second.lineage().digest());
}

#[test]
fn future_and_option_products_are_rejected_by_this_estimator() {
    for product_type in [ProductType::Future, ProductType::Option] {
        let result = FairPriceEstimator::try_new(EstimatorConfigInput {
            policy_id: PolicyId::new("unsupported-product-v1").expect("policy"),
            base_asset: btc(),
            reference_asset: usd(),
            product_type,
            minimum_venues: 2,
            maximum_venues: 64,
            quote_ttl: DurationNanos::new(100),
            conversion_ttl: DurationNanos::new(100),
            depth_cap_reference: notional("1000"),
            minimum_quality: Ppm::new(500_000).expect("quality"),
            minimum_freshness: Ppm::new(500_000).expect("freshness"),
            maximum_venue_weight: Ppm::new(500_000).expect("weight"),
            outlier_threshold: Ppm::new(200_000).expect("outlier"),
            minimum_conversion_sources: 2,
            maximum_conversion_interval_width: Ppm::new(100_000).expect("interval"),
            require_catalog_provenance: true,
        });
        assert!(matches!(
            result,
            Err(consolidated_market::ConsolidatedError::InvalidConfig)
        ));
    }
}

#[test]
fn oversized_input_set_fails_before_any_partial_decision() {
    let repeated = quote("alpha", "100", "10", SourceHealthState::Healthy, None);
    let quotes = vec![repeated; 65];
    assert_eq!(
        estimator().estimate(AS_OF, &quotes),
        Err(consolidated_market::ConsolidatedError::InvalidInputSet)
    );
}

#[test]
fn near_i128_limit_ratios_do_not_overflow_intermediate_products() {
    let near_limit = FixedDecimal::new(i128::MAX - 1, 0).expect("near-limit decimal");
    let limit = FixedDecimal::new(i128::MAX, 0).expect("limit decimal");
    let estimator = FairPriceEstimator::try_new(EstimatorConfigInput {
        policy_id: PolicyId::new("near-limit-v1").expect("policy"),
        base_asset: btc(),
        reference_asset: usd(),
        product_type: ProductType::Spot,
        minimum_venues: 2,
        maximum_venues: 2,
        quote_ttl: DurationNanos::new(100),
        conversion_ttl: DurationNanos::new(100),
        depth_cap_reference: Notional::new(limit).expect("depth cap"),
        minimum_quality: Ppm::new(1).expect("quality"),
        minimum_freshness: Ppm::new(1).expect("freshness"),
        maximum_venue_weight: Ppm::new(500_000).expect("weight"),
        outlier_threshold: Ppm::new(1).expect("outlier"),
        minimum_conversion_sources: 2,
        maximum_conversion_interval_width: Ppm::new(1).expect("interval"),
        require_catalog_provenance: false,
    })
    .expect("near-limit config should be valid");
    let quotes: Vec<_> = ["alpha", "beta"]
        .into_iter()
        .map(|venue| {
            VenueQuote::try_new(VenueQuoteInput {
                source: source(venue),
                instrument: definition(venue, usd(), ProductType::Spot, ContractKind::None),
                candidate_kind: CandidatePriceKind::Midpoint,
                bid: Price::new(near_limit).expect("bid"),
                ask: Price::new(near_limit).expect("ask"),
                executable_quantity: quantity("1"),
                quality: Ppm::new(1_000_000).expect("quality"),
                freshness: Ppm::new(1_000_000).expect("freshness"),
                source_health: SourceHealthState::Healthy,
                metadata_status: MetadataStatus::Current,
                venue_status: VenueTradingState::Normal,
                clock_healthy: true,
                parser_healthy: true,
                book_healthy: true,
                event_time: UnixNanos::new(950),
                as_known_at: UnixNanos::new(960),
                conversion: None,
                instrument_provenance: InstrumentProvenance::direct_definition(),
            })
            .expect("near-limit quote should be valid")
        })
        .collect();
    let ConsolidatedOutcome::Available(result) = estimator
        .estimate(AS_OF, &quotes)
        .expect("exact big-integer comparison should avoid overflow")
    else {
        panic!("two agreeing near-limit quotes should remain available");
    };
    assert_eq!(result.price(), Price::new(near_limit).expect("price"));
}
