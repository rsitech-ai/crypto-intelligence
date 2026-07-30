use hazard::{BucketSpec, softmax_with_survival};

const EPSILON: f64 = 1.0e-12;

#[test]
fn survival_is_the_reference_category_in_the_bucket_softmax() {
    let probability = softmax_with_survival(&[2.0_f64.ln(), 3.0_f64.ln()])
        .expect("finite logits should produce probabilities");

    assert!((probability.causes()[0] - 2.0 / 6.0).abs() < EPSILON);
    assert!((probability.causes()[1] - 3.0 / 6.0).abs() < EPSILON);
    assert!((probability.survival() - 1.0 / 6.0).abs() < EPSILON);
    assert!(
        (probability.causes().iter().sum::<f64>() + probability.survival() - 1.0).abs() < EPSILON
    );
}

#[test]
fn nonfinite_bucket_logits_are_rejected() {
    assert!(softmax_with_survival(&[0.0, f64::INFINITY]).is_err());
    assert!(softmax_with_survival(&[f64::NAN]).is_err());
}

#[test]
fn v1_bucket_contract_exposes_the_approved_forecast_horizons() {
    let buckets = BucketSpec::v1();
    assert_eq!(buckets.version(), 1);
    assert_eq!(buckets.edges_seconds(), &[900, 3_600, 14_400, 86_400]);
}
