use domain::{
    AssetId, AssetNamespace, ContractKind, ContractValueUnit, InstrumentDefinition,
    InstrumentDefinitionInput, InstrumentId, ProductType, SourceId, SourceKind, UnixNanos, VenueId,
};
use fast_state::{
    TriggerError, TriggerHealth, TriggerMissingness, TriggerStateBuilder, TriggerStateTarget,
};
use feature_engine::features::{
    task_five_definitions, task_four_definitions, task_six_definitions,
};
use feature_registry::{
    CodeRevision, FeatureConsumptionRole, FeatureDatum, FeatureDefinition, FeatureEntity,
    FeatureObservation, FeatureObservationInput, FeatureRegistry, FeatureValue, FeatureValueType,
    FinalityState, FiniteF64, LineageHash, MissingnessReason, ObservationRevision, QualityScore,
    SourceCoverage, SourceCoverageEntry, WindowParameter,
};
use fixed_decimal::{FixedDecimal, Price, Quantity};
use instrument_registry::{InstrumentRegistry, RevisionMetadata};
use quality::SourceHealthState;

const AS_OF_NS: i64 = 2_040_000_000_000;
const KNOWN_AT_NS: i64 = AS_OF_NS + 10_000_000_000;
const ONE_SECOND_NS: i64 = 1_000_000_000;

#[test]
fn liquidation_pressure_requires_complete_sources_gate_and_oi_confirmation() {
    let fixture = Fixture::new();
    let sampled = fixture
        .build_state(FixtureOptions {
            completeness_class: 3,
            cascade_eligible: false,
            liquidation_velocity: 4.0,
            open_interest_relative_change: 0.0,
            ..FixtureOptions::default()
        })
        .expect("sampled liquidation state");
    assert_eq!(sampled.liquidation_velocity(), Some(4.0));
    assert_eq!(sampled.liquidation_pressure(), Some(0.0));
    assert!(
        sampled
            .missingness()
            .contains(&TriggerMissingness::IncompleteLiquidationCoverage)
    );

    let confirmed = fixture
        .build_state(FixtureOptions {
            completeness_class: 1,
            cascade_eligible: true,
            liquidation_velocity: 4.0,
            open_interest_relative_change: -0.25,
            ..FixtureOptions::default()
        })
        .expect("confirmed cascade state");
    assert!(
        confirmed.liquidation_pressure().expect("pressure")
            > sampled.liquidation_pressure().expect("sampled pressure")
    );
    assert_eq!(confirmed.liquidation_completeness_millionths(), 1_000_000);
    assert!(
        !confirmed
            .missingness()
            .contains(&TriggerMissingness::IncompleteLiquidationCoverage)
    );
    assert_eq!(confirmed.health(), TriggerHealth::Healthy);
}

#[test]
fn stale_book_feature_is_missing_not_zero() {
    let fixture = Fixture::new();
    let state = fixture
        .build_state(FixtureOptions {
            stale_book: true,
            cascade_eligible: false,
            ..FixtureOptions::default()
        })
        .expect("stale state remains inspectable");

    assert_eq!(state.depth_disappearance(), None);
    assert!(state.missingness().contains(&TriggerMissingness::StaleBook));
    assert!(
        state
            .missingness()
            .contains(&TriggerMissingness::DepthDisappearance)
    );
    assert_eq!(state.health(), TriggerHealth::Degraded);
}

#[test]
fn source_outage_fails_closed_without_erasing_the_quality_reason() {
    let fixture = Fixture::new();
    let state = fixture
        .build_state(FixtureOptions {
            source_outage: true,
            cascade_eligible: false,
            ..FixtureOptions::default()
        })
        .expect("outage state remains inspectable");

    assert_eq!(state.depth_disappearance(), None);
    assert_eq!(state.cancellation_burst(), None);
    assert_eq!(state.sweep_direction(), None);
    assert_eq!(state.order_flow_imbalance(), None);
    assert_eq!(state.cross_venue_dispersion_acceleration(), None);
    assert_eq!(state.liquidation_pressure(), None);
    assert_eq!(state.open_interest_destruction(), None);
    assert_eq!(state.mark_index_divergence(), None);
    assert!(
        state
            .missingness()
            .contains(&TriggerMissingness::SourceLoss)
    );
    assert_eq!(state.health(), TriggerHealth::Unavailable);
}

#[test]
fn trigger_aggregations_preserve_direction_scale_and_age() {
    let fixture = Fixture::new();
    let state = fixture
        .build_state(FixtureOptions::default())
        .expect("complete trigger state");

    assert_close(state.depth_disappearance().expect("depth"), 4.0);
    assert_close(
        state.cancellation_burst().expect("cancellation"),
        10.0_f64.ln_1p(),
    );
    assert_close(state.sweep_direction().expect("sweep"), 0.75);
    assert_close(state.order_flow_imbalance().expect("OFI"), 12.0);
    assert_close(
        state
            .cross_venue_dispersion_acceleration()
            .expect("dispersion acceleration"),
        0.02,
    );
    assert_close(
        state.open_interest_destruction().expect("OI destruction"),
        0.2,
    );
    assert_close(state.mark_index_divergence().expect("mark/index"), 0.01);
    assert_eq!(state.maximum_feature_age_ns(), Some(500));
    assert_eq!(state.minimum_quality_score().millionths(), 900_000);
    assert_eq!(state.minimum_source_coverage_millionths(), 1_000_000);
}

#[test]
fn input_order_does_not_change_the_trigger_record() {
    let fixture = Fixture::new();
    let observations = fixture.observations(FixtureOptions::default());
    let expected = fixture
        .build(observations.clone())
        .expect("canonical state");
    let mut reversed = observations;
    reversed.reverse();
    assert_eq!(fixture.build(reversed), Ok(expected));
}

#[test]
fn quality_and_coverage_summary_use_the_weakest_validated_evidence() {
    let fixture = Fixture::new();
    let mut observations = fixture.observations(FixtureOptions {
        cascade_eligible: false,
        ..FixtureOptions::default()
    });
    let first_source = fixture.sources[0].clone();
    let index = observations
        .iter()
        .position(|observation| {
            observation.feature_id().as_str() == "feature_age_ns"
                && observation.entity() == &FeatureEntity::Source(first_source.clone())
        })
        .expect("source age observation");
    observations[index] = fixture.observation(
        "feature_age_ns",
        FeatureEntity::Source(first_source),
        integer(200),
        AS_OF_NS,
        200,
        600_000,
        partial_coverage(&fixture.sources),
        91,
    );

    let state = fixture.build(observations).expect("degraded state");
    assert_eq!(state.minimum_quality_score().millionths(), 600_000);
    assert_eq!(state.minimum_source_coverage_millionths(), 500_000);
    assert_eq!(state.health(), TriggerHealth::Degraded);
    assert!(state.depth_disappearance().is_some());
}

#[test]
fn explicitly_degraded_source_health_makes_trigger_values_unavailable() {
    let fixture = Fixture::new();
    let mut observations = fixture.observations(FixtureOptions {
        cascade_eligible: false,
        ..FixtureOptions::default()
    });
    let first_source = fixture.sources[0].clone();
    let index = observations
        .iter()
        .position(|observation| {
            observation.feature_id().as_str() == "feature_age_ns"
                && observation.entity() == &FeatureEntity::Source(first_source.clone())
        })
        .expect("source age observation");
    observations[index] = fixture.observation(
        "feature_age_ns",
        FeatureEntity::Source(first_source),
        integer(200),
        AS_OF_NS,
        200,
        900_000,
        coverage_with_health(&fixture.sources, SourceHealthState::Degraded),
        92,
    );

    let state = fixture.build(observations).expect("unavailable state");
    assert_eq!(state.health(), TriggerHealth::Unavailable);
    assert_eq!(state.depth_disappearance(), None);
    assert!(
        state
            .missingness()
            .contains(&TriggerMissingness::SourceQualityDegraded)
    );
}

#[test]
fn insufficient_dispersion_history_is_explicitly_missing() {
    let fixture = Fixture::new();
    let mut observations = fixture.observations(FixtureOptions::default());
    observations.retain(|observation| {
        observation.feature_id().as_str() != "cross_venue_median_absolute_dispersion"
            || observation.event_time_end().value() == AS_OF_NS
    });

    let state = fixture.build(observations).expect("missing-aware state");
    assert_eq!(state.cross_venue_dispersion_acceleration(), None);
    assert!(
        state
            .missingness()
            .contains(&TriggerMissingness::InsufficientHistory)
    );
}

#[test]
fn oi_destruction_requires_both_inputs_at_the_same_event_time() {
    let fixture = Fixture::new();
    let mut missing_return = fixture.observations(FixtureOptions::default());
    missing_return.retain(|observation| observation.feature_id().as_str() != "log_return");
    let state = fixture
        .build(missing_return)
        .expect("missing aligned return remains inspectable");
    assert_eq!(state.open_interest_destruction(), None);
    assert!(
        state
            .missingness()
            .contains(&TriggerMissingness::OpenInterestDestruction)
    );

    let mut misaligned = fixture.observations(FixtureOptions::default());
    let index = misaligned
        .iter()
        .position(|observation| observation.feature_id().as_str() == "log_return")
        .expect("aligned return");
    misaligned[index] = fixture.observation(
        "log_return",
        FeatureEntity::Asset(fixture.asset.clone()),
        float(-1.0),
        AS_OF_NS - 60 * ONE_SECOND_NS,
        100,
        900_000,
        full_coverage(&fixture.sources),
        94,
    );
    assert_eq!(
        fixture.build(misaligned),
        Err(TriggerError::InvalidFeatureHistory {
            feature: "open_interest_destruction"
        })
    );
}

#[test]
fn oi_destruction_freezes_the_aligned_return_horizon() {
    let fixture = Fixture::new();
    let mut observations = fixture.observations(FixtureOptions::default());
    let index = observations
        .iter()
        .position(|observation| observation.feature_id().as_str() == "log_return")
        .expect("aligned return");
    let definition = definition(&fixture.registry, "log_return");
    let window = definition
        .windows()
        .iter()
        .find(|window| window.id().as_str() == "rolling_15m")
        .expect("fifteen-minute return window");
    let mut input = observations[index].clone().into_input();
    input.window_id = window.id().clone();
    input.event_time_start = UnixNanos::new(
        AS_OF_NS
            - i64::try_from(match window.parameter() {
                WindowParameter::Time { extent, .. } => extent.value(),
                other => panic!("unexpected return window: {other:?}"),
            })
            .expect("bounded extent"),
    );
    observations[index] = FeatureObservation::try_new(input).expect("valid alternate horizon");

    assert_eq!(
        fixture.build(observations),
        Err(TriggerError::FeatureContractMismatch {
            feature: "log_return"
        })
    );
}

#[test]
fn duplicate_observations_and_target_mismatch_fail_closed() {
    let fixture = Fixture::new();
    let mut duplicate = fixture.observations(FixtureOptions::default());
    duplicate.push(duplicate[0].clone());
    assert_eq!(
        fixture.build(duplicate),
        Err(TriggerError::DuplicateObservation)
    );

    let mut wrong_target = fixture.observations(FixtureOptions::default());
    let index = wrong_target
        .iter()
        .position(|observation| observation.feature_id().as_str() == "mark_index_divergence")
        .expect("mark/index observation");
    wrong_target[index] = fixture.observation(
        "mark_index_divergence",
        FeatureEntity::Instrument(
            InstrumentId::new_for_product(
                VenueId::new("kraken").expect("venue"),
                "XBTUSD",
                ProductType::Perpetual,
                1,
            )
            .expect("other instrument"),
        ),
        float(0.01),
        AS_OF_NS,
        100,
        900_000,
        full_coverage(&fixture.sources),
        99,
    );
    assert_eq!(
        fixture.build(wrong_target),
        Err(TriggerError::ObservationTargetMismatch)
    );
}

#[test]
fn invalid_liquidation_completeness_class_is_rejected() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.build_state(FixtureOptions {
            completeness_class: 9,
            ..FixtureOptions::default()
        }),
        Err(TriggerError::InvalidFeatureValue {
            feature: "liquidation_source_completeness_flag"
        })
    );
}

#[test]
fn absent_or_contradictory_cascade_gate_never_confirms_pressure() {
    let fixture = Fixture::new();
    let mut absent_gate = fixture.observations(FixtureOptions::default());
    absent_gate
        .retain(|observation| observation.feature_id().as_str() != "cascade_eligibility_gate");
    let state = fixture.build(absent_gate).expect("missing-aware state");
    assert_eq!(state.liquidation_pressure(), None);
    assert!(
        state
            .missingness()
            .contains(&TriggerMissingness::CascadeIneligible)
    );

    assert_eq!(
        fixture.build_state(FixtureOptions {
            completeness_class: 3,
            cascade_eligible: true,
            ..FixtureOptions::default()
        }),
        Err(TriggerError::InconsistentCascadeEligibility)
    );
}

#[test]
fn finalized_feature_age_uses_the_emitted_sample_age_not_later_finality_time() {
    let fixture = Fixture::new();
    let mut observations = fixture.observations(FixtureOptions::default());
    let first_source = fixture.sources[0].clone();
    let index = observations
        .iter()
        .position(|observation| {
            observation.feature_id().as_str() == "feature_age_ns"
                && observation.entity() == &FeatureEntity::Source(first_source.clone())
        })
        .expect("source age observation");
    let mut input = observations[index].clone().into_input();
    input.datum = integer(200);
    input.as_known_at = UnixNanos::new(AS_OF_NS + 6_000_000_000);
    input.computed_at = UnixNanos::new(AS_OF_NS + 6_000_000_001);
    input.watermark = Some(UnixNanos::new(AS_OF_NS + 5_000_000_000));
    input.finality_as_known_at = UnixNanos::new(AS_OF_NS + 6_000_000_000);
    input.finality_state = FinalityState::Final;
    observations[index] = FeatureObservation::try_new(input).expect("final feature age");

    let state = fixture
        .build(observations)
        .expect("final observation accepted");
    assert_eq!(state.maximum_feature_age_ns(), Some(500));
}

#[test]
fn cascade_gate_must_cover_the_exact_target_source_universe() {
    let fixture = Fixture::new();
    let mut observations = fixture.observations(FixtureOptions::default());
    let index = observations
        .iter()
        .position(|observation| observation.feature_id().as_str() == "cascade_eligibility_gate")
        .expect("cascade gate");
    observations[index] = fixture.observation(
        "cascade_eligibility_gate",
        FeatureEntity::Asset(fixture.asset.clone()),
        boolean(true),
        AS_OF_NS,
        100,
        900_000,
        SourceCoverage::try_new(vec![SourceCoverageEntry::new(
            fixture.sources[0].clone(),
            SourceHealthState::Healthy,
        )])
        .expect("subset universe"),
        93,
    );
    assert_eq!(
        fixture.build(observations),
        Err(TriggerError::SourceUniverseMismatch)
    );
}

#[test]
fn consolidated_inputs_must_cover_the_exact_target_source_universe() {
    let fixture = Fixture::new();
    for feature in ["cross_venue_median_absolute_dispersion", "log_return"] {
        let mut observations = fixture.observations(FixtureOptions::default());
        let index = observations
            .iter()
            .position(|observation| observation.feature_id().as_str() == feature)
            .expect("consolidated observation");
        let mut input = observations[index].clone().into_input();
        input.source_coverage = SourceCoverage::try_new(vec![
            SourceCoverageEntry::new(fixture.sources[0].clone(), SourceHealthState::Healthy),
            SourceCoverageEntry::new(source("coinbase"), SourceHealthState::Healthy),
        ])
        .expect("alternate complete source universe");
        observations[index] =
            FeatureObservation::try_new(input).expect("individually valid observation");

        assert_eq!(
            fixture.build(observations),
            Err(TriggerError::SourceUniverseMismatch),
            "{feature} must not cross source universes"
        );
    }
}

#[test]
fn missing_return_evidence_does_not_invent_global_book_or_source_failure() {
    let fixture = Fixture::new();
    for reason in [MissingnessReason::Stale, MissingnessReason::SequenceGap] {
        let mut observations = fixture.observations(FixtureOptions::default());
        let index = observations
            .iter()
            .position(|observation| observation.feature_id().as_str() == "log_return")
            .expect("aligned return");
        let mut input = observations[index].clone().into_input();
        input.datum = FeatureDatum::Missing(reason);
        input.value_type = FeatureValueType::Float64;
        input.quality_score = QualityScore::from_millionths(0).expect("zero quality");
        input.finality_state = FinalityState::Invalid;
        observations[index] =
            FeatureObservation::try_new(input).expect("canonical explicit missing return");

        let state = fixture
            .build(observations)
            .expect("feature-local missingness remains inspectable");
        assert!(state.depth_disappearance().is_some());
        assert!(state.cancellation_burst().is_some());
        assert_eq!(state.open_interest_destruction(), None);
        assert_eq!(state.liquidation_pressure(), None);
        assert!(
            state
                .missingness()
                .contains(&TriggerMissingness::OpenInterestDestruction)
        );
        assert!(!state.missingness().contains(&TriggerMissingness::StaleBook));
        assert!(
            !state
                .missingness()
                .contains(&TriggerMissingness::SourceLoss)
        );
        assert_eq!(state.health(), TriggerHealth::Degraded);
    }
}

struct Fixture {
    registry: FeatureRegistry,
    target: TriggerStateTarget,
    instrument: InstrumentId,
    asset: AssetId,
    sources: Vec<SourceId>,
}

#[derive(Clone, Copy)]
struct FixtureOptions {
    completeness_class: i64,
    cascade_eligible: bool,
    liquidation_velocity: f64,
    open_interest_relative_change: f64,
    aligned_price_return: f64,
    stale_book: bool,
    source_outage: bool,
}

impl Default for FixtureOptions {
    fn default() -> Self {
        Self {
            completeness_class: 1,
            cascade_eligible: true,
            liquidation_velocity: 1.0,
            open_interest_relative_change: -0.2,
            aligned_price_return: -1.0,
            stale_book: false,
            source_outage: false,
        }
    }
}

impl Fixture {
    fn new() -> Self {
        let asset = AssetId::new(AssetNamespace::Native, "bitcoin", "", "BTC", 1).expect("asset");
        let instrument = InstrumentId::new_for_product(
            VenueId::new("binance").expect("venue"),
            "BTCUSDT",
            ProductType::Perpetual,
            1,
        )
        .expect("instrument");
        let sources = vec![source("binance"), source("kraken")];
        let quote =
            AssetId::new(AssetNamespace::Synthetic, "", "", "USDT", 1).expect("quote asset");
        let instrument_definition = InstrumentDefinition::new(InstrumentDefinitionInput {
            id: instrument.clone(),
            product_type: ProductType::Perpetual,
            base_asset: asset.clone(),
            quote_asset: quote.clone(),
            settlement_asset: quote,
            contract_multiplier: FixedDecimal::parse_canonical("1").expect("multiplier"),
            contract_value_unit: ContractValueUnit::Base,
            contract_kind: ContractKind::Linear,
            expiry_time: None,
            strike: None,
            option_side: None,
            price_tick: Price::new(FixedDecimal::parse_canonical("0.1").expect("price tick"))
                .expect("price tick"),
            quantity_step: Quantity::new(
                FixedDecimal::parse_canonical("0.001").expect("quantity step"),
            )
            .expect("quantity step"),
            listing_time: UnixNanos::new(1),
            delisting_time: None,
        })
        .expect("instrument definition");
        let mut instrument_registry = InstrumentRegistry::new();
        instrument_registry
            .append_definition(
                instrument_definition,
                RevisionMetadata::try_new(UnixNanos::new(1), "fast-state:test")
                    .expect("revision metadata"),
            )
            .expect("catalogue definition");
        let catalog = instrument_registry.snapshot().expect("catalogue snapshot");
        let target =
            TriggerStateTarget::try_from_catalog(&catalog, &instrument, AS_OF_NS, sources.clone())
                .expect("target");
        let mut registry = FeatureRegistry::new();
        for definition in task_four_definitions()
            .expect("Task 4 catalogue")
            .into_iter()
            .chain(task_five_definitions().expect("Task 5 catalogue"))
            .chain(task_six_definitions().expect("Task 6 catalogue"))
        {
            registry.register(definition).expect("unique definition");
        }
        Self {
            registry,
            target,
            instrument,
            asset,
            sources,
        }
    }

    fn build_state(
        &self,
        options: FixtureOptions,
    ) -> Result<fast_state::TriggerState, TriggerError> {
        self.build(self.observations(options))
    }

    fn build(
        &self,
        observations: Vec<FeatureObservation>,
    ) -> Result<fast_state::TriggerState, TriggerError> {
        TriggerStateBuilder::try_new(
            &self.registry,
            self.target.clone(),
            AS_OF_NS,
            KNOWN_AT_NS,
            observations,
        )?
        .build()
    }

    #[allow(clippy::too_many_lines)]
    fn observations(&self, options: FixtureOptions) -> Vec<FeatureObservation> {
        let instrument = FeatureEntity::Instrument(self.instrument.clone());
        let asset = FeatureEntity::Asset(self.asset.clone());
        let outage = options.source_outage;
        let mut observations = vec![
            self.observation(
                "bid_depth_change",
                instrument.clone(),
                decimal("-4"),
                AS_OF_NS,
                100,
                900_000,
                full_coverage(&self.sources),
                1,
            ),
            self.observation(
                "ask_depth_change",
                instrument.clone(),
                decimal("-2"),
                AS_OF_NS,
                100,
                900_000,
                full_coverage(&self.sources),
                2,
            ),
            self.observation(
                "cancellation_rate_per_second",
                instrument.clone(),
                float(5.0),
                AS_OF_NS,
                100,
                900_000,
                full_coverage(&self.sources),
                3,
            ),
            self.observation(
                "cancellation_to_trade_ratio",
                instrument.clone(),
                float(2.0),
                AS_OF_NS,
                100,
                900_000,
                full_coverage(&self.sources),
                4,
            ),
            self.observation(
                "trade_print_sweep_direction",
                instrument.clone(),
                float(0.75),
                AS_OF_NS,
                100,
                900_000,
                full_coverage(&self.sources),
                5,
            ),
            self.observation(
                "top_of_book_ofi",
                instrument.clone(),
                decimal("12"),
                AS_OF_NS,
                100,
                900_000,
                full_coverage(&self.sources),
                6,
            ),
            self.observation(
                "cross_venue_median_absolute_dispersion",
                asset.clone(),
                float(0.01),
                AS_OF_NS - ONE_SECOND_NS,
                100,
                900_000,
                full_coverage(&self.sources),
                7,
            ),
            self.observation(
                "cross_venue_median_absolute_dispersion",
                asset.clone(),
                float(0.03),
                AS_OF_NS,
                100,
                900_000,
                full_coverage(&self.sources),
                8,
            ),
            self.observation(
                "liquidation_observed_velocity",
                instrument.clone(),
                float(options.liquidation_velocity),
                AS_OF_NS,
                100,
                900_000,
                full_coverage(&self.sources),
                9,
            ),
            self.observation(
                "open_interest_relative_change",
                instrument.clone(),
                float(options.open_interest_relative_change),
                AS_OF_NS,
                100,
                900_000,
                full_coverage(&self.sources),
                10,
            ),
            self.observation(
                "log_return",
                asset.clone(),
                float(options.aligned_price_return),
                AS_OF_NS,
                100,
                900_000,
                full_coverage(&self.sources),
                13,
            ),
            self.observation(
                "mark_index_divergence",
                instrument,
                float(0.01),
                AS_OF_NS,
                100,
                900_000,
                full_coverage(&self.sources),
                11,
            ),
            self.observation(
                "cascade_eligibility_gate",
                asset,
                boolean(options.cascade_eligible),
                AS_OF_NS,
                100,
                900_000,
                full_coverage(&self.sources),
                12,
            ),
        ];
        for (index, source) in self.sources.iter().enumerate() {
            let entity = FeatureEntity::Source(source.clone());
            observations.extend([
                self.observation(
                    "liquidation_source_completeness_flag",
                    entity.clone(),
                    integer(options.completeness_class),
                    AS_OF_NS,
                    100,
                    900_000,
                    full_coverage(&self.sources),
                    20 + index as u8,
                ),
                self.observation(
                    "feature_age_ns",
                    entity.clone(),
                    integer(if index == 0 { 200 } else { 500 }),
                    AS_OF_NS,
                    if index == 0 { 200 } else { 500 },
                    900_000,
                    full_coverage(&self.sources),
                    30 + index as u8,
                ),
                self.observation(
                    "stale_quote_duration_ns",
                    entity.clone(),
                    integer(if options.stale_book { 1_000 } else { 0 }),
                    AS_OF_NS,
                    100,
                    900_000,
                    full_coverage(&self.sources),
                    40 + index as u8,
                ),
                self.observation(
                    "source_outage_indicator",
                    entity,
                    boolean(outage),
                    AS_OF_NS,
                    100,
                    900_000,
                    full_coverage(&self.sources),
                    50 + index as u8,
                ),
            ]);
        }
        observations
    }

    #[allow(clippy::too_many_arguments)]
    fn observation(
        &self,
        id: &str,
        entity: FeatureEntity,
        datum: FeatureDatum,
        event_time_end_ns: i64,
        known_offset_ns: i64,
        quality_millionths: u32,
        source_coverage: SourceCoverage,
        lineage_seed: u8,
    ) -> FeatureObservation {
        let definition = definition(&self.registry, id);
        let window = if id == "log_return" {
            definition
                .windows()
                .iter()
                .find(|window| window.id().as_str() == "rolling_5m")
                .expect("five-minute aligned return window")
        } else {
            &definition.windows()[0]
        };
        let extent = match window.parameter() {
            WindowParameter::Time { extent, .. } => extent.value(),
            other => panic!("unexpected test window: {other:?}"),
        };
        let extent = i64::try_from(extent).expect("bounded extent");
        let as_known_at = event_time_end_ns + known_offset_ns;
        FeatureObservation::try_new(FeatureObservationInput {
            feature_id: definition.id().clone(),
            feature_version: definition.version().clone(),
            entity,
            window_id: window.id().clone(),
            resolution: definition.output_resolution(),
            value_type: datum
                .value()
                .map(FeatureValue::value_type)
                .unwrap_or(definition.value_type()),
            datum,
            event_time_start: UnixNanos::new(event_time_end_ns - extent),
            event_time_end: UnixNanos::new(event_time_end_ns),
            as_known_at: UnixNanos::new(as_known_at),
            computed_at: UnixNanos::new(as_known_at + 1),
            watermark: Some(UnixNanos::new(event_time_end_ns)),
            finality_as_known_at: UnixNanos::new(as_known_at),
            finality_state: FinalityState::Provisional,
            revision: ObservationRevision::new(1).expect("revision"),
            source_coverage,
            quality_score: QualityScore::from_millionths(quality_millionths).expect("quality"),
            normalization_version: definition.normalization().version().clone(),
            formula_hash: definition.formula_hash(),
            code_commit: CodeRevision::new("0123456789abcdef0123456789abcdef01234567")
                .expect("commit"),
            lineage_hash: LineageHash::new([lineage_seed; 32]).expect("lineage"),
        })
        .expect("observation")
    }
}

fn definition<'a>(registry: &'a FeatureRegistry, id: &str) -> &'a FeatureDefinition {
    let feature_id = feature_registry::FeatureId::new(id).expect("feature ID");
    let version = semver::Version::new(1, 0, 0);
    registry.get(&feature_id, &version).expect("definition")
}

fn float(value: f64) -> FeatureDatum {
    FeatureDatum::Present(FeatureValue::Float64(
        FiniteF64::new(value).expect("finite"),
    ))
}

fn decimal(value: &str) -> FeatureDatum {
    FeatureDatum::Present(FeatureValue::FixedDecimal(
        FixedDecimal::parse_canonical(value).expect("canonical decimal"),
    ))
}

const fn integer(value: i64) -> FeatureDatum {
    FeatureDatum::Present(FeatureValue::Integer(value))
}

const fn boolean(value: bool) -> FeatureDatum {
    FeatureDatum::Present(FeatureValue::Boolean(value))
}

fn source(name: &str) -> SourceId {
    SourceId::new(SourceKind::Exchange, name, 1).expect("source")
}

fn full_coverage(sources: &[SourceId]) -> SourceCoverage {
    SourceCoverage::try_new(
        sources
            .iter()
            .cloned()
            .map(|source| SourceCoverageEntry::new(source, SourceHealthState::Healthy))
            .collect(),
    )
    .expect("full coverage")
}

fn coverage_with_health(sources: &[SourceId], health: SourceHealthState) -> SourceCoverage {
    SourceCoverage::try_new(
        sources
            .iter()
            .cloned()
            .map(|source| SourceCoverageEntry::new(source, health))
            .collect(),
    )
    .expect("full coverage")
}

fn partial_coverage(sources: &[SourceId]) -> SourceCoverage {
    SourceCoverage::try_new_partial(
        sources.to_vec(),
        sources
            .iter()
            .take(1)
            .cloned()
            .map(|source| SourceCoverageEntry::new(source, SourceHealthState::Healthy))
            .collect(),
    )
    .expect("partial coverage")
}

fn assert_close(actual: f64, expected: f64) {
    assert!((actual - expected).abs() <= 1e-12, "{actual} != {expected}");
}

#[test]
fn selected_catalogue_roles_are_not_reinterpreted() {
    let fixture = Fixture::new();
    for id in [
        "bid_depth_change",
        "ask_depth_change",
        "cancellation_rate_per_second",
        "cancellation_to_trade_ratio",
        "trade_print_sweep_direction",
        "top_of_book_ofi",
        "cross_venue_median_absolute_dispersion",
        "liquidation_observed_velocity",
        "open_interest_relative_change",
        "log_return",
        "mark_index_divergence",
    ] {
        assert_eq!(
            definition(&fixture.registry, id).consumption_role(),
            FeatureConsumptionRole::ModelEligible
        );
    }
    for id in [
        "liquidation_source_completeness_flag",
        "feature_age_ns",
        "stale_quote_duration_ns",
        "source_outage_indicator",
    ] {
        assert_eq!(
            definition(&fixture.registry, id).consumption_role(),
            FeatureConsumptionRole::UncertaintyOnly
        );
    }
    assert_eq!(
        definition(&fixture.registry, "cascade_eligibility_gate").consumption_role(),
        FeatureConsumptionRole::GatingOnly
    );
    assert_eq!(
        definition(&fixture.registry, "top_of_book_ofi").value_type(),
        FeatureValueType::FixedDecimal
    );
}
