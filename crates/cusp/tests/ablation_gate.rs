use cusp::ablation::{
    ABLATION_REPORT_SCHEMA_VERSION, AblationError, AblationReport, AblationReportInput, CuspGate,
    FoldMetricInput, GateCheckKind, GateDecision, RequiredBaseline,
};

fn passing_fold(index: usize, regime_id: &str, candidate_brier_score: f64) -> FoldMetricInput {
    FoldMetricInput {
        fold_id: format!("outer-{index}"),
        regime_id: regime_id.to_owned(),
        test_start_ns: 1_800_000_000_000_000_000 + index as i64 * 1_000_000_000,
        test_end_ns: 1_800_000_000_500_000_000 + index as i64 * 1_000_000_000,
        observation_count: 1_000,
        positive_event_count: 10,
        independent_event_episode_count: 2,
        baseline_brier_score: 0.20,
        candidate_brier_score,
        calibration_slope: 1.0,
        calibration_intercept: 0.01,
        expected_calibration_error: 0.02,
        alert_budget_score_delta: 0.03,
    }
}

fn passing_input() -> AblationReportInput {
    AblationReportInput {
        schema_version: ABLATION_REPORT_SCHEMA_VERSION,
        dataset_manifest_hash: [0x11; 32],
        candidate_model_hash: [0x22; 32],
        baseline_model_hash: [0x33; 32],
        required_baselines: vec![
            RequiredBaseline::Volatility,
            RequiredBaseline::Leverage,
            RequiredBaseline::Regime,
            RequiredBaseline::Microstructure,
        ],
        folds: vec![
            passing_fold(1, "low-volatility", 0.15),
            passing_fold(2, "high-volatility", 0.16),
            passing_fold(3, "liquidity-stress", 0.17),
            passing_fold(4, "low-volatility", 0.18),
            passing_fold(5, "high-volatility", 0.19),
        ],
        maximum_standardized_coefficient_drift: 0.75,
        sign_scaling_parity: true,
        numerical_failures: 0,
        attempted_model_variants: 3,
    }
}

fn evaluation(input: AblationReportInput) -> cusp::ablation::GateEvaluation {
    let report = AblationReport::try_new(input).expect("valid ablation report");
    CuspGate::default()
        .evaluate(&report)
        .expect("valid gate evaluation")
}

#[test]
fn stable_incremental_brier_skill_can_become_eligible() {
    let report = AblationReport::try_new(passing_input()).expect("valid ablation report");
    let gate = CuspGate::default();

    assert_eq!(
        gate.decide(&report).expect("valid gate decision"),
        GateDecision::EligibleForProductionWeight
    );
    let evaluation = gate.evaluate(&report).expect("valid gate evaluation");
    assert!(evaluation.checks().iter().all(|check| check.passed()));
    assert_eq!(evaluation.report_evidence_hash(), report.evidence_hash());
}

#[test]
fn one_crash_only_improvement_is_not_production_eligible() {
    let mut input = passing_input();
    for fold in &mut input.folds[1..] {
        fold.candidate_brier_score = 0.21;
    }
    let evaluation = evaluation(input);

    assert_eq!(evaluation.decision(), GateDecision::ResearchOnly);
    assert!(
        !evaluation
            .check(GateCheckKind::ImprovedFoldFraction)
            .passed()
    );
    assert!(
        !evaluation
            .check(GateCheckKind::MultiRegimeImprovement)
            .passed()
    );
}

#[test]
fn insufficient_events_and_episodes_fail_closed() {
    let mut input = passing_input();
    input.folds.truncate(2);
    for fold in &mut input.folds {
        fold.positive_event_count = 5;
        fold.independent_event_episode_count = 1;
    }
    let evaluation = evaluation(input);

    assert_eq!(evaluation.decision(), GateDecision::ResearchOnly);
    assert!(
        !evaluation
            .check(GateCheckKind::MinimumPositiveEvents)
            .passed()
    );
    assert!(
        !evaluation
            .check(GateCheckKind::IndependentEpisodes)
            .passed()
    );
}

#[test]
fn zero_event_folds_are_valid_bad_evidence_and_fail_the_aggregate_gate() {
    let mut input = passing_input();
    for fold in &mut input.folds {
        fold.positive_event_count = 0;
        fold.independent_event_episode_count = 0;
    }
    let evaluation = evaluation(input);

    assert_eq!(evaluation.decision(), GateDecision::ResearchOnly);
    assert!(
        !evaluation
            .check(GateCheckKind::MinimumPositiveEvents)
            .passed()
    );
    assert!(
        !evaluation
            .check(GateCheckKind::IndependentEpisodes)
            .passed()
    );
}

#[test]
fn bounded_large_event_totals_do_not_overflow_the_gate() {
    let mut input = passing_input();
    input.folds = (0..256)
        .map(|index| {
            let mut fold = passing_fold(index, "high-volume", 0.15);
            fold.observation_count = 1_000_000_000;
            fold.positive_event_count = 1_000_000_000;
            fold.independent_event_episode_count = 1_000_000_000;
            fold
        })
        .collect();

    let evaluation = evaluation(input);

    assert_eq!(
        evaluation
            .check(GateCheckKind::MinimumPositiveEvents)
            .observed(),
        256_000_000_000
    );
    assert_eq!(
        evaluation
            .check(GateCheckKind::IndependentEpisodes)
            .observed(),
        256_000_000_000
    );
}

#[test]
fn sign_scaling_disagreement_is_research_only() {
    let mut input = passing_input();
    input.sign_scaling_parity = false;
    let evaluation = evaluation(input);

    assert_eq!(evaluation.decision(), GateDecision::ResearchOnly);
    assert!(!evaluation.check(GateCheckKind::SignScalingParity).passed());
}

#[test]
fn numerical_failure_is_research_only() {
    let mut input = passing_input();
    input.numerical_failures = 1;
    let evaluation = evaluation(input);

    assert_eq!(evaluation.decision(), GateDecision::ResearchOnly);
    assert!(
        !evaluation
            .check(GateCheckKind::NumericalReliability)
            .passed()
    );
}

#[test]
fn calibration_and_coefficient_instability_are_research_only() {
    let mut input = passing_input();
    input.maximum_standardized_coefficient_drift = 2.5;
    input.folds[2].calibration_slope = 1.3;
    input.folds[3].calibration_intercept = 0.11;
    input.folds[4].expected_calibration_error = 0.031;
    let evaluation = evaluation(input);

    assert_eq!(evaluation.decision(), GateDecision::ResearchOnly);
    assert!(
        !evaluation
            .check(GateCheckKind::CoefficientStability)
            .passed()
    );
    assert!(!evaluation.check(GateCheckKind::Calibration).passed());
}

#[test]
fn extreme_finite_bad_metrics_remain_structured_research_only_evidence() {
    let mut input = passing_input();
    input.maximum_standardized_coefficient_drift = f64::MAX;
    input
        .folds
        .last_mut()
        .expect("last fold")
        .baseline_brier_score = f64::MIN_POSITIVE;
    input
        .folds
        .last_mut()
        .expect("last fold")
        .candidate_brier_score = 1.0;

    let evaluation = evaluation(input);

    assert_eq!(evaluation.decision(), GateDecision::ResearchOnly);
    assert!(
        !evaluation
            .check(GateCheckKind::CoefficientStability)
            .passed()
    );
    assert!(!evaluation.check(GateCheckKind::RecentFoldSafety).passed());
}

#[test]
fn negative_calibration_slope_is_valid_bad_evidence_and_research_only() {
    let mut input = passing_input();
    input.folds[0].calibration_slope = -0.25;
    let evaluation = evaluation(input);

    assert_eq!(evaluation.decision(), GateDecision::ResearchOnly);
    assert!(!evaluation.check(GateCheckKind::Calibration).passed());
}

#[test]
fn recent_catastrophic_degradation_is_research_only() {
    let mut input = passing_input();
    input
        .folds
        .last_mut()
        .expect("last fold")
        .candidate_brier_score = 0.40;
    let evaluation = evaluation(input);

    assert_eq!(evaluation.decision(), GateDecision::ResearchOnly);
    assert!(!evaluation.check(GateCheckKind::RecentFoldSafety).passed());
}

#[test]
fn missing_control_baseline_is_research_only() {
    let mut input = passing_input();
    input
        .required_baselines
        .retain(|baseline| *baseline != RequiredBaseline::Microstructure);
    let evaluation = evaluation(input);

    assert_eq!(evaluation.decision(), GateDecision::ResearchOnly);
    assert!(!evaluation.check(GateCheckKind::RequiredBaselines).passed());
}

#[test]
fn invalid_or_ambiguous_reports_are_rejected_before_decision() {
    let mut nonfinite = passing_input();
    nonfinite.folds[0].candidate_brier_score = f64::NAN;
    assert_eq!(
        AblationReport::try_new(nonfinite),
        Err(AblationError::NonFiniteMetric)
    );

    let mut duplicate_fold = passing_input();
    duplicate_fold.folds[1].fold_id = duplicate_fold.folds[0].fold_id.clone();
    assert_eq!(
        AblationReport::try_new(duplicate_fold),
        Err(AblationError::DuplicateFold)
    );

    let mut zero_hash = passing_input();
    zero_hash.candidate_model_hash = [0; 32];
    assert_eq!(
        AblationReport::try_new(zero_hash),
        Err(AblationError::InvalidIdentity)
    );

    let mut invalid_count = passing_input();
    invalid_count.folds[0].independent_event_episode_count =
        invalid_count.folds[0].positive_event_count + 1;
    assert_eq!(
        AblationReport::try_new(invalid_count),
        Err(AblationError::InvalidCount)
    );

    let mut colliding_models = passing_input();
    colliding_models.baseline_model_hash = colliding_models.candidate_model_hash;
    assert_eq!(
        AblationReport::try_new(colliding_models),
        Err(AblationError::InvalidIdentity)
    );

    let mut reordered = passing_input();
    reordered.folds.swap(0, 1);
    assert_eq!(
        AblationReport::try_new(reordered),
        Err(AblationError::NonChronologicalFolds)
    );

    let mut ambiguous_identifier = passing_input();
    ambiguous_identifier.folds[0].regime_id = " low volatility ".to_owned();
    assert_eq!(
        AblationReport::try_new(ambiguous_identifier),
        Err(AblationError::InvalidIdentity)
    );
}

#[test]
fn serialized_input_rejects_unknown_fields() {
    let mut value = serde_json::to_value(passing_input()).expect("serialize input");
    value
        .as_object_mut()
        .expect("input object")
        .insert("manual_override".to_owned(), serde_json::json!(true));

    assert!(serde_json::from_value::<AblationReportInput>(value).is_err());
}

#[test]
fn report_and_decision_evidence_are_deterministic_and_mutation_sensitive() {
    let first_report = AblationReport::try_new(passing_input()).expect("first report");
    let second_report = AblationReport::try_new(passing_input()).expect("second report");
    let gate = CuspGate::default();
    let first = gate.evaluate(&first_report).expect("first evaluation");
    let second = gate.evaluate(&second_report).expect("second evaluation");

    assert_eq!(first_report.evidence_hash(), second_report.evidence_hash());
    assert_eq!(first.evidence_hash(), second.evidence_hash());
    assert_eq!(
        serde_json::to_vec(&first).expect("first JSON"),
        serde_json::to_vec(&second).expect("second JSON")
    );

    let mut mutated = passing_input();
    mutated.attempted_model_variants += 1;
    let mutated_report = AblationReport::try_new(mutated).expect("mutated report");
    let mutated_evaluation = gate.evaluate(&mutated_report).expect("mutated evaluation");
    assert_ne!(first_report.evidence_hash(), mutated_report.evidence_hash());
    assert_ne!(first.evidence_hash(), mutated_evaluation.evidence_hash());
}

#[test]
fn production_thresholds_are_predeclared_and_versioned() {
    let policy = CuspGate::default().policy();

    assert_eq!(policy.schema_version(), 1);
    assert_eq!(policy.minimum_improved_fold_fraction_ppm(), 700_000);
    assert_eq!(policy.minimum_positive_events(), 50);
    assert_eq!(policy.minimum_regimes(), 3);
    assert_eq!(policy.minimum_independent_episodes(), 3);
    assert_eq!(policy.calibration_slope_ppm(), (800_000, 1_200_000));
    assert_eq!(policy.maximum_absolute_calibration_intercept_ppm(), 100_000);
    assert_eq!(policy.maximum_expected_calibration_error_ppm(), 30_000);
    assert_eq!(
        policy.maximum_standardized_coefficient_drift_ppm(),
        2_000_000
    );
    assert_eq!(policy.minimum_recent_fold_brier_skill_ppm(), -100_000);
}
