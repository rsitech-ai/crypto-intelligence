use scenarios::{
    CrossingDirection, EmpiricalInnovations, EventThreshold, ImpactModel, ImpactParameters,
    LiquiditySnapshot, MarketProcess, OptionsCondition, ProcessParameters, ScenarioCondition,
    ScenarioConditionInput, ScenarioConfig, ScenarioConfigInput, ScenarioEngine, SimulationLabel,
    StressInjection,
};

fn engine() -> ScenarioEngine {
    let innovations = EmpiricalInnovations::try_new(
        vec![-5.0, -2.0, -1.0, -0.5, 0.0, 0.5, 1.0, 2.0, 5.0],
        vec![-0.12, -0.08, 0.06],
        [1; 32],
    )
    .unwrap();
    let process = MarketProcess::try_new(
        ProcessParameters {
            calibration_step_seconds: 60,
            long_run_volatility_annualized: 0.70,
            volatility_persistence: 0.95,
            volatility_of_volatility: 0.08,
            base_jump_probability_per_step: 0.02,
            regime_jump_multiplier: 1.5,
            transition_jump_multiplier: 1.0,
            cusp_jump_multiplier: 0.5,
            systemic_correlation: 0.35,
            minimum_volatility_annualized: 0.05,
            maximum_volatility_annualized: 4.0,
            maximum_absolute_step_return: 0.35,
        },
        innovations,
        "scenario-process-v1",
        [2; 32],
    )
    .unwrap();
    let impact = ImpactModel::try_new(
        ImpactParameters {
            square_root_impact_bps: 8.0,
            fragility_multiplier: 2.0,
            volatility_multiplier: 0.5,
            maximum_sweep_cost_bps: 2_500.0,
            liquidation_trigger_return: 0.01,
            liquidation_sensitivity: 0.20,
            liquidation_price_feedback: 0.50,
        },
        "scenario-impact-v1",
        [3; 32],
    )
    .unwrap();
    ScenarioEngine::try_new(process, impact).unwrap()
}

fn condition(liquidity_fraction: f64, options: OptionsCondition) -> ScenarioCondition {
    ScenarioCondition::try_new(ScenarioConditionInput {
        as_of_ns: 1_000,
        as_known_at_ns: 900,
        entity_id: "btc".to_owned(),
        starting_price: 100.0,
        starting_volatility_annualized: 0.80,
        regime_probability: 0.70,
        calibrated_transition_probability: 0.25,
        cusp_instability: 0.60,
        fast_stress: 0.40,
        systemic_return_condition: -0.01,
        liquidity: LiquiditySnapshot::try_new(
            20_000_000.0 * liquidity_fraction,
            1.0 - liquidity_fraction,
            1_000_000_000.0,
            900,
            [4; 32],
        )
        .unwrap(),
        options,
        forecast_evidence_hash: [5; 32],
        applicability_evidence_hash: [6; 32],
        source_evidence_hash: [7; 32],
        model_package_ids: vec!["hazard-v1".to_owned(), "calibration-v1".to_owned()],
    })
    .unwrap()
}

fn config(seed: u64) -> ScenarioConfig {
    ScenarioConfig::try_new(ScenarioConfigInput {
        seed,
        path_count: 512,
        step_seconds: 60,
        horizons_seconds: vec![300, 900],
        thresholds: vec![
            EventThreshold::try_new("downside_5pct", CrossingDirection::Below, -0.05).unwrap(),
            EventThreshold::try_new("upside_5pct", CrossingDirection::Above, 0.05).unwrap(),
        ],
        sweep_notionals_usd: vec![100_000.0, 1_000_000.0],
        representative_quantiles: vec![0.01, 0.50, 0.99],
        liquidation_amplification: true,
        stress_injections: vec![],
    })
    .unwrap()
}

fn missing_options() -> OptionsCondition {
    OptionsCondition::unavailable("source_not_configured").unwrap()
}

#[test]
fn fixed_seed_produces_identical_summary_and_path_digest() {
    let engine = engine();
    let condition = condition(1.0, missing_options());
    let base_config = config(42);
    let left = engine.simulate(&condition, &base_config).unwrap();
    let repeated = engine.simulate(&condition, &base_config).unwrap();
    let alternate = engine.simulate(&condition, &config(43)).unwrap();

    assert_eq!(left, repeated);
    assert_ne!(left.path_digest, alternate.path_digest);
    left.verify(&engine, &condition, &base_config).unwrap();
}

#[test]
fn worse_liquidity_increases_downside_and_sweep_cost_tail() {
    let engine = engine();
    let config = config(17);
    let liquid = engine
        .simulate(&condition(1.0, missing_options()), &config)
        .unwrap();
    let fragile = engine
        .simulate(&condition(0.2, missing_options()), &config)
        .unwrap();
    let liquid_horizon = liquid.horizon(900).unwrap();
    let fragile_horizon = fragile.horizon(900).unwrap();

    assert!(
        fragile_horizon.return_quantile(0.01).unwrap()
            < liquid_horizon.return_quantile(0.01).unwrap()
    );
    assert!(
        fragile_horizon.sweep_cost(1_000_000.0).unwrap().tail_bps
            > liquid_horizon.sweep_cost(1_000_000.0).unwrap().tail_bps
    );
}

#[test]
fn summaries_are_bounded_and_paths_are_explicitly_simulated() {
    let result = engine()
        .simulate(&condition(0.8, missing_options()), &config(9))
        .unwrap();

    assert!(!result.assumptions.options_used);
    assert_eq!(
        result.assumptions.options_unavailable_reason.as_deref(),
        Some("source_not_configured")
    );
    assert!(
        result
            .representative_paths
            .iter()
            .all(|path| path.label == SimulationLabel::Simulated)
    );
    for horizon in &result.horizons {
        assert!(horizon.return_quantiles.windows(2).all(|pair| {
            pair[0].probability < pair[1].probability && pair[0].value <= pair[1].value
        }));
        assert!(horizon.volatility_quantiles.windows(2).all(|pair| {
            pair[0].probability < pair[1].probability && pair[0].value <= pair[1].value
        }));
        assert!(horizon.threshold_probabilities.iter().all(|value| {
            (0.0..=1.0).contains(&value.probability)
                && value.crossing_count <= result.path_count
                && value.probability
                    == f64::from(value.crossing_count) / f64::from(result.path_count)
        }));
        assert!(horizon.sweep_costs.iter().all(|value| {
            value.expected_bps.is_finite()
                && value.expected_bps >= 0.0
                && value.tail_bps.is_finite()
                && value.tail_bps >= 0.0
        }));
        for range in [
            horizon.liquidation_notional_range,
            horizon.open_interest_range,
        ] {
            assert!(range.minimum <= range.lower);
            assert!(range.lower <= range.median);
            assert!(range.median <= range.upper);
            assert!(range.upper <= range.maximum);
        }
    }
    for path in &result.representative_paths {
        assert_eq!(path.points.len(), 15);
        for (index, point) in path.points.iter().enumerate() {
            assert_eq!(point.elapsed_seconds, (index as u64 + 1) * 60);
            assert!(point.simulated_price.is_finite() && point.simulated_price > 0.0);
            let reconstructed = 100.0 * (1.0 + point.price_return);
            assert!((point.simulated_price - reconstructed).abs() <= 1e-10);
        }
    }
}

#[test]
fn signed_options_skew_and_systemic_condition_have_expected_direction() {
    let engine = engine();
    let config = config(123);
    let negative_skew = engine
        .simulate(
            &condition(
                1.0,
                OptionsCondition::available(1.0, -1.0, 900, [8; 32]).unwrap(),
            ),
            &config,
        )
        .unwrap();
    let positive_skew = engine
        .simulate(
            &condition(
                1.0,
                OptionsCondition::available(1.0, 1.0, 900, [8; 32]).unwrap(),
            ),
            &config,
        )
        .unwrap();
    assert!(
        negative_skew
            .horizon(900)
            .unwrap()
            .return_quantile(0.01)
            .unwrap()
            < positive_skew
                .horizon(900)
                .unwrap()
                .return_quantile(0.01)
                .unwrap()
    );

    let mut downside_input = condition(1.0, missing_options()).as_input();
    downside_input.systemic_return_condition = -0.05;
    let downside = engine
        .simulate(
            &ScenarioCondition::try_new(downside_input).unwrap(),
            &config,
        )
        .unwrap();
    let mut upside_input = condition(1.0, missing_options()).as_input();
    upside_input.systemic_return_condition = 0.05;
    let upside = engine
        .simulate(&ScenarioCondition::try_new(upside_input).unwrap(), &config)
        .unwrap();
    assert!(
        downside
            .horizon(900)
            .unwrap()
            .return_quantile(0.50)
            .unwrap()
            < upside.horizon(900).unwrap().return_quantile(0.50).unwrap()
    );
}

#[test]
fn stress_liquidation_and_options_branches_are_identity_significant() {
    let engine = engine();
    let base_condition = condition(0.5, missing_options());
    let base_config = config(77);
    let base = engine.simulate(&base_condition, &base_config).unwrap();

    let mut stressed_input = base_config.as_input();
    stressed_input.stress_injections =
        vec![StressInjection::try_new("downside_shock", 2, 5, -0.02, 1.5, 0.5, 0.10).unwrap()];
    let stressed_config = ScenarioConfig::try_new(stressed_input).unwrap();
    let stressed = engine.simulate(&base_condition, &stressed_config).unwrap();
    assert!(
        stressed
            .horizon(900)
            .unwrap()
            .return_quantile(0.50)
            .unwrap()
            < base.horizon(900).unwrap().return_quantile(0.50).unwrap()
    );

    let mut no_feedback_input = base_config.as_input();
    no_feedback_input.liquidation_amplification = false;
    let no_feedback = engine
        .simulate(
            &base_condition,
            &ScenarioConfig::try_new(no_feedback_input).unwrap(),
        )
        .unwrap();
    assert_ne!(base.path_digest, no_feedback.path_digest);

    let options = OptionsCondition::available(1.25, -0.20, 900, [8; 32]).unwrap();
    let with_options = engine
        .simulate(&condition(0.5, options), &base_config)
        .unwrap();
    assert!(with_options.assumptions.options_used);
    assert_ne!(base.path_digest, with_options.path_digest);
}

#[test]
fn invalid_time_numeric_geometry_and_capacity_fail_closed() {
    assert!(OptionsCondition::available(f64::NAN, 0.0, 900, [8; 32]).is_err());
    assert!(LiquiditySnapshot::try_new(0.0, 0.5, 1.0, 900, [4; 32]).is_err());

    let mut invalid = config(1).as_input();
    invalid.path_count = u32::MAX;
    assert!(ScenarioConfig::try_new(invalid).is_err());

    let mut invalid = config(1).as_input();
    invalid.horizons_seconds = vec![301];
    assert!(ScenarioConfig::try_new(invalid).is_err());

    let mut wrong_step = config(1).as_input();
    wrong_step.step_seconds = 30;
    wrong_step.horizons_seconds = vec![300, 900];
    let wrong_step = ScenarioConfig::try_new(wrong_step).unwrap();
    assert_eq!(
        engine().simulate(&condition(1.0, missing_options()), &wrong_step),
        Err(scenarios::ScenarioError::InvalidConfig)
    );

    let mut invalid_condition = condition(1.0, missing_options()).as_input();
    invalid_condition.as_known_at_ns = invalid_condition.as_of_ns + 1;
    assert!(ScenarioCondition::try_new(invalid_condition).is_err());

    let mut future_component = condition(1.0, missing_options()).as_input();
    future_component.liquidity =
        LiquiditySnapshot::try_new(1_000_000.0, 0.5, 2_000_000.0, 901, [9; 32]).unwrap();
    assert!(ScenarioCondition::try_new(future_component).is_err());

    let mut future_options = condition(1.0, missing_options()).as_input();
    future_options.options = OptionsCondition::available(1.0, 0.0, 901, [9; 32]).unwrap();
    assert!(ScenarioCondition::try_new(future_options).is_err());
}
