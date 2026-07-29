use labels::{
    DefinitionScope, EventType, ExclusionReason, JumpTreatment, LabelDefinition,
    LabelDefinitionInput, LabelEngine, LabelError, LabelOutcome, LabelPath, LabelRule,
    MarketEligibility, MarketFrameInput, OverlapPolicy, PriceThreshold, ResolvedLabels,
    VolatilityEstimator, fixtures, resolve_competing,
};
use quality::SourceHealthState;
use semver::Version;

#[test]
fn first_passage_uses_log_return_and_first_threshold_crossing() {
    // ln(95.1 / 100) < -0.05, although the simple return is greater than -5%.
    let path = fixtures::price_path([100.0, 99.0, 95.1, 94.0]).expect("price fixture");
    let outcome = LabelEngine::downside_fixture(0.05)
        .expect("downside fixture engine")
        .label(&path)
        .expect("valid downside label");
    assert_eq!(
        outcome,
        LabelOutcome::Occurred {
            offset_seconds: 120
        }
    );
}

#[test]
fn outage_or_quality_loss_before_horizon_is_right_censored() {
    let path = fixtures::censored_path().expect("censored fixture");
    assert_eq!(
        LabelEngine::downside_fixture(0.05)
            .expect("downside fixture engine")
            .label(&path)
            .expect("censored path is a valid label result"),
        LabelOutcome::Censored {
            observed_seconds: 60
        }
    );

    let quality_loss = LabelPath::try_new(vec![
        frame(0, 100.0),
        frame(60, 99.0),
        frame(120, 94.0).with_quality(899_999, 1_000_000),
        frame(180, 94.0),
    ])
    .expect("structurally valid quality-loss path");
    assert_eq!(
        LabelEngine::downside_fixture(0.05)
            .expect("downside fixture engine")
            .label(&quality_loss)
            .expect("quality loss is explicit"),
        LabelOutcome::Censored {
            observed_seconds: 60
        }
    );
}

#[test]
fn price_thresholds_use_only_point_in_time_origin_scale_and_combined_is_stricter() {
    let path = LabelPath::try_new(vec![
        frame(0, 100.0).with_volatility(0.02),
        frame(60, 97.0).with_volatility(0.50),
        frame(120, 95.0).with_volatility(0.50),
        frame(180, 94.0).with_volatility(0.50),
    ])
    .expect("valid point-in-time path");
    let scaled = LabelEngine::try_new(definition(
        "downside_vol_scaled",
        LabelRule::Downside {
            threshold: PriceThreshold::VolatilityScaled { multiple: 3.0 },
        },
        0,
    ))
    .expect("valid label engine");
    assert_eq!(
        scaled.label(&path).expect("valid label"),
        LabelOutcome::Occurred {
            offset_seconds: 180
        }
    );

    let combined_path = LabelPath::try_new(vec![
        frame(0, 100.0).with_volatility(0.03),
        frame(60, 94.5).with_volatility(0.50),
        frame(120, 94.0).with_volatility(0.50),
        frame(180, 94.0).with_volatility(0.50),
    ])
    .expect("combined path");
    let combined = LabelEngine::try_new(definition(
        "downside_combined",
        LabelRule::Downside {
            threshold: PriceThreshold::Combined {
                absolute_floor_fraction: 0.05,
                volatility_multiple: 2.0,
            },
        },
        0,
    ))
    .expect("combined engine");
    assert_eq!(
        combined.label(&combined_path).expect("combined label"),
        LabelOutcome::Occurred {
            offset_seconds: 120
        }
    );
}

#[test]
fn volatility_and_systemic_liquidity_events_require_declared_evidence_and_persistence() {
    let persistent = LabelPath::try_new(vec![
        frame(0, 100.0)
            .with_volatility(0.02)
            .with_liquidity(10.0, 100.0)
            .with_liquidity_mechanism(10.0, 5.0, 2.0)
            .with_sources(2),
        frame(60, 100.0)
            .with_volatility(0.05)
            .with_liquidity(30.0, 30.0)
            .with_liquidity_mechanism(30.0, 15.0, 6.0)
            .with_sources(2),
        frame(120, 100.0)
            .with_volatility(0.05)
            .with_liquidity(30.0, 30.0)
            .with_liquidity_mechanism(30.0, 15.0, 6.0)
            .with_sources(2),
        frame(180, 100.0)
            .with_volatility(0.02)
            .with_liquidity(10.0, 100.0)
            .with_liquidity_mechanism(10.0, 5.0, 2.0)
            .with_sources(2),
    ])
    .expect("valid point-in-time path");
    let volatility = LabelEngine::try_new(definition(
        "volatility_explosion",
        LabelRule::VolatilityExplosion {
            estimator: VolatilityEstimator::LogReturnRms,
            sampling_resolution_seconds: 60,
            reference_distribution_hash: [7; 32],
            percentile_millionths: 950_000,
            reference_threshold: 0.02,
            minimum_multiple: 2.0,
            jump_treatment: JumpTreatment::Included,
        },
        60,
    ))
    .expect("volatility engine");
    let liquidity = LabelEngine::try_new(definition(
        "liquidity_vacuum",
        LabelRule::LiquidityVacuum {
            minimum_spread_multiple: 2.0,
            maximum_depth_fraction: 0.5,
            minimum_cancellation_multiple: 2.0,
            minimum_sweep_cost_multiple: 2.0,
            minimum_resiliency_multiple: 2.0,
            minimum_corroborating_sources: 2,
        },
        60,
    ))
    .expect("liquidity engine");

    assert_eq!(
        volatility.label(&persistent).expect("volatility label"),
        LabelOutcome::Occurred { offset_seconds: 60 }
    );
    assert_eq!(
        liquidity.label(&persistent).expect("liquidity label"),
        LabelOutcome::Occurred { offset_seconds: 60 }
    );

    let spread_only = LabelPath::try_new(vec![
        frame(0, 100.0)
            .with_liquidity(10.0, 100.0)
            .with_liquidity_mechanism(10.0, 5.0, 2.0)
            .with_sources(2),
        frame(60, 100.0)
            .with_liquidity(30.0, 90.0)
            .with_liquidity_mechanism(30.0, 15.0, 6.0)
            .with_sources(2),
        frame(120, 100.0)
            .with_liquidity(30.0, 90.0)
            .with_liquidity_mechanism(30.0, 15.0, 6.0)
            .with_sources(2),
        frame(180, 100.0)
            .with_liquidity(10.0, 100.0)
            .with_liquidity_mechanism(10.0, 5.0, 2.0)
            .with_sources(2),
    ])
    .expect("valid spread-only path");
    assert_eq!(
        liquidity.label(&spread_only).expect("liquidity label"),
        LabelOutcome::NotOccurred
    );
}

#[test]
fn liquidation_cascade_requires_all_conditions_and_records_confidence() {
    let cascade = LabelEngine::try_new(definition(
        "liquidation_cascade",
        LabelRule::LiquidationCascade {
            price_threshold: PriceThreshold::VolatilityScaled { multiple: 2.5 },
            minimum_liquidation_notional: 1_000.0,
            minimum_open_interest_drop_fraction: 0.08,
            minimum_spread_multiple: 2.0,
            maximum_depth_fraction: 0.5,
            minimum_corroborating_sources: 2,
            minimum_confidence_millionths: 800_000,
        },
        60,
    ))
    .expect("cascade engine");
    let complete = LabelPath::try_new(vec![
        frame(0, 100.0)
            .with_volatility(0.02)
            .with_liquidity(10.0, 100.0)
            .with_derivatives(0.0, 100.0, 2, 900_000),
        frame(60, 94.0)
            .with_volatility(0.02)
            .with_liquidity(30.0, 40.0)
            .with_derivatives(1_500.0, 90.0, 2, 900_000),
        frame(120, 94.0)
            .with_volatility(0.02)
            .with_liquidity(30.0, 40.0)
            .with_derivatives(1_500.0, 90.0, 2, 900_000),
        frame(180, 94.0)
            .with_volatility(0.02)
            .with_liquidity(30.0, 40.0)
            .with_derivatives(1_500.0, 90.0, 2, 900_000),
    ])
    .expect("complete cascade path");
    let evaluation = cascade
        .evaluate_at_horizon(&complete, 180)
        .expect("cascade evaluation");
    assert_eq!(
        evaluation.outcome,
        LabelOutcome::Occurred { offset_seconds: 60 }
    );
    assert_eq!(evaluation.outcome_known_at_offset_seconds, 120);
    assert_eq!(evaluation.confidence_millionths, 900_000);
    assert_eq!(
        evaluation.definition_hash,
        cascade.definition().definition_hash()
    );

    let high_confidence_only = LabelEngine::try_new(definition(
        "high_confidence_cascade",
        LabelRule::LiquidationCascade {
            price_threshold: PriceThreshold::VolatilityScaled { multiple: 2.5 },
            minimum_liquidation_notional: 1_000.0,
            minimum_open_interest_drop_fraction: 0.08,
            minimum_spread_multiple: 2.0,
            maximum_depth_fraction: 0.5,
            minimum_corroborating_sources: 2,
            minimum_confidence_millionths: 950_000,
        },
        60,
    ))
    .expect("higher-confidence engine");
    assert_eq!(
        high_confidence_only
            .label(&complete)
            .expect("explicit confidence exclusion"),
        LabelOutcome::Excluded(ExclusionReason::InsufficientConfidence)
    );

    let incomplete = LabelPath::try_new(vec![
        frame(0, 100.0)
            .with_volatility(0.02)
            .with_liquidity(10.0, 100.0)
            .with_derivatives(0.0, 100.0, 1, 700_000),
        frame(60, 94.0)
            .with_volatility(0.02)
            .with_liquidity(30.0, 40.0)
            .with_derivatives(1_500.0, 90.0, 1, 700_000),
        frame(120, 94.0)
            .with_volatility(0.02)
            .with_liquidity(30.0, 40.0)
            .with_derivatives(1_500.0, 90.0, 1, 700_000),
        frame(180, 94.0)
            .with_volatility(0.02)
            .with_liquidity(30.0, 40.0)
            .with_derivatives(1_500.0, 90.0, 1, 700_000),
    ])
    .expect("structurally valid incomplete path");
    assert_eq!(
        cascade.label(&incomplete).expect("explicit exclusion"),
        LabelOutcome::Excluded(ExclusionReason::InsufficientCorroboration)
    );
}

#[test]
fn unhealthy_or_low_quality_origin_is_excluded_and_future_known_input_is_censored() {
    let unhealthy = LabelPath::try_new(vec![
        frame(0, 100.0).with_health(SourceHealthState::Unhealthy),
        frame(180, 90.0),
    ])
    .expect("structurally valid unhealthy path");
    assert_eq!(
        LabelEngine::downside_fixture(0.05)
            .expect("downside fixture engine")
            .label(&unhealthy)
            .expect("explicit exclusion"),
        LabelOutcome::Excluded(ExclusionReason::UnhealthyOrigin)
    );

    let low_quality = LabelPath::try_new(vec![
        frame(0, 100.0).with_quality(899_999, 1_000_000),
        frame(180, 90.0),
    ])
    .expect("low-quality origin");
    assert_eq!(
        LabelEngine::downside_fixture(0.05)
            .expect("downside fixture engine")
            .label(&low_quality)
            .expect("explicit exclusion"),
        LabelOutcome::Excluded(ExclusionReason::LowQualityOrigin)
    );

    let mut future_known = frame(60, 99.0);
    future_known.known_at_offset_seconds = 181;
    let future_path = LabelPath::try_new(vec![
        frame(0, 100.0),
        future_known,
        frame(120, 99.0),
        frame(180, 99.0),
    ])
    .expect_err("knowledge time regression must fail closed");
    assert_eq!(future_path, LabelError::InvalidChronology);

    let future_only = LabelPath::try_new(vec![
        frame(0, 100.0),
        future_known,
        MarketFrameInput {
            known_at_offset_seconds: 181,
            ..frame(120, 99.0)
        },
        MarketFrameInput {
            known_at_offset_seconds: 181,
            ..frame(180, 99.0)
        },
    ])
    .expect("monotonic delayed knowledge path");
    assert_eq!(
        LabelEngine::downside_fixture(0.05)
            .expect("downside fixture engine")
            .label(&future_only)
            .expect("future-known evidence censors"),
        LabelOutcome::Censored {
            observed_seconds: 0
        }
    );
}

#[test]
fn production_v1_horizons_are_sealed_while_research_horizons_are_configurable() {
    let mut production = definition(
        "production_downside",
        LabelRule::Downside {
            threshold: PriceThreshold::Combined {
                absolute_floor_fraction: 0.05,
                volatility_multiple: 2.0,
            },
        },
        60,
    );
    production.scope = DefinitionScope::ProductionV1;
    production.horizons_seconds = vec![900, 3_600, 14_400, 86_400];
    assert!(LabelDefinition::try_new(production.clone()).is_ok());

    production.horizons_seconds = vec![1_800];
    assert_eq!(
        LabelDefinition::try_new(production),
        Err(LabelError::InvalidDefinition)
    );
}

#[test]
fn market_state_exclusions_are_explicit_and_not_silent_negatives() {
    for (eligibility, reason) in [
        (
            MarketEligibility::HaltedOrDelisted,
            ExclusionReason::HaltedOrDelisted,
        ),
        (
            MarketEligibility::InstrumentDefinitionChanged,
            ExclusionReason::InstrumentDefinitionChanged,
        ),
        (
            MarketEligibility::UnresolvedCorrection,
            ExclusionReason::UnresolvedCorrection,
        ),
        (
            MarketEligibility::TimestampIntegrityCompromised,
            ExclusionReason::TimestampIntegrityCompromised,
        ),
    ] {
        let path = LabelPath::try_new(vec![
            frame(0, 100.0).with_eligibility(eligibility),
            frame(180, 90.0),
        ])
        .expect("structurally valid excluded path");
        assert_eq!(
            LabelEngine::downside_fixture(0.05)
                .expect("engine")
                .label(&path)
                .expect("typed exclusion"),
            LabelOutcome::Excluded(reason)
        );
    }
}

#[test]
fn overlap_policy_preserves_raw_labels_and_uses_documented_v1_priority() {
    let outcomes = [
        (
            EventType::Downside,
            LabelOutcome::Occurred { offset_seconds: 60 },
        ),
        (
            EventType::LiquidityVacuum,
            LabelOutcome::Occurred { offset_seconds: 60 },
        ),
        (
            EventType::VolatilityExplosion,
            LabelOutcome::Occurred {
                offset_seconds: 120,
            },
        ),
    ];
    assert_eq!(
        resolve_competing(&outcomes, OverlapPolicy::AllowCompeting),
        ResolvedLabels::Multiple(vec![
            (EventType::LiquidityVacuum, 60),
            (EventType::Downside, 60),
            (EventType::VolatilityExplosion, 120),
        ])
    );
    assert_eq!(
        resolve_competing(&outcomes, OverlapPolicy::EarliestWins),
        ResolvedLabels::Primary {
            event_type: EventType::LiquidityVacuum,
            offset_seconds: 60,
            simultaneous: vec![EventType::Downside],
        }
    );
    assert_eq!(
        resolve_competing(&outcomes, OverlapPolicy::ExcludeTies),
        ResolvedLabels::Excluded(ExclusionReason::SimultaneousCompetingEvents)
    );
}

#[test]
fn every_semantic_definition_change_changes_the_hash_and_invalid_numbers_fail_closed() {
    let base = definition(
        "downside_absolute",
        LabelRule::Downside {
            threshold: PriceThreshold::AbsoluteFraction(0.05),
        },
        0,
    );
    let mut mutations = Vec::new();
    let mut changed = base.clone();
    changed.id = "downside_changed".to_owned();
    mutations.push(changed);
    let mut changed = base.clone();
    changed.version = Version::new(1, 1, 0);
    mutations.push(changed);
    let mut changed = base.clone();
    changed.rule = LabelRule::Downside {
        threshold: PriceThreshold::AbsoluteFraction(0.06),
    };
    mutations.push(changed);
    let mut changed = base.clone();
    changed.horizons_seconds = vec![120];
    mutations.push(changed);
    let mut changed = base.clone();
    changed.minimum_duration_seconds = 60;
    mutations.push(changed);
    let mut changed = base.clone();
    changed.minimum_quality_millionths = 950_000;
    mutations.push(changed);
    let mut changed = base.clone();
    changed.minimum_source_coverage_millionths = 950_000;
    mutations.push(changed);
    let mut changed = base.clone();
    changed.overlap_policy = OverlapPolicy::ExcludeTies;
    mutations.push(changed);
    let mut scope_base = base.clone();
    scope_base.horizons_seconds = vec![900, 3_600, 14_400, 86_400];
    let mut changed_scope = scope_base.clone();
    changed_scope.scope = DefinitionScope::ProductionV1;
    assert_ne!(
        LabelDefinition::try_new(scope_base)
            .expect("research definition")
            .definition_hash(),
        LabelDefinition::try_new(changed_scope)
            .expect("production definition")
            .definition_hash()
    );

    let base_definition = LabelDefinition::try_new(base.clone()).expect("base definition");
    assert_eq!(
        base_definition.definition_hash(),
        [
            221, 112, 88, 74, 156, 7, 6, 38, 191, 161, 68, 162, 40, 109, 117, 144, 58, 19, 121,
            230, 54, 60, 250, 34, 78, 19, 47, 245, 21, 209, 195, 198,
        ]
    );
    for changed in mutations {
        assert_ne!(
            base_definition.definition_hash(),
            LabelDefinition::try_new(changed)
                .expect("changed definition")
                .definition_hash()
        );
    }

    let mut invalid = base;
    invalid.rule = LabelRule::Downside {
        threshold: PriceThreshold::AbsoluteFraction(f64::NAN),
    };
    assert_eq!(
        LabelDefinition::try_new(invalid),
        Err(LabelError::InvalidDefinition)
    );
}

#[test]
fn every_compound_rule_field_participates_in_definition_identity() {
    let volatility_base = volatility_rule(
        VolatilityEstimator::LogReturnRms,
        60,
        [1; 32],
        950_000,
        0.02,
        2.0,
        JumpTreatment::Included,
    );
    assert_rule_hash_changes(
        volatility_base,
        [
            volatility_rule(
                VolatilityEstimator::RealizedVariance,
                60,
                [1; 32],
                950_000,
                0.02,
                2.0,
                JumpTreatment::Included,
            ),
            volatility_rule(
                VolatilityEstimator::LogReturnRms,
                300,
                [1; 32],
                950_000,
                0.02,
                2.0,
                JumpTreatment::Included,
            ),
            volatility_rule(
                VolatilityEstimator::LogReturnRms,
                60,
                [2; 32],
                950_000,
                0.02,
                2.0,
                JumpTreatment::Included,
            ),
            volatility_rule(
                VolatilityEstimator::LogReturnRms,
                60,
                [1; 32],
                975_000,
                0.02,
                2.0,
                JumpTreatment::Included,
            ),
            volatility_rule(
                VolatilityEstimator::LogReturnRms,
                60,
                [1; 32],
                950_000,
                0.03,
                2.0,
                JumpTreatment::Included,
            ),
            volatility_rule(
                VolatilityEstimator::LogReturnRms,
                60,
                [1; 32],
                950_000,
                0.02,
                3.0,
                JumpTreatment::Included,
            ),
            volatility_rule(
                VolatilityEstimator::LogReturnRms,
                60,
                [1; 32],
                950_000,
                0.02,
                2.0,
                JumpTreatment::Separated,
            ),
        ],
    );

    let liquidity_base = liquidity_rule(2.0, 0.5, 2.0, 2.0, 2.0, 2);
    assert_rule_hash_changes(
        liquidity_base,
        [
            liquidity_rule(3.0, 0.5, 2.0, 2.0, 2.0, 2),
            liquidity_rule(2.0, 0.4, 2.0, 2.0, 2.0, 2),
            liquidity_rule(2.0, 0.5, 3.0, 2.0, 2.0, 2),
            liquidity_rule(2.0, 0.5, 2.0, 3.0, 2.0, 2),
            liquidity_rule(2.0, 0.5, 2.0, 2.0, 3.0, 2),
            liquidity_rule(2.0, 0.5, 2.0, 2.0, 2.0, 3),
        ],
    );

    let cascade_base = cascade_rule(2.0, 1_000.0, 0.08, 2.0, 0.5, 2, 800_000);
    assert_rule_hash_changes(
        cascade_base,
        [
            cascade_rule(3.0, 1_000.0, 0.08, 2.0, 0.5, 2, 800_000),
            cascade_rule(2.0, 2_000.0, 0.08, 2.0, 0.5, 2, 800_000),
            cascade_rule(2.0, 1_000.0, 0.10, 2.0, 0.5, 2, 800_000),
            cascade_rule(2.0, 1_000.0, 0.08, 3.0, 0.5, 2, 800_000),
            cascade_rule(2.0, 1_000.0, 0.08, 2.0, 0.4, 2, 800_000),
            cascade_rule(2.0, 1_000.0, 0.08, 2.0, 0.5, 3, 800_000),
            cascade_rule(2.0, 1_000.0, 0.08, 2.0, 0.5, 2, 900_000),
        ],
    );
}

#[test]
fn malformed_frames_and_paths_fail_closed() {
    let mut impossible = frame(60, f64::INFINITY);
    impossible.known_at_offset_seconds = 59;
    assert_eq!(
        LabelPath::try_new(vec![frame(0, 100.0), impossible]),
        Err(LabelError::PreEventKnowledge)
    );

    assert_eq!(
        LabelPath::try_new(vec![frame(0, 100.0), frame(60, 99.0), frame(121, 98.0)]),
        Err(LabelError::InvalidChronology)
    );
}

fn definition(id: &str, rule: LabelRule, minimum_duration_seconds: u64) -> LabelDefinitionInput {
    LabelDefinitionInput {
        id: id.to_owned(),
        version: Version::new(1, 0, 0),
        scope: DefinitionScope::Research,
        rule,
        horizons_seconds: vec![180],
        minimum_duration_seconds,
        minimum_quality_millionths: 900_000,
        minimum_source_coverage_millionths: 900_000,
        overlap_policy: OverlapPolicy::EarliestWins,
    }
}

fn assert_rule_hash_changes<const N: usize>(base_rule: LabelRule, mutations: [LabelRule; N]) {
    let base_hash = LabelDefinition::try_new(definition("rule_hash", base_rule, 60))
        .expect("base rule")
        .definition_hash();
    for mutation in mutations {
        assert_ne!(
            base_hash,
            LabelDefinition::try_new(definition("rule_hash", mutation, 60))
                .expect("mutated rule")
                .definition_hash()
        );
    }
}

fn volatility_rule(
    estimator: VolatilityEstimator,
    sampling_resolution_seconds: u64,
    reference_distribution_hash: [u8; 32],
    percentile_millionths: u32,
    reference_threshold: f64,
    minimum_multiple: f64,
    jump_treatment: JumpTreatment,
) -> LabelRule {
    LabelRule::VolatilityExplosion {
        estimator,
        sampling_resolution_seconds,
        reference_distribution_hash,
        percentile_millionths,
        reference_threshold,
        minimum_multiple,
        jump_treatment,
    }
}

fn liquidity_rule(
    minimum_spread_multiple: f64,
    maximum_depth_fraction: f64,
    minimum_cancellation_multiple: f64,
    minimum_sweep_cost_multiple: f64,
    minimum_resiliency_multiple: f64,
    minimum_corroborating_sources: u32,
) -> LabelRule {
    LabelRule::LiquidityVacuum {
        minimum_spread_multiple,
        maximum_depth_fraction,
        minimum_cancellation_multiple,
        minimum_sweep_cost_multiple,
        minimum_resiliency_multiple,
        minimum_corroborating_sources,
    }
}

fn cascade_rule(
    volatility_multiple: f64,
    minimum_liquidation_notional: f64,
    minimum_open_interest_drop_fraction: f64,
    minimum_spread_multiple: f64,
    maximum_depth_fraction: f64,
    minimum_corroborating_sources: u32,
    minimum_confidence_millionths: u32,
) -> LabelRule {
    LabelRule::LiquidationCascade {
        price_threshold: PriceThreshold::VolatilityScaled {
            multiple: volatility_multiple,
        },
        minimum_liquidation_notional,
        minimum_open_interest_drop_fraction,
        minimum_spread_multiple,
        maximum_depth_fraction,
        minimum_corroborating_sources,
        minimum_confidence_millionths,
    }
}

fn frame(offset_seconds: u64, price: f64) -> MarketFrameInput {
    MarketFrameInput::price(offset_seconds, price)
}
