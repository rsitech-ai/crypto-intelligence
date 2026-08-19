use calibration::{
    BetaCalibrator, CalibrationError, CalibrationMethod, CalibrationObservation, CalibrationPeriod,
    CalibrationSet, FitConfig, IsotonicCalibrator, PlattCalibrator,
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
fn deserialized_state_is_revalidated_before_fit_or_calibration() {
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
}

#[test]
fn deserialized_logits_cannot_bypass_the_extreme_input_bound() {
    let mut forged = serde_json::to_value(calibration_set()).expect("calibration JSON");
    forged["observations"][0]["raw_logit"] = serde_json::json!(1_000_001.0);
    forged["observations"][0]["uncalibrated_probability"] = serde_json::json!(1.0);
    let forged: CalibrationSet =
        serde_json::from_value(forged).expect("structurally valid calibration JSON");
    assert!(PlattCalibrator::fit(&forged, FitConfig::default()).is_err());
    assert!(IsotonicCalibrator::fit(&forged).is_err());
}

#[test]
fn finite_subnormal_logits_do_not_overflow_fit_diagnostics() {
    let logits = [
        f64::from_bits(1),
        -2.0,
        -1.25,
        -0.5,
        0.25,
        0.75,
        1.5,
        2.25,
        -0.75,
        1.0,
    ];
    let rows = logits
        .into_iter()
        .enumerate()
        .map(|(index, logit)| {
            let origin = 100 + i64::try_from(index).expect("fixture index");
            CalibrationObservation::from_logit(
                logit,
                matches!(index, 2 | 4 | 5 | 7 | 9),
                origin,
                origin + 1,
                origin + 1,
                1.0,
            )
            .expect("valid subnormal observation")
        })
        .collect();
    let set = CalibrationSet::new(
        "subnormal-validation",
        CalibrationPeriod::new(100, 120).expect("period"),
        rows,
    )
    .expect("valid calibration set");
    let fit = PlattCalibrator::fit(&set, FitConfig::default()).expect("finite fit");
    assert!(fit.diagnostics.feature_log_scale_span.is_finite());
    assert!(
        fit.calibrate_logit(f64::from_bits(1))
            .expect("score")
            .is_finite()
    );
}

#[test]
fn rank_deficient_all_subnormal_logits_use_intercept_only_fits() {
    let rows = (0..10)
        .map(|index| {
            let origin = 100 + i64::from(index);
            CalibrationObservation::from_logit(
                f64::from_bits(u64::try_from(index + 1).expect("subnormal bits")),
                index % 2 == 0,
                origin,
                origin + 1,
                origin + 1,
                1.0,
            )
            .expect("valid subnormal observation")
        })
        .collect();
    let set = CalibrationSet::new(
        "rank-deficient-subnormal",
        CalibrationPeriod::new(100, 120).expect("period"),
        rows,
    )
    .expect("set");
    let platt = PlattCalibrator::fit(&set, FitConfig::default()).expect("Platt fallback");
    let beta = BetaCalibrator::fit(&set, FitConfig::default()).expect("beta fallback");
    assert_eq!(platt.slope, 0.0);
    assert_eq!(beta.a, 0.0);
    assert_eq!(beta.b, 0.0);
    assert!((platt.calibrate_logit(0.0).expect("Platt score") - 0.5).abs() < 1.0e-12);
    assert!((beta.calibrate_logit(0.0).expect("beta score") - 0.5).abs() < 1.0e-12);
}
