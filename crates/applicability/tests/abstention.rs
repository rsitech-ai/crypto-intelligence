use applicability::{
    ApplicabilityPolicy, ApplicabilityThresholds, Availability, CategoricalSupport, FeatureRange,
    NoveltyProfile, PolicySignals, RobustDistribution,
};

#[test]
fn robust_distribution_matches_a_hand_derived_symmetric_reference() {
    let rows = vec![
        vec![-2.0, -1.0],
        vec![-1.0, -2.0],
        vec![1.0, 2.0],
        vec![2.0, 1.0],
    ];
    let fitted = RobustDistribution::fit(&rows, 0.10).expect("finite robust distribution");

    assert_eq!(fitted.feature_count(), 2);
    assert_eq!(fitted.row_count(), 4);
    assert!(fitted.mahalanobis_squared(&[0.0, 0.0]).unwrap() < 1.0e-20);
    assert!(fitted.nearest_neighbor_distance(&[0.0, 0.0]).unwrap() > 0.0);
    assert!(fitted.mahalanobis_squared(&[100.0, -100.0]).unwrap() > 100.0);

    let repeated = RobustDistribution::fit(&rows, 0.10).expect("deterministic refit");
    assert_eq!(fitted, repeated);
}

#[test]
fn invalid_or_singular_distribution_fails_closed() {
    assert!(RobustDistribution::fit(&[], 0.10).is_err());
    assert!(RobustDistribution::fit(&[vec![1.0], vec![1.0], vec![1.0]], 0.10).is_err());
    assert!(RobustDistribution::fit(&[vec![0.0], vec![f64::NAN]], 0.10).is_err());
    assert!(RobustDistribution::fit(&[vec![0.0], vec![1.0]], -1.0).is_err());
}

#[test]
fn novelty_profile_reports_range_category_venue_product_and_missingness() {
    let profile = NoveltyProfile::try_new(
        vec![
            FeatureRange::try_new("spread", true, -3.0, 3.0).unwrap(),
            FeatureRange::try_new("optional_flow", false, -2.0, 2.0).unwrap(),
            FeatureRange::try_new("depth", true, -4.0, 4.0).unwrap(),
        ],
        vec![CategoricalSupport::try_new("regime", ["calm", "stress"]).unwrap()],
        ["binance", "kraken"],
        ["spot", "perpetual"],
    )
    .expect("novelty profile");

    let healthy = profile
        .evaluate(
            &[Some(0.0), Some(1.0), Some(2.0)],
            &[("regime", "calm")],
            "kraken",
            "spot",
        )
        .expect("healthy evaluation");
    assert!(healthy.reasons.is_empty());
    assert!(!healthy.required_missing);

    let novel = profile
        .evaluate(
            &[None, None, Some(100.0)],
            &[("regime", "unknown")],
            "unsupported",
            "option",
        )
        .expect("explicit novelty");
    assert!(novel.required_missing);
    assert!(novel.optional_missing);
    assert!(novel.out_of_range);
    assert!(novel.categorical_novelty);
    assert!(novel.unsupported_venue);
    assert!(novel.unsupported_product);
    assert_eq!(novel.reasons.len(), 6);

    let extra_category = profile
        .evaluate(
            &[Some(0.0), Some(1.0), Some(2.0)],
            &[("regime", "calm"), ("new_field", "new_value")],
            "kraken",
            "spot",
        )
        .expect("extra category is an explicit novelty result");
    assert!(extra_category.categorical_novelty);
    assert!(
        extra_category
            .reasons
            .contains(&"categorical_novelty:new_field".to_owned())
    );
}

#[test]
fn policy_uses_the_approved_fail_closed_precedence() {
    let policy =
        ApplicabilityPolicy::try_new(ApplicabilityThresholds::default()).expect("valid policy");
    let mut signals = PolicySignals::healthy();
    assert_eq!(policy.decide(signals).availability, Availability::Available);

    signals.degraded = true;
    assert_eq!(policy.decide(signals).availability, Availability::Degraded);
    signals.experimental = true;
    assert_eq!(
        policy.decide(signals).availability,
        Availability::Experimental
    );
    signals.out_of_distribution = true;
    assert_eq!(
        policy.decide(signals).availability,
        Availability::OutOfDistribution
    );
    signals.insufficient_data = true;
    assert_eq!(
        policy.decide(signals).availability,
        Availability::InsufficientData
    );
    signals.source_unhealthy = true;
    assert_eq!(
        policy.decide(signals).availability,
        Availability::SourceUnhealthy
    );
    signals.model_incompatible = true;
    assert_eq!(
        policy.decide(signals).availability,
        Availability::ModelIncompatible
    );
}

#[test]
fn availability_wire_names_are_exact_and_complete() {
    assert_eq!(
        Availability::ALL.map(Availability::as_str),
        [
            "available",
            "degraded",
            "experimental",
            "out_of_distribution",
            "insufficient_data",
            "source_unhealthy",
            "model_incompatible",
        ]
    );
}
