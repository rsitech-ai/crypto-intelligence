use hazard::{
    BucketSpec, CandidateHazardArtifact, CompetingRiskHazard, FeatureSchema, HazardConfig,
    HazardConfigInput, HazardOutcome, HazardPredictionInput, HazardSampleInput, HazardTrainingSet,
    HazardTrainingSetInput, InputQuality, fit_regularization_path,
};

const SECOND_NS: i64 = 1_000_000_000;
const EPSILON: f64 = 2.0e-4;

fn config(lambda: f64, max_iterations: u32) -> HazardConfig {
    HazardConfig::try_new(HazardConfigInput {
        lambda,
        alpha: 0.5,
        max_iterations,
        tolerance: 1.0e-7,
        initial_step: 1.0,
        minimum_step: 1.0e-12,
        max_backtracking: 64,
        work_limit: 500_000_000,
        minimum_at_risk_rows_per_bucket: 1,
    })
    .expect("fixture config should be valid")
}

fn sample(id: u64, features: Vec<f64>, outcome: HazardOutcome) -> HazardSampleInput {
    let observed_seconds = match outcome {
        HazardOutcome::Event { offset_seconds, .. } => offset_seconds,
        HazardOutcome::RightCensored { observed_seconds } => observed_seconds,
    };
    let origin_time_ns = id as i64 * SECOND_NS;
    HazardSampleInput {
        id,
        origin_time_ns,
        feature_as_known_at_ns: origin_time_ns,
        outcome_known_at_ns: origin_time_ns + observed_seconds as i64 * SECOND_NS,
        features,
        quality: InputQuality::Available,
        feature_lineage: [id as u8; 32],
        label_lineage: [(id + 64) as u8; 32],
        outcome,
    }
}

fn training_set(feature_ids: &[&str], samples: Vec<HazardSampleInput>) -> HazardTrainingSet {
    training_set_with_spec(feature_ids, BucketSpec::v1(), samples)
}

fn training_set_with_spec(
    feature_ids: &[&str],
    bucket_spec: BucketSpec,
    samples: Vec<HazardSampleInput>,
) -> HazardTrainingSet {
    HazardTrainingSet::try_new(HazardTrainingSetInput {
        feature_schema: FeatureSchema::try_new(
            1,
            feature_ids
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
        )
        .expect("fixture schema should be valid"),
        bucket_spec,
        cause_ids: vec!["cause_a".to_owned(), "cause_b".to_owned()],
        cause_definition_hashes: vec![[1; 32], [2; 32]],
        training_cutoff_ns: 100_000 * SECOND_NS,
        samples,
    })
    .expect("fixture training set should be valid")
}

fn intercept_reference_fixture() -> HazardTrainingSet {
    let outcomes = [
        HazardOutcome::Event {
            offset_seconds: 100,
            cause_index: 0,
        },
        HazardOutcome::Event {
            offset_seconds: 200,
            cause_index: 1,
        },
        HazardOutcome::Event {
            offset_seconds: 1_000,
            cause_index: 0,
        },
        HazardOutcome::Event {
            offset_seconds: 2_000,
            cause_index: 1,
        },
        HazardOutcome::Event {
            offset_seconds: 5_000,
            cause_index: 0,
        },
        HazardOutcome::Event {
            offset_seconds: 8_000,
            cause_index: 1,
        },
        HazardOutcome::Event {
            offset_seconds: 20_000,
            cause_index: 0,
        },
        HazardOutcome::Event {
            offset_seconds: 40_000,
            cause_index: 1,
        },
        HazardOutcome::RightCensored {
            observed_seconds: 86_400,
        },
        HazardOutcome::RightCensored {
            observed_seconds: 86_400,
        },
    ];
    training_set(
        &["constant_feature"],
        outcomes
            .into_iter()
            .enumerate()
            .map(|(index, outcome)| sample(index as u64 + 1, vec![7.0], outcome))
            .collect(),
    )
}

fn separable_fixture() -> HazardTrainingSet {
    let mut samples = Vec::new();
    for index in 0..20_u64 {
        let magnitude = 1.0 + index as f64 / 20.0;
        let event_offset = [300, 1_000, 5_000, 20_000][index as usize % 4];
        samples.push(sample(
            index * 3 + 1,
            vec![magnitude, 0.2],
            HazardOutcome::Event {
                offset_seconds: event_offset,
                cause_index: 0,
            },
        ));
        samples.push(sample(
            index * 3 + 2,
            vec![-magnitude, -0.2],
            HazardOutcome::Event {
                offset_seconds: event_offset,
                cause_index: 1,
            },
        ));
        samples.push(sample(
            index * 3 + 3,
            vec![0.0, index as f64 / 20.0 - 0.5],
            HazardOutcome::RightCensored {
                observed_seconds: 86_400,
            },
        ));
    }
    training_set(&["signed_pressure", "secondary"], samples)
}

fn prediction_input(schema: &FeatureSchema, features: Vec<f64>) -> HazardPredictionInput {
    HazardPredictionInput {
        entity_id: "asset".to_owned(),
        episode_cluster_id: "episode".to_owned(),
        feature_schema: schema.clone(),
        origin_time_ns: 200_000 * SECOND_NS,
        as_known_at_ns: 200_000 * SECOND_NS,
        features,
        quality: InputQuality::Available,
        feature_lineage: [77; 32],
    }
}

#[test]
fn constant_feature_fit_matches_hand_derived_bucket_frequencies() {
    let data = intercept_reference_fixture();
    let model = CompetingRiskHazard::fit(&data, config(0.0, 20_000))
        .expect("bounded convex fit should complete");
    assert!(model.diagnostics().converged());
    assert_eq!(model.normalization().means(), &[7.0]);
    assert_eq!(model.normalization().scales(), &[1.0]);
    assert_eq!(model.normalization().constant_features(), &[true]);

    let prediction = model
        .predict(prediction_input(data.feature_schema(), vec![7.0]))
        .expect("schema-compatible prediction should succeed");
    let expected = [
        [0.10, 0.10, 0.80],
        [0.125, 0.125, 0.75],
        [1.0 / 6.0, 1.0 / 6.0, 2.0 / 3.0],
        [0.25, 0.25, 0.50],
    ];
    for (bucket, expected_bucket) in prediction.buckets().iter().zip(expected) {
        assert!((bucket.causes()[0] - expected_bucket[0]).abs() < EPSILON);
        assert!((bucket.causes()[1] - expected_bucket[1]).abs() < EPSILON);
        assert!((bucket.survival() - expected_bucket[2]).abs() < EPSILON);
    }
}

#[test]
fn deterministic_fit_discriminates_causes_and_emits_coherent_curves() {
    let data = separable_fixture();
    let first =
        CompetingRiskHazard::fit(&data, config(0.01, 20_000)).expect("first fit should complete");
    let second =
        CompetingRiskHazard::fit(&data, config(0.01, 20_000)).expect("second fit should complete");
    let support_two_config = HazardConfig::try_new(HazardConfigInput {
        minimum_at_risk_rows_per_bucket: 2,
        ..HazardConfigInput::from(config(0.01, 20_000))
    })
    .expect("higher support threshold");
    let support_two = CompetingRiskHazard::fit(&data, support_two_config)
        .expect("fixture supports two rows per bucket");

    assert!(
        first.diagnostics().converged(),
        "unexpected diagnostics: {:?}",
        first.diagnostics()
    );
    assert_eq!(first.model_id(), second.model_id());
    assert_eq!(first.coefficients(), second.coefficients());
    assert_eq!(first.coefficients(), support_two.coefficients());
    assert_ne!(first.model_id(), support_two.model_id());
    let candidate =
        CandidateHazardArtifact::try_new(&first).expect("a converged fit may become a candidate");
    assert_eq!(candidate.model_id(), first.model_id());

    let positive = first
        .predict(prediction_input(data.feature_schema(), vec![1.5, 0.2]))
        .expect("positive prediction should succeed");
    let negative = first
        .predict(prediction_input(data.feature_schema(), vec![-1.5, -0.2]))
        .expect("negative prediction should succeed");
    assert_eq!(positive.cause_ids(), &["cause_a", "cause_b"]);
    assert_eq!(positive.bucket_spec(), BucketSpec::v1());
    assert_eq!(
        positive.bucket_edges_seconds(),
        &[900, 3_600, 14_400, 86_400]
    );
    assert_eq!(
        positive.forecast_horizons_seconds(),
        &[900, 3_600, 14_400, 86_400]
    );
    let at_fifteen = positive.at_horizon(900).expect("exact product horizon");
    assert_eq!(at_fifteen.horizon_seconds(), 900);
    assert_eq!(at_fifteen.bucket_spec(), BucketSpec::v1());
    assert_eq!(at_fifteen.model_id(), positive.model_id());
    assert_eq!(at_fifteen.evidence_id(), positive.evidence_id());
    assert_eq!(at_fifteen.causes()[0].cause_id(), "cause_a");
    assert_eq!(at_fifteen.causes()[1].cause_id(), "cause_b");
    assert_eq!(
        positive.cumulative_incidence().bucket_spec(),
        Some(BucketSpec::v1())
    );
    assert!(positive.buckets()[0].causes()[0] > positive.buckets()[0].causes()[1]);
    assert!(negative.buckets()[0].causes()[1] > negative.buckets()[0].causes()[0]);
    assert!(
        positive
            .cumulative_incidence()
            .by_cause()
            .iter()
            .all(|curve| curve.windows(2).all(|window| window[1] >= window[0]))
    );
}

#[test]
fn normalization_is_frozen_from_training_and_prediction_cannot_mutate_it() {
    let data = separable_fixture();
    let model = CompetingRiskHazard::fit(&data, config(0.01, 20_000)).expect("fit should complete");
    let before = model.normalization().clone();

    model
        .predict(prediction_input(
            data.feature_schema(),
            vec![1_000.0, -1_000.0],
        ))
        .expect("finite held-out values should be normalized by training statistics");

    assert_eq!(model.normalization(), &before);
}

#[test]
fn prediction_identity_binds_lineage_without_changing_the_math() {
    let data = separable_fixture();
    let model = CompetingRiskHazard::fit(&data, config(0.01, 20_000)).expect("fit should complete");
    let first = model
        .predict(prediction_input(data.feature_schema(), vec![1.0, 0.0]))
        .expect("first prediction should succeed");
    let mut changed_input = prediction_input(data.feature_schema(), vec![1.0, 0.0]);
    changed_input.feature_lineage = [88; 32];
    let changed = model
        .predict(changed_input)
        .expect("changed lineage remains structurally valid");

    assert_eq!(first.buckets(), changed.buckets());
    assert_ne!(first.evidence_id(), changed.evidence_id());
}

#[test]
fn stronger_regularization_shrinks_coefficients_and_path_is_deterministic() {
    let data = separable_fixture();
    let low = config(0.001, 20_000);
    let high = config(0.5, 20_000);
    let path = fit_regularization_path(&data, &[low.clone(), high.clone()])
        .expect("bounded path should fit");
    let repeated = fit_regularization_path(&data, &[low, high]).expect("path should repeat");

    assert_eq!(path.len(), 2);
    assert!(path[1].coefficient_l1_norm() < path[0].coefficient_l1_norm());
    assert_eq!(
        path.iter()
            .map(|model| model.model_id())
            .collect::<Vec<_>>(),
        repeated
            .iter()
            .map(|model| model.model_id())
            .collect::<Vec<_>>()
    );
}

#[test]
fn nonconvergence_remains_inspectable_but_cannot_become_a_candidate() {
    let data = separable_fixture();
    let model = CompetingRiskHazard::fit(&data, config(0.01, 1))
        .expect("iteration exhaustion should return diagnostics");

    assert!(!model.diagnostics().converged());
    assert!(CandidateHazardArtifact::try_new(&model).is_err());
}

#[test]
fn invalid_prediction_and_work_budget_fail_closed() {
    let data = separable_fixture();
    let model = CompetingRiskHazard::fit(&data, config(0.01, 20_000)).expect("fit should complete");

    let mut wrong_schema = data.feature_schema().clone();
    wrong_schema = FeatureSchema::try_new(
        wrong_schema.version() + 1,
        wrong_schema.feature_ids().to_vec(),
    )
    .expect("alternate schema should be structurally valid");
    assert!(
        model
            .predict(prediction_input(&wrong_schema, vec![1.0, 0.0]))
            .is_err()
    );
    let mut unavailable = prediction_input(data.feature_schema(), vec![1.0, 0.0]);
    unavailable.quality = InputQuality::Unavailable;
    assert!(model.predict(unavailable).is_err());

    let tiny_budget = HazardConfig::try_new(HazardConfigInput {
        work_limit: 1,
        ..HazardConfigInput::from(config(0.01, 10))
    })
    .expect("a small positive budget is a valid config");
    assert_eq!(
        CompetingRiskHazard::fit(&data, tiny_budget).unwrap_err(),
        hazard::HazardError::WorkCapacity
    );
}

#[test]
fn configuration_rejects_nonfinite_derived_penalty_steps() {
    let invalid = HazardConfig::try_new(HazardConfigInput {
        lambda: f64::MAX,
        alpha: 0.0,
        max_iterations: 10,
        tolerance: 1.0e-6,
        initial_step: f64::MAX,
        minimum_step: 1.0,
        max_backtracking: 10,
        work_limit: 1_000_000,
        minimum_at_risk_rows_per_bucket: 1,
    });

    assert_eq!(invalid.unwrap_err(), hazard::HazardError::InvalidConfig);
    let zero_support = HazardConfig::try_new(HazardConfigInput {
        minimum_at_risk_rows_per_bucket: 0,
        ..HazardConfigInput::from(config(0.01, 10))
    });
    assert_eq!(
        zero_support.unwrap_err(),
        hazard::HazardError::InvalidConfig
    );
}

#[test]
fn unsupported_production_buckets_fail_before_fit_and_supported_v2_predicts_all_horizons() {
    let spec = BucketSpec::production_v2();
    let partial = training_set_with_spec(
        &["constant_feature"],
        spec,
        vec![sample(
            1,
            vec![1.0],
            HazardOutcome::RightCensored {
                observed_seconds: 900,
            },
        )],
    );
    assert_eq!(
        CompetingRiskHazard::fit(&partial, config(0.01, 100)).unwrap_err(),
        hazard::HazardError::InsufficientBucketSupport
    );

    let mut samples = Vec::new();
    let mut id = 1_u64;
    for edge in spec.edges_seconds() {
        for cause_index in 0..2 {
            samples.push(sample(
                id,
                vec![1.0],
                HazardOutcome::Event {
                    offset_seconds: *edge,
                    cause_index,
                },
            ));
            id += 1;
        }
    }
    samples.push(sample(
        id,
        vec![1.0],
        HazardOutcome::RightCensored {
            observed_seconds: 86_400,
        },
    ));
    let supported = training_set_with_spec(&["constant_feature"], spec, samples);
    let production_config = HazardConfig::try_new(HazardConfigInput {
        tolerance: 1.0e-4,
        work_limit: 100_000_000_000,
        ..HazardConfigInput::from(config(0.0, 5_000))
    })
    .expect("production fixture config");
    let model = CompetingRiskHazard::fit(&supported, production_config)
        .expect("fully supported production grid should fit");
    let prediction = model
        .predict(prediction_input(supported.feature_schema(), vec![1.0]))
        .expect("production prediction");
    assert_eq!(prediction.bucket_spec(), spec);
    assert_eq!(prediction.buckets().len(), spec.len());
    for horizon in spec.forecast_horizons_seconds() {
        assert!(prediction.at_horizon(*horizon).is_ok());
    }
}

#[test]
fn regularization_path_rejects_excess_aggregate_work_before_fitting() {
    let data = separable_fixture();
    let configs = vec![config(0.01, 20_000); 32];

    assert_eq!(
        fit_regularization_path(&data, &configs).unwrap_err(),
        hazard::HazardError::WorkCapacity
    );
}
