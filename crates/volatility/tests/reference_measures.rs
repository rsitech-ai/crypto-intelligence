use volatility::{
    MeasureError, OhlcBar, SeasonalBaseline, bipower_variation, downside_semivariance,
    forecast_residual, garman_klass_variance, jump_variation, parkinson_variance,
    realized_correlation, realized_covariance, realized_variance, realized_volatility,
    sample_correlation, sample_covariance, seasonality_adjusted_volatility, upside_semivariance,
    volatility_of_volatility, volatility_term_ratio,
};

const EPSILON: f64 = 1e-12;

#[test]
fn reference_return_path_matches_hand_calculation() {
    let returns = [0.01, -0.02, 0.03];

    assert!((realized_variance(&returns).expect("returns are valid") - 0.0014).abs() < EPSILON);
    assert!(
        (realized_volatility(&returns).expect("history is sufficient") - 0.037_416_573_867_739_42)
            .abs()
            < EPSILON
    );
    assert!((downside_semivariance(&returns).expect("returns are valid") - 0.0004).abs() < EPSILON);
    assert!((upside_semivariance(&returns).expect("returns are valid") - 0.001).abs() < EPSILON);
    assert!(
        (bipower_variation(&returns).expect("history is sufficient") - 0.001_256_637_061_435_917_2)
            .abs()
            < EPSILON
    );
    assert!(
        (jump_variation(&returns).expect("history is sufficient") - 0.000_143_362_938_564_082_83)
            .abs()
            < EPSILON
    );
}

#[test]
fn invalid_nonfinite_and_overflowing_inputs_never_publish_a_value() {
    for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(
            realized_variance(&[invalid]),
            Err(MeasureError::InvalidInput)
        );
        assert_eq!(
            downside_semivariance(&[-invalid.abs()]),
            Err(MeasureError::InvalidInput)
        );
        assert_eq!(
            upside_semivariance(&[invalid]),
            Err(MeasureError::InvalidInput)
        );
        assert_eq!(
            realized_volatility(&[invalid, 0.0]),
            Err(MeasureError::InvalidInput)
        );
    }
    assert_eq!(
        realized_variance(&[f64::MAX]),
        Err(MeasureError::AnalyticalUnavailable)
    );
    assert_eq!(
        downside_semivariance(&[-f64::MAX]),
        Err(MeasureError::AnalyticalUnavailable)
    );
    assert_eq!(
        upside_semivariance(&[f64::MAX]),
        Err(MeasureError::AnalyticalUnavailable)
    );
    assert_eq!(
        realized_volatility(&[f64::MAX, 0.0]),
        Err(MeasureError::AnalyticalUnavailable)
    );
    assert_eq!(
        bipower_variation(&[0.01]),
        Err(MeasureError::InsufficientHistory)
    );
    assert_eq!(
        volatility_of_volatility(&[0.1]),
        Err(MeasureError::InsufficientHistory)
    );
}

#[test]
fn flat_market_is_valid_only_with_sufficient_history() {
    assert_eq!(realized_variance(&[0.0, 0.0]), Ok(0.0));
    assert_eq!(realized_volatility(&[0.0, 0.0]), Ok(0.0));
    assert_eq!(bipower_variation(&[0.0, 0.0]), Ok(0.0));
    assert_eq!(jump_variation(&[0.0, 0.0]), Ok(0.0));
}

#[test]
fn jump_variation_clamps_negative_roundoff_or_continuous_excess_to_zero() {
    let continuous_like = [1.0, 1.0, 1.0];
    assert!(
        bipower_variation(&continuous_like).expect("history is sufficient")
            > realized_variance(&continuous_like).expect("history is sufficient")
    );
    assert_eq!(jump_variation(&continuous_like), Ok(0.0));
}

#[test]
fn range_estimators_match_reference_ohlc_and_reject_impossible_bars() {
    let bar = OhlcBar::try_new(100.0, 110.0, 90.0, 105.0).expect("OHLC is valid");
    assert!(
        (parkinson_variance(&[bar]).expect("bar is valid") - 0.014_523_873_553_353_111).abs()
            < EPSILON
    );
    assert!(
        (garman_klass_variance(&[bar]).expect("bar is valid") - 0.019_214_797_961_641_29).abs()
            < EPSILON
    );

    assert_eq!(
        OhlcBar::try_new(100.0, 99.0, 90.0, 98.0),
        Err(MeasureError::InvalidOhlc)
    );
    assert_eq!(
        OhlcBar::try_new(100.0, 110.0, 0.0, 105.0),
        Err(MeasureError::InvalidOhlc)
    );
}

#[test]
fn seasonality_term_structure_and_residuals_are_fail_closed() {
    let baseline =
        SeasonalBaseline::try_new(1.5, 100, 110, [7; 32]).expect("baseline is point-in-time valid");
    assert!(
        (seasonality_adjusted_volatility(0.3, baseline, 120).expect("factor is valid") - 0.2).abs()
            < EPSILON
    );
    assert_eq!(
        seasonality_adjusted_volatility(0.3, baseline, 105),
        Err(MeasureError::InvalidParameter)
    );
    assert_eq!(
        SeasonalBaseline::try_new(0.0, 100, 110, [7; 32]),
        Err(MeasureError::InvalidParameter)
    );
    assert_eq!(volatility_term_ratio(0.4, 0.2), Ok(2.0));
    assert_eq!(
        volatility_term_ratio(0.4, 0.0),
        Err(MeasureError::InvalidParameter)
    );
    assert!((forecast_residual(0.3, 0.2).expect("inputs are valid") - 0.1).abs() < EPSILON);
}

#[test]
fn covariance_correlation_and_volatility_of_volatility_use_sample_statistics() {
    let left = [1.0, 2.0, 3.0];
    let right = [2.0, 4.0, 6.0];
    assert!((sample_covariance(&left, &right).expect("samples align") - 2.0).abs() < EPSILON);
    assert!((sample_correlation(&left, &right).expect("samples align") - 1.0).abs() < EPSILON);
    assert!((realized_covariance(&left, &right).expect("returns align") - 28.0).abs() < EPSILON);
    assert!((realized_correlation(&left, &right).expect("returns align") - 1.0).abs() < EPSILON);
    assert!(
        (volatility_of_volatility(&left).expect("history is sufficient") - 1.0).abs() < EPSILON
    );
    assert_eq!(
        sample_covariance(&left, &[1.0, 2.0]),
        Err(MeasureError::LengthMismatch)
    );
    assert_eq!(
        sample_correlation(&[1.0, 1.0], &[2.0, 3.0]),
        Err(MeasureError::ZeroVariance)
    );
}

#[test]
fn analytical_input_capacity_is_explicit() {
    let oversized = vec![0.0; volatility::MAX_MEASURE_OBSERVATIONS + 1];
    assert_eq!(
        realized_variance(&oversized),
        Err(MeasureError::CapacityExceeded)
    );
    assert_eq!(
        bipower_variation(&oversized),
        Err(MeasureError::CapacityExceeded)
    );
}
