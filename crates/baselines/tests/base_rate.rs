use std::collections::BTreeSet;

use baselines::{
    BaseRateConfig, BaseRateModel, BaseRateQuery, BaselineError, EstimateKind, Observation,
    ObservationInput, beta_binomial_estimate,
};
use labels::{ExclusionReason, LabelOutcome};

const HOUR_NS: i64 = 3_600_000_000_000;
const HORIZON_SECONDS: u64 = 3_600;
const FIT_CUTOFF_NS: i64 = 1_000 * HOUR_NS;

#[test]
fn test_period_outcomes_never_enter_base_rate_fit() {
    let train = rate_observations("btc", 98, 9, 1);
    let test = rate_observations("btc", 100, 100, 10_000);

    let model =
        BaseRateModel::fit(&train, FIT_CUTOFF_NS, config(5)).expect("training rows are valid");
    let estimate = model
        .estimate(&BaseRateQuery::try_new("btc", HORIZON_SECONDS).expect("query"))
        .expect("known baseline");
    assert!((estimate.probability - 0.10).abs() < 1e-12);
    assert_eq!(estimate.total_count, train.len() as u64);
    assert_eq!(model.fit_rows(), train.len());
    assert_eq!(test.iter().filter(|row| row.is_positive()).count(), 100);
}

#[test]
fn sparse_stratum_falls_back_with_explicit_level() {
    let mut rows = rate_observations("btc", 20, 2, 1);
    rows[0] = observation(
        1,
        "btc",
        10 * HOUR_NS,
        LabelOutcome::Occurred { offset_seconds: 60 },
        Some("risk_off"),
    );
    let model = BaseRateModel::fit(&rows, FIT_CUTOFF_NS, config(10)).expect("model");
    let query = BaseRateQuery::try_new("btc", HORIZON_SECONDS)
        .expect("query")
        .with_origin_time_ns(10 * HOUR_NS)
        .expect("origin")
        .with_regime("risk_off")
        .expect("predeclared regime");

    let estimate = model.estimate(&query).expect("fallback estimate");
    assert_eq!(estimate.kind, EstimateKind::Unconditional);
    assert_eq!(estimate.fallback_level, 3);
    assert!(estimate.minimum_count_met);
}

#[test]
fn censored_and_excluded_outcomes_are_never_coerced_to_negatives() {
    let rows = vec![
        observation(
            1,
            "btc",
            HOUR_NS,
            LabelOutcome::Occurred { offset_seconds: 60 },
            None,
        ),
        observation(2, "btc", 2 * HOUR_NS, LabelOutcome::NotOccurred, None),
        observation(
            3,
            "btc",
            3 * HOUR_NS,
            LabelOutcome::Censored {
                observed_seconds: 300,
            },
            None,
        ),
        observation(
            4,
            "btc",
            4 * HOUR_NS,
            LabelOutcome::Excluded(ExclusionReason::UnresolvedCorrection),
            None,
        ),
    ];
    let model = BaseRateModel::fit(&rows, FIT_CUTOFF_NS, config(1)).expect("model");
    let estimate = model
        .estimate(&BaseRateQuery::try_new("btc", HORIZON_SECONDS).expect("query"))
        .expect("estimate");

    assert_eq!(estimate.positive_count, 1);
    assert_eq!(estimate.total_count, 2);
    assert_eq!(estimate.censored_count, 1);
    assert_eq!(estimate.excluded_count, 1);
    assert!((estimate.probability - 0.5).abs() < 1e-12);

    let unavailable_only = vec![
        observation(
            10,
            "eth",
            10 * HOUR_NS,
            LabelOutcome::Censored {
                observed_seconds: 300,
            },
            None,
        ),
        observation(
            11,
            "eth",
            11 * HOUR_NS,
            LabelOutcome::Excluded(ExclusionReason::MissingEvidence),
            None,
        ),
    ];
    let model = BaseRateModel::fit(&unavailable_only, FIT_CUTOFF_NS, config(5)).expect("model");
    let estimate = model
        .estimate(&BaseRateQuery::try_new("eth", HORIZON_SECONDS).expect("query"))
        .expect("prior with explicit unavailable evidence");
    assert_eq!(estimate.total_count, 0);
    assert_eq!(estimate.censored_count, 1);
    assert_eq!(estimate.excluded_count, 1);
    assert!(!estimate.minimum_count_met);
    assert!((estimate.probability - 0.5).abs() < 1e-12);
}

#[test]
fn seasonal_regime_and_rolling_estimates_use_only_their_declared_rows() {
    let mut rows = vec![
        observation(
            1,
            "btc",
            HOUR_NS,
            LabelOutcome::Occurred { offset_seconds: 60 },
            Some("risk_on"),
        ),
        observation(
            2,
            "btc",
            169 * HOUR_NS,
            LabelOutcome::Occurred { offset_seconds: 60 },
            Some("risk_on"),
        ),
        observation(
            3,
            "btc",
            337 * HOUR_NS,
            LabelOutcome::NotOccurred,
            Some("risk_on"),
        ),
        observation(
            4,
            "btc",
            2 * HOUR_NS,
            LabelOutcome::NotOccurred,
            Some("risk_off"),
        ),
        observation(
            5,
            "btc",
            170 * HOUR_NS,
            LabelOutcome::NotOccurred,
            Some("risk_off"),
        ),
        observation(
            6,
            "btc",
            338 * HOUR_NS,
            LabelOutcome::NotOccurred,
            Some("risk_off"),
        ),
    ];
    for index in 0..4_u64 {
        rows.push(observation(
            7 + index,
            "btc",
            i64::try_from(1_196 + index).expect("small index") * HOUR_NS,
            LabelOutcome::Occurred { offset_seconds: 60 },
            None,
        ));
    }
    let config = BaseRateConfig::try_new(1.0, 1.0, 0.95, 2, 4 * 3_600, ["risk_on", "risk_off"])
        .expect("valid config");
    let model = BaseRateModel::fit(&rows, 1_200 * HOUR_NS, config).expect("model");

    let rolling = model
        .rolling_estimate("btc", HORIZON_SECONDS)
        .expect("rolling estimate");
    assert_eq!(rolling.kind, EstimateKind::Rolling);
    assert_eq!(rolling.positive_count, 4);
    assert_eq!(rolling.total_count, 4);

    let combined = model
        .estimate(
            &BaseRateQuery::try_new("btc", HORIZON_SECONDS)
                .expect("query")
                .with_origin_time_ns(505 * HOUR_NS)
                .expect("origin")
                .with_regime("risk_on")
                .expect("regime"),
        )
        .expect("combined estimate");
    assert_eq!(combined.kind, EstimateKind::RegimeAndTimeOfWeek);
    assert_eq!(combined.fallback_level, 0);
    assert!(combined.total_count >= 2);

    let historical_only = rate_observations("eth", 20, 2, 100);
    let sparse_rolling = BaseRateModel::fit(
        &historical_only,
        FIT_CUTOFF_NS,
        BaseRateConfig::try_new(1.0, 1.0, 0.95, 5, 3_600, ["risk_on"]).expect("valid config"),
    )
    .expect("model")
    .rolling_estimate("eth", HORIZON_SECONDS)
    .expect("explicit rolling fallback");
    assert_eq!(sparse_rolling.kind, EstimateKind::Unconditional);
    assert_eq!(sparse_rolling.fallback_level, 1);
}

#[test]
fn beta_posterior_interval_is_exact_finite_and_bounded() {
    let estimate = beta_binomial_estimate(9, 98, 1.0, 1.0, 0.95).expect("valid posterior estimate");
    assert!((estimate.probability - 0.10).abs() < 1e-12);
    assert!(
        (estimate.lower - 0.04951087220866212).abs() < 1e-12,
        "{estimate:?}"
    );
    assert!(
        (estimate.upper - 0.16556897574201246).abs() < 1e-12,
        "{estimate:?}"
    );
    assert!(0.0 <= estimate.lower);
    assert!(estimate.lower < estimate.probability);
    assert!(estimate.probability < estimate.upper);
    assert!(estimate.upper <= 1.0);

    let uniform = beta_binomial_estimate(0, 0, 1.0, 1.0, 0.95).expect("uniform prior");
    assert!((uniform.lower - 0.025).abs() < 1e-12);
    assert!((uniform.upper - 0.975).abs() < 1e-12);
    for edge in [
        beta_binomial_estimate(0, 1_000_000, 1.0, 1.0, 0.95),
        beta_binomial_estimate(1_000_000, 1_000_000, 1.0, 1.0, 0.95),
    ] {
        let edge = edge.expect("bounded edge posterior");
        assert!(edge.probability.is_finite());
        assert!(edge.lower.is_finite());
        assert!(edge.upper.is_finite());
        assert!(0.0 <= edge.lower && edge.lower <= edge.upper && edge.upper <= 1.0);
    }

    for invalid in [
        beta_binomial_estimate(2, 1, 1.0, 1.0, 0.95),
        beta_binomial_estimate(0, 0, 0.0, 1.0, 0.95),
        beta_binomial_estimate(0, 0, 1.0, f64::NAN, 0.95),
        beta_binomial_estimate(0, 0, 1.0, 1.0, 0.0),
        beta_binomial_estimate(0, 0, 1.0, 1.0, 1.0),
        beta_binomial_estimate(0, 1_000_001, 1.0, 1.0, 0.95),
    ] {
        assert_eq!(invalid, Err(BaselineError::InvalidInput));
    }
}

#[test]
fn model_and_evidence_identities_are_canonical_and_mutation_sensitive() {
    let rows = rate_observations("btc", 20, 2, 1);
    let mut reversed = rows.clone();
    reversed.reverse();
    let model = BaseRateModel::fit(&rows, FIT_CUTOFF_NS, config(5)).expect("model");
    let reordered =
        BaseRateModel::fit(&reversed, FIT_CUTOFF_NS, config(5)).expect("canonical order");
    assert_eq!(model.model_id(), reordered.model_id());
    assert_eq!(
        model.model_id(),
        [
            121, 87, 2, 241, 223, 211, 120, 60, 138, 126, 30, 89, 2, 108, 180, 70, 135, 17, 248,
            185, 216, 222, 7, 142, 75, 80, 99, 199, 109, 153, 255, 251,
        ]
    );

    let query = BaseRateQuery::try_new("btc", HORIZON_SECONDS).expect("query");
    let estimate = model.estimate(&query).expect("estimate");
    let repeated = model.estimate(&query).expect("repeat estimate");
    assert_eq!(estimate.model_id, model.model_id());
    assert_eq!(
        estimate.evidence_hash,
        [
            167, 52, 79, 108, 146, 20, 103, 52, 109, 25, 23, 245, 152, 227, 166, 106, 211, 173, 64,
            11, 165, 76, 181, 159, 103, 214, 45, 160, 204, 247, 47, 235,
        ]
    );
    assert_eq!(
        estimate.forecast_id,
        [
            164, 137, 49, 34, 106, 65, 18, 20, 28, 82, 85, 16, 111, 233, 24, 202, 118, 30, 199, 29,
            120, 150, 72, 249, 138, 142, 93, 118, 198, 195, 91, 217,
        ]
    );
    assert_eq!(estimate.forecast_id, repeated.forecast_id);
    assert_eq!(estimate.evidence_hash, repeated.evidence_hash);
    let persisted = serde_json::to_value(estimate).expect("estimate serializes");
    let fields = persisted
        .as_object()
        .expect("estimate is an object")
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        fields,
        BTreeSet::from([
            "censored_count",
            "evidence_hash",
            "excluded_count",
            "fallback_level",
            "forecast_id",
            "kind",
            "lower",
            "minimum_count_met",
            "model_id",
            "positive_count",
            "probability",
            "total_count",
            "upper",
        ])
    );

    let changed_cutoff =
        BaseRateModel::fit(&rows, FIT_CUTOFF_NS + 1, config(5)).expect("changed cutoff model");
    assert_ne!(model.model_id(), changed_cutoff.model_id());

    let changed_prior = BaseRateModel::fit(
        &rows,
        FIT_CUTOFF_NS,
        BaseRateConfig::try_new(0.5, 0.5, 0.95, 5, 30 * 86_400, ["risk_on", "risk_off"])
            .expect("changed prior"),
    )
    .expect("changed model");
    assert_ne!(model.model_id(), changed_prior.model_id());
}

#[test]
fn late_duplicate_and_invalid_inputs_fail_closed() {
    let mut late = rate_observations("btc", 5, 1, 1);
    late[0] = Observation::try_new(ObservationInput {
        observation_id: 1,
        asset: "btc".to_owned(),
        horizon_seconds: HORIZON_SECONDS,
        origin_time_ns: FIT_CUTOFF_NS - HOUR_NS,
        outcome: LabelOutcome::NotOccurred,
        outcome_known_at_ns: FIT_CUTOFF_NS + 1,
        regime: None,
        label_definition_hash: [1; 32],
        source_range_hash: [2; 32],
    })
    .expect("row is internally valid but late for this fit");
    assert_eq!(
        BaseRateModel::fit(&late, FIT_CUTOFF_NS, config(1)),
        Err(BaselineError::OutcomeKnownAfterFitCutoff { observation_id: 1 })
    );

    let mut duplicate = rate_observations("btc", 5, 1, 1);
    duplicate.push(duplicate[0].clone());
    assert_eq!(
        BaseRateModel::fit(&duplicate, FIT_CUTOFF_NS, config(1)),
        Err(BaselineError::DuplicateObservation { observation_id: 1 })
    );

    assert_eq!(
        BaseRateConfig::try_new(1.0, 1.0, 0.95, 0, 86_400, ["risk_on"]),
        Err(BaselineError::InvalidConfiguration)
    );
    assert_eq!(
        BaseRateConfig::try_new(1.0, 1.0, 0.0, 1, 86_400, ["risk_on"]),
        Err(BaselineError::InvalidConfiguration)
    );
    assert_eq!(
        BaseRateQuery::try_new("BTC", HORIZON_SECONDS),
        Err(BaselineError::InvalidQuery)
    );

    let model = BaseRateModel::fit(&rate_observations("btc", 5, 1, 1), FIT_CUTOFF_NS, config(1))
        .expect("model");
    let undeclared_query = BaseRateQuery::try_new("btc", HORIZON_SECONDS)
        .expect("query")
        .with_regime("undeclared")
        .expect("syntactically valid regime");
    assert_eq!(
        model.estimate(&undeclared_query),
        Err(BaselineError::InvalidQuery)
    );
}

fn config(minimum_count: u64) -> BaseRateConfig {
    BaseRateConfig::try_new(
        1.0,
        1.0,
        0.95,
        minimum_count,
        30 * 86_400,
        ["risk_on", "risk_off"],
    )
    .expect("reference config")
}

fn rate_observations(asset: &str, total: u64, positive: u64, first_id: u64) -> Vec<Observation> {
    assert!(positive <= total);
    (0..total)
        .map(|index| {
            observation(
                first_id + index,
                asset,
                i64::try_from(index + 1).expect("bounded fixture") * HOUR_NS,
                if index < positive {
                    LabelOutcome::Occurred { offset_seconds: 60 }
                } else {
                    LabelOutcome::NotOccurred
                },
                None,
            )
        })
        .collect()
}

fn observation(
    id: u64,
    asset: &str,
    origin_time_ns: i64,
    outcome: LabelOutcome,
    regime: Option<&str>,
) -> Observation {
    let outcome_known_at_ns = match outcome {
        LabelOutcome::Occurred { offset_seconds } => {
            origin_time_ns + i64::try_from(offset_seconds).expect("fixture offset") * 1_000_000_000
        }
        LabelOutcome::NotOccurred => origin_time_ns + HOUR_NS,
        LabelOutcome::Censored { observed_seconds } => {
            origin_time_ns
                + i64::try_from(observed_seconds).expect("fixture offset") * 1_000_000_000
        }
        LabelOutcome::Excluded(_) => origin_time_ns,
    };
    Observation::try_new(ObservationInput {
        observation_id: id,
        asset: asset.to_owned(),
        horizon_seconds: HORIZON_SECONDS,
        origin_time_ns,
        outcome,
        outcome_known_at_ns,
        regime: regime.map(str::to_owned),
        label_definition_hash: [1; 32],
        source_range_hash: [2; 32],
    })
    .expect("valid fixture observation")
}
