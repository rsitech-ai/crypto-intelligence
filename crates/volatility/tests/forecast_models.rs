use dataset::{FoldBuilder, Sample, WalkForwardSchedule};
use volatility::{
    CandidateModel, EwmaVolatility, ForecastError, HarRvModel, HarRvObservation, ModelFamily,
    SelectionConfig, SelectionObservation, select_volatility_model, volatility_normalized_state,
};

const DAY_SECONDS: u64 = 86_400;
const DAY_NS: i64 = 86_400_000_000_000;
const EPSILON: f64 = 1e-12;

#[test]
fn ewma_matches_first_squared_return_initialization_and_recursive_reference() {
    let mut model = EwmaVolatility::try_new(0.94).expect("lambda is valid");
    model.update(0.01).expect("first return initializes");
    assert!((model.variance().expect("variance") - 0.0001).abs() < EPSILON);

    model.update(-0.02).expect("second return updates");
    model.update(0.03).expect("third return updates");
    let forecast = model.forecast().expect("history supports uncertainty");

    assert!((forecast.variance - 0.000_164_92).abs() < EPSILON);
    assert!((forecast.volatility - 0.000_164_92_f64.sqrt()).abs() < EPSILON);
    assert_eq!(forecast.observation_count, 3);
    assert!(0.0 <= forecast.lower);
    assert!(forecast.lower <= forecast.volatility);
    assert!(forecast.volatility <= forecast.upper);
}

#[test]
fn ewma_invalid_parameters_and_nonfinite_updates_fail_without_mutating_state() {
    for invalid in [0.0, 1.0, -0.1, f64::NAN, f64::INFINITY] {
        assert_eq!(
            EwmaVolatility::try_new(invalid),
            Err(ForecastError::InvalidLambda)
        );
    }

    let mut model = EwmaVolatility::try_new(0.94).expect("valid model");
    model.update(0.01).expect("initial observation");
    let before = model.clone();
    assert_eq!(model.update(f64::NAN), Err(ForecastError::NonFinite));
    assert_eq!(model, before);
    assert_eq!(
        EwmaVolatility::try_new(0.94)
            .expect("valid empty model")
            .forecast(),
        Err(ForecastError::InsufficientHistory)
    );
}

#[test]
fn har_fit_uses_only_passed_training_rows_and_training_normalization() {
    let training = har_fixture(30, 1);
    let model =
        HarRvModel::fit(&training, 32 * DAY_NS, 1e-12).expect("well-conditioned training fit");
    let forecast = model
        .forecast(0.17, 0.26, 0.34)
        .expect("finite nonnegative predictors");
    let expected = synthetic_target(0.17, 0.26, 0.34);

    assert_eq!(model.fit_rows(), training.len());
    assert_eq!(model.fit_cutoff_ns(), 32 * DAY_NS);
    assert!(
        (forecast.volatility - expected).abs() < 1e-8,
        "{forecast:?}"
    );
    assert!(forecast.lower <= forecast.volatility);
    assert!(forecast.volatility <= forecast.upper);
    assert_eq!(model.normalization().count, training.len() as u64);

    let mut external_test = har_fixture(8, 10_000);
    for row in &mut external_test {
        *row = HarRvObservation::try_new(
            row.observation_id(),
            row.origin_time_ns() + 100 * DAY_NS,
            row.predictors_as_known_at_ns() + 100 * DAY_NS,
            row.target_known_at_ns() + 100 * DAY_NS,
            row.daily(),
            row.weekly(),
            row.monthly(),
            100.0,
            row.lineage_hash(),
        )
        .expect("extreme untouched test row");
    }
    assert_eq!(
        model.model_id(),
        HarRvModel::fit(&training, 32 * DAY_NS, 1e-12)
            .expect("same training fit")
            .model_id()
    );
    assert_eq!(external_test.len(), 8);
}

#[test]
fn har_ridge_handles_collinearity_and_rejects_late_or_invalid_training_data() {
    let constant = (1..=8_u64)
        .map(|id| {
            HarRvObservation::try_new(
                id,
                i64::try_from(id).expect("small id") * DAY_NS,
                i64::try_from(id).expect("small id") * DAY_NS,
                (i64::try_from(id).expect("small id") + 1) * DAY_NS,
                0.2,
                0.2,
                0.2,
                0.25,
                [3; 32],
            )
            .expect("valid constant row")
        })
        .collect::<Vec<_>>();
    let model = HarRvModel::fit(&constant, 10 * DAY_NS, 1e-3)
        .expect("ridge stabilizes collinear predictors");
    let forecast = model.forecast(0.2, 0.2, 0.2).expect("forecast");
    assert!(forecast.volatility.is_finite());
    assert!(forecast.volatility >= 0.0);

    let mut late = har_fixture(8, 100);
    let first = late[0];
    late[0] = HarRvObservation::try_new(
        first.observation_id(),
        first.origin_time_ns(),
        first.predictors_as_known_at_ns(),
        20 * DAY_NS,
        first.daily(),
        first.weekly(),
        first.monthly(),
        first.target(),
        first.lineage_hash(),
    )
    .expect("internally valid but late for fit");
    assert_eq!(
        HarRvModel::fit(&late, 10 * DAY_NS, 1e-6),
        Err(ForecastError::OutcomeKnownAfterFitCutoff {
            observation_id: first.observation_id()
        })
    );
    assert_eq!(
        HarRvObservation::try_new(1, DAY_NS, DAY_NS, 2 * DAY_NS, -0.1, 0.2, 0.3, 0.4, [1; 32],),
        Err(ForecastError::InvalidObservation)
    );
    assert_eq!(
        HarRvObservation::try_new(1, DAY_NS, DAY_NS, DAY_NS, 0.1, 0.2, 0.3, 0.4, [1; 32],),
        Err(ForecastError::InvalidObservation)
    );
    assert_eq!(
        HarRvObservation::try_new(
            1,
            DAY_NS,
            DAY_NS + 1,
            2 * DAY_NS,
            0.1,
            0.2,
            0.3,
            0.4,
            [1; 32],
        ),
        Err(ForecastError::InvalidObservation)
    );
    let zero_horizon_sample =
        Sample::try_new(1, DAY_NS, DAY_NS, DAY_NS).expect("dataset permits explicit zero horizon");
    assert_eq!(
        SelectionObservation::try_new(
            zero_horizon_sample,
            DAY_NS,
            0.1,
            0.1,
            0.2,
            0.3,
            0.4,
            [1; 32],
        ),
        Err(ForecastError::InvalidObservation)
    );
    let future_predictor_sample =
        Sample::try_new(2, DAY_NS, 2 * DAY_NS, 2 * DAY_NS).expect("future target sample");
    assert_eq!(
        SelectionObservation::try_new(
            future_predictor_sample,
            DAY_NS + 1,
            0.1,
            0.1,
            0.2,
            0.3,
            0.4,
            [1; 32],
        ),
        Err(ForecastError::InvalidObservation)
    );
}

#[test]
fn inner_walk_forward_selection_reports_both_families_and_ignores_outer_test() {
    let samples = Sample::daily_fixture(80).expect("daily samples");
    let schedule = WalkForwardSchedule {
        initial_training_seconds: 30 * DAY_SECONDS,
        minimum_inner_training_seconds: 10 * DAY_SECONDS,
        inner_validation_seconds: 5 * DAY_SECONDS,
        calibration_seconds: 5 * DAY_SECONDS,
        test_seconds: 5 * DAY_SECONDS,
        step_seconds: 5 * DAY_SECONDS,
    };
    let outer = FoldBuilder::try_with_schedule(DAY_SECONDS, 0, schedule)
        .expect("valid fold builder")
        .outer_folds(&samples)
        .expect("walk-forward folds")
        .remove(0);
    let observations = selection_fixture(&samples, None, None);
    let config =
        SelectionConfig::try_new([0.90, 0.94], [1e-8, 1e-4], 5).expect("bounded candidate grid");

    let report = select_volatility_model(&observations, outer.inner_folds(), &config)
        .expect("inner-fold selection");
    assert_eq!(report.candidates.len(), 4);
    assert!(report.candidates.iter().any(|score| {
        matches!(
            score.candidate,
            CandidateModel::Ewma { lambda } if (lambda - 0.90).abs() < EPSILON
        )
    }));
    assert!(report.candidates.iter().any(|score| {
        matches!(
            score.candidate,
            CandidateModel::HarRv { ridge } if (ridge - 1e-8).abs() < EPSILON
        )
    }));
    assert!(
        report
            .candidates
            .iter()
            .all(|score| score.validation_rows > 0 && score.mean_squared_error.is_finite())
    );
    assert!(matches!(
        report.selected.family(),
        ModelFamily::Ewma | ModelFamily::HarRv
    ));

    let mutated = selection_fixture(
        &samples,
        Some((outer.test().start_ns(), outer.test().end_ns())),
        None,
    );
    let repeated = select_volatility_model(&mutated, outer.inner_folds(), &config)
        .expect("outer-test mutation cannot enter selection");
    assert_eq!(report, repeated);

    let lineage_mutated = selection_fixture(&samples, None, Some(2));
    let lineage_report = select_volatility_model(&lineage_mutated, outer.inner_folds(), &config)
        .expect("lineage mutation remains numerically valid");
    assert_eq!(report.candidates, lineage_report.candidates);
    assert_eq!(report.selected, lineage_report.selected);
    assert_ne!(report.selection_id, lineage_report.selection_id);
}

#[test]
fn normalized_state_uses_the_declared_floor_and_rejects_invalid_inputs() {
    assert!(
        (volatility_normalized_state(0.03, 0.01, 0.02, 0.005).expect("state") - 1.0).abs()
            < EPSILON
    );
    assert!(
        (volatility_normalized_state(0.03, 0.01, 0.001, 0.005).expect("floored state") - 4.0).abs()
            < EPSILON
    );
    assert_eq!(
        volatility_normalized_state(f64::NAN, 0.0, 0.1, 0.01),
        Err(ForecastError::NonFinite)
    );
    assert_eq!(
        volatility_normalized_state(0.1, 0.0, -0.1, 0.01),
        Err(ForecastError::InvalidVolatility)
    );
    assert_eq!(
        volatility_normalized_state(0.1, 0.0, 0.1, 0.0),
        Err(ForecastError::InvalidVolatility)
    );
}

fn har_fixture(count: u64, first_id: u64) -> Vec<HarRvObservation> {
    (0..count)
        .map(|index| {
            let daily = 0.10 + (index % 5) as f64 * 0.02;
            let weekly = 0.20 + ((index / 5) % 5) as f64 * 0.03;
            let monthly = 0.30 + ((index * 7) % 11) as f64 * 0.01;
            let id = first_id + index;
            HarRvObservation::try_new(
                id,
                i64::try_from(index + 1).expect("bounded fixture") * DAY_NS,
                i64::try_from(index + 1).expect("bounded fixture") * DAY_NS,
                i64::try_from(index + 2).expect("bounded fixture") * DAY_NS,
                daily,
                weekly,
                monthly,
                synthetic_target(daily, weekly, monthly),
                [2; 32],
            )
            .expect("valid HAR fixture")
        })
        .collect()
}

fn synthetic_target(daily: f64, weekly: f64, monthly: f64) -> f64 {
    0.05 + 0.4 * daily + 0.3 * weekly + 0.2 * monthly
}

fn selection_fixture(
    samples: &[Sample],
    outer_test_range: Option<(i64, i64)>,
    lineage_mutation_id: Option<u64>,
) -> Vec<SelectionObservation> {
    samples
        .iter()
        .map(|sample| {
            let index = sample.id() - 1;
            let daily = 0.10 + (index % 5) as f64 * 0.02;
            let weekly = 0.20 + ((index / 5) % 5) as f64 * 0.03;
            let monthly = 0.30 + ((index * 7) % 11) as f64 * 0.01;
            let target = if outer_test_range.is_some_and(|(start, end)| {
                sample.origin_time_ns() >= start && sample.origin_time_ns() < end
            }) {
                100.0
            } else {
                synthetic_target(daily, weekly, monthly)
            };
            SelectionObservation::try_new(
                *sample,
                sample.origin_time_ns(),
                if index % 2 == 0 { target } else { -target },
                daily,
                weekly,
                monthly,
                target,
                if lineage_mutation_id == Some(sample.id()) {
                    [5; 32]
                } else {
                    [4; 32]
                },
            )
            .expect("valid selection observation")
        })
        .collect()
}
