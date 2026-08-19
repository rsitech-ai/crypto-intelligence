use hazard::{
    BucketBoundaryConvention, BucketSpec, HazardError, HorizonInterpolationPolicy,
    softmax_with_survival,
};

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

#[test]
fn production_bucket_contract_is_dense_versioned_and_contains_every_product_horizon() {
    let legacy = BucketSpec::v1();
    let production = BucketSpec::production_v2();

    assert_eq!(production.version(), 2);
    assert_ne!(production, legacy);
    assert_ne!(production.fingerprint(), legacy.fingerprint());
    assert_eq!(production.len(), 56);
    assert_eq!(
        production.forecast_horizons_seconds(),
        &[900, 3_600, 14_400, 86_400]
    );
    assert_eq!(
        production.horizon_policy(),
        HorizonInterpolationPolicy::ExactBucketEdgeOnly
    );
    assert_eq!(
        production.boundary_convention(),
        BucketBoundaryConvention::InclusiveEnd
    );
    assert_eq!(
        production.fingerprint(),
        [
            179, 185, 181, 128, 17, 202, 171, 75, 210, 132, 76, 12, 74, 1, 211, 202, 115, 219, 161,
            240, 241, 136, 162, 68, 36, 69, 124, 110, 185, 57, 210, 8,
        ]
    );
    assert_eq!(
        &production.edges_seconds()[..15],
        &(60..=900).step_by(60).collect::<Vec<_>>()
    );
    assert_eq!(production.edges_seconds()[15], 1_200);
    assert_eq!(production.edges_seconds()[23], 3_600);
    assert_eq!(production.edges_seconds()[35], 14_400);
    assert_eq!(production.edges_seconds()[55], 86_400);
    assert!(
        production
            .edges_seconds()
            .windows(2)
            .all(|pair| pair[0] < pair[1])
    );
    for horizon in production.forecast_horizons_seconds() {
        assert!(production.bucket_index_for_horizon(*horizon).is_ok());
    }
    assert_eq!(
        production.bucket_index_for_horizon(901),
        Err(HazardError::UnsupportedHorizon)
    );
}
