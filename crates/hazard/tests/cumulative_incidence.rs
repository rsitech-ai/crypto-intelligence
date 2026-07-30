use hazard::{BucketProbability, cumulative_incidence};

const EPSILON: f64 = 1.0e-12;

#[test]
fn cumulative_incidence_matches_a_hand_derived_two_bucket_example() {
    let buckets = vec![
        BucketProbability::try_new(vec![0.10, 0.20], 0.70)
            .expect("first bucket should be coherent"),
        BucketProbability::try_new(vec![0.20, 0.10], 0.70)
            .expect("second bucket should be coherent"),
    ];

    let result = cumulative_incidence(&buckets).expect("coherent buckets should accumulate");

    assert!((result.by_cause()[0][1] - 0.24).abs() < EPSILON);
    assert!((result.by_cause()[1][1] - 0.27).abs() < EPSILON);
    assert!((result.total_survival()[1] - 0.49).abs() < EPSILON);
}

#[test]
fn cumulative_incidence_is_monotone_for_every_cause() {
    let buckets = vec![
        BucketProbability::try_new(vec![0.05, 0.10], 0.85).expect("valid bucket"),
        BucketProbability::try_new(vec![0.20, 0.05], 0.75).expect("valid bucket"),
        BucketProbability::try_new(vec![0.10, 0.20], 0.70).expect("valid bucket"),
    ];

    let result = cumulative_incidence(&buckets).expect("coherent buckets should accumulate");

    assert!(
        result
            .by_cause()
            .iter()
            .all(|curve| curve.windows(2).all(|window| window[1] >= window[0]))
    );
}

#[test]
fn incoherent_bucket_is_rejected_instead_of_repaired() {
    assert!(BucketProbability::try_new(vec![0.8, 0.5], 0.0).is_err());
    assert!(BucketProbability::try_new(vec![0.4, 0.4], 0.1).is_err());
    assert!(BucketProbability::try_new(vec![0.4, f64::NAN], 0.6).is_err());
}

#[test]
fn an_empty_bucket_sequence_has_no_incidence_curve() {
    assert!(cumulative_incidence(&[]).is_err());
}
