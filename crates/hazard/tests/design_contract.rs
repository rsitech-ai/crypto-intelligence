use hazard::{
    BucketSpec, BucketTarget, FeatureSchema, HazardOutcome, HazardSampleInput, HazardTrainingSet,
    HazardTrainingSetInput, InputQuality, augment_design,
};

const SECOND_NS: i64 = 1_000_000_000;

fn available_sample(id: u64, origin_seconds: i64, outcome: HazardOutcome) -> HazardSampleInput {
    let observed_seconds = match outcome {
        HazardOutcome::Event { offset_seconds, .. } => offset_seconds,
        HazardOutcome::RightCensored { observed_seconds } => observed_seconds,
    };
    let origin_time_ns = origin_seconds * SECOND_NS;
    HazardSampleInput {
        id,
        origin_time_ns,
        feature_as_known_at_ns: origin_time_ns,
        outcome_known_at_ns: origin_time_ns + observed_seconds as i64 * SECOND_NS,
        features: vec![1.0, -2.0],
        quality: InputQuality::Available,
        feature_lineage: [id as u8; 32],
        label_lineage: [(id + 8) as u8; 32],
        outcome,
    }
}

fn training_set(samples: Vec<HazardSampleInput>) -> Result<HazardTrainingSet, hazard::HazardError> {
    training_set_with_spec(samples, BucketSpec::v1())
}

fn training_set_with_spec(
    samples: Vec<HazardSampleInput>,
    bucket_spec: BucketSpec,
) -> Result<HazardTrainingSet, hazard::HazardError> {
    HazardTrainingSet::try_new(HazardTrainingSetInput {
        feature_schema: FeatureSchema::try_new(
            1,
            vec!["realized_volatility".to_owned(), "spread_bps".to_owned()],
        )
        .expect("fixture schema should be valid"),
        bucket_spec,
        cause_ids: vec![
            "liquidity_vacuum".to_owned(),
            "downside_transition".to_owned(),
        ],
        cause_definition_hashes: vec![[1; 32], [2; 32]],
        training_cutoff_ns: 100_000 * SECOND_NS,
        samples,
    })
}

#[test]
fn production_bucket_version_changes_training_identity_and_risk_set_expansion() {
    let sample = available_sample(
        1,
        1,
        HazardOutcome::RightCensored {
            observed_seconds: 900,
        },
    );
    let legacy = training_set_with_spec(vec![sample.clone()], BucketSpec::v1())
        .expect("legacy training set");
    let production = training_set_with_spec(vec![sample], BucketSpec::production_v2())
        .expect("production training set");

    assert_ne!(legacy.evidence_id(), production.evidence_id());
    assert_eq!(augment_design(&legacy).expect("legacy rows").len(), 1);
    assert_eq!(
        augment_design(&production).expect("production rows").len(),
        15
    );
}

#[test]
fn event_and_censoring_expand_only_while_the_sample_is_at_risk() {
    let data = training_set(vec![
        available_sample(
            1,
            1,
            HazardOutcome::Event {
                offset_seconds: 1_000,
                cause_index: 1,
            },
        ),
        available_sample(
            2,
            2,
            HazardOutcome::RightCensored {
                observed_seconds: 3_600,
            },
        ),
    ])
    .expect("fixture should satisfy the point-in-time contract");

    let rows = augment_design(&data).expect("bounded data should expand");

    assert_eq!(rows.len(), 4);
    assert_eq!(
        rows.iter()
            .map(|row| (row.sample_id(), row.bucket_index(), row.target()))
            .collect::<Vec<_>>(),
        vec![
            (1, 0, BucketTarget::Survival),
            (1, 1, BucketTarget::Cause(1)),
            (2, 0, BucketTarget::Survival),
            (2, 1, BucketTarget::Survival),
        ]
    );
}

#[test]
fn future_known_or_degraded_samples_are_rejected_not_relabelled_as_survival() {
    let mut future_known = available_sample(
        1,
        1,
        HazardOutcome::RightCensored {
            observed_seconds: 900,
        },
    );
    future_known.feature_as_known_at_ns = future_known.origin_time_ns + 1;
    assert!(training_set(vec![future_known]).is_err());

    let mut degraded = available_sample(
        1,
        1,
        HazardOutcome::RightCensored {
            observed_seconds: 900,
        },
    );
    degraded.quality = InputQuality::Degraded;
    assert!(training_set(vec![degraded]).is_err());
}

#[test]
fn invalid_event_cause_and_insufficient_censoring_are_rejected() {
    assert!(
        training_set(vec![available_sample(
            1,
            1,
            HazardOutcome::Event {
                offset_seconds: 900,
                cause_index: 2,
            },
        )])
        .is_err()
    );
    assert!(
        training_set(vec![available_sample(
            1,
            1,
            HazardOutcome::RightCensored {
                observed_seconds: 899,
            },
        )])
        .is_err()
    );
}

#[test]
fn schema_order_and_lineage_are_bound_into_training_evidence() {
    let original = training_set(vec![available_sample(
        1,
        1,
        HazardOutcome::RightCensored {
            observed_seconds: 900,
        },
    )])
    .expect("fixture should be valid");

    let mut changed_input = available_sample(
        1,
        1,
        HazardOutcome::RightCensored {
            observed_seconds: 900,
        },
    );
    changed_input.label_lineage = [99; 32];
    let changed = training_set(vec![changed_input]).expect("changed lineage remains valid");

    assert_ne!(original.evidence_id(), changed.evidence_id());
    assert_eq!(
        original.feature_schema().feature_ids(),
        &["realized_volatility", "spread_bps"]
    );
}

#[test]
fn nonfinite_features_and_zero_lineage_fail_closed() {
    let mut nonfinite = available_sample(
        1,
        1,
        HazardOutcome::RightCensored {
            observed_seconds: 900,
        },
    );
    nonfinite.features[0] = f64::NAN;
    assert!(training_set(vec![nonfinite]).is_err());

    let mut no_lineage = available_sample(
        1,
        1,
        HazardOutcome::RightCensored {
            observed_seconds: 900,
        },
    );
    no_lineage.feature_lineage = [0; 32];
    assert!(training_set(vec![no_lineage]).is_err());
}
