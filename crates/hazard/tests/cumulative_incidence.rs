use hazard::{
    BucketProbability, BucketSpec, HazardError, cumulative_incidence, cumulative_incidence_for_spec,
};

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
    for index in 0..buckets.len() {
        let mass = result
            .by_cause()
            .iter()
            .map(|curve| curve[index])
            .sum::<f64>()
            + result.total_survival()[index];
        assert!((mass - 1.0).abs() < EPSILON);
    }
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

#[test]
fn equivalent_bucket_partitioning_preserves_incidence_and_survival() {
    let single =
        cumulative_incidence(&[BucketProbability::try_new(vec![0.3], 0.7).expect("single bucket")])
            .expect("single curve");
    let conditional_survival = 0.7_f64.sqrt();
    let split = cumulative_incidence(&[
        BucketProbability::try_new(vec![1.0 - conditional_survival], conditional_survival)
            .expect("first split bucket"),
        BucketProbability::try_new(vec![1.0 - conditional_survival], conditional_survival)
            .expect("second split bucket"),
    ])
    .expect("split curve");

    assert!((single.by_cause()[0][0] - split.by_cause()[0][1]).abs() < EPSILON);
    assert!((single.total_survival()[0] - split.total_survival()[1]).abs() < EPSILON);
}

#[test]
fn long_curves_retain_log_survival_after_linear_survival_underflows() {
    let buckets = (0..4_096)
        .map(|_| BucketProbability::try_new(vec![0.5], 0.5).expect("coherent bucket"))
        .collect::<Vec<_>>();
    let result = cumulative_incidence(&buckets).expect("bounded long curve");

    assert_eq!(result.total_survival().last(), Some(&0.0));
    let log_survival = *result
        .log_total_survival()
        .last()
        .expect("log survival point");
    assert!(log_survival.is_finite());
    assert!((log_survival - 4_096.0 * 0.5_f64.ln()).abs() < 1.0e-9);
    assert!(
        result.by_cause()[0]
            .windows(2)
            .all(|pair| pair[1] >= pair[0])
    );
    assert!((result.by_cause()[0].last().expect("incidence") - 1.0).abs() < EPSILON);
}

#[test]
fn supported_horizon_extraction_is_exact_and_dimension_checked() {
    let spec = BucketSpec::production_v2();
    let buckets = (0..spec.len())
        .map(|_| BucketProbability::try_new(vec![0.01], 0.99).expect("coherent bucket"))
        .collect::<Vec<_>>();
    let cause_ids = vec!["cause_a".to_owned()];
    let result =
        cumulative_incidence_for_spec(spec, &cause_ids, &buckets).expect("production curve");
    assert_eq!(result.bucket_spec(), Some(spec));

    let at_fifteen = result.at_horizon(900).expect("supported exact horizon");
    assert_eq!(at_fifteen.causes()[0].cause_id(), "cause_a");
    assert!((at_fifteen.causes()[0].probability() - (1.0 - 0.99_f64.powi(15))).abs() < EPSILON);
    assert!((at_fifteen.survival() - 0.99_f64.powi(15)).abs() < EPSILON);
    assert_eq!(result.at_horizon(901), Err(HazardError::UnsupportedHorizon));
    assert_eq!(
        cumulative_incidence_for_spec(
            spec,
            &cause_ids,
            &[BucketProbability::try_new(vec![0.1], 0.9).expect("coherent bucket")],
        ),
        Err(HazardError::CurveDimensionMismatch)
    );
    assert_eq!(
        cumulative_incidence_for_spec(spec, &[], &buckets),
        Err(HazardError::InvalidCauseSchema)
    );
    assert_eq!(
        cumulative_incidence(&buckets)
            .expect("short curve")
            .at_horizon(900),
        Err(HazardError::UnboundBucketSpec)
    );
}

#[test]
fn absorbing_survival_remains_zero_without_nonfinite_incidence() {
    let result = cumulative_incidence(&[
        BucketProbability::try_new(vec![1.0], 0.0).expect("absorbing event"),
        BucketProbability::try_new(vec![0.5], 0.5).expect("unreachable bucket"),
    ])
    .expect("absorbing curve");

    assert_eq!(result.total_survival(), &[0.0, 0.0]);
    assert_eq!(
        result.log_total_survival(),
        &[f64::NEG_INFINITY, f64::NEG_INFINITY]
    );
    assert_eq!(result.by_cause()[0], vec![1.0, 1.0]);
}

#[test]
fn locally_tolerated_roundoff_cannot_accumulate_into_incoherent_global_mass() {
    let bucket = BucketProbability::try_new(vec![5.0e-13], 1.0)
        .expect("local floating-point tolerance permits one bucket");
    assert_eq!(
        cumulative_incidence(&vec![bucket; 4_096]),
        Err(HazardError::ProbabilityMassDrift)
    );
}
