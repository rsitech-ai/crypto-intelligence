use changepoint::{
    Bocpd, BocpdConfig, BocpdError, BocpdObservation, NormalInverseGamma, QualityState, ResetPolicy,
};

const TOLERANCE: f64 = 1e-12;

#[test]
fn nig_update_and_student_t_predictive_match_literal_reference() {
    // This catches swapped NIG parameters, a missing kappa factor, and a
    // Gaussian plug-in density replacing the Student-t predictive.
    let prior = NormalInverseGamma::try_new(0.0, 1.0, 1.0, 1.0).expect("valid prior");
    assert!(
        (prior
            .log_predictive_density(2.0)
            .expect("finite Student-t density")
            - -2.426_015_131_959_809)
            .abs()
            < TOLERANCE
    );

    let posterior = prior.updated(2.0).expect("finite conjugate update");
    assert_eq!(posterior.mu(), 1.0);
    assert_eq!(posterior.kappa(), 2.0);
    assert_eq!(posterior.alpha(), 1.5);
    assert_eq!(posterior.beta(), 2.0);
}

#[test]
fn run_length_posterior_matches_independent_reference_vector() {
    // This catches scoring after updating sufficient statistics, omitting the
    // predictive likelihood, or applying the hazard to the wrong branch.
    let mut model = fixture(16);
    for (index, value) in [0.0, 0.0, 3.0].into_iter().enumerate() {
        model
            .update(observation(
                index as u64 + 1,
                value,
                QualityState::Trusted,
                1,
            ))
            .expect("reference update");
    }
    let output = model.current_output();
    let expected = [
        0.1,
        0.250_687_471_128_382_13,
        0.085_348_717_516_672_56,
        0.563_963_811_354_945_6,
    ];

    assert_eq!(output.posterior.len(), expected.len());
    for (actual, expected) in output.posterior.iter().zip(expected) {
        assert!((actual - expected).abs() < TOLERANCE, "{output:?}");
    }
    assert!((output.expected_run_length - 2.113_276_340_226_563_7).abs() < TOLERANCE);
    assert!((output.run_length_entropy - 1.110_159_598_149_979).abs() < TOLERANCE);
    assert!((output.reset_probability - 0.1).abs() < TOLERANCE);
}

#[test]
fn abrupt_shift_raises_recent_changepoint_probability_without_mislabeling_reset_mass() {
    // With a constant hazard, exact P(r_t=0) is the hazard. The data-sensitive
    // signal is posterior mass within the versioned recent-change window.
    let mut model = fixture(128);
    for (index, value) in [0.0; 50].into_iter().enumerate() {
        model
            .update(observation(
                index as u64 + 1,
                value,
                QualityState::Trusted,
                1,
            ))
            .expect("stable update");
    }
    let before = model.current_output();
    let shifted = model
        .update(observation(51, 8.0, QualityState::Degraded, 1))
        .expect("finite shift");

    assert!(shifted.changepoint_probability > before.changepoint_probability);
    assert!((shifted.reset_probability - 0.1).abs() < TOLERANCE);
    assert_eq!(shifted.quality, QualityState::Degraded);
}

#[test]
fn hard_tail_truncation_stays_bounded_normalized_and_explicit() {
    // This catches dropping tail evidence without disclosure, growing state
    // past the configured cap, and normalization drift on a long stream.
    let mut model = fixture(8);
    let mut saw_discarded_tail = false;
    for index in 0..500 {
        let output = model
            .update(observation(
                index + 1,
                index as f64 / 100.0,
                QualityState::Trusted,
                1,
            ))
            .expect("bounded long-stream update");
        assert!(output.posterior.len() <= 9);
        assert!((output.posterior.iter().sum::<f64>() - 1.0).abs() < TOLERANCE);
        assert!(
            output
                .posterior
                .iter()
                .all(|value| value.is_finite() && *value >= 0.0)
        );
        assert!(output.expected_run_length.is_finite());
        assert!(output.run_length_entropy.is_finite());
        saw_discarded_tail |= output.discarded_tail_probability > 0.0;
    }
    assert!(saw_discarded_tail);
}

#[test]
fn manual_reset_restores_prior_state_but_remains_visible_in_evidence() {
    // This catches resets that retain old sufficient statistics, silently
    // alter configuration identity, or become invisible in the evidence hash.
    let mut model = fixture(32);
    let model_id = model.model_id();
    let initial_evidence = model.current_output().evidence_id;
    model
        .update(observation(1, 1.5, QualityState::Trusted, 1))
        .expect("pre-reset update");
    let reset = model.reset().expect("manual reset");

    assert_eq!(model.model_id(), model_id);
    assert_eq!(reset.posterior, vec![1.0]);
    assert_eq!(reset.observation_count, 0);
    assert_eq!(reset.reset_count, 1);
    assert_eq!(reset.quality, QualityState::Unavailable);
    assert_ne!(reset.evidence_id, initial_evidence);
}

#[test]
fn invalid_configuration_and_failed_updates_are_fail_closed() {
    // This catches inclusive probability boundaries, unbounded allocation,
    // untrusted quality admission, and partial mutation on numerical failure.
    assert_eq!(
        NormalInverseGamma::try_new(0.0, 0.0, 1.0, 1.0),
        Err(BocpdError::InvalidPrior)
    );
    let prior = NormalInverseGamma::try_new(0.0, 1.0, 1.0, 1.0).expect("valid prior");
    for hazard in [0.0, 1.0, -0.1, f64::NAN] {
        assert_eq!(
            BocpdConfig::try_new(
                prior,
                hazard,
                32,
                1,
                "standardized_returns",
                ResetPolicy::Manual
            ),
            Err(BocpdError::InvalidConfiguration)
        );
    }
    assert_eq!(
        BocpdConfig::try_new(
            prior,
            0.1,
            8,
            9,
            "standardized_returns",
            ResetPolicy::Manual
        ),
        Err(BocpdError::InvalidConfiguration)
    );

    let mut model = fixture(32);
    let before = model.current_output();
    assert_eq!(
        BocpdObservation::try_new(1, 10, 10, f64::NAN, QualityState::Trusted, [1; 32]),
        Err(BocpdError::NonFiniteObservation)
    );
    assert_eq!(
        BocpdObservation::try_new(1, 10, 10, 1.0, QualityState::Unavailable, [1; 32]),
        Err(BocpdError::UnavailableQuality)
    );
    for invalid in [
        BocpdObservation::try_new(0, 10, 10, 1.0, QualityState::Trusted, [1; 32]),
        BocpdObservation::try_new(1, 0, 10, 1.0, QualityState::Trusted, [1; 32]),
        BocpdObservation::try_new(1, 10, 9, 1.0, QualityState::Trusted, [1; 32]),
        BocpdObservation::try_new(1, 10, 10, 1.0, QualityState::Trusted, [0; 32]),
    ] {
        assert_eq!(invalid, Err(BocpdError::InvalidObservation));
    }
    assert_eq!(
        model.update(observation(1, f64::MAX, QualityState::Trusted, 1)),
        Err(BocpdError::NumericalFailure)
    );
    assert_eq!(model.current_output(), before);

    model
        .update(observation(1, 0.0, QualityState::Trusted, 1))
        .expect("first ordered observation");
    let ordered = model.current_output();
    assert_eq!(
        model.update(observation(1, 0.1, QualityState::Trusted, 1)),
        Err(BocpdError::NonMonotonicObservation)
    );
    assert_eq!(model.current_output(), ordered);
}

#[test]
fn model_identity_versions_every_behavioral_configuration_field() {
    // This catches an artifact identity that omits hazard, cap, recent-change
    // semantics, feature family, prior, or reset policy.
    let prior = NormalInverseGamma::try_new(0.0, 1.0, 1.0, 1.0).expect("valid prior");
    let base = config(prior, 0.1, 32, 1, "standardized_returns");
    let variants = [
        config(prior, 0.2, 32, 1, "standardized_returns"),
        config(prior, 0.1, 16, 1, "standardized_returns"),
        config(prior, 0.1, 32, 2, "standardized_returns"),
        config(prior, 0.1, 32, 1, "realized_volatility"),
        config(
            NormalInverseGamma::try_new(1.0, 1.0, 1.0, 1.0).expect("valid alternate prior"),
            0.1,
            32,
            1,
            "standardized_returns",
        ),
    ];
    let base_id = Bocpd::try_new(base).expect("base model").model_id();
    assert!(
        variants.into_iter().all(|variant| {
            Bocpd::try_new(variant).expect("variant model").model_id() != base_id
        })
    );
}

#[test]
fn evidence_identity_binds_point_in_time_lineage_without_changing_posterior_math() {
    // This catches evidence hashes that bind only numerical values while
    // ignoring the source observation, knowledge time, or lineage DAG.
    let mut left = fixture(32);
    let mut right = fixture(32);
    let left_output = left
        .update(observation(1, 0.25, QualityState::Trusted, 1))
        .expect("left observation");
    let right_output = right
        .update(observation(1, 0.25, QualityState::Trusted, 2))
        .expect("right observation");

    assert_eq!(left_output.posterior, right_output.posterior);
    assert_eq!(
        left_output.changepoint_probability,
        right_output.changepoint_probability
    );
    assert_ne!(left_output.evidence_id, right_output.evidence_id);
    assert_eq!(left_output.last_observation_id, Some(1));
    assert_eq!(left_output.last_event_time_ns, Some(10));
    assert_eq!(left_output.last_as_known_at_ns, Some(10));
}

fn fixture(max_run_length: usize) -> Bocpd {
    let prior = NormalInverseGamma::try_new(0.0, 1.0, 1.0, 1.0).expect("valid prior");
    Bocpd::try_new(config(
        prior,
        0.1,
        max_run_length,
        1,
        "standardized_returns",
    ))
    .expect("valid detector")
}

fn config(
    prior: NormalInverseGamma,
    hazard: f64,
    max_run_length: usize,
    changepoint_window: usize,
    feature_family: &str,
) -> BocpdConfig {
    BocpdConfig::try_new(
        prior,
        hazard,
        max_run_length,
        changepoint_window,
        feature_family,
        ResetPolicy::Manual,
    )
    .expect("valid configuration")
}

fn observation(id: u64, value: f64, quality: QualityState, lineage_byte: u8) -> BocpdObservation {
    let time_ns = i64::try_from(id).expect("bounded fixture id") * 10;
    BocpdObservation::try_new(id, time_ns, time_ns, value, quality, [lineage_byte; 32])
        .expect("valid point-in-time observation")
}
