use calibration::{
    BetaCalibrator, CalibrationError, CalibrationMethod, CalibrationObservation, CalibrationPeriod,
    CalibrationSet, EvaluationSet, FitConfig, IsotonicCalibrator, PlattCalibrator,
    ProbabilityMetrics,
};

fn observations() -> Vec<CalibrationObservation> {
    [
        (0.05, false),
        (0.12, false),
        (0.20, false),
        (0.32, false),
        (0.44, false),
        (0.56, true),
        (0.68, true),
        (0.78, true),
        (0.88, true),
        (0.95, true),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (probability, outcome))| {
        let origin = 100 + i64::try_from(index).expect("fixture index");
        CalibrationObservation::new(probability, outcome, origin, origin + 1, origin + 1)
            .expect("valid point-in-time fixture")
    })
    .collect()
}

fn calibration_set() -> CalibrationSet {
    CalibrationSet::new(
        "validation-fold-7",
        CalibrationPeriod::new(100, 120).expect("valid period"),
        observations(),
    )
    .expect("valid calibration set")
}

#[test]
fn calibrators_are_bounded_monotone_and_deterministic() {
    let set = calibration_set();
    let config = FitConfig::default();
    let platt = PlattCalibrator::fit(&set, config).expect("Platt fit");
    let beta = BetaCalibrator::fit(&set, config).expect("beta fit");
    let isotonic = IsotonicCalibrator::fit(&set).expect("isotonic fit");

    for calibrator in [
        CalibrationMethod::Platt(platt.clone()),
        CalibrationMethod::Beta(beta.clone()),
        CalibrationMethod::Isotonic(isotonic.clone()),
    ] {
        let mut previous = 0.0;
        for step in 1..100 {
            let raw = f64::from(step) / 100.0;
            let calibrated = calibrator.calibrate(raw).expect("bounded input");
            assert!(calibrated.is_finite());
            assert!((0.0..=1.0).contains(&calibrated));
            assert!(calibrated + 1e-12 >= previous);
            previous = calibrated;
        }
    }

    assert_eq!(
        platt,
        PlattCalibrator::fit(&set, config).expect("repeat Platt fit")
    );
    assert_eq!(
        beta,
        BetaCalibrator::fit(&set, config).expect("repeat beta fit")
    );
    assert_eq!(
        isotonic,
        IsotonicCalibrator::fit(&set).expect("repeat isotonic fit")
    );
}

#[test]
fn invalid_and_degenerate_calibration_data_fail_closed() {
    assert!(matches!(
        CalibrationObservation::new(f64::NAN, false, 1, 2, 2),
        Err(CalibrationError::InvalidProbability)
    ));
    assert!(matches!(
        CalibrationObservation::new(0.5, false, 2, 1, 2),
        Err(CalibrationError::InvalidObservationTime)
    ));

    let all_negative = (0..8)
        .map(|index| {
            CalibrationObservation::new(
                0.1 + f64::from(index) / 100.0,
                false,
                100 + i64::from(index),
                101 + i64::from(index),
                101 + i64::from(index),
            )
            .expect("valid observation")
        })
        .collect();
    assert!(matches!(
        CalibrationSet::new(
            "degenerate",
            CalibrationPeriod::new(100, 120).expect("period"),
            all_negative
        ),
        Err(CalibrationError::DegenerateOutcomes)
    ));
}

#[test]
fn evaluation_must_be_later_than_calibration_and_metrics_serialize() {
    let calibration = calibration_set();
    let evaluation_points = observations()
        .into_iter()
        .enumerate()
        .map(|(index, point)| {
            CalibrationObservation::new(
                point.uncalibrated_probability(),
                point.outcome(),
                200 + i64::try_from(index).expect("fixture index"),
                201 + i64::try_from(index).expect("fixture index"),
                201 + i64::try_from(index).expect("fixture index"),
            )
            .expect("evaluation observation")
        })
        .collect();
    let evaluation = EvaluationSet::new(
        "outer-test-7",
        CalibrationPeriod::new(200, 220).expect("period"),
        calibration.period(),
        evaluation_points,
    )
    .expect("strictly later evaluation set");

    let calibrator = CalibrationMethod::Platt(
        PlattCalibrator::fit(&calibration, FitConfig::default()).expect("fit"),
    );
    let metrics = ProbabilityMetrics::evaluate(&evaluation, Some(&calibrator), 5).expect("metrics");
    assert_eq!(metrics.observations, 10);
    assert_eq!(metrics.reliability.len(), 5);
    assert!(metrics.log_loss.is_finite());
    assert!(metrics.brier_score.is_finite());
    assert!(metrics.expected_calibration_error.is_finite());
    let encoded = serde_json::to_vec(&metrics).expect("metrics serialize");
    assert_eq!(
        encoded,
        serde_json::to_vec(&metrics).expect("metrics serialize deterministically")
    );
    let isotonic =
        CalibrationMethod::Isotonic(IsotonicCalibrator::fit(&calibration).expect("isotonic fit"));
    let isotonic_metrics =
        ProbabilityMetrics::evaluate(&evaluation, Some(&isotonic), 5).expect("isotonic metrics");
    assert!(isotonic_metrics.log_loss.is_finite());
    assert!(isotonic_metrics.calibration_slope.is_finite());

    let overlapping = EvaluationSet::new(
        "leaky",
        CalibrationPeriod::new(120, 140).expect("period"),
        calibration.period(),
        observations(),
    );
    assert!(matches!(
        overlapping,
        Err(CalibrationError::EvaluationOverlap)
    ));
}

#[test]
fn exact_endpoint_probabilities_require_an_explicit_policy() {
    let calibrator = CalibrationMethod::Platt(
        PlattCalibrator::fit(&calibration_set(), FitConfig::default()).expect("fit"),
    );
    assert!(matches!(
        calibrator.calibrate(0.0),
        Err(CalibrationError::EndpointProbability)
    ));
    assert!(matches!(
        calibrator.calibrate(1.0),
        Err(CalibrationError::EndpointProbability)
    ));
}

#[test]
fn deserialized_state_is_revalidated_before_fit_calibration_or_evaluation() {
    let mut forged_set = serde_json::to_value(calibration_set()).expect("calibration JSON");
    forged_set["period"]["start_ns"] = serde_json::json!(105);
    let forged_set: CalibrationSet =
        serde_json::from_value(forged_set).expect("structurally valid calibration JSON");
    assert!(matches!(
        PlattCalibrator::fit(&forged_set, FitConfig::default()),
        Err(CalibrationError::ObservationOutsidePeriod)
    ));

    let valid = PlattCalibrator::fit(&calibration_set(), FitConfig::default()).expect("fit");
    let mut forged_calibrator = serde_json::to_value(valid).expect("calibrator JSON");
    forged_calibrator["slope"] = serde_json::json!(-1.0);
    let forged_calibrator: PlattCalibrator =
        serde_json::from_value(forged_calibrator).expect("structurally valid calibrator JSON");
    assert!(matches!(
        forged_calibrator.calibrate(0.25),
        Err(CalibrationError::InvalidCalibrator)
    ));

    let calibration = calibration_set();
    let evaluation_points = observations()
        .into_iter()
        .enumerate()
        .map(|(index, point)| {
            CalibrationObservation::new(
                point.uncalibrated_probability(),
                point.outcome(),
                200 + i64::try_from(index).expect("fixture index"),
                201 + i64::try_from(index).expect("fixture index"),
                201 + i64::try_from(index).expect("fixture index"),
            )
            .expect("evaluation observation")
        })
        .collect();
    let evaluation = EvaluationSet::new(
        "outer-test-serialized",
        CalibrationPeriod::new(200, 220).expect("period"),
        calibration.period(),
        evaluation_points,
    )
    .expect("evaluation set");
    let mut forged_evaluation = serde_json::to_value(evaluation).expect("evaluation JSON");
    forged_evaluation["calibration_period"]["end_ns"] = serde_json::json!(205);
    let forged_evaluation: EvaluationSet =
        serde_json::from_value(forged_evaluation).expect("structurally valid evaluation JSON");
    assert!(matches!(
        ProbabilityMetrics::evaluate(&forged_evaluation, None, 5),
        Err(CalibrationError::EvaluationOverlap)
    ));
}
